//! OS input to a page measured from outside (SPEC §9.10, §13.6): against a native implementation a
//! page gets the input the native side gets, from the OS's own posters (`a11y::input`: posted window
//! messages on Windows, Quartz events on macOS, XTest on X11), never DevTools. Elements are still
//! found through the page probe; their page point goes to the screen through a map strobe measures
//! at the block's start, from where two posted pointer moves landed in the page (which also proves
//! posted input reaches it). Each kind of input is checked again as the block goes: a key, button or
//! wheel strobe posted that no trusted event in the page matched refuses the block `InputMismatch`
//! instead of timing input the page never got. Interaction latency then follows the native rule
//! (SPEC §7.4), with the page's own changes (`CHANGES_SCRIPT`: DOM mutations, input and scroll
//! events) standing in for an accessibility tree's. Chromium pages only for now.

use super::{Driver, no_selector};
use crate::driver::native::{Input, Log};
use crate::host;
use crate::model::{Error, Refusal, Result, ValidatorId};
use crate::probe::present::PresentCapture;
use crate::probe::present::frames::Sent;
use crate::target::a11y::input::{self, Button, Key, Mods, Named, Poster, Target};
use crate::target::a11y::keys;
use crate::target::web::WebPage as Page;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The block's OS present capture, shared with the block that reads its frames afterwards.
pub type Presents = Rc<RefCell<Option<PresentCapture>>>;

/// The page calls that give input: through the OS in this mode.
pub const INPUT: &[&str] = &["click", "hover", "move", "type", "press", "down", "up", "drag", "scroll", "wheel"];

/// Installed in every document before its scripts: counts the trusted keys, presses and wheel turns
/// that reached the page, and keeps the first pointer moves (where a posted move landed). Listeners
/// only, passive, at the window's capture phase ahead of the page's own.
const SCRIPT: &str = r#"window.__strobeOs ??= (() => {
  const s = { keys: 0, presses: 0, wheels: 0, moves: [] }
  const on = (e, f) => addEventListener(e, f, { capture: true, passive: true })
  on("keydown", (e) => { if (e.isTrusted) s.keys++ })
  on("pointerdown", (e) => { if (e.isTrusted) s.presses++ })
  on("wheel", (e) => { if (e.isTrusted) s.wheels++ })
  on("pointermove", (e) => { if (e.isTrusted && s.moves.length < 16) s.moves.push([e.clientX, e.clientY]) })
  return s
})(), true
//# sourceURL=strobe-os-input.js"#;

/// A window's frame and client size.
type Geometry = ((i32, i32, i32, i32), (i32, i32));

pub struct OsInput {
    poster: Poster,
    pids: RefCell<Vec<u32>>,
    /// The browser's window (keys go to it), and its frame and client size when strobe last mapped
    /// the page onto the screen: a window that moved, resized or was replaced is mapped again.
    top: Cell<isize>,
    geometry: Cell<Geometry>,
    /// Screen = origin + page point × scale (screen units: physical pixels, points on macOS).
    origin: Cell<(f64, f64)>,
    scale: Cell<(f64, f64)>,
    /// The display's refresh period under the window (ms), from its display path.
    frame_ms: Cell<f64>,
    presents: Presents,
    log: RefCell<Log>,
    last_input: Cell<u64>,
    /// Where the pointer is (screen point and its window), and the button `down` or `drag` holds.
    pointer: Cell<Option<(f64, f64, Target)>>,
    pressed: Cell<Option<Button>>,
    /// Key downs, button presses and wheel steps posted since the last check that they arrived, and
    /// the document they went to (`performance.timeOrigin`).
    posted: Cell<[u64; 3]>,
    document: Cell<f64>,
    parked: Cell<Option<bool>>,
    pacer: super::Pacer,
    /// The series reads `expect.same`, which shows whether a menu chord did on the page what it did on
    /// the other side (macOS, `press`).
    pub vouched: Cell<bool>,
}

fn refused(summary: String) -> Error {
    Refusal::new(ValidatorId::InputMismatch, summary)
        .fix("strobe gives a page measured against a native app the OS's input, as the native side gets; where the OS's posted input cannot reach the page, compare two pages or two native apps")
        .into()
}

