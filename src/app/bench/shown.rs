//! What a block showed (SPEC §9.6 `expect`, §10.2 `ScreenChanged`): before its warmup runs and after
//! its measured runs, outside every timed span, strobe reads what was on screen: the window titles,
//! the dialogs and headings of the accessibility tree (a page's title, address and headings), and a
//! perceptual hash of the window's pixels (a small thumbnail kept for the first block of a variant and
//! for any block that looked different). A bench measured on the wrong screen (a first-run dialog over
//! the pane it meant) then says so, and a case's `expect` refuses such a block instead of timing it.

use super::*;
use crate::driver::native::Session;

/// How far apart two perceptual hashes (of 64 bits) are before two windows count as different screens.
const DIFFERENT_BITS: u32 = 20;

/// Thumbnails are at most this wide.
const THUMB_WIDTH: usize = 320;

/// One moment of a block: what its window or page showed.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct Screen {
    /// The app's visible windows' titles, main first, or the page's title.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
    /// A page's address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Open dialogs, then headings, as the accessibility tree (or the page) names them: at most 6.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<String>,
    /// A 64-bit difference hash of the window's pixels (hex).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// A thumbnail of the window (a PNG in the experiment's folder), when one was kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
    /// What strobe could not read, and why (a permission, a window that did not draw itself).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

/// A block's screen before its runs (`start`) and after them (`end`). A launch has only `end`.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct Shown {
    pub start: Screen,
    pub end: Screen,
}

fn short(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 60 { format!("{}…", s.chars().take(59).collect::<String>()) } else { s }
}

/// A page address without its origin (base and head serve on different ports), query or fragment.
fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest.find('/').map_or("/", |i| &rest[i..]);
    path.split(['?', '#']).next().unwrap_or("/").to_string()
}

impl Screen {
    /// `App Settings · General`: the first title (a page without one: its path), then up to two
    /// headings that differ from it.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = self.titles.iter().find(|t| !t.trim().is_empty()) {
            parts.push(short(t));
        } else if let Some(u) = &self.url {
            parts.push(path_of(u));
        }
        for h in self.headings.iter().map(|h| short(h)).filter(|h| !h.is_empty()) {
            if parts.len() >= 3 {
                break;
            }
            if !parts.iter().any(|p| p.eq_ignore_ascii_case(&h)) {
                parts.push(h);
            }
        }
        parts.join(" · ")
    }

    /// What two screens are compared by: the label, and a page's path.
    fn key(&self) -> String {
        format!("{}\n{}", self.label(), self.url.as_deref().map(path_of).unwrap_or_default())
    }

    fn bits(&self) -> Option<u64> {
        u64::from_str_radix(self.hash.as_deref()?, 16).ok()
    }

    fn is_empty(&self) -> bool {
        self.titles.is_empty() && self.headings.is_empty() && self.url.is_none() && self.hash.is_none()
    }
}

/// A difference hash: the image as 9 × 8 grey cells, one bit per cell brighter than its right neighbour.
fn dhash(img: &png::Image) -> Option<u64> {
    let (w, h) = (img.width, img.height);
    if w < 9 || h < 8 {
        return None;
    }
    let mut cells = [[0f64; 9]; 8];
    for (cy, row) in cells.iter_mut().enumerate() {
        for (cx, cell) in row.iter_mut().enumerate() {
            let (x0, x1, y0, y1) = (cx * w / 9, ((cx + 1) * w / 9).max(cx * w / 9 + 1), cy * h / 8, ((cy + 1) * h / 8).max(cy * h / 8 + 1));
            let (mut sum, mut n) = (0f64, 0f64);
            for y in (y0..y1).step_by(2) {
                for x in (x0..x1).step_by(2) {
                    let p = &img.rgba[(y * w + x) * 4..(y * w + x) * 4 + 3];
                    sum += 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
                    n += 1.0;
                }
            }
            *cell = sum / n.max(1.0);
        }
    }
    let mut bits = 0u64;
    for row in &cells {
        for x in 0..8 {
            bits = bits << 1 | (row[x] > row[x + 1]) as u64;
        }
    }
    Some(bits)
}

