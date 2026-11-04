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
    /// `Hex Settings · General`: the first title (a page without one: its path), then up to two
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
    match page.eval_timeout(PAGE_SCREEN, Duration::from_secs(5)).ok().and_then(|v| v.as_str().and_then(|t| serde_json::from_str::<Value>(t).ok())) {
        Some(p) => {
            screen.titles = p["title"].as_str().filter(|t| !t.trim().is_empty()).map(|t| vec![t.trim().to_string()]).unwrap_or_default();
            screen.url = p["url"].as_str().map(str::to_string);
            screen.headings = p["headings"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        }
        None => screen.missing.push("page: it did not answer for its title and headings in 5 s".into()),
    }
    use base64::Engine;
    let img = page
        .send_timeout("Page.captureScreenshot", json!({ "format": "png" }), Duration::from_secs(5))
        .ok()
        .and_then(|r| base64::engine::general_purpose::STANDARD.decode(r["data"].as_str()?).ok())
        .and_then(|b| png::read(&b).ok());
    if img.is_none() {
        screen.missing.push("pixels: the page gave no screenshot in 5 s".into());
    }
    finish(ctx, s, v, pair, at, screen, img)
}

/// The page's side of `page`: what a person sees at the top of it.
const PAGE_SCREEN: &str = r#"(() => {
  const shown = (e) => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== "hidden" }
  const text = (e) => (e.getAttribute("aria-label") || e.innerText || e.textContent || "").trim().replace(/\s+/g, " ").slice(0, 80)
  const out = []
  for (const d of document.querySelectorAll("dialog[open], [role=dialog], [role=alertdialog], [aria-modal=true]"))
    if (shown(d)) out.push(text(d.querySelector("h1, h2, h3, [role=heading]") || d))
  for (const h of document.querySelectorAll("h1, h2, [role=heading][aria-level='1'], [role=heading][aria-level='2']"))
    if (shown(h)) out.push(text(h))
  return JSON.stringify({ title: document.title, url: location.href, headings: [...new Set(out.filter(Boolean))].slice(0, 6) })
})()"#;

/// Whether a CSS selector matches a visible element of the page, within 5 s.
pub(super) fn page_shows(page: &Page, sel: &Value) -> Result<bool> {
    let sel = sel.as_str().ok_or_else(|| Error::usage(format!("expect.visible on a page is a CSS selector string, not {sel}")))?;
    let expr = format!("(() => {{ const e = document.querySelector({}); if (!e) return false; const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 }})()", json!(sel));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if page.eval_timeout(&expr, Duration::from_secs(5)).map_err(|e| Error::usage(format!("expect.visible {sel:?}: {}", e.message)))? == Value::Bool(true) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
    let thumb = first.and_then(|f| f.thumb.as_deref()).and_then(|p| std::fs::read(p).ok()).map(|b| {
        use base64::Engine;
        format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(b))
    });
    // Pixels that moved far from the first block's, where no text tells the screens apart.
    let far = blocks.iter().filter(|b| matches!((first.and_then(Screen::bits), b.end.bits()), (Some(a), Some(x)) if (a ^ x).count_ones() >= DIFFERENT_BITS)).count();
    json!({
        "start": blocks.first().map(|b| b.start.label()).filter(|l| !l.is_empty()),
        "end": counts.first().map(|c| c.0.clone()).filter(|l| !l.is_empty()),
        "blocks": blocks.len(),
        "others": counts.iter().skip(1).map(|(l, n)| json!({ "label": l, "blocks": n })).collect::<Vec<_>>(),
        "looked_apart": far,
        "hash": first.and_then(|f| f.hash.clone()),
        "thumb": thumb,
        "thumb_path": first.and_then(|f| f.thumb.clone()),
        "missing": first.map(|f| f.missing.clone()).unwrap_or_default(),
    })
}

/// A series' screens per variant, and `ScreenChanged` when base and head (or blocks of one variant)
/// ended their runs on different screens.
pub(super) fn summarize(blocks: &[Block]) -> (Value, Vec<Warning>) {
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
    if let (Some(b), Some(h)) = (out.get("base"), out.get("head")) {
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

/// One variant's screen in a line: `'Hex · General'`, or `'Welcome' → 'Hex · General'` when the runs
/// moved it.
fn side_text(v: &Value) -> Option<String> {
    let end = v["end"].as_str()?;
    Some(match v["start"].as_str().filter(|s| *s != end) {
        Some(start) => format!("'{start}' → '{end}'"),
        None => format!("'{end}'"),
    })
}

/// What a series measured on screen, for the text output, the PR block and the viewer: `'Hex ·
/// General'`, or `base '…' · head '…'` when they differed. None when strobe read nothing.
pub fn shown_line(series: &Value) -> Option<String> {
    let sh = &series["shown"];
    let (b, h) = (side_text(&sh["base"]), side_text(&sh["head"]));
    match (b, h) {
        (Some(b), Some(h)) if b != h => Some(format!("base {b} · head {h}")),
        (Some(x), _) | (None, Some(x)) => Some(x),
        _ => None,
    }
}
