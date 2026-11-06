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

/// Quiet that counts as the page at rest before a measured input (as for native apps, §7.4).
const REST_MAX: Duration = Duration::from_millis(300);

pub struct OsInput {
    poster: Poster,
    pids: RefCell<Vec<u32>>,
    /// The browser's window: keys go to it.
    top: isize,
    /// Screen = origin + page point × scale (screen units: physical pixels, points on macOS).
    origin: (f64, f64),
    scale: (f64, f64),
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
}

fn refused(summary: String) -> Error {
    Refusal::new(ValidatorId::InputMismatch, summary)
        .fix("strobe gives a page measured against a native app the OS's input, as the native side gets; where the OS's posted input cannot reach the page, compare two pages or two native apps")
        .into()
}

impl OsInput {
    /// Measures where the page lies on the screen from two posted pointer moves, which must reach it.
    pub fn attach(page: &Page, pids: Vec<u32>, presents: Presents) -> Result<OsInput> {
        page.add_init_script(SCRIPT)?;
        page.eval(SCRIPT)?;
        if host::session_locked() {
            return Err(Error::refused(
                "SessionLocked",
                "OS input to the page: the session is locked, so the OS hit-tests every point to the lock screen",
                "run pointer and key journeys on an unlocked session",
            ));
        }
        let w = host::main_window(&pids).ok_or_else(|| Error::msg("OS input to the page: the browser shows no window"))?;
        let poster = Poster::start()?;
        let (x, y, ww, wh) = (w.rect.0 as f64, w.rect.1 as f64, w.rect.2 as f64, w.rect.3 as f64);
        // Two points well inside the window's lower part, where the page draws (a title bar or a
        // browser's toolbar is above it), far apart for an exact scale.
        let probes = [(x + 0.3 * ww, y + 0.55 * wh), (x + 0.7 * ww, y + 0.85 * wh)];
        let mut landed = Vec::new();
        for (px, py) in probes {
            let t = poster.target(&pids, px, py)?;
            poster.park(px, py)?;
            page.eval("__strobeOs.moves = [], true")?;
            // A move onto the point the pointer already sits on sends nothing on X11: come from beside it.
            poster.move_to(t.moved(px - 3.0, py - 3.0), 0)?;
            poster.move_to(t, 0)?;
            let until = Instant::now() + Duration::from_secs(1);
            let got = loop {
                let m = page.eval("JSON.stringify(__strobeOs.moves.at(-1) ?? null)")?;
                if let Some(p) = m.as_str().and_then(|s| serde_json::from_str::<Option<[f64; 2]>>(s).ok()).flatten() {
                    // The move onto the point, not the one beside it.
                    std::thread::sleep(Duration::from_millis(30));
                    let last = page.eval("JSON.stringify(__strobeOs.moves.at(-1))")?;
                    break last.as_str().and_then(|s| serde_json::from_str::<[f64; 2]>(s).ok()).unwrap_or(p);
                }
                if Instant::now() >= until {
                    return Err(refused(format!(
                        "OS pointer input did not reach the page: no trusted pointer move came to it within a second of strobe posting one at ({px:.0}, {py:.0}) through the OS{}",
                        if cfg!(windows) { " (posted window messages)" } else { "" }
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            };
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
        let origin = (probes[0].0 - landed[0][0] * scale.0, probes[0].1 - landed[0][1] * scale.1);
        let document = page.eval("performance.timeOrigin")?.as_f64().unwrap_or(0.0);
        Ok(OsInput {
            poster,
            pids: RefCell::new(pids),
            top: w.hwnd,
            origin,
            scale,
            presents,
            log: RefCell::new(Log::default()),
            last_input: Cell::new(0),
            pointer: Cell::new(None),
            pressed: Cell::new(None),
            posted: Cell::new([0; 3]),
            document: Cell::new(document),
            parked: Cell::new(None),
            pacer: super::Pacer::default(),
        })
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
        (self.origin.0 + x * self.scale.0, self.origin.1 + y * self.scale.1)
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

    /// The display's frame period at a screen point.
    fn frame(&self, x: f64, y: f64) -> Duration {
        Duration::from_millis(host::refresh_at(x as i32, y as i32).map_or(16, |hz| (1000 / hz.max(1)).max(4) as u64))
    }

    fn park(&self, x: f64, y: f64) -> Result<()> {
        self.parked.set(Some(self.poster.park(x, y)?));
        Ok(())
    }

    /// The window the OS hit-tests at a screen point, which must be the browser's.
    fn target(&self, x: f64, y: f64) -> Result<Target> {
        if host::session_locked() {
            return Err(Error::refused("SessionLocked", "OS input to the page: the session is locked", "run pointer and key journeys on an unlocked session"));
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

    fn button(&self, b: Button, down: bool) -> Result<u64> {
        let (_, _, t) = self.pointer.get().ok_or_else(|| Error::usage("page.down(\"Mouse\") presses where the pointer is: move it first (hover, move, or down's at option)"))?;
        let at = self.poster.button(t, b, down, false, Mods::default(), self.top)?;
        self.pressed.set(down.then_some(b));
        if down {
            self.count(1);
            self.log.borrow_mut().drags.push(vec![at]);
        }
        self.input(at, if down { "pointer" } else { "discrete" }, false);
        Ok(at)
    }

    /// Before a measured input: at least `min`, then until the page's processes presented nothing for
    /// two refreshes (at most 300 ms), as for a native app (SPEC §7.4).
    fn rest(&self, min: Duration, refresh: Duration) -> Result<()> {
        std::thread::sleep(min);
        let quiet = (refresh * 2).as_nanos() as u64;
        let until = Instant::now() + REST_MAX;
        while let Some(p) = self.presents.borrow_mut().as_mut() {
            let last = p.live(&self.pids.borrow(), u64::MAX, Duration::ZERO)?.last_start_ns;
            let now = host::now_ns();
            if last + quiet <= now || Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_nanos(last + quiet - now).min(Duration::from_millis(20)));
        }
        Ok(())
    }

    /// Lets go of a held button and keys (between runs).
    pub fn release_all(&self) {
        if let Some(b) = self.pressed.get() {
            let _ = self.button(b, false);
        }
        self.poster.release_all(self.top);
    }

    /// The run's inputs a latency is measured from (a click's release, a key press, a scroll's first
    /// wheel step), with whether each was a scroll: what `run_inputs` keeps.
    pub fn sent(log: &Log) -> Vec<(u64, bool)> {
        log.inputs.iter().filter(|i| i.kind == "discrete" || (i.kind == "scroll" && i.first)).map(|i| (i.t_ns, i.kind == "scroll")).collect()
    }

    /// Refuses `InputMismatch` when keys, button presses or wheel turns strobe posted since the last
    /// check reached the page as none, a second after it began looking (posted input reaches the page
    /// on its own time; a document that changed meanwhile, a navigation, leaves it to the next check).
    /// Read between runs.
    pub fn delivered(&self, page: &Page) -> Result<()> {
        let posted = self.posted.get();
        if posted == [0; 3] {
            return Ok(());
        }
        let until = Instant::now() + Duration::from_secs(1);
        let r = loop {
            let v = page.eval("JSON.stringify([performance.timeOrigin, __strobeOs?.keys ?? 0, __strobeOs?.presses ?? 0, __strobeOs?.wheels ?? 0])")?;
            let r: Vec<f64> = v.as_str().and_then(|s| serde_json::from_str::<Vec<f64>>(s).ok()).unwrap_or_default();
            if r.len() < 4 || r[0] != self.document.get() {
                self.document.set(r.first().copied().unwrap_or(0.0));
                return Ok(());
            }
            if (0..3).all(|i| posted[i] == 0 || r[i + 1] > 0.0) || Instant::now() >= until {
                break r;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        page.eval("__strobeOs && (__strobeOs.keys = __strobeOs.presses = __strobeOs.wheels = 0), true")?;
        self.posted.set([0; 3]);
        for (i, what) in ["keys", "mouse button presses", "wheel turns"].iter().enumerate() {
            if posted[i] > 0 && r[i + 1] == 0.0 {
                return Err(refused(format!("OS input did not reach the page: strobe posted {} {what} through the OS and the page received none in a second", posted[i])));
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
                os.rest(Duration::ZERO, os.frame(os.origin.0, os.origin.1))?;
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
                    last = os.poster.press(os.top, None, Mods::default(), key)?;
                    os.count(0);
                    os.input(last, "discrete", false);
                }
                (o, last)
            }
            "press" => {
                let o = opts(1);
                self.os_focus(os, &o["selector"], &o)?;
                let (mods, key) = keys::parse_chord(&s(0))?;
                let at = os.poster.press(os.top, None, mods, key)?;
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
                (o.clone(), os.button(b, call == "down")?)
            }
            "down" | "up" => {
                let (mods, key) = keys::parse_chord(&s(0))?;
                if mods.any() {
                    return Err(Error::usage(format!("page.{call}({:?}): one key, not a chord (hold each with its own down)", s(0))));
                }
                let at = if call == "down" { os.poster.hold(os.top, key)? } else { os.poster.release(os.top, key)? };
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
                let frame = os.frame(x, y);
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
                os.rest(Duration::ZERO, os.frame(x, y))?;
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
        let (cx, cy) = self.locate(&Self::sel(sel)?, o)?;
        let (x, y) = os.screen(cx, cy);
        Ok((x, y, os.target(x, y)?))
    }

    fn os_click(&self, os: &OsInput, sel: &Value, o: &Value, kind: &'static str) -> Result<u64> {
        let (x, y, t) = self.os_aim(os, sel, o)?;
        let b = Button::parse(o["button"].as_str())?;
        let mods = Mods::parse_list(&o["modifiers"])?;
        let count = o["count"].as_u64().unwrap_or(1).clamp(1, 3);
        os.park(x, y)?;
        os.input(os.poster.move_to(t, 0)?, "pointer", false);
        os.pointer.set(Some((x, y, t)));
        let gap = os.frame(x, y);
        let mut last = 0;
        for k in 0..count {
            // The press shows a pressed look at most; the release is the click's action, held as a
            // person holds a click (a double click's second press follows at once).
            os.input(os.poster.button(t, b, true, k > 0, mods, os.top)?, "pointer", false);
            os.count(1);
            if count > 1 {
                std::thread::sleep(gap);
            } else {
                os.rest(Duration::from_millis(60), gap)?;
            }
            last = os.poster.button(t, b, false, false, mods, os.top)?;
            os.input(last, kind, false);
        }
        Ok(last)
    }

    /// Clicks into the element unless focus is already in it, as a person clicks into a field (that
    /// click is no keystroke).
    fn os_focus(&self, os: &OsInput, sel: &Value, o: &Value) -> Result<()> {
        if no_selector(sel) {
            return Ok(());
        }
        let parsed = Self::sel(sel)?;
        self.locate(&parsed, o)?;
        if self.call(&format!("__strobe.focused({})", parsed.page_spec()))?.as_bool() != Some(true) {
            self.os_click(os, sel, &json!({}), "pointer")?;
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
            (x0 + dx * os.scale.0, y0 + dy * os.scale.1)
        };
        let (x, y) = point(0.0);
        os.pointer_to(x, y, t0.moved(x, y))?;
        let held = os.pressed.get().is_some();
        if held {
            os.log.borrow_mut().drags.push(Vec::new());
        } else {
            os.button(b, true)?;
        }
        let duration = o["duration"].as_f64().unwrap_or(1000.0).max(1.0);
        // The refresh period unrounded (16.67 ms at 60 Hz), as the native driver paces its drags.
        let frame = host::refresh_at(x0 as i32, y0 as i32).map_or(1000.0 / 60.0, |hz| 1000.0 / hz.max(1) as f64);
        let steps = (duration / frame).ceil().max(1.0) as u32;
        let start = Instant::now();
        let mut last = 0;
        for k in 1..=steps {
            os.pacer.wait(start + Duration::from_secs_f64(frame * k as f64 / 1e3), Duration::from_secs_f64(frame / 1e3));
            let (x, y) = point(k as f64 / steps as f64);
            last = os.pointer_to(x, y, t0.moved(x, y))?;
        }
        if !held {
            last = os.button(b, false)?;
        }
        Ok(last)
    }
}