/// The image shrunk (box filter) to at most `THUMB_WIDTH` wide.
fn thumbnail(img: &png::Image) -> png::Image {
    let scale = (img.width as f64 / THUMB_WIDTH as f64).max(1.0);
    let (w, h) = (((img.width as f64 / scale).round() as usize).max(1), ((img.height as f64 / scale).round() as usize).max(1));
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let (y0, y1) = (y * img.height / h, ((y + 1) * img.height / h).max(y * img.height / h + 1));
        for x in 0..w {
            let (x0, x1) = (x * img.width / w, ((x + 1) * img.width / w).max(x * img.width / w + 1));
            let mut sum = [0u32; 4];
            let mut n = 0u32;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let p = &img.rgba[(yy * img.width + xx) * 4..(yy * img.width + xx) * 4 + 4];
                    for c in 0..4 {
                        sum[c] += p[c] as u32;
                    }
                    n += 1;
                }
            }
            rgba.extend(sum.iter().map(|s| (s / n.max(1)) as u8));
        }
    }
    png::Image { width: w, height: h, rgba }
}

/// Hashes the pixels, and keeps a thumbnail for the first block of a series and variant and for any
/// block that showed something else than that one did (`ctx.shown_seen`).
fn finish(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, at: &str, mut screen: Screen, img: Option<png::Image>) -> Screen {
    let bits = img.as_ref().and_then(dhash);
    screen.hash = bits.map(|b| format!("{b:016x}"));
    let key = format!("{}\n{}\n{at}", s.label(), v.role);
    let first = ctx.shown_seen.get(&key).cloned();
    let other = match &first {
        None => true,
        Some((label, hash)) => *label != screen.key() || matches!((hash, bits), (Some(a), Some(b)) if (a ^ b).count_ones() >= DIFFERENT_BITS),
    };
    if first.is_none() {
        ctx.shown_seen.insert(key, (screen.key(), bits));
    }
    if other && let Some(img) = &img {
        let slug: String = s.label().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let pair = if pair == usize::MAX { "x".to_string() } else { pair.to_string() };
        let file = store::experiment_dir(&ctx.exp).map(|d| d.join("shown").join(format!("{slug}-{}-{pair}-{at}.png", v.role)));
        if let Ok(file) = file
            && let Ok(bytes) = png::write(&thumbnail(img))
            && std::fs::create_dir_all(file.parent().unwrap_or(Path::new("."))).is_ok()
            && std::fs::write(&file, bytes).is_ok()
        {
            screen.thumb = Some(file.display().to_string());
        }
    }
    screen
}

/// Why a native window's pixels could not be read.
fn no_pixels() -> String {
    #[cfg(target_os = "macos")]
    if !host::macos::screen_capture_allowed() {
        return format!("pixels: Screen Recording is not granted to {}", host::macos::screen_recording_grantee());
    }
    "pixels: the window did not draw itself into an image".to_string()
}

/// What a native app shows now: its windows' titles and pixels, and with a driving session its
/// accessibility tree's dialogs and headings (`session` is why there is none, when there is none).
#[allow(clippy::too_many_arguments)]
pub(super) fn native(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, at: &str, pids: &[u32], main: Option<isize>, session: std::result::Result<&Session, String>) -> Screen {
    let mut screen = Screen::default();
    let windows = host::top_windows(pids);
    let mut titles: Vec<String> = windows.iter().filter(|w| Some(w.0) == main).chain(windows.iter().filter(|w| Some(w.0) != main)).map(|w| w.1.trim().to_string()).filter(|t| !t.is_empty()).collect();
    match session {
        Ok(se) => match se.screen(Duration::from_secs(3)) {
            Ok((names, headings)) => {
                for n in names {
                    if !titles.contains(&n) {
                        titles.push(n);
                    }
                }
                screen.headings = headings;
            }
            Err(e) => screen.missing.push(format!("accessibility: {}", e.message)),
        },
        Err(why) => screen.missing.push(format!("accessibility: {why}")),
    }
    screen.titles = titles;
    let img = main.and_then(host::window_image);
    if img.is_none() {
        screen.missing.push(no_pixels());
    }
    finish(ctx, s, v, pair, at, screen, img)
}

