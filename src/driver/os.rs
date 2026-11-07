//! OS input to a page measured from outside (SPEC §9.10, §13.6): against a native implementation or a
//! page in another engine a page gets the input the native side gets, from the OS's own posters
//! (`a11y::input`: posted window messages on Windows, Quartz events on macOS, XTest on X11), never
//! DevTools or WebDriver. Every engine the same way: elements are still found through the page probe;
//! their page point goes to the screen through a map strobe measures at the block's start, from where
//! posted pointer moves landed in the page (which also proves posted input reaches it), never from an
//! engine's own idea of where its viewport is. The map holds while the window and the page's viewport
//! stay where they were, or as a `page.window` strobe was asked for left them (the map follows that
//! size, `commanded`): checked after every run and reset (`placed`). Each kind of input is checked
//! again as the block goes: a key, button or wheel strobe posted that no trusted event in the page
//! matched refuses the block `InputMismatch` instead of timing input the page never got. Interaction
//! latency then follows the native rule (SPEC §7.4), with the page's own changes (`CHANGES_SCRIPT`: DOM
//! mutations, input and scroll events) standing in for an accessibility tree's.

use super::{Driver, no_selector};
use crate::driver::native::{Input, Log};
use crate::host;
use crate::model::{Error, Refusal, Result, ValidatorId, WebEngine};
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

/// Installed in every document before its scripts: counts the trusted inputs that reached the page in
/// `COUNTED`'s order (key downs and ups, button presses and releases, wheel turns, pointer moves), keeps
/// the last 16 pointer moves (where a posted move landed), and counts what moves or resizes the viewport
/// (`resize`, `fullscreenchange`, the visual viewport's `resize`). Listeners only, passive, at the
/// window's capture phase ahead of the page's own.
const SCRIPT: &str = r#"window.__strobeOs ??= (() => {
  const s = { n: [0, 0, 0, 0, 0, 0], moves: [], viewport: 0 }
  const on = (t, e, f) => t?.addEventListener(e, f, { capture: true, passive: true })
  ;["keydown", "keyup", "pointerdown", "pointerup", "wheel", "pointermove"].forEach((e, i) => on(window, e, (ev) => { if (ev.isTrusted) s.n[i]++ }))
  on(window, "pointermove", (e) => { if (e.isTrusted) { s.moves.push([e.clientX, e.clientY]); if (s.moves.length > 16) s.moves.shift() } })
  for (const [t, e] of [[window, "resize"], [window, "fullscreenchange"], [window.visualViewport, "resize"]]) on(t, e, () => { s.viewport++ })
  return s
})(), true
//# sourceURL=strobe-os-input.js"#;

/// What `SCRIPT` counts, by index into its `n` and `OsInput::posted`: each kind's name, and whether
/// every one posted must arrive (a discrete key or button edge) or at least one (a continuous kind
/// the browser or the OS coalesces: wheel turns and pointer moves).
const COUNTED: [(&str, bool); 6] = [("key downs", true), ("key ups", true), ("button presses", true), ("button releases", true), ("wheel turns", false), ("pointer moves", false)];
const KEY_DOWN: usize = 0;
const KEY_UP: usize = 1;
const PRESS: usize = 2;
const RELEASE: usize = 3;
const WHEEL: usize = 4;
const MOVE: usize = 5;

/// A window's frame and client size.
type Geometry = ((i32, i32, i32, i32), (i32, i32));

/// Where the page sees its viewport: the window's place on the screen, the viewport's size, the device
/// pixels per CSS pixel (which a page zoom changes), the window's outer size (what a toolbar above the
/// page adds), and the visual viewport's scale and offsets (a pinch zoom). Read in every engine the
/// same way.
const VIEWPORT: &str = "JSON.stringify([screenX, screenY, innerWidth, innerHeight, devicePixelRatio, outerWidth, outerHeight, visualViewport?.scale ?? 1, visualViewport?.offsetLeft ?? 0, visualViewport?.offsetTop ?? 0])";