fn locked() -> Error {
    Error::refused("SessionLocked", "OS input to the page: the session is locked, so the OS hit-tests every point to the lock screen", "run pointer and key journeys on an unlocked session")
}

impl OsInput {
    /// Measures where the page lies on the screen from two posted pointer moves, which must reach it.
    pub fn attach(page: &Page, pids: Vec<u32>, presents: Presents) -> Result<OsInput> {
        page.add_init_script(SCRIPT)?;
        page.eval(SCRIPT)?;
        let os = OsInput {
            poster: Poster::start()?,
            pids: RefCell::new(pids),
            top: Cell::new(0),
            geometry: Cell::new(((0, 0, 0, 0), (0, 0))),
            origin: Cell::new((0.0, 0.0)),
            scale: Cell::new((1.0, 1.0)),
            frame_ms: Cell::new(1000.0 / 60.0),
            presents,
            log: RefCell::new(Log::default()),
            last_input: Cell::new(0),
            pointer: Cell::new(None),
            pressed: Cell::new(None),
            posted: Cell::new([0; 3]),
            document: Cell::new(0.0),
            parked: Cell::new(None),
            pacer: super::Pacer::default(),
            vouched: Cell::new(false),
        };
        os.map(page)?;
        Ok(os)
    }

    /// Maps the page onto the screen again when its window moved, resized or was replaced since the
    /// last map (setup may do any of those): after setup, before the runs.
    pub fn recheck(&self, page: &Page) -> Result<()> {
        let w = host::main_window(&self.pids.borrow()).ok_or_else(|| Error::msg("OS input to the page: the browser shows no window"))?;
        if w.hwnd != self.top.get() || (w.rect, w.client) != self.geometry.get() { self.map(page) } else { Ok(()) }
    }

    /// Two points in the page's lower part, as screen points through the input thread's own
    /// (DPI-aware) coordinates, each posted as a move from just beside it; where the trusted
    /// `pointermove`s landed in the page gives the origin and scale, and proves posted input reaches it.
    fn map(&self, page: &Page) -> Result<()> {
        if host::session_locked() {
            return Err(locked());
        }
        let pids = self.pids.borrow().clone();
        let w = host::main_window(&pids).ok_or_else(|| Error::msg("OS input to the page: the browser shows no window"))?;
        let size = page.eval("[innerWidth, innerHeight]")?;
        let (iw, ih) = (size[0].as_f64().unwrap_or(0.0), size[1].as_f64().unwrap_or(0.0));
        let dip = [(0.3 * iw, 0.55 * ih), (0.7 * iw, 0.85 * ih)];
        let mut probes = Vec::new();
        let mut landed = Vec::new();
        for (dx, dy) in dip {
            let (px, py) = self.poster.client_point(w.hwnd, dx, dy)?;
            let t = self.poster.target(&pids, px, py)?;
            self.poster.park(px, py)?;
            // A move onto the point the pointer already sits on sends nothing on X11: come from beside
            // it, let that move arrive, and only then look for the one onto the point.
            page.eval("__strobeOs.moves = [], true")?;
            self.poster.move_to(t.moved(px - 3.0, py - 3.0), 0)?;
            self.wait_move(page, Duration::from_millis(300))?;
            page.eval("__strobeOs.moves = [], true")?;
            self.poster.move_to(t, 0)?;
            let got = self.wait_move(page, Duration::from_secs(1))?.ok_or_else(|| {
                refused(format!(
                    "OS pointer input did not reach the page: no trusted pointer move came to it within a second of strobe posting one at ({px:.0}, {py:.0}) through the OS{}",
                    if cfg!(windows) { " (posted window messages)" } else { "" }
                ))
            })?;
            probes.push((px, py));
            landed.push(got);
        }
        let (dx, dy) = (landed[1][0] - landed[0][0], landed[1][1] - landed[0][1]);
        let scale = ((probes[1].0 - probes[0].0) / dx, (probes[1].1 - probes[0].1) / dy);
        let sane = |s: f64| s.is_finite() && (0.2..=8.0).contains(&s);
        if !sane(scale.0) || !sane(scale.1) || (scale.0 - scale.1).abs() > 0.03 * scale.0.max(scale.1) {
            return Err(refused(format!(
                "the page's points do not map to the screen: moves posted {:.0} and {:.0} apart landed {dx:.1} and {dy:.1} page pixels apart",
                probes[1].0 - probes[0].0,
                probes[1].1 - probes[0].1
            )));
        }
        self.origin.set((probes[0].0 - landed[0][0] * scale.0, probes[0].1 - landed[0][1] * scale.1));
        self.scale.set(scale);
        self.top.set(w.hwnd);
        self.geometry.set((w.rect, w.client));
        self.frame_ms.set(host::display_of(w.hwnd).filter(|d| d.hz > 1.0).map_or(1000.0 / 60.0, |d| 1000.0 / d.hz));
        self.document.set(page.eval("performance.timeOrigin")?.as_f64().unwrap_or(0.0));
        Ok(())
    }