/// What a page shows now: its title, address, open dialogs and top headings, and a screenshot.
pub(super) fn page(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, at: &str, page: &Page) -> Screen {
    let mut screen = Screen::default();
    match crate::driver::screen(page, 6, 0, Duration::from_secs(5)).ok() {
        Some(p) => {
            screen.titles = p["title"].as_str().filter(|t| !t.trim().is_empty()).map(|t| vec![t.trim().to_string()]).unwrap_or_default();
            screen.url = p["url"].as_str().map(str::to_string);
            screen.headings = p["headings"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        }
        None => screen.missing.push("page: it did not answer for its title and headings in 5 s".into()),
    }
    let img = page.screenshot(Duration::from_secs(5)).ok().and_then(|b| png::read(&b).ok());
    if img.is_none() {
        screen.missing.push("pixels: the page gave no screenshot in 5 s".into());
    }
    finish(ctx, s, v, pair, at, screen, img)
}

/// What a run starts from that its input acts on (`ResetIncomplete`, SPEC §10.1), by control: the value
/// of every visible, enabled, writable editable control (text fields, search and combo boxes, spin
/// buttons, sliders, editing hosts; a secure field by its length) and the state of every check box,
/// radio button and switch. Nothing else: labels, counters and clocks an app rewrites by itself between
/// runs, and screens (headings, dialogs) are `WrongScreen`'s and `ScreenChanged`'s. Focus is left out
/// too: every click moves it, and where it rests changes a run's work only when the run then types
/// without naming a control, which shows in a value.
pub(super) type Start = BTreeMap<String, String>;

/// Values longer than this are cut (a run that appends its text grows them on every run).
const START_VALUE: usize = 200;

fn cut(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > START_VALUE { format!("{}…", s.chars().take(START_VALUE).collect::<String>()) } else { s.to_string() }
}

/// A native app's start, from one read of its tree through the case's own driving session, and the
/// count of changes the app had announced then (to wait for the next one).
pub(super) fn native_start(se: &Session) -> Option<(Start, u64)> {
    use crate::target::a11y::{PATTERN_TOGGLE, role_name};
    let (tree, seen_changes) = se.tree().ok()?;
    let mut out = Start::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut put = |key: String, v: String| {
        let n = seen.entry(key.clone()).or_default();
        *n += 1;
        out.insert(if *n == 1 { key } else { format!("{key} #{n}") }, v);
    };
    for (i, n) in tree.nodes.iter().enumerate() {
        // Shown (not offscreen, with a box; a scroll position that clips it does not count) and enabled.
        if n.offscreen || n.rect[2] <= 0.0 || n.rect[3] <= 0.0 || !n.enabled {
            continue;
        }
        let role = if role_name(n.role).is_empty() { n.role_text.as_str() } else { role_name(n.role) };
        let key = || {
            let path = tree.key_path(i);
            if path.is_empty() { format!("{role} {:?}", n.name.split_whitespace().collect::<Vec<_>>().join(" ")) } else { path }
        };
        let toggle = matches!(role, "checkbox" | "radio") || (n.role == 50000 && n.patterns & PATTERN_TOGGLE != 0);
        if matches!(role, "textbox" | "combobox" | "spinbutton" | "slider") {
            let value = n.value.as_deref().unwrap_or("");
            // A secure field (AXSecureTextField, a password box) by its length only.
            let secure = n.role_text.to_ascii_lowercase().contains("secure") || n.role_text.to_ascii_lowercase().contains("password");
            put(key(), if secure { format!("{} characters", value.chars().count()) } else { format!("{:?}", cut(value)) });
        } else if toggle && let Some(t) = n.toggled {
            put(key(), if t { "checked" } else { "unchecked" }.into());
        }
    }
    Some((out, seen_changes))
}

/// A page's start, through the page probe (`driver::state`).
pub(super) fn page_start(page: &Page) -> Option<Start> {
    crate::driver::state(page, Duration::from_secs(5)).ok()
}

/// Every run's start, checked against the first one read: a run that starts from another state measures
/// other work (a reset that took back nothing, so each run appended its text again).
pub(super) struct Starts {
    first: Option<(String, Start)>,
    /// Controls whose value the app changed by itself while the first start was read, with no input
    /// (a level meter, a clock in a field): no reset governs them, so they are not compared.
    live: std::collections::BTreeSet<String>,
    on: bool,
}

impl Starts {
    /// Off for a case with `resetCheck: false`, and for blocks no verdict comes from in the way a run's
    /// start matters: attribution, leak runs (a leak case may grow what it leaves on purpose) and
    /// pixel blocks (`--pixels`).
    pub(super) fn new(s: &Series, x: &Exec) -> Starts {
        Starts { first: None, live: Default::default(), on: s.reset_check && !matches!(x.set, Set::Attribution | Set::Memory) && x.pixels.is_none() }
    }

    /// The first start, read until it holds still, so a tree a lazy provider is still building, a
    /// toggle whose state the app has not read yet, or a page still filling in is not what every run is
    /// held to (an idle case refused a switch that was "not there" before run 1 and there after it).
    /// Read every 200 ms until every control has held its value (or absence) for `HOLD` since it last
    /// changed, for at least `HOLD` and at most `SETTLE_MAX`. With `find_live` (before run 1 only: no
    /// run has left anything running yet), a control whose value changed by itself in two different
    /// intervals is live (`live`: a level meter, a clock in a field) and left out. One change is
    /// hydration or a debounced update, not a live control: it is read on until it holds, and compared.
    fn reference(&mut self, read: &dyn Fn() -> Option<Start>, find_live: bool) -> Option<Start> {
        // Longer than a 1 Hz field takes to tick twice.
        const HOLD: Duration = Duration::from_millis(1200);
        const SETTLE_MAX: Duration = Duration::from_secs(4);
        let started = Instant::now();
        let mut now = read()?;
        let mut changed_at: BTreeMap<String, Instant> = BTreeMap::new();
        let mut changes: BTreeMap<String, u32> = BTreeMap::new();
        loop {
            let held = changed_at.iter().all(|(k, t)| self.live.contains(k) || t.elapsed() >= HOLD);
            if (held && started.elapsed() >= HOLD) || started.elapsed() >= SETTLE_MAX {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            let Some(again) = read() else { break };
            let keys: std::collections::BTreeSet<&String> = now.keys().chain(again.keys()).collect();
            for k in keys {
                let (a, b) = (now.get(k), again.get(k));
                if a == b {
                    continue;
                }
                changed_at.insert(k.clone(), Instant::now());
                // A value that became another value (not one that appeared or went) is the app's own
                // change; twice is live.
                if find_live && a.is_some() && b.is_some() {
                    let n = changes.entry(k.clone()).or_default();
                    *n += 1;
                    if *n >= 2 {
                        self.live.insert(k.clone());
                    }
                }
            }
            now = again;
        }
        Some(now)
    }

    /// `s` without the live controls.
    fn held(&self, s: &Start) -> Start {
        s.iter().filter(|(k, _)| !self.live.contains(*k)).map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub(super) fn on(&self) -> bool {
        self.on
    }

    /// Before run `run` (1-based); `after` true: after the last run and its reset, where a run after it
    /// would start (so a single `--check` run is checked too). `read` reads the start (None: it could not
    /// be read); `wait(t)` returns at the target's next change or after `t`. A start that differs is read
    /// again at each change (and at least every 250 ms, for a value set without an event) for up to a
    /// second, then refuses the block; a read that fails there counts as differing. A start that cannot
    /// be read before any was is skipped: the first start read (`reference`) is what the others are
    /// compared with, without its live controls (found only when that read is before run 1).
    pub(super) fn check(&mut self, run: usize, after: bool, read: &dyn Fn() -> Option<Start>, wait: &dyn Fn(Duration)) -> Result<()> {
        if !self.on {
            return Ok(());
        }
        if self.first.is_none() {
            if let Some(now) = self.reference(read, run == 1 && !after) {
                self.first = Some((format!("run {run}"), self.held(&now)));
            }
            return Ok(());
        }
        let Some((first_when, first)) = &self.first else { return Ok(()) };
        let read = || read().map(|s| self.held(&s));
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut now = read();
        while now.as_ref() != Some(first) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            wait(left.min(Duration::from_millis(250)));
            now = read();
        }
        const RESET: &str = "every run must start from the same state: the case's reset has to take back what its run changes (add a reset if it has none); select-all is Meta+a on macOS and Control+a elsewhere: ControlOrMeta+a works on every OS. A case whose runs build on each other on purpose sets resetCheck: false";
        const LOADING: &str = "if the screen was still loading when run 1 started, wait in the case's setup until it has (page.waitFor the control, or page.idle()); if the run opens what holds them, its reset has to close it. A case whose runs build on each other on purpose sets resetCheck: false";
        // Controls whose value changed (the reset did not restore them), those that appeared (the screen
        // was still loading at the first start, or the run left something open) and those that went.
        let (mut changed, mut appeared, mut gone) = (Vec::new(), Vec::new(), Vec::new());
        match &now {
            Some(n) if n == first => return Ok(()),
            None => changed.push("its state could not be read again".to_string()),
            Some(n) => {
                let keys: std::collections::BTreeSet<&String> = first.keys().chain(n.keys()).collect();
                for k in keys {
                    match (first.get(k), n.get(k)) {
                        (Some(a), Some(b)) if a != b => changed.push(format!("{k} {b} (was {a})")),
                        (None, Some(b)) => appeared.push(format!("{k} {b}")),
                        (Some(a), None) => gone.push(format!("{k} (was {a})")),
                        _ => {}
                    }
                }
            }
        }
        let when = if after { format!("a run after the last (run {}) would start", run.saturating_sub(1)) } else { format!("run {run} started") };
        let list = |v: &[String]| {
            let more = if v.len() > 3 { format!(" and {} more", v.len() - 3) } else { String::new() };
            format!("{}{more}", v.iter().take(3).cloned().collect::<Vec<_>>().join("; "))
        };
        let mut parts = Vec::new();
        if !changed.is_empty() {
            parts.push(format!("the reset did not restore {}", list(&changed)));
        }
        if !appeared.is_empty() {
            parts.push(format!("not there at {first_when} (still loading then, or left open by the run): {}", list(&appeared)));
        }
        if !gone.is_empty() {
            parts.push(format!("gone since {first_when}: {}", list(&gone)));
        }
        let fix = if changed.is_empty() && gone.is_empty() { LOADING } else { RESET };
        Err(Error::refused("ResetIncomplete", format!("{when} from a different state than {first_when} did: {}", parts.join(" · ")), fix))
    }
}

/// Whether a selector matches a visible element of the page within 5 s, by the driver's own probe
/// (what a `page.waitFor` would see).
pub(super) fn page_shows(page: &Page, sel: &Value) -> Result<bool> {
    let sel = crate::target::a11y::Selector::parse(sel, crate::target::a11y::Dialect::Page)?;
    crate::driver::shows(page, &sel, Duration::from_secs(5)).map_err(|e| Error::usage(format!("expect.visible {:?}: {}", sel.source, e.message)))
}

/// A string (a substring) or a RegExp (`{ regex: { source, flags } }`) from the bench file.
type Matcher = Box<dyn Fn(&str) -> bool>;

fn matcher(v: &Value) -> Result<Matcher> {
    if let Some(s) = v.as_str() {
        let s = s.to_string();
        return Ok(Box::new(move |t: &str| t.contains(&s)));
    }
    let src = v["regex"]["source"].as_str().ok_or_else(|| Error::usage(format!("expect: a string or a RegExp, not {v}")))?;
    let flags = v["regex"]["flags"].as_str().unwrap_or("");
    let re = regex_lite::Regex::new(&format!("{}{src}", if flags.contains('i') { "(?i)" } else { "" })).map_err(|e| Error::usage(format!("expect: /{src}/: {e}")))?;
    Ok(Box::new(move |t: &str| re.is_match(t)))
}

fn shown_text(v: &Value) -> String {
    v.as_str().map(|s| format!("{s:?}")).unwrap_or_else(|| format!("/{}/{}", v["regex"]["source"].as_str().unwrap_or(""), v["regex"]["flags"].as_str().unwrap_or("")))
}

/// The case's (or its target's) `expect`: `title` (any window title, or the page's), `heading` (an
/// open dialog or a heading), `url` (a page's address) and `visible` (selectors that must show). With
/// `full` false (before a native app's runs, with no driving session yet) only titles are checked:
/// reading the tree there would connect strobe to the app before the runs. A screen that fails is
/// `WrongScreen`, which refuses the block.
pub(super) fn expect(e: &Value, screen: &Screen, at: &str, full: bool, visible: &dyn Fn(&Value) -> Result<bool>) -> Result<()> {
    if !e.is_object() {
        return Ok(());
    }
    let what = if screen.url.is_some() { "page" } else { "app" };
    let showed = || {
        let l = screen.label();
        let missing = if screen.missing.is_empty() { String::new() } else { format!(" (not read: {})", screen.missing.join("; ")) };
        if l.is_empty() { format!("strobe could read nothing it showed{missing}") } else { format!("it showed {l:?}{missing}") }
    };
    let refuse = |want: String| {
        Err(Error::refused(
            "WrongScreen",
            format!("{at}, the {what} was not on the screen the bench expects: {want}, but {}", showed()),
            "a first-run dialog, another pane or a page the case did not mean was showing: fix the setup (or the app's seeded state) so the block shows what the case measures, or correct its expect",
        ))
    };
    if !e["title"].is_null() {
        let m = matcher(&e["title"])?;
        if !screen.titles.iter().any(|t| m(t)) {
            return refuse(format!("a title matching {}", shown_text(&e["title"])));
        }
    }
    if !e["url"].is_null() && screen.url.is_some() {
        let m = matcher(&e["url"])?;
        if !screen.url.as_deref().is_some_and(&*m) {
            return refuse(format!("an address matching {}", shown_text(&e["url"])));
        }
    }
    if !full {
        return Ok(());
    }
    if !e["heading"].is_null() {
        let m = matcher(&e["heading"])?;
        if !screen.headings.iter().any(|h| m(h)) {
            return refuse(format!("a dialog or heading matching {}", shown_text(&e["heading"])));
        }
    }
    let sels: Vec<Value> = match &e["visible"] {
        Value::Null => Vec::new(),
        Value::Array(a) => a.clone(),
        v => vec![v.clone()],
    };
    for sel in &sels {
        if !visible(sel)? {
            return refuse(format!("{} visible", shown_text(sel.get("selector").unwrap_or(sel))));
        }
    }
    Ok(())
}

/// One variant's screens over its blocks: the end screen most of them showed, the others with how
/// many blocks showed each, the first block's start screen, hash and thumbnail.
fn side(blocks: &[&Shown]) -> Value {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for b in blocks {
        let l = b.end.label();
        match counts.iter_mut().find(|(x, _)| *x == l) {
            Some(c) => c.1 += 1,
            None => counts.push((l, 1)),
        }
    }
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    let first = blocks.first().map(|b| &b.end);
    let data_url = |s: Option<&Screen>| {
        s.and_then(|f| f.thumb.as_deref()).and_then(|p| std::fs::read(p).ok()).map(|b| {
            use base64::Engine;
            format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(b))
        })
    };
    let thumb_end = data_url(first);
    // The first block's screens before its warmup and after its runs, for a side-by-side look at both
    // implementations (SPEC §11.2); a launch has no start.
    let thumb_start = data_url(blocks.first().map(|b| &b.start));
    // Pixels that moved far from the first block's, where no text tells the screens apart.
    let far = blocks.iter().filter(|b| matches!((first.and_then(Screen::bits), b.end.bits()), (Some(a), Some(x)) if (a ^ x).count_ones() >= DIFFERENT_BITS)).count();
    json!({
        "start": blocks.first().map(|b| b.start.label()).filter(|l| !l.is_empty()),
        "end": counts.first().map(|c| c.0.clone()).filter(|l| !l.is_empty()),
        "blocks": blocks.len(),
        "others": counts.iter().skip(1).map(|(l, n)| json!({ "label": l, "blocks": n })).collect::<Vec<_>>(),
        "looked_apart": far,
        "hash": first.and_then(|f| f.hash.clone()),
        "thumb_start": thumb_start,
        "thumb_end": thumb_end,
        "thumb_path": first.and_then(|f| f.thumb.clone()),
        "missing": first.map(|f| f.missing.clone()).unwrap_or_default(),
    })
}

/// A series' screens per variant, and `ScreenChanged` when base and head (or blocks of one variant)
/// ended their runs on different screens. Two implementations (`across`, SPEC 9.10) draw their own
/// titles and screens by design: only each side's blocks are compared with each other.
pub(super) fn summarize(blocks: &[Block], across: bool) -> (Value, Vec<Warning>) {
    let mut out = serde_json::Map::new();
    let mut warnings = Vec::new();
    let mut roles: Vec<&str> = Vec::new();
    for b in blocks {
        if b.shown.is_some() && !roles.contains(&b.variant.as_str()) {
            roles.push(&b.variant);
        }
    }
    for role in &roles {
        let mine: Vec<&Shown> = blocks.iter().filter(|b| b.variant == *role).filter_map(|b| b.shown.as_ref()).filter(|s| !s.end.is_empty()).collect();
        if mine.is_empty() {
            continue;
        }
        let v = side(&mine);
        let others: Vec<String> = v["others"].as_array().into_iter().flatten().map(|o| format!("{} on {:?}", o["blocks"], o["label"].as_str().unwrap_or(""))).collect();
        if !others.is_empty() {
            let rest: usize = v["others"].as_array().into_iter().flatten().filter_map(|o| o["blocks"].as_u64()).sum::<u64>() as usize;
            warnings.push(validity::warn(
                "ScreenChanged",
                format!(
                    "{role}: {} of {} blocks ended their runs on {:?}, the others {}: those blocks measured another screen",
                    mine.len().saturating_sub(rest),
                    mine.len(),
                    v["end"].as_str().unwrap_or(""),
                    others.join(", ")
                ),
            ));
        } else if v["end"].is_null() && v["looked_apart"].as_u64().unwrap_or(0) > 0 {
            warnings.push(validity::warn(
                "ScreenChanged",
                format!(
                    "{role}: {} of {} blocks looked different from its first at the end of their runs (perceptual hash); strobe read no title or heading to say what they showed",
                    v["looked_apart"],
                    mine.len()
                ),
            ));
        }
        out.insert(role.to_string(), v);
    }
    if let (false, Some(b), Some(h)) = (across, out.get("base"), out.get("head")) {
        let (bl, hl) = (b["end"].as_str(), h["end"].as_str());
        let changed = match (bl, hl) {
            (Some(x), Some(y)) => x != y,
            (None, None) => {
                matches!((b["hash"].as_str().and_then(|x| u64::from_str_radix(x, 16).ok()), h["hash"].as_str().and_then(|x| u64::from_str_radix(x, 16).ok())), (Some(x), Some(y)) if (x ^ y).count_ones() >= DIFFERENT_BITS)
            }
            _ => false,
        };
        if changed {
            let what = match (bl, hl) {
                (Some(x), Some(y)) => format!("base ended its runs on {x:?} and head on {y:?}"),
                _ => "base and head looked different at the end of their runs (perceptual hashes far apart; strobe read no title or heading)".to_string(),
            };
            warnings.push(validity::warn(
                "ScreenChanged",
                format!("{what}: the builds measured different screens, so the difference may be the screen, not the change; make setup bring both to the same screen, or add expect: {{ title, heading or visible }} to the case"),
            ));
        }
        out.insert("changed".into(), json!(changed));
    }
    (Value::Object(out), warnings)
}

/// One variant's screen in a line: `'App · General'`, or `'Welcome' → 'App · General'` when the runs
/// moved it.
fn side_text(v: &Value) -> Option<String> {
    let end = v["end"].as_str()?;
    Some(match v["start"].as_str().filter(|s| *s != end) {
        Some(start) => format!("'{start}' → '{end}'"),
        None => format!("'{end}'"),
    })
}

/// What a series measured on screen, for the text output, the PR block and the viewer: `'App ·
/// General'`, or `base '…' · head '…'` when they differed, each side by `names` (an implementation
/// comparison's side names). None when strobe read nothing.
pub fn shown_line(series: &Value, names: (&str, &str)) -> Option<String> {
    let sh = &series["shown"];
    let (b, h) = (side_text(&sh["base"]), side_text(&sh["head"]));
    match (b, h) {
        (Some(b), Some(h)) if b != h => Some(format!("{} {b} · {} {h}", names.0, names.1)),
        (Some(x), _) | (None, Some(x)) => Some(x),
        _ => None,
    }
}