pub struct OsInput {
    poster: Poster,
    pids: RefCell<Vec<u32>>,
    /// The browser's window (keys go to it), its frame and client size, and the page's viewport
    /// (`VIEWPORT`) when strobe last mapped the page onto the screen: after setup a window or viewport
    /// that moved, resized, zoomed or was replaced is mapped again; after a run or reset it refuses.
    top: Cell<isize>,
    geometry: Cell<Geometry>,
    viewport: RefCell<Vec<f64>>,
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
    /// What strobe posted since the last check that it arrived (`COUNTED`), and the document it went to
    /// (`performance.timeOrigin`). A move counts only onto another point than the last one posted: a
    /// browser fires no `pointermove` for a pointer that stayed where it was.
    posted: Cell<[u64; 6]>,
    moved_to: Cell<Option<(i64, i64)>>,
    /// Keys `down` holds, whose `up` posts a key up.
    held: RefCell<Vec<Key>>,
    document: Cell<f64>,
    parked: Cell<Option<bool>>,
    pacer: super::Pacer,
    /// The series reads `expect.same`, which shows whether a menu chord did on the page what it did on
    /// the other side (macOS, `press`).
    pub vouched: Cell<bool>,
    /// The window's frame as the input thread reads it (`Poster::frame`) when strobe last mapped the page.
    frame: Cell<(f64, f64, f64, f64)>,
    /// The viewport resizes the page counted since `begin` that a `page.window` strobe asked for made:
    /// `placed` refuses only those past them.
    resized: Cell<f64>,
    /// After a map strobe worked out from a size it asked for (`commanded`), the page points the pointer
    /// moves posted since should land on: `placed` checks the newest against where the moves landed.
    expect: RefCell<Option<Vec<(f64, f64)>>>,
    /// Where such a move should have landed and where the page saw moves instead (`look_landed`).
    missed: RefCell<Option<String>>,
    /// The display's device pixels per screen unit when strobe last mapped the page (`device_per_unit`).
    backing: Cell<f64>,
}

fn refused(summary: String) -> Error {
    Refusal::new(ValidatorId::InputMismatch, summary)
        .fix("strobe gives a page measured from outside (against a native app, or a page in another engine) the OS's input, as the other side gets; where the OS's posted input cannot reach the page, compare pages in one engine or two native apps")
        .into()
}

/// Why no OS input reaches a page of `engine` here, if none does: a headless browser has no window,
/// and Safari keeps a WebDriver session's window from every input but its driver's (pointer moves
/// strobe posted through the OS reached no page on air, Safari 27; a click there asks whether to stop
/// the session). Such a page takes its driver's input outside its runs, and none in them.
pub fn unreachable(engine: WebEngine, headless: bool) -> Option<Refusal> {
    if headless {
        return Some(
            Refusal::new(ValidatorId::Headless, "a headless browser has no window, so the OS has nowhere to send input")
                .fix("run the pages headed (drop --headless and the target's headless), or keep input out of runs (setup and reset may use it)"),
        );
    }
    (engine == WebEngine::Webkit && cfg!(target_os = "macos")).then(|| {
        Refusal::new(ValidatorId::InputMismatch, "Safari keeps the window of a WebDriver session from every input but its driver's, so the OS's input never reaches the page")
            .fix("compare the Safari page with Safari pages only (input through its driver on every side), or keep input out of its runs (setup and reset may use it)")
    })
}

/// The display's device pixels per OS screen unit under a window: its backing scale on macOS (screen
/// units are points), 1 elsewhere (physical pixels).
fn device_per_unit(hwnd: isize) -> f64 {
    if cfg!(target_os = "macos") { host::window_state(hwnd).map_or(1.0, |w| w.scale()).max(0.01) } else { 1.0 }
}

/// The page's `VIEWPORT` now.
fn viewport(page: &Page) -> Result<Vec<f64>> {
    let v = page.eval(VIEWPORT)?;
    Ok(v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default())
}