    /// The last trusted pointer move the page saw, once one came within `t`.
    fn wait_move(&self, page: &Page, t: Duration) -> Result<Option<[f64; 2]>> {
        let until = Instant::now() + t;
        loop {
            let m = page.eval("JSON.stringify(__strobeOs.moves.at(-1) ?? null)")?;
            if let Some(p) = m.as_str().and_then(|s| serde_json::from_str::<Option<[f64; 2]>>(s).ok()).flatten() {
                return Ok(Some(p));
            }
            if Instant::now() >= until {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The browser's processes now (a renderer may come or go): what the pointer may hit.
    pub fn set_pids(&self, pids: Vec<u32>) {
        *self.pids.borrow_mut() = pids;
    }

    /// The run's inputs and drags so far, taken.
    pub fn take_log(&self) -> Log {
        std::mem::take(&mut self.log.borrow_mut())
    }

    /// Whether the OS cursor could be parked on the pointer's points.
    pub fn parked(&self) -> Option<bool> {
        self.parked.get()
    }

    fn screen(&self, x: f64, y: f64) -> (f64, f64) {
        let (o, k) = (self.origin.get(), self.scale.get());
        (o.0 + x * k.0, o.1 + y * k.1)
    }

    fn input(&self, t_ns: u64, kind: &'static str, first: bool) {
        self.last_input.set(t_ns);
        self.log.borrow_mut().inputs.push(Input { t_ns, kind, first });
    }

    fn count(&self, i: usize) {
        let mut p = self.posted.get();
        p[i] += 1;
        self.posted.set(p);
    }

    /// The display's refresh period, whole milliseconds (the native driver's waits between steps).
    fn frame(&self) -> Duration {
        Duration::from_millis((self.frame_ms.get() as u64).max(4))
    }

    fn park(&self, x: f64, y: f64) -> Result<()> {
        self.parked.set(Some(self.poster.park(x, y)?));
        Ok(())
    }

    /// The window the OS hit-tests at a screen point, which must be the browser's.
    fn target(&self, x: f64, y: f64) -> Result<Target> {
        if host::session_locked() {
            return Err(locked());
        }
        self.poster.target(&self.pids.borrow(), x, y)
    }

    /// A pointer move to a screen point; while a button is held a drag's move, kept for `input.rate`.
    fn pointer_to(&self, x: f64, y: f64, t: Target) -> Result<u64> {
        self.park(x, y)?;
        let at = self.poster.move_to(t, self.pressed.get().map_or(0, input::held_flag))?;
        self.pointer.set(Some((x, y, t)));
        if self.pressed.get().is_some()
            && let Some(d) = self.log.borrow_mut().drags.last_mut()
        {
            d.push(at);
        }
        self.input(at, "pointer", false);
        Ok(at)
    }

    fn button(&self, b: Button, down: bool, sample: bool) -> Result<u64> {
        let (_, _, t) = self.pointer.get().ok_or_else(|| Error::usage("page.down(\"Mouse\") presses where the pointer is: move it first (hover, move, or down's at option)"))?;
        let at = self.poster.button(t, b, down, false, Mods::default(), self.top.get())?;
        self.pressed.set(down.then_some(b));
        if down {
            self.count(1);
            self.log.borrow_mut().drags.push(vec![at]);
        }
        self.input(at, if !down && sample { "discrete" } else { "pointer" }, false);
        Ok(at)
    }

    /// Before a measured input, and before a press: `driver::settle` on the page's processes (SPEC 7.4).
    /// Returns whether the page went quiet.
    fn rest(&self, min: Duration, refresh: Duration) -> Result<bool> {
        super::settle(&self.presents, &self.pids.borrow(), min, refresh)
    }

    /// Lets go of a held button and keys (between runs).
    pub fn release_all(&self) {
        if let Some(b) = self.pressed.get() {
            let _ = self.button(b, false, false);
        }
        self.poster.release_all(self.top.get());
    }

    /// The run's inputs a latency is measured from (a click's release, a key press, a scroll's first
    /// wheel step), with whether each was a scroll: what `run_inputs` keeps.
    pub fn sent(log: &Log) -> Vec<Sent> {
        log.inputs.iter().filter(|i| i.kind == "discrete" || (i.kind == "scroll" && i.first)).map(|i| Sent { t_ns: i.t_ns, scroll: i.kind == "scroll" }).collect()
    }

    /// Starts a count of what arrives (before each run, and its reset): strobe's posted input and the
    /// page's trusted events from zero, so one stretch's input never covers for another's.
    pub fn begin(&self, page: &Page) -> Result<()> {
        self.posted.set([0; 3]);
        let v = page.eval("window.__strobeOs && (__strobeOs.keys = __strobeOs.presses = __strobeOs.wheels = 0), performance.timeOrigin")?;
        self.document.set(v.as_f64().unwrap_or(0.0));
        Ok(())
    }

    /// Refuses `InputMismatch` unless what strobe posted since `begin` reached the page as trusted
    /// events: keys and button presses at least 95% of those posted, wheel turns some (a page
    /// coalesces them), within a second of looking (posted input reaches the page on its own time). A
    /// document that changed meanwhile (a navigation) took its counts with it: nothing to check.
    pub fn delivered(&self, page: &Page) -> Result<()> {
        let posted = self.posted.get();
        if posted == [0; 3] {
            return Ok(());
        }
        let enough = |i: usize, got: f64| if i == 2 { posted[i] == 0 || got > 0.0 } else { got >= 0.95 * posted[i] as f64 };
        let until = Instant::now() + Duration::from_secs(1);
        let r = loop {
            let v = page.eval("JSON.stringify([performance.timeOrigin, window.__strobeOs?.keys ?? 0, window.__strobeOs?.presses ?? 0, window.__strobeOs?.wheels ?? 0])")?;
            let r: Vec<f64> = v.as_str().and_then(|s| serde_json::from_str::<Vec<f64>>(s).ok()).unwrap_or_default();
            if r.len() < 4 || r[0] != self.document.get() {
                self.posted.set([0; 3]);
                self.document.set(r.first().copied().unwrap_or(0.0));
                return Ok(());
            }
            if (0..3).all(|i| enough(i, r[i + 1])) || Instant::now() >= until {
                break r;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        for (i, what) in ["keys", "mouse button presses", "wheel turns"].iter().enumerate() {
            if !enough(i, r[i + 1]) {
                return Err(refused(format!("OS input did not reach the page: strobe posted {} {what} through the OS and the page received {} in a second", posted[i], r[i + 1])));
            }
        }
        Ok(())
    }

    /// The first present of the browser that started after `after` and reached the screen.
    fn frame_after(&self, after: u64, t: Duration) -> Result<Option<u64>> {
        let mut p = self.presents.borrow_mut();
        let Some(p) = p.as_mut() else { return Ok(None) };
        let v = p.live(&self.pids.borrow(), after, t)?;
        if v.next_screen_ns == 0 {
            return Err(Error::timeout(format!("until frame: the page showed no frame in {} s", t.as_secs())));
        }
        Ok(Some(v.next_screen_ns))
    }
}

/// macOS: Command+A, C, V, X and Z posted through the OS reach the page's keydown with their
/// modifiers, but the browser's menu, which does what they do in a field (select all, copy, paste,
/// cut, undo), never sees them, whether posted to its process or into the session's or the HID event
/// stream (checked on air): the page side would skip what the native side did. Refused unless
/// `expect.same` shows both sides reached the same place.
fn menu_chord(os: &OsInput, text: &str, mods: Mods, key: Key) -> Result<()> {
    let edit = matches!(key, Key::Char(c) if "acvxz".contains(c.to_ascii_lowercase()));
    if cfg!(target_os = "macos") && mods.meta && edit && !os.vouched.get() {
        return Err(Refusal::new(
            ValidatorId::InputMismatch,
            format!("press({text:?}) through the OS: macOS gives the page the keys, never the browser's menu, which does what the chord does in a field, so the page side would skip it"),
        )
        .fix("declare expect.same on what the chord changes, so a pair whose sides ended apart is refused; or give the page side the chord's effect another way")
        .into());
    }
    Ok(())
}

impl Driver {
    /// A page input call through the OS (`OsInput`).
    pub(super) fn os_dispatch(&self, os: &OsInput, call: &str, args: &[Value]) -> Result<Value> {
        let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        let opts = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        let sel = |i: usize| args.get(i).cloned().unwrap_or(Value::String(String::new()));
        let (o, at) = match call {
            "click" => (opts(1), self.os_click(os, &sel(0), &opts(1), "discrete")?),
            "hover" => {
                let (x, y, t) = self.os_aim(os, &sel(0), &opts(1))?;
                (opts(1), os.pointer_to(x, y, t)?)
            }
            "move" => {
                let at = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0);
                let (x, y) = os.screen(at(0), at(1));
                let t = match os.pointer.get().filter(|_| os.pressed.get().is_some()) {
                    Some((_, _, t)) => t.moved(x, y),
                    None => os.target(x, y)?,
                };
                (opts(2), os.pointer_to(x, y, t)?)
            }
            "type" => {
                let o = opts(2);
                self.os_focus(os, &sel(0), &o)?;
                let delay = Duration::from_millis(o["delay"].as_f64().unwrap_or(0.0).max(0.0) as u64);
                os.rest(Duration::ZERO, os.frame())?;
                let mut last = host::now_ns();
                for (k, c) in s(1).chars().enumerate() {
                    if k > 0 && !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    let key = match c {
                        '\n' => Key::Named(Named::Enter),
                        '\t' => Key::Named(Named::Tab),
                        c => Key::Char(c),
                    };
                    last = os.poster.press(os.top.get(), None, Mods::default(), key)?;
                    os.count(0);
                    os.input(last, "discrete", false);
                }
                (o, last)
            }
            "press" => {
                let o = opts(1);
                self.os_focus(os, &o["selector"], &o)?;
                let (mods, key) = keys::parse_chord(&s(0))?;
                menu_chord(os, &s(0), mods, key)?;
                // After the click into the field, if there was one: its frames settle first.
                os.rest(Duration::ZERO, os.frame())?;
                let at = os.poster.press(os.top.get(), None, mods, key)?;
                os.count(0);
                os.input(at, "discrete", false);
                (o, at)
            }
            "down" | "up" if keys::mouse_button(&s(0)).is_some() => {
                let o = opts(1);
                let b = keys::mouse_button(&s(0)).unwrap_or(Button::Left);
                match o.get("at") {
                    Some(a @ (Value::String(_) | Value::Object(_))) => {
                        let (x, y, t) = self.os_aim(os, a, &o)?;
                        os.pointer_to(x, y, t)?;
                    }
                    Some(Value::Array(p)) => {
                        let (x, y) = os.screen(p.first().and_then(Value::as_f64).unwrap_or(0.0), p.get(1).and_then(Value::as_f64).unwrap_or(0.0));
                        os.pointer_to(x, y, os.target(x, y)?)?;
                    }
                    _ => {}
                }
                // A release is measured as a click's is: what the press drew settles first.
                if call == "up" {
                    os.rest(Duration::ZERO, os.frame())?;
                }
                (o.clone(), os.button(b, call == "down", true)?)
            }
            "down" | "up" => {
                let (mods, key) = keys::parse_chord(&s(0))?;
                if mods.any() {
                    return Err(Error::usage(format!("page.{call}({:?}): one key, not a chord (hold each with its own down)", s(0))));
                }
                // A key going down is measured, as a press is.
                if call == "down" {
                    os.rest(Duration::ZERO, os.frame())?;
                }
                let at = if call == "down" { os.poster.hold(os.top.get(), key)? } else { os.poster.release(os.top.get(), key)? };
                if call == "down" {
                    os.count(0);
                }
                os.input(at, if call == "down" { "discrete" } else { "pointer" }, false);
                (opts(1), at)
            }
            "drag" => (opts(2), self.os_drag(os, &sel(0), &opts(1), &opts(2))?),
            "scroll" => {
                let (d, o) = (opts(1), opts(2));
                let (x, y, t) = self.os_aim(os, &sel(0), &o)?;
                os.park(x, y)?;
                os.input(os.poster.move_to(t, 0)?, "pointer", false);
                let frame = os.frame();
                os.rest(Duration::ZERO, frame)?;
                let (by_y, by_x) = (d["by"].as_f64().unwrap_or(0.0), d["byX"].as_f64().unwrap_or(0.0));
                // The native driver's pace: one wheel step per refresh at `speed` (default 3000 px/s).
                let speed = d["speed"].as_f64().unwrap_or(3000.0).max(1.0);
                let steps = ((by_y.abs().max(by_x.abs()) / speed) / frame.as_secs_f64()).ceil().max(1.0) as usize;
                let start = Instant::now();
                let mut last = 0;
                for k in 0..steps {
                    if let Some(w) = (start + frame * k as u32).checked_duration_since(Instant::now()) {
                        std::thread::sleep(w);
                    }
                    let at = os.poster.wheel(t, by_x / steps as f64, by_y / steps as f64)?;
                    if at != 0 {
                        os.count(2);
                        os.input(at, "scroll", last == 0);
                        last = at;
                    }
                }
                if last == 0 {
                    return Err(Error::usage(format!(
                        "a wheel movement of {} px sends no wheel click: on X11 the wheel moves in clicks (strobe sends one per 100 px); move it at least 50 px",
                        by_x.abs().max(by_y.abs())
                    )));
                }
                (o, last)
            }
            "wheel" => {
                let (d, o) = (opts(1), opts(2));
                let (x, y, t) = if no_selector(&sel(0)) {
                    let c = self.page.eval("[innerWidth / 2, innerHeight / 2]")?;
                    let (x, y) = os.screen(c[0].as_f64().unwrap_or(0.0), c[1].as_f64().unwrap_or(0.0));
                    (x, y, os.target(x, y)?)
                } else {
                    self.os_aim(os, &sel(0), &o)?
                };
                os.park(x, y)?;
                os.input(os.poster.move_to(t, 0)?, "pointer", false);
                os.rest(Duration::ZERO, os.frame())?;
                let (dx, dy) = (d["dx"].as_f64().unwrap_or(0.0), d["dy"].as_f64().unwrap_or(0.0));
                let at = os.poster.wheel(t, dx, dy)?;
                if at == 0 {
                    return Err(Error::usage(format!(
                        "a wheel movement of {} px sends no wheel click: on X11 the wheel moves in clicks (strobe sends one per 100 px); move it at least 50 px",
                        dx.abs().max(dy.abs())
                    )));
                }
                os.count(2);
                os.input(at, "scroll", true);
                (o, at)
            }
            other => return Err(Error::msg(format!("internal: page.{other}() is no OS input"))),
        };
        // `until: "frame"`: the browser's next frame on the screen from the OS's presents, as a native
        // app's; anything else as the page waits for it.
        if o.get("until").and_then(Value::as_str) == Some("frame") && os.frame_after(at, self.timeout(&o))?.is_some() {
            return Ok(Value::Bool(true));
        }
        self.until(&o).map(|_| Value::Bool(true))
    }

    /// The element's centre (scrolled into view) as a screen point, and the browser window the OS
    /// hit-tests there (another window over it refuses `WindowHidden`).
    fn os_aim(&self, os: &OsInput, sel: &Value, o: &Value) -> Result<(f64, f64, Target)> {
        self.os_aim_focus(os, sel, o).map(|(x, y, t, _)| (x, y, t))
    }

    /// `os_aim`, and whether focus is in the element: one look at the page.
    fn os_aim_focus(&self, os: &OsInput, sel: &Value, o: &Value) -> Result<(f64, f64, Target, bool)> {
        let (cx, cy, focused) = self.aim(&Self::sel(sel)?, o)?;
        let (x, y) = os.screen(cx, cy);
        Ok((x, y, os.target(x, y)?, focused))
    }

    fn os_click(&self, os: &OsInput, sel: &Value, o: &Value, kind: &'static str) -> Result<u64> {
        let aimed = self.os_aim(os, sel, o)?;
        self.os_click_at(os, aimed, o, kind)
    }

    /// A click at an aimed point.
    fn os_click_at(&self, os: &OsInput, (x, y, t): (f64, f64, Target), o: &Value, kind: &'static str) -> Result<u64> {
        let b = Button::parse(o["button"].as_str())?;
        let mods = Mods::parse_list(&o["modifiers"])?;
        let count = o["count"].as_u64().unwrap_or(1).clamp(1, 3);
        os.park(x, y)?;
        os.input(os.poster.move_to(t, 0)?, "pointer", false);
        os.pointer.set(Some((x, y, t)));
        let gap = os.frame();
        // A frame of what came before (a hover, an earlier step) settles before the press.
        let quiet = os.rest(Duration::ZERO, gap)?;
        let mut last = 0;
        for k in 0..count {
            // The press shows a pressed look at most; the release is the click's action, held as a
            // person holds a click, until the pressed look is on the screen (the minimum only when the
            // page was still drawing before the press). A double click's presses follow at once, with
            // no settling inside the gesture, and its releases are no samples: only a single click's
            // release is (SPEC 7.4).
            os.input(os.poster.button(t, b, true, k > 0, mods, os.top.get())?, "pointer", false);
            os.count(1);
            if count == 1 && quiet {
                os.rest(Duration::from_millis(60), gap)?;
            } else {
                std::thread::sleep(if count == 1 { Duration::from_millis(60) } else { gap });
            }
            last = os.poster.button(t, b, false, false, mods, os.top.get())?;
            os.input(last, if count == 1 { kind } else { "pointer" }, false);
        }
        Ok(last)
    }

    /// Clicks into the element unless focus is already in it, as a person clicks into a field (that
    /// click is no keystroke).
    fn os_focus(&self, os: &OsInput, sel: &Value, o: &Value) -> Result<()> {
        if no_selector(sel) {
            return Ok(());
        }
        let (x, y, t, focused) = self.os_aim_focus(os, sel, o)?;
        if !focused {
            self.os_click_at(os, (x, y, t), &json!({}), "pointer")?;
            // Its frames (a focus ring, a caret) settle before the first key, which is measured: at least
            // 2 refreshes, since the click's first frame may not have begun yet.
            os.rest(os.frame() * 2, os.frame())?;
        }
        Ok(())
    }

    /// `drag` through the OS: one move per refresh on strobe's clock, the path's page pixels scaled
    /// to the screen, posted to the window it pressed in.
    fn os_drag(&self, os: &OsInput, sel: &Value, path: &Value, o: &Value) -> Result<u64> {
        let path = keys::parse_path(path)?;
        let (x0, y0, t0) = if no_selector(sel) {
            os.pointer.get().ok_or_else(|| Error::usage("page.drag(\"\", …) starts where the pointer is: move it first, or name an element"))?
        } else {
            self.os_aim(os, sel, o)?
        };
        let b = Button::parse(o["button"].as_str())?;
        let point = |t: f64| {
            let (dx, dy) = keys::path_at(&path, t);
            (x0 + dx * os.scale.get().0, y0 + dy * os.scale.get().1)
        };
        let (x, y) = point(0.0);
        os.pointer_to(x, y, t0.moved(x, y))?;
        let held = os.pressed.get().is_some();
        if held {
            os.log.borrow_mut().drags.push(Vec::new());
        } else {
            os.button(b, true, false)?;
        }
        let duration = o["duration"].as_f64().unwrap_or(1000.0).max(1.0);
        // The refresh period unrounded (16.67 ms at 60 Hz), as the native driver paces its drags.
        let frame = os.frame_ms.get();
        let steps = (duration / frame).ceil().max(1.0) as u32;
        let start = Instant::now();
        let mut last = 0;
        for k in 1..=steps {
            os.pacer.wait(start + Duration::from_secs_f64(frame * k as f64 / 1e3), Duration::from_secs_f64(frame / 1e3));
            let (x, y) = point(k as f64 / steps as f64);
            last = os.pointer_to(x, y, t0.moved(x, y))?;
        }
        if !held {
            last = os.button(b, false, false)?;
        }
        Ok(last)
    }
}