/// The browser shows no window strobe's OS input can reach. On Linux with a Wayland session that is
/// most likely a Wayland window: XTest (strobe's OS input there) reaches X11 windows only.
fn no_window(pids: &[u32]) -> Error {
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Refusal::new(
            ValidatorId::InputMismatch,
            format!("OS input to the page: the browser (pids {pids:?}) shows no X11 window, and this session runs Wayland (WAYLAND_DISPLAY is set): it opened a Wayland window, which XTest, strobe's OS input on Linux, cannot reach"),
        )
        .fix("measure pages from outside in an X11 session (or with WAYLAND_DISPLAY unset, so the browser opens an X11 window)")
        .into();
    }
    Error::msg("OS input to the page: the browser shows no window")
}

/// Another window of the browser itself (a dialog, a popup, its developer tools) is where strobe would
/// send input, not the page's window.
fn covered_by_own(x: f64, y: f64) -> Error {
    Error::refused(
        "WindowHidden",
        format!("another window of the browser (a dialog, a popup, its developer tools) is at ({x:.0}, {y:.0}), where strobe would send the page's input"),
        "close it, or keep it off the page while strobe drives it",
    )
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
            viewport: RefCell::new(Vec::new()),
            origin: Cell::new((0.0, 0.0)),
            scale: Cell::new((1.0, 1.0)),
            frame_ms: Cell::new(1000.0 / 60.0),
            presents,
            log: RefCell::new(Log::default()),
            last_input: Cell::new(0),
            pointer: Cell::new(None),
            pressed: Cell::new(None),
            posted: Cell::new([0; 6]),
            moved_to: Cell::new(None),
            held: RefCell::new(Vec::new()),
            document: Cell::new(0.0),
            parked: Cell::new(None),
            pacer: super::Pacer::default(),
            vouched: Cell::new(false),
            frame: Cell::new((0.0, 0.0, 0.0, 0.0)),
            resized: Cell::new(0.0),
            expect: RefCell::new(None),
            missed: RefCell::new(None),
            backing: Cell::new(1.0),
        };
        os.map(page)?;
        Ok(os)
    }

    /// The browser's processes strobe follows.
    pub fn pids(&self) -> Vec<u32> {
        self.pids.borrow().clone()
    }

    /// Before a `page.window` strobe asked for: the move check of an earlier one (`look_landed`), which
    /// the new map would replace, and the page's count of viewport resizes so far (`commanded` adds what
    /// comes after it).
    pub fn sizing(&self, page: &Page) -> Result<f64> {
        self.look_landed(page)?;
        Ok(page.eval("window.__strobeOs?.viewport ?? 0")?.as_f64().unwrap_or(0.0))
    }

    /// After a `page.window` strobe asked for and saw the window take: the map from page to screen follows
    /// the new size without probes (a run is going): the viewport keeps its left edge's and bottom
    /// edge's distances from the window's frame (a browser's toolbar is on top, and may change its height
    /// as the window maximizes), and its scale (screen units per CSS pixel) changes as the device pixels
    /// per CSS pixel over the screen's device pixels per unit do (macOS: points, so a window that moves
    /// between a Retina and a 1x display keeps a point per CSS pixel), those distances with it. Checked
    /// in `placed`, from where the pointer moves strobe posts after it land in the page. The viewport
    /// resizes the page counted since `before` (`sizing`) are this one's.
    pub fn commanded(&self, page: &Page, before: f64) -> Result<()> {
        let pids = self.pids();
        let w = host::main_window(&pids).ok_or_else(|| no_window(&pids))?;
        if w.hwnd != self.top.get() {
            return Err(refused("the browser's window was replaced as strobe sized it, so the page's points no longer map to the screen".into()));
        }
        if let Some(c) = self.presents.borrow_mut().as_mut() {
            c.resized(&pids);
        }
        // The page's resize events come on its next frames.
        page.eval("new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))")?;
        let (old, new) = (self.frame.get(), self.poster.frame(w.hwnd)?);
        let view = viewport(page)?;
        let at = |v: &[f64], i: usize| v.get(i).copied().unwrap_or(0.0);
        let (o, k) = (self.origin.get(), self.scale.get());
        let was = self.viewport.borrow().clone();
        let backing = device_per_unit(w.hwnd);
        let (d0, d1) = (at(&was, 4) / self.backing.get(), at(&view, 4) / backing);
        let ratio = if d0 > 0.0 && d1 > 0.0 { d1 / d0 } else { 1.0 };
        let k2 = (k.0 * ratio, k.1 * ratio);
        let left = (o.0 - old.0) * ratio;
        let bottom = (old.1 + old.3 - (o.1 + at(&was, 3) * k.1)) * ratio;
        self.origin.set((new.0 + left, new.1 + new.3 - bottom - at(&view, 3) * k2.1));
        self.scale.set(k2);
        self.backing.set(backing);
        self.frame.set(new);
        self.geometry.set((w.rect, w.client));
        *self.viewport.borrow_mut() = view;
        let now = page.eval("window.__strobeOs?.viewport ?? 0")?.as_f64().unwrap_or(0.0);
        self.resized.set(self.resized.get() + (now - before).max(0.0));
        page.eval("window.__strobeOs && (__strobeOs.moves = []), true")?;
        *self.expect.borrow_mut() = Some(Vec::new());
        Ok(())
    }

    /// Maps the page onto the screen again when its window or viewport moved, resized, zoomed or was
    /// replaced since the last map (setup may do any of those): after setup, before the runs.
    pub fn recheck(&self, page: &Page) -> Result<()> {
        match self.moved(page)? {
            Some(_) => self.map(page),
            None => Ok(()),
        }
    }

    /// Refuses `InputMismatch` when the window or the page's viewport moved, resized, zoomed or was
    /// replaced since strobe mapped the page onto the screen: input after that went to points mapped
    /// for where it was. After each run and reset (`what`), outside its span.
    /// The page's own word counts too: a `resize`, `fullscreenchange` or visual viewport `resize` since
    /// `begin` refuses even when the viewport came back (resized and restored within the run).
    /// A size strobe asked for (`page.window`, `commanded`) is no such change: only what came besides it,
    /// and a map worked out for it that the pointer moves posted since do not land on.
    pub fn placed(&self, page: &Page, what: &str) -> Result<()> {
        let why = match self.moved(page)? {
            Some(why) => Some(why.to_string()),
            None => match page.eval("window.__strobeOs?.viewport ?? 0")?.as_f64().unwrap_or(0.0) - self.resized.get() {
                n if n <= 0.0 => None,
                n => Some(format!("the page's viewport resized {n} time(s) (resize, fullscreenchange or a visual viewport resize) besides any page.window strobe was asked for")),
            },
        };
        if why.is_none() {
            self.landed(page, what)?;
        }
        match why {
            Some(why) => Err(Refusal::new(ValidatorId::InputMismatch, format!("{why} during the {what}: strobe's OS input after that went to screen points mapped for where the page was"))
                .fix("keep the browser's window, its size and the page's zoom as they are while the runs go (setup may change them: strobe maps the page again after it)")
                .into()),
            None => Ok(()),
        }
    }

    /// After a map worked out from a size strobe asked for: the newest pointer move strobe posted to a new
    /// point since is among the trusted moves the page saw last (its last 16), within 2 page pixels of
    /// where the map put it, else `InputMismatch`. Moves the page saw that strobe did not post (a
    /// browser's own as the window changes under the cursor, the cursor put back where the user left it
    /// after the runs) are no evidence either way. Once one landed, the map has proved itself and is
    /// checked no more; until strobe posts a move there is nothing to check.
    fn landed(&self, page: &Page, what: &str) -> Result<()> {
        self.look_landed(page)?;
        match self.missed.take() {
            Some(why) => Err(refused(format!("after the window size strobe was asked for (page.window), the last pointer move strobe posted in the {what} {why}: input went to points mapped for another place"))),
            None => Ok(()),
        }
    }

    /// `landed`'s look, waiting up to 500 ms for the move to arrive (a browser dispatches moves once a
    /// frame), before anything else moves the pointer: `Driver::release_all` puts the cursor back where
    /// the user left it, and a browser that coalesces moves keeps only that one. A miss is kept for
    /// `placed` to refuse.
    pub(super) fn look_landed(&self, page: &Page) -> Result<()> {
        let Some(want) = self.expect.borrow().as_ref().and_then(|e| e.last().copied()) else { return Ok(()) };
        let until = Instant::now() + Duration::from_millis(500);
        let (near, m, any) = loop {
            let v = page.eval("JSON.stringify(window.__strobeOs?.moves ?? [])")?;
            let moves: Vec<[f64; 2]> = v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
            let (near, m) = moves.iter().map(|m| ((want.0 - m[0]).hypot(want.1 - m[1]), *m)).fold((f64::INFINITY, [0.0; 2]), |a, b| if b.0 < a.0 { b } else { a });
            if near <= 2.0 || Instant::now() >= until {
                break (near, m, !moves.is_empty());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        *self.expect.borrow_mut() = None;
        if near > 2.0 {
            let (o, k) = (self.origin.get(), self.scale.get());
            let seen = if any { format!("the nearest the page saw landed at ({:.0}, {:.0}), {near:.0} page pixels away", m[0], m[1]) } else { "the page saw no pointer move".to_string() };
            *self.missed.borrow_mut() = Some(format!(
                "should have landed at ({:.0}, {:.0}) in the page by the map strobe worked out for the new size (origin ({:.0}, {:.0}), scale {:.3}), and {seen}",
                want.0, want.1, o.0, o.1, k.0
            ));
        }
        Ok(())
    }

    /// What changed since the last map, if anything: the window (replaced, moved or resized, from the
    /// OS) or the viewport (moved, resized or zoomed, as the page sees it).
    fn moved(&self, page: &Page) -> Result<Option<&'static str>> {
        let w = host::main_window(&self.pids.borrow()).ok_or_else(|| no_window(&self.pids.borrow()))?;
        Ok(if w.hwnd != self.top.get() {
            Some("the browser's window was replaced")
        } else if (w.rect, w.client) != self.geometry.get() {
            Some("the browser's window moved or resized")
        } else if viewport(page)? != *self.viewport.borrow() {
            Some("the page's viewport moved, resized or zoomed")
        } else {
            None
        })
    }

    /// Three points in the page, in the input thread's own (DPI-aware) screen units, each posted as a
    /// move from just beside it; where the trusted `pointermove`s landed in the page gives the origin
    /// and scale from two, and proves posted input reaches it. The points are fractions of the window's
    /// frame as the input thread reads it, below the share of the window's height the browser draws
    /// above the page (its outer height less the viewport's, over its outer height, as the page sees
    /// them: a ratio of one unit, so a device scale, a page zoom or a Retina display changes nothing),
    /// and whole screen units, as the posters send them. The map must hold: the third point lands
    /// where it says, and the viewport it puts on the screen lies in the window's frame; else the
    /// block is refused `InputMismatch`.
    fn map(&self, page: &Page) -> Result<()> {
        if host::session_locked() {
            return Err(locked());
        }
        let pids = self.pids.borrow().clone();
        let w = host::main_window(&pids).ok_or_else(|| no_window(&pids))?;
        let view = viewport(page)?;
        let at = |i: usize| view.get(i).copied().unwrap_or(0.0);
        let (iw, ih, oh) = (at(2), at(3), at(6));
        let r = self.poster.frame(w.hwnd)?;
        // An estimate (borders, and a title bar the page does not count, move it a little): the points
        // are 0.3 of the content's height from its edges either way.
        let chrome = if oh > 0.0 { ((oh - ih) / oh).clamp(0.0, 0.5) } else { 0.0 };
        let (top, height) = (r.1 + chrome * r.3, r.3 * (1.0 - chrome));
        let points = [(0.3, 0.4), (0.7, 0.7), (0.7, 0.4)].map(|(fx, fy): (f64, f64)| ((r.0 + fx * r.2).round(), (top + fy * height).round()));
        let mut probes = Vec::new();
        let mut landed = Vec::new();
        for (px, py) in points {
            let t = self.poster.target(&pids, px, py)?;
            if t.top() != w.hwnd {
                return Err(covered_by_own(px, py));
            }
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
        let origin = (probes[0].0 - landed[0][0] * scale.0, probes[0].1 - landed[0][1] * scale.1);
        // The third move, which the map did not use, lands where the map puts it (within 2 page pixels).
        let at = ((probes[2].0 - origin.0) / scale.0, (probes[2].1 - origin.1) / scale.1);
        if (at.0 - landed[2][0]).abs() > 2.0 || (at.1 - landed[2][1]).abs() > 2.0 {
            return Err(refused(format!(
                "the page's points do not map to the screen by one origin and scale: a third move posted at ({:.0}, {:.0}) landed at ({:.1}, {:.1}) in the page, where the map put ({:.1}, {:.1})",
                probes[2].0, probes[2].1, landed[2][0], landed[2][1], at.0, at.1
            )));
        }
        // The viewport the map puts on the screen lies in the window's frame (within 3 screen units).
        let slack = 3.0;
        let (x0, y0, x1, y1) = (origin.0, origin.1, origin.0 + iw * scale.0, origin.1 + ih * scale.1);
        if x0 < r.0 - slack || y0 < r.1 - slack || x1 > r.0 + r.2 + slack || y1 > r.1 + r.3 + slack {
            return Err(refused(format!(
                "the page's viewport, mapped onto the screen from where strobe's moves landed, is ({x0:.0}, {y0:.0})-({x1:.0}, {y1:.0}), outside the browser's window ({:.0}, {:.0})-({:.0}, {:.0}) as the OS reports it",
                r.0,
                r.1,
                r.0 + r.2,
                r.1 + r.3
            )));
        }
        self.origin.set(origin);
        self.scale.set(scale);
        self.frame.set(r);
        self.backing.set(device_per_unit(w.hwnd));
        *self.expect.borrow_mut() = None;
        self.top.set(w.hwnd);
        self.geometry.set((w.rect, w.client));
        *self.viewport.borrow_mut() = view;
        self.moved_to.set(probes.last().map(|p| (p.0 as i64, p.1 as i64)));
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
        super::sent(t_ns, kind == "discrete" || (kind == "scroll" && first), None);
        self.last_input.set(t_ns);
        self.log.borrow_mut().inputs.push(Input { t_ns, kind, first, press_ns: None, burst: false });
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

    /// The window the OS hit-tests at a screen point, which must be the page's window itself: not
    /// another app's (`WindowHidden` from the poster), nor another of the browser's (a dialog, a popup,
    /// its developer tools).
    fn target(&self, x: f64, y: f64) -> Result<Target> {
        if host::session_locked() {
            return Err(locked());
        }
        let t = self.poster.target(&self.pids.borrow(), x, y)?;
        if t.top() != self.top.get() {
            return Err(covered_by_own(x, y));
        }
        Ok(t)
    }

    /// A pointer move posted to `t`, counted for `delivered` when it goes to another point.
    fn move_to(&self, t: Target, buttons: usize) -> Result<u64> {
        let at = self.poster.move_to(t, buttons)?;
        let p = t.point();
        if self.moved_to.replace(Some(p)) != Some(p) {
            self.count(MOVE);
            if let Some(e) = self.expect.borrow_mut().as_mut() {
                let (o, k) = (self.origin.get(), self.scale.get());
                e.push(((p.0 as f64 - o.0) / k.0, (p.1 as f64 - o.1) / k.1));
            }
        }
        Ok(at)
    }

    /// A pointer move to a screen point; while a button is held a drag's move, kept for `input.rate`.
    fn pointer_to(&self, x: f64, y: f64, t: Target) -> Result<u64> {
        self.park(x, y)?;
        let at = self.move_to(t, self.pressed.get().map_or(0, input::held_flag))?;
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
        self.count(if down { PRESS } else { RELEASE });
        if down {
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

    /// After strobe scrolled an element into view: two refreshes, then until the page rests.
    pub(super) fn rest_frames(&self) -> Result<bool> {
        self.rest(self.frame() * 2, self.frame())
    }

    /// Lets go of a held button and keys (between runs).
    pub fn release_all(&self) {
        if let Some(b) = self.pressed.get() {
            let _ = self.button(b, false, false);
        }
        self.held.borrow_mut().clear();
        self.poster.release_all(self.top.get());
    }

    /// Starts a count of what arrives (before each run, and its reset): strobe's posted input and the
    /// page's trusted events from zero, so one stretch's input never covers for another's.
    pub fn begin(&self, page: &Page) -> Result<()> {
        self.posted.set([0; 6]);
        self.resized.set(0.0);
        let v = page.eval("window.__strobeOs && (__strobeOs.n.fill(0), __strobeOs.viewport = 0), performance.timeOrigin")?;
        self.document.set(v.as_f64().unwrap_or(0.0));
        Ok(())
    }

    /// Refuses `InputMismatch` unless what strobe posted since `begin` reached the page as trusted
    /// events (`COUNTED`): every key down and up and every button press and release (a page may get
    /// more, such as the Shift a capital letter needs, never fewer), and at least one wheel turn and one
    /// pointer move where strobe posted any (browsers and the OS coalesce those), within a second of
    /// looking (posted input reaches the page on its own time). A document that changed meanwhile (a
    /// navigation) took its counts with it: nothing to check.
    pub fn delivered(&self, page: &Page) -> Result<()> {
        let posted = self.posted.get();
        if posted == [0; 6] {
            return Ok(());
        }
        let enough = |i: usize, got: f64| if COUNTED[i].1 { got >= posted[i] as f64 } else { posted[i] == 0 || got > 0.0 };
        let until = Instant::now() + Duration::from_secs(1);
        let r = loop {
            let v = page.eval("JSON.stringify([performance.timeOrigin, ...(window.__strobeOs?.n ?? [0, 0, 0, 0, 0, 0])])")?;
            let r: Vec<f64> = v.as_str().and_then(|s| serde_json::from_str::<Vec<f64>>(s).ok()).unwrap_or_default();
            if r.len() < 7 || r[0] != self.document.get() {
                self.posted.set([0; 6]);
                self.document.set(r.first().copied().unwrap_or(0.0));
                return Ok(());
            }
            if (0..6).all(|i| enough(i, r[i + 1])) || Instant::now() >= until {
                break r;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        for (i, (what, _)) in COUNTED.iter().enumerate() {
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
                let typing = super::Typing::of(&o);
                let mut paced = typing == super::Typing::Paced;
                for (k, c) in s(1).chars().enumerate() {
                    // Each key is measured: what the last one drew settles first (SPEC 7.4).
                    if k > 0 && paced {
                        paced = super::settle_key(&os.presents, &os.pids.borrow(), last, delay, os.frame())?;
                    } else if k > 0 {
                        std::thread::sleep(delay);
                    }
                    let key = match c {
                        '\n' => Key::Named(Named::Enter),
                        '\t' => Key::Named(Named::Tab),
                        c => Key::Char(c),
                    };
                    last = os.poster.press(os.top.get(), None, Mods::default(), key)?;
                    os.count(KEY_DOWN);
                    os.count(KEY_UP);
                    os.input(last, "discrete", false);
                    if typing == super::Typing::Burst
                        && let Some(i) = os.log.borrow_mut().inputs.last_mut()
                    {
                        i.burst = true;
                    }
                }
                if typing == super::Typing::Burst {
                    super::unmeasured();
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
                os.count(KEY_DOWN);
                // macOS sends no key up while Command is down: the page never sees one.
                if !(cfg!(target_os = "macos") && mods.meta) {
                    os.count(KEY_UP);
                }
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
                // An `up` posts a key up only for a key strobe holds.
                let mut held = os.held.borrow_mut();
                if call == "down" && !held.contains(&key) {
                    held.push(key);
                    os.count(KEY_DOWN);
                } else if call == "up"
                    && let Some(i) = held.iter().position(|k| *k == key)
                {
                    held.remove(i);
                    os.count(KEY_UP);
                }
                drop(held);
                os.input(at, if call == "down" { "discrete" } else { "pointer" }, false);
                (opts(1), at)
            }
            "drag" => (opts(2), self.os_drag(os, &sel(0), &opts(1), &opts(2))?),
            "scroll" => {
                let (d, o) = (opts(1), opts(2));
                let (x, y, t) = self.os_aim(os, &sel(0), &o)?;
                os.park(x, y)?;
                os.input(os.move_to(t, 0)?, "pointer", false);
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
                        os.count(WHEEL);
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
                os.input(os.move_to(t, 0)?, "pointer", false);
                os.rest(Duration::ZERO, os.frame())?;
                let (dx, dy) = (d["dx"].as_f64().unwrap_or(0.0), d["dy"].as_f64().unwrap_or(0.0));
                let at = os.poster.wheel(t, dx, dy)?;
                if at == 0 {
                    return Err(Error::usage(format!(
                        "a wheel movement of {} px sends no wheel click: on X11 the wheel moves in clicks (strobe sends one per 100 px); move it at least 50 px",
                        dx.abs().max(dy.abs())
                    )));
                }
                os.count(WHEEL);
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
        os.input(os.move_to(t, 0)?, "pointer", false);
        os.pointer.set(Some((x, y, t)));
        let gap = os.frame();
        // A frame of what came before (a hover, an earlier step) settles before the press.
        let quiet = os.rest(Duration::ZERO, gap)?;
        let mut last = 0;
        for k in 0..count {
            // The press shows a pressed look, or does the click (a `mousedown` handler); the release
            // does it otherwise. It is held as a person holds a click, until what the press drew is on
            // the screen (the minimum only when the page was still drawing before the press): a
            // reaction to the press is timed from it (SPEC 7.4). A double click's presses follow at
            // once, with no settling inside the gesture, and its releases are no samples: only a
            // single click is.
            let pressed = os.poster.button(t, b, true, k > 0, mods, os.top.get())?;
            os.input(pressed, "pointer", false);
            os.count(PRESS);
            if count == 1 && quiet {
                os.rest(Duration::from_millis(60), gap)?;
            } else {
                std::thread::sleep(if count == 1 { Duration::from_millis(60) } else { gap });
            }
            last = os.poster.button(t, b, false, false, mods, os.top.get())?;
            os.count(RELEASE);
            os.input(last, if count == 1 { kind } else { "pointer" }, false);
            if count == 1
                && kind == "discrete"
                && let Some(i) = os.log.borrow_mut().inputs.last_mut()
            {
                i.press_ns = Some(pressed);
                super::sent(last, true, Some(pressed));
            }
        }
        Ok(last)
    }

    /// Clicks into the element unless focus is already in it, as a person clicks into a field (that
    /// click is no keystroke). After the click focus must be in it, in a document that has focus (one
    /// read): keys the OS sends go to what has focus, which may be elsewhere (the address bar, another
    /// field), and `InputMismatch` refuses rather than time keys the element never got.
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
            let s = Self::sel(sel)?;
            if self.read(&format!("document.hasFocus() && __strobe.focused({})", s.page_spec()), self.timeout(o))? != Value::Bool(true) {
                return Err(refused(format!("the click into {} put no focus in it, so the keys strobe would send through the OS would go elsewhere", s.source)));
            }
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
