//! The `page` driver for native targets (SPEC §9.6, §13.7): the bench file's page calls on any native
//! app, through the OS's accessibility tree (finding elements, their bounds and states, waiting on
//! their changes) and the OS's input (`a11y::input`: posted window messages on Windows, Quartz events
//! posted to the app on macOS, XTest on Linux), timed against what reached the screen. The app needs no strobe support; it needs an accessibility tree for
//! selectors, and the session's present events for `until: "frame"` and frame metrics.

use crate::model::{Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::time::Duration;

/// One input strobe gave the app, on the host clock at the moment it was posted.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Input {
    pub t_ns: u64,
    /// `discrete` (a click's release, a key press), `scroll` (a wheel step), `pointer` (a move, a
    /// hover, a click's press, a key release) or `pattern` (an accessibility action: no hit test, no
    /// pointer handling). Only `discrete` and `scroll` are interactions (SPEC §7.4).
    pub kind: &'static str,
    /// A scroll step that starts a gesture (the first of a `scroll` or a lone `wheel`).
    pub first: bool,
}

/// What a run's page calls did: inputs and phase marks, both on the host clock.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Log {
    pub inputs: Vec<Input>,
    pub marks: Vec<(String, u64)>,
    /// Each drag's move times (a button held by `down` or `drag`), for `input.rate`.
    pub drags: Vec<Vec<u64>>,
}

pub use imp::Session;

mod imp {
    use super::*;
    use crate::probe::present::PresentCapture;
    use crate::host;
    use crate::target::a11y::input::{self, Button, Key, Mods, Named, Poster, Target};
    use crate::target::a11y::{self, Accessibility, Action, Selector, Tree};
    use std::cell::Cell;
    use std::time::Instant;

    /// Quiet that counts as idle: no present and no accessibility change for this long.
    const IDLE_NS: u64 = 300_000_000;

    /// The least time between two reads of the app's tree while a wait follows its changes: an app
    /// that announces changes in a burst (an animation's every frame) is read at most once a frame,
    /// not once per change, since each read costs the app's own accessibility provider work.
    const REREAD: Duration = Duration::from_millis(16);

    pub struct Session {
        a11y: Box<dyn Accessibility>,
        poster: Poster,
        pids: RefCell<Vec<u32>>,
        main: Cell<isize>,
        log: RefCell<Log>,
        /// Change counts at the start of the run, for the per-run list of change times.
        last_input: Cell<u64>,
        /// The tree's size at the last snapshot (`StateDrift` reads it at each run's end).
        size: Cell<Option<usize>>,
        /// Whether the OS cursor could be parked on the pointer's point (false on a locked session).
        parked: Cell<Option<bool>>,
        /// Where the pointer last went (screen point and where it landed), and the mouse button
        /// `down` or `drag` holds with the window it pressed in.
        pointer: Cell<Option<(f64, f64, Target)>>,
        pressed: Cell<Option<(Button, isize)>>,
        pacer: crate::driver::Pacer,
    }

    impl Session {
        pub fn open(pids: &[u32], main: isize) -> Result<Session> {
            Ok(Session {
                a11y: a11y::open(pids)?,
                poster: Poster::start()?,
                pids: RefCell::new(pids.to_vec()),
                main: Cell::new(main),
                log: RefCell::new(Log::default()),
                last_input: Cell::new(0),
                size: Cell::new(None),
                parked: Cell::new(None),
                pointer: Cell::new(None),
                pressed: Cell::new(None),
                pacer: crate::driver::Pacer::default(),
            })
        }

        /// The button flag a move carries while a button is held (Windows' `MK_*`; other OSes track it).
        fn held_flag(&self) -> usize {
            self.pressed.get().map_or(0, |(b, _)| input::held_flag(b))
        }

        /// A pointer move to `t` at screen point (x, y), the cursor parked there; while a button is held it
        /// is a drag's move, kept for `input.rate`.
        fn pointer_to(&self, x: f64, y: f64, t: Target) -> Result<u64> {
            self.park(x, y)?;
            let at = self.poster.move_to(t, self.held_flag())?;
            self.pointer.set(Some((x, y, t)));
            let drag = self.pressed.get().is_some();
            if drag && let Some(d) = self.log.borrow_mut().drags.last_mut() {
                d.push(at);
            }
            self.input(at, "pointer", false);
            Ok(at)
        }

        /// A mouse button going down or up where the pointer is.
        fn pointer_button(&self, b: Button, down: bool) -> Result<u64> {
            let (_, _, t) = self.pointer.get().ok_or_else(|| Error::usage("page.down(\"Mouse\") presses where the pointer is: move it first (hover, move, or down's at option)"))?;
            let top = self.main.get();
            let at = self.poster.button(t, b, down, false, Mods::default(), top)?;
            self.pressed.set(down.then_some((b, top)));
            if down {
                self.log.borrow_mut().drags.push(vec![at]);
            }
            self.input(at, if down { "pointer" } else { "discrete" }, false);
            Ok(at)
        }

        pub fn set_pids(&self, pids: &[u32]) {
            self.a11y.set_pids(pids);
            *self.pids.borrow_mut() = pids.to_vec();
        }

        pub fn set_main(&self, hwnd: isize) {
            self.main.set(hwnd);
        }

        pub fn take_log(&self) -> Log {
            std::mem::take(&mut *self.log.borrow_mut())
        }

        pub fn mark(&self, name: &str) {
            self.log.borrow_mut().marks.push((name.to_string(), host::now_ns()));
        }

        /// Elements in the app's windows at the last snapshot.
        pub fn elements(&self) -> Option<usize> {
            self.size.get()
        }

        pub fn release_all(&self) {
            if let Some((b, _)) = self.pressed.get() {
                let _ = self.pointer_button(b, false);
            }
            self.poster.release_all(self.main.get());
        }

        /// Times of the accessibility changes heard so far (the newest 65,536).
        pub fn changes(&self) -> Vec<u64> {
            self.a11y.change_times()
        }

        /// Whether the OS cursor could be parked on the pointer's points (hover tracking agrees).
        pub fn parked(&self) -> Option<bool> {
            self.parked.get()
        }

        fn input(&self, t_ns: u64, kind: &'static str, first: bool) {
            self.last_input.set(t_ns);
            self.log.borrow_mut().inputs.push(Input { t_ns, kind, first });
        }

        /// The app's windows to search, main first: `window` (a title, or part of one) picks one.
        fn windows(&self, window: Option<&str>) -> Result<Vec<isize>> {
            let all = host::top_windows(&self.pids.borrow());
            let main = self.main.get();
            if let Some(w) = window.filter(|w| *w != "main") {
                return all
                    .iter()
                    .find(|x| x.1.contains(w))
                    .map(|x| vec![x.0])
                    .ok_or_else(|| Error::usage(format!("window {w:?}: the app's visible windows are {}", all.iter().map(|x| format!("{:?}", x.1)).collect::<Vec<_>>().join(", "))));
            }
            let mut v = vec![main];
            v.extend(all.iter().map(|x| x.0).filter(|h| *h != main));
            Ok(v)
        }

        fn snapshot(&self, window: Option<&str>) -> Result<Tree> {
            let t = self.a11y.snapshot(&self.windows(window)?)?;
            self.size.set(Some(t.nodes.len()));
            Ok(t)
        }

        /// A read of the tree inside a wait (`reads`: how many it took so far, and when the last
        /// began). The first and the first re-read after a change go at once, so a step whose
        /// reaction comes in one change costs no sleep; from the second re-read on, a read waits until
        /// `REREAD` after the one before (never past `deadline`): only a burst is spread out.
        fn reread(&self, window: Option<&str>, reads: &mut (u32, Option<Instant>), deadline: Instant) -> Result<Tree> {
            if reads.0 >= 2
                && let Some(w) = reads.1.map(|at| (at + REREAD).min(deadline)).and_then(|next| next.checked_duration_since(Instant::now()))
            {
                std::thread::sleep(w);
            }
            *reads = (reads.0 + 1, Some(Instant::now()));
            self.snapshot(window)
        }

        /// The first visible element `sel` matches, waiting up to `timeout` for the app's tree to show
        /// it (re-read on each change it announces, never on a timer, except a placeholder tree:
        /// `settle`). An app with no tree at all says so.
        fn find(&self, sel: &Selector, window: Option<&str>, timeout: Duration) -> Result<(Tree, usize)> {
            let deadline = Instant::now() + timeout;
            let mut reads = (0, None);
            loop {
                let seen = self.a11y.changes().count;
                let tree = self.reread(window, &mut reads, deadline)?;
                if let Some(&i) = tree.find(sel).first() {
                    return Ok((tree, i));
                }
                if Instant::now() >= deadline {
                    let secs = timeout.as_secs_f64();
                    let why = if let Some(n) = sel.index_misses(&tree) {
                        format!("only {n} visible element{} matched in {secs:.1} s, and index={} needs at least {}", if n == 1 { "" } else { "s" }, sel.index(), sel.index() + 1)
                    } else if let Some(miss) = sel.text_misses(&tree) {
                        format!("{miss} in {secs:.1} s")
                    } else if tree.nodes.len() <= tree.windows.len() {
                        format!(
                            "the app's windows expose no accessibility tree ({} element{}): strobe finds elements through it, so this app can be measured from outside only",
                            tree.nodes.len(),
                            if tree.nodes.len() == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("nothing visible matched in {secs:.1} s ({} elements in its windows)", tree.nodes.len())
                    };
                    return Err(Error::timeout(format!("selector {:?}: {why}", sel.source)).hint("check the key with page.query(\"role=button\") or an accessibility inspector"));
                }
                self.a11y.wait_change(seen, settle(&tree, deadline));
            }
        }

        /// The centre of the element's visible part, checked against what the OS hit-tests there
        /// (an element under another one is not clickable), and the window that point posts to.
        fn aim(&self, tree: &Tree, i: usize) -> Result<(f64, f64, Target)> {
            let r = tree.visible_rect(i).ok_or_else(|| Error::msg("the element is not visible"))?;
            let (x, y) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
            if let Err(what) = self.a11y.hit(i, x, y)? {
                if host::session_locked() {
                    return Err(Error::refused(
                        "SessionLocked",
                        format!("pointer input at {:?}: the session is locked, so the OS hit-tests every point to the lock screen ({what}) and a click cannot be aimed", tree.key_path(i)),
                        "run pointer cases on an unlocked session; keys reach a focused element while locked (page.act(sel, \"focus\"), then type or press)",
                    ));
                }
                return Err(Error::refused(
                    "WindowHidden",
                    format!("{:?} at ({x:.0}, {y:.0}) is under {what}: a click there reaches that, not the element", tree.key_path(i)),
                    "wait until the element is uncovered (until: a selector), or close what covers it",
                ));
            }
            let t = self.poster.target(&self.pids.borrow(), x, y)?;
            Ok((x, y, t))
        }

        /// Parks the OS cursor on the point so hover tracking agrees with the posted input.
        fn park(&self, x: f64, y: f64) -> Result<()> {
            let ok = self.poster.park(x, y)?;
            self.parked.set(Some(ok));
            Ok(())
        }

        fn frame_ms(&self, x: f64, y: f64) -> u64 {
            host::refresh_at(x as i32, y as i32).map_or(16, |hz| (1000 / hz.max(1)).max(4) as u64)
        }

        /// Resolves after an input: a selector shows, the next frame reached the screen, or idle.
        fn until(&self, opts: &Value, after: u64, presents: &RefCell<Option<PresentCapture>>) -> Result<()> {
            let Some(u) = opts.get("until").filter(|u| !u.is_null()) else { return Ok(()) };
            let t = timeout(opts);
            match u.as_str() {
                Some("idle") => self.idle(after, t, presents).map(|_| ()),
                Some("frame") => self.frame(after, t, presents).map(|_| ()),
                _ => self.find(&parse_sel(u)?, opts["window"].as_str(), t).map(|_| ()),
            }
        }

        /// The first present of the app that started after `after` and reached the screen.
        fn frame(&self, after: u64, t: Duration, presents: &RefCell<Option<PresentCapture>>) -> Result<u64> {
            let mut p = presents.borrow_mut();
            let p = p.as_mut().ok_or_else(|| {
                Error::refused(
                    "NoPresentSource",
                    "until: \"frame\" waits for the app's next frame on screen, from OS present events, which this block has none of",
                    if cfg!(windows) {
                        "run strobe up for present events, or wait for a selector"
                    } else if cfg!(target_os = "macos") {
                        "grant Screen Recording (frames come from ScreenCaptureKit; strobe status says to which app), or wait for a selector"
                    } else {
                        "frames come from X Present on an X11 server with a real display and no compositing manager; wait for a selector instead"
                    },
                )
            })?;
            let v = p.live(&self.pids.borrow(), after, t)?;
            if v.next_screen_ns == 0 {
                return Err(Error::timeout(format!("until frame: the app showed no frame in {} s", t.as_secs())));
            }
            Ok(v.next_screen_ns)
        }

        /// Waits until the app presented nothing and announced no accessibility change for 300 ms
        /// after `after`; returns when the quiet began.
        pub fn idle(&self, after: u64, t: Duration, presents: &RefCell<Option<PresentCapture>>) -> Result<u64> {
            let deadline = Instant::now() + t;
            let first = self.a11y.changes().count;
            loop {
                let c = self.a11y.changes();
                let mut last = after.max(c.last_ns).max(self.last_input.get());
                if let Some(p) = presents.borrow_mut().as_mut() {
                    last = last.max(p.live(&self.pids.borrow(), u64::MAX, Duration::ZERO)?.last_start_ns);
                }
                // Present events reach strobe within a flush (10 ms): allow for it before calling it quiet.
                let quiet_at = last + IDLE_NS + 30_000_000;
                let now = host::now_ns();
                if now >= quiet_at {
                    return Ok(last);
                }
                if Instant::now() >= deadline {
                    return Err(self.busy(t, c.count - first, presents));
                }
                let wake = Instant::now() + Duration::from_nanos(quiet_at - now);
                self.a11y.wait_change(c.count, wake.min(deadline));
            }
        }

        /// Why `idle` never saw the app quiet, with counts: the frames of the app that reached the
        /// screen in the last second (from the block's present events, when it has them) and its
        /// accessibility changes in the last second and over the whole wait.
        fn busy(&self, t: Duration, changes: u64, presents: &RefCell<Option<PresentCapture>>) -> Error {
            let now = host::now_ns();
            let since = now.saturating_sub(1_000_000_000);
            let recent = self.a11y.change_times().iter().filter(|&&c| c > since).count();
            // Each step finds the next present after the last one found; a second of frames is at most a few hundred.
            let frames = presents.borrow_mut().as_mut().map(|p| {
                let (mut n, mut after) = (0usize, since);
                while n < 1000 {
                    match p.live(&self.pids.borrow(), after, Duration::ZERO) {
                        Ok(v) if v.next_start_ns != 0 && v.next_start_ns <= now => {
                            n += 1;
                            after = v.next_start_ns;
                        }
                        _ => break,
                    }
                }
                n
            });
            let input = self.last_input.get() > since;
            let drew = frames.is_some_and(|f| f > 0);
            let what = match (drew, recent > 0) {
                (true, false) => "it kept drawing (an animation, a caret or a spinner) without changing its accessibility tree",
                (false, true) => "its accessibility tree kept changing (a clock, a counter or a progress value)",
                (true, true) => "it kept drawing and changing its accessibility tree",
                (false, false) if frames.is_none() => "its accessibility tree went quiet only at the end, and this block has no present events to tell whether it kept drawing",
                (false, false) => "it went quiet only at the end",
            };
            let counted = match frames {
                Some(f) => format!("{f} frame{} reached the screen", if f == 1 { "" } else { "s" }),
                None => "frames not counted (no present events)".to_string(),
            };
            Error::timeout(format!(
                "idle: the app never went quiet for {} ms in {} s: {what}. In the last second {counted} and its tree changed {recent} time{} ({changes} changes over the wait){}",
                IDLE_NS / 1_000_000,
                t.as_secs(),
                if recent == 1 { "" } else { "s" },
                if input { "; strobe's own input came in that second too" } else { "" }
            ))
            .hint("wait for what the step brings instead (until: a selector), or stop what keeps the app busy in setup")
        }

        /// A launch's `until` selector: when strobe began the last read of the app's tree that lacked
        /// it (None when the first read had it) and when it finished the first that had it.
        pub fn appeared(&self, sel: &Value, t: Duration) -> Result<(Option<u64>, u64)> {
            let sel = parse_sel(sel)?;
            let deadline = Instant::now() + t;
            let mut lacked = None;
            loop {
                let seen = self.a11y.changes().count;
                let tree = self.snapshot(None)?;
                let done = host::now_ns();
                if !tree.find(&sel).is_empty() {
                    return Ok((lacked, done));
                }
                lacked = Some(tree.at_ns);
                if Instant::now() >= deadline {
                    return Err(Error::timeout(format!("selector {:?}: nothing visible matched in {} s", sel.source, t.as_secs())));
                }
                self.a11y.wait_change(seen, settle(&tree, deadline));
            }
        }

        /// What the app's windows show: their names, then their visible open dialogs' and headings'
        /// names in tree order (at most 6). A provider that answers first with a placeholder root
        /// (lazy: `sparse`) gets up to `t` to build its tree; a full tree is read
        /// once.
        pub fn screen(&self, t: Duration) -> Result<(Vec<String>, Vec<String>)> {
            let deadline = Instant::now() + t;
            let mut seen = self.a11y.changes().count;
            let mut tree = self.snapshot(None)?;
            // A lazy provider's first answer is a few nodes (6 of over 100 in one app) and the rest comes a moment
            // later, without a notification on macOS: while the tree is a placeholder, read again on
            // each change and every 250 ms, until it is not or two reads agree on a tree (a small app's
            // whole tree is a few nodes).
            while sparse(&tree) && Instant::now() < deadline {
                self.a11y.wait_change(seen, (Instant::now() + Duration::from_millis(250)).min(deadline));
                seen = self.a11y.changes().count;
                let again = self.snapshot(None)?;
                let agreed = again.nodes.len() == tree.nodes.len() && again.nodes.len() > again.windows.len();
                tree = again;
                if agreed {
                    break;
                }
            }
            let text = |i: usize| {
                let n = &tree.nodes[i];
                let s = if n.name.trim().is_empty() { n.value.clone().unwrap_or_default() } else { n.name.clone() };
                s.split_whitespace().collect::<Vec<_>>().join(" ")
            };
            let names: Vec<String> = (0..tree.nodes.len()).filter(|&i| tree.nodes[i].parent.is_none()).map(text).filter(|s| !s.is_empty()).collect();
            let mut marks: Vec<String> = Vec::new();
            for dialog in [true, false] {
                for i in 0..tree.nodes.len() {
                    let n = &tree.nodes[i];
                    let wanted = if dialog { n.dialog && n.parent.is_some() } else { n.heading };
                    if wanted && tree.visible_rect(i).is_some() {
                        let s = text(i);
                        if !s.is_empty() && !marks.contains(&s) && marks.len() < 6 {
                            marks.push(s);
                        }
                    }
                }
            }
            Ok((names, marks))
        }

        /// The visible elements that carry a key (an automation id), in tree order, at most `max`:
        /// (key path, role, name).
        pub fn keys(&self, max: usize) -> Result<Vec<(String, String, String)>> {
            let tree = self.snapshot(None)?;
            Ok((0..tree.nodes.len())
                .filter(|&i| !tree.nodes[i].id.is_empty() && tree.visible_rect(i).is_some())
                .take(max)
                .map(|i| {
                    let d = tree.describe(i);
                    (d["path"].as_str().unwrap_or("").to_string(), d["role"].as_str().unwrap_or("").to_string(), tree.nodes[i].name.split_whitespace().collect::<Vec<_>>().join(" "))
                })
                .collect())
        }

        /// Whether a selector matches a visible element within `t`.
        pub fn shows(&self, sel: &Value, t: Duration) -> Result<bool> {
            match self.find(&parse_sel(sel)?, None, t) {
                Ok(_) => Ok(true),
                Err(e) if e.code == 124 => Ok(false),
                Err(e) => Err(e),
            }
        }

        /// Elements in the app's windows now (`StateDrift`).
        pub fn count_elements(&self) -> Result<usize> {
            Ok(self.snapshot(None)?.nodes.len())
        }

        /// The app's windows' tree now, main first (what a run starts from: `ResetIncomplete`), and the
        /// count of changes the app had announced when it was read.
        pub fn tree(&self) -> Result<(Tree, u64)> {
            let seen = self.a11y.changes().count;
            Ok((self.snapshot(None)?, seen))
        }

        /// Waits until the app announces a change after the `seen`th, or `t` passes.
        pub fn wait_change(&self, seen: u64, t: Duration) {
            self.a11y.wait_change(seen, Instant::now() + t);
        }

        /// Before an input that is measured: at least `min`, then until the app has presented
        /// nothing for two refreshes (at most 300 ms), so a frame of what came before (a hover, a
        /// click's pressed look) is not taken for the input's (SPEC §7.4). Reads only strobe's own
        /// copy of the present events.
        fn rest(&self, min: Duration, refresh: Duration, presents: &RefCell<Option<PresentCapture>>) -> Result<()> {
            std::thread::sleep(min);
            let quiet = (refresh * 2).as_nanos() as u64;
            let until = Instant::now() + Duration::from_millis(300);
            while let Some(p) = presents.borrow_mut().as_mut() {
                let last = p.live(&self.pids.borrow(), u64::MAX, Duration::ZERO)?.last_start_ns;
                let now = host::now_ns();
                if last + quiet <= now || Instant::now() >= until {
                    break;
                }
                std::thread::sleep(Duration::from_nanos(last + quiet - now).min(Duration::from_millis(20)));
            }
            Ok(())
        }

        /// Where keys for `sel` go: its window, and the element's own window when it is one (a
        /// classic control takes its keys there, as it does once it has focus); without a selector,
        /// the main window's UI thread's focus window. An element without focus is clicked first, as
        /// a person clicks into a field (that click is no keystroke), except a control with its own
        /// window on a locked session, where no click can be aimed.
        fn key_target(&self, sel: &Value, o: &Value, presents: &RefCell<Option<PresentCapture>>) -> Result<(isize, Option<isize>)> {
            if no_selector(sel) {
                return Ok((self.main.get(), None));
            }
            let (tree, i) = self.find(&parse_sel(sel)?, o["window"].as_str(), timeout(o))?;
            let top = tree.windows[tree.nodes[i].window];
            let own = tree.nodes[i].hwnd;
            let to = (own != 0 && own != top).then_some(own);
            if !tree.focus_within(i) && !(to.is_some() && host::session_locked()) {
                self.click(sel, &json!({ "window": o["window"] }), presents, "pointer")?;
            }
            Ok((top, to))
        }

        /// `drag(selector, path, { duration, button })`: presses at the element's centre (or where the
        /// pointer is, with `""`), moves along `path` (offsets in device-independent pixels, spread evenly
        /// over `duration`) one move per display refresh on strobe's clock, each posted to the window it
        /// pressed in without waiting for the app, and releases. A button `down` holds stays held.
        /// Returns when the last input was posted.
        fn drag(&self, sel: &Value, path: &Value, o: &Value) -> Result<u64> {
            let path = a11y::keys::parse_path(path)?;
            let (x0, y0, t0) = if no_selector(sel) {
                self.pointer.get().ok_or_else(|| Error::usage("page.drag(\"\", …) starts where the pointer is: move it first, or name an element"))?
            } else {
                let (tree, i) = self.find(&parse_sel(sel)?, o["window"].as_str(), timeout(o))?;
                self.aim(&tree, i)?
            };
            let b = Button::parse(o["button"].as_str())?;
            let main = self.main.get();
            // Screen units per device-independent pixel in the main window (physical pixels; points on macOS).
            let (ox, _) = self.poster.client_point(main, 0.0, 0.0)?;
            let (ux, _) = self.poster.client_point(main, 100.0, 0.0)?;
            let unit = ((ux - ox) / 100.0).abs().max(0.01);
            let point = |t: f64| {
                let (dx, dy) = a11y::keys::path_at(&path, t);
                (x0 + dx * unit, y0 + dy * unit)
            };
            let (x, y) = point(0.0);
            self.pointer_to(x, y, t0.moved(x, y))?;
            let held = self.pressed.get().is_some();
            if held {
                self.log.borrow_mut().drags.push(Vec::new());
            } else {
                self.pointer_button(b, true)?;
            }
            let duration = o["duration"].as_f64().unwrap_or(1000.0).max(1.0);
            let frame = host::refresh_at(x0 as i32, y0 as i32).map_or(1000.0 / 60.0, |hz| 1000.0 / hz.max(1) as f64);
            let steps = (duration / frame).ceil().max(1.0) as u32;
            let start = Instant::now();
            let mut last = 0;
            for k in 1..=steps {
                self.pacer.wait(start + Duration::from_secs_f64(frame * k as f64 / 1e3), Duration::from_secs_f64(frame / 1e3));
                let (x, y) = point(k as f64 / steps as f64);
                last = self.pointer_to(x, y, t0.moved(x, y))?;
            }
            if !held {
                last = self.pointer_button(b, false)?;
            }
            Ok(last)
        }

        /// `find`, then until the element is enabled: a click on a disabled one does nothing.
        fn find_enabled(&self, sel: &Selector, window: Option<&str>, t: Duration) -> Result<(Tree, usize)> {
            let deadline = Instant::now() + t;
            loop {
                let seen = self.a11y.changes().count;
                let (tree, i) = self.find(sel, window, deadline.saturating_duration_since(Instant::now()))?;
                if tree.nodes[i].enabled {
                    return Ok((tree, i));
                }
                if Instant::now() >= deadline {
                    let n = &tree.nodes[i];
                    let what = Some(tree.key_path(i)).filter(|p| !p.is_empty()).unwrap_or_else(|| format!("{} {:?}", n.role_text, n.name));
                    return Err(Error::timeout(format!("selector {:?}: {what} stayed disabled for {} s, and a click on a disabled element does nothing", sel.source, t.as_secs()))
                        .hint("wait for what enables it, or pick an enabled element (page.query lists enabled)"));
                }
                self.a11y.wait_change(seen, deadline);
            }
        }

        /// After `setValue`: waits up to a second (on the app's change events) for node `i` to hold
        /// `want`, and fails when it does not, since a bench that set a value the app ignored measures
        /// something else. A toolkit applies the write on its own turn, so it is read back from the app.
        fn took(&self, i: usize, want: &str, sel: &Value) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let seen = self.a11y.changes().count;
                let Some(got) = self.a11y.value(i)? else { return Ok(()) };
                if same_value(want, &got) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(Error::usage(format!(
                        "page.act({sel}, \"setValue\", {{ value: {want:?} }}): the element still holds {got:?} a second later, so the app ignored the write or keeps the value in another form"
                    ))
                    .hint("check what it holds with page.query; a control that takes no accessibility writes takes typing (page.type), and a value it clamps or rounds is set as the value it shows"));
                }
                // An app may apply the value without announcing it: read again at least every 50 ms.
                self.a11y.wait_change(seen, (Instant::now() + Duration::from_millis(50)).min(deadline));
            }
        }

        fn click(&self, sel: &Value, opts: &Value, presents: &RefCell<Option<PresentCapture>>, kind: &'static str) -> Result<()> {
            let (tree, i) = self.find_enabled(&parse_sel(sel)?, opts["window"].as_str(), timeout(opts))?;
            let (x, y, t) = self.aim(&tree, i)?;
            let b = Button::parse(opts["button"].as_str())?;
            let mods = Mods::parse_list(&opts["modifiers"])?;
            let count = opts["count"].as_u64().unwrap_or(1).clamp(1, 3);
            self.park(x, y)?;
            let top = tree.windows[tree.nodes[i].window];
            let gap = Duration::from_millis(self.frame_ms(x, y));
            self.input(self.poster.move_to(t, 0)?, "pointer", false);
            self.pointer.set(Some((x, y, t)));
            let mut last = 0;
            for k in 0..count {
                // The press shows a pressed look at most; the release is the click's action, held
                // as a person holds a click (a double click's second press follows at once).
                self.input(self.poster.button(t, b, true, k > 0, mods, top)?, "pointer", false);
                if count > 1 {
                    std::thread::sleep(gap);
                } else {
                    self.rest(Duration::from_millis(60), gap, presents)?;
                }
                last = self.poster.button(t, b, false, false, mods, top)?;
                self.input(last, kind, false);
            }
            self.until(opts, last, presents)
        }
    }

    /// A tree that is a placeholder at most: none yet, or a few elements per window. A lazy provider
    /// answers first with one and builds the rest a moment later, without a change event
    /// on macOS.
    fn sparse(tree: &Tree) -> bool {
        tree.nodes.len() <= tree.windows.len().max(1) * 8
    }

    /// How long to wait for the next change: until the deadline, except while the tree is a
    /// placeholder (`sparse`: no event may come as it fills in), when it is read again in 50 ms.
    fn settle(tree: &Tree, deadline: Instant) -> Instant {
        if sparse(tree) { deadline.min(Instant::now() + Duration::from_millis(50)) } else { deadline }
    }

    /// A wheel movement too small for a wheel that moves in whole clicks (X11's XTest device).
    fn no_click(px: f64) -> Error {
        Error::usage(format!("a wheel movement of {px} px sends no wheel click: on X11 the wheel moves in clicks (strobe sends one per 100 px); move it at least 50 px"))
    }

    fn timeout(opts: &Value) -> Duration {
        opts.get("timeout").and_then(Value::as_u64).map(Duration::from_millis).unwrap_or(Duration::from_secs(30))
    }

    /// Whether a value read back is the one written: the same text (trimmed), or the same number, also
    /// when the app shows it with a unit (`0.5` and `"0.50 ×"`).
    /// Single precision is close enough: a toolkit that keeps a slider's value in an f32 reads 0.2 back
    /// as 0.200000003.
    fn same_value(want: &str, got: &str) -> bool {
        want.trim() == got.trim() || matches!((want.trim().parse::<f64>().ok(), leading_number(got)), (Some(a), Some(b)) if (a - b).abs() <= 1e-5 * a.abs().max(b.abs()).max(1.0))
    }

    /// The number a text starts with (`"0.50 ×"`, `"10em"`, `"-3e2 px"`): a sign, digits, a point and
    /// an exponent only when digits follow it.
    fn leading_number(s: &str) -> Option<f64> {
        let b = s.trim().as_bytes();
        let digits = |mut i: usize| {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            i
        };
        let mut end = digits(usize::from(matches!(b.first(), Some(b'-' | b'+'))));
        if b.get(end) == Some(&b'.') {
            end = digits(end + 1);
        }
        if matches!(b.get(end), Some(b'e' | b'E')) {
            let at = end + 1 + usize::from(matches!(b.get(end + 1), Some(b'-' | b'+')));
            if b.get(at).is_some_and(u8::is_ascii_digit) {
                end = digits(at);
            }
        }
        std::str::from_utf8(&b[..end]).ok()?.parse().ok()
    }

    fn parse_sel(v: &Value) -> Result<Selector> {
        Selector::parse(v, a11y::Dialect::Native)
    }

    /// No selector given: `""`, or none at all.
    fn no_selector(sel: &Value) -> bool {
        sel.is_null() || sel.as_str() == Some("")
    }

    impl Session {
        /// One page call from the runner. `pixels`: a pixels block's folder for `snapshot`.
        pub fn dispatch(&self, call: &str, args: &[Value], presents: &RefCell<Option<PresentCapture>>, pixels: Option<&std::path::Path>) -> Result<Value> {
            let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("").to_string();
            let opts = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
            // A selector as the bench gave it: a string, or `{ selector, text, exact, index }`.
            let sel = |i: usize| args.get(i).cloned().unwrap_or(Value::String(String::new()));
            let main = self.main.get();
            match call {
                "click" => self.click(&sel(0), &opts(1), presents, "discrete")?,
                "hover" => {
                    let o = opts(1);
                    let (tree, i) = self.find(&parse_sel(&sel(0))?, o["window"].as_str(), timeout(&o))?;
                    let (x, y, t) = self.aim(&tree, i)?;
                    let at = self.pointer_to(x, y, t)?;
                    self.until(&o, at, presents)?;
                }
                "move" => {
                    let at = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0);
                    let o = opts(2);
                    let (x, y) = self.poster.client_point(main, at(0), at(1))?;
                    // While a button is held the move goes to the window it pressed in, as capture does.
                    let t = match self.pointer.get().filter(|_| self.pressed.get().is_some()) {
                        Some((_, _, t)) => t.moved(x, y),
                        None => self.poster.target(&self.pids.borrow(), x, y)?,
                    };
                    let at = self.pointer_to(x, y, t)?;
                    self.until(&o, at, presents)?;
                }
                "drag" => {
                    let o = opts(2);
                    let at = self.drag(&sel(0), &opts(1), &o)?;
                    self.until(&o, at, presents)?;
                }
                "type" => {
                    let o = opts(2);
                    let delay = Duration::from_millis(o["delay"].as_f64().unwrap_or(0.0).max(0.0) as u64);
                    let (top, to) = self.key_target(&sel(0), &o, presents)?;
                    self.rest(Duration::ZERO, Duration::from_millis(self.frame_ms(0.0, 0.0)), presents)?;
                    let mut last = 0;
                    for (k, c) in s(1).chars().enumerate() {
                        if k > 0 && !delay.is_zero() {
                            std::thread::sleep(delay);
                        }
                        let key = if c == '\n' { Key::Named(Named::Enter) } else { Key::Char(c) };
                        last = self.poster.press(top, to, Mods::default(), key)?;
                        self.input(last, "discrete", false);
                    }
                    self.until(&o, last, presents)?;
                }
                "press" => {
                    let (mods, key) = input::parse_chord(&s(0))?;
                    let o = opts(1);
                    let (top, to) = self.key_target(&o["selector"], &o, presents)?;
                    let at = self.poster.press(top, to, mods, key)?;
                    self.input(at, "discrete", false);
                    self.until(&opts(1), at, presents)?;
                }
                "down" | "up" if a11y::keys::mouse_button(&s(0)).is_some() => {
                    let o = opts(1);
                    let b = a11y::keys::mouse_button(&s(0)).unwrap_or(Button::Left);
                    // `at`: an element to move onto first, or a point in the main window's client area.
                    match o.get("at") {
                        Some(at @ (Value::String(_) | Value::Object(_))) => {
                            let (tree, i) = self.find(&parse_sel(at)?, o["window"].as_str(), timeout(&o))?;
                            let (x, y, t) = self.aim(&tree, i)?;
                            self.pointer_to(x, y, t)?;
                        }
                        Some(Value::Array(p)) => {
                            let (x, y) = self.poster.client_point(main, p.first().and_then(Value::as_f64).unwrap_or(0.0), p.get(1).and_then(Value::as_f64).unwrap_or(0.0))?;
                            let t = self.poster.target(&self.pids.borrow(), x, y)?;
                            self.pointer_to(x, y, t)?;
                        }
                        _ => {}
                    }
                    let at = self.pointer_button(b, call == "down")?;
                    self.until(&o, at, presents)?;
                }
                "down" | "up" => {
                    let (mods, key) = input::parse_chord(&s(0))?;
                    if mods.any() {
                        return Err(Error::usage(format!("page.{call}({:?}): one key, not a chord (hold each with its own down)", s(0))));
                    }
                    let at = if call == "down" { self.poster.hold(main, key)? } else { self.poster.release(main, key)? };
                    self.input(at, if call == "down" { "discrete" } else { "pointer" }, false);
                    self.until(&opts(1), at, presents)?;
                }
                "scroll" => {
                    let d = opts(1);
                    let o = opts(2);
                    let (tree, i) = self.find(&parse_sel(&sel(0))?, o["window"].as_str(), timeout(&o))?;
                    let (x, y, t) = self.aim(&tree, i)?;
                    self.park(x, y)?;
                    self.input(self.poster.move_to(t, 0)?, "pointer", false);
                    self.rest(Duration::ZERO, Duration::from_millis(self.frame_ms(x, y)), presents)?;
                    let (by_y, by_x) = (d["by"].as_f64().unwrap_or(0.0), d["byX"].as_f64().unwrap_or(0.0));
                    let speed = d["speed"].as_f64().unwrap_or(3000.0).max(1.0);
                    // One wheel step per refresh at that speed, as a trackpad sends them.
                    let frame = Duration::from_millis(self.frame_ms(x, y));
                    let steps = ((by_y.abs().max(by_x.abs()) / speed) / frame.as_secs_f64()).ceil().max(1.0) as usize;
                    let start = Instant::now();
                    let mut last = 0;
                    for k in 0..steps {
                        let due = start + frame * k as u32;
                        if let Some(w) = due.checked_duration_since(Instant::now()) {
                            std::thread::sleep(w);
                        }
                        // A wheel that moves in clicks (X11) sends a step only once the steps add up to one.
                        let at = self.poster.wheel(t, by_x / steps as f64, by_y / steps as f64)?;
                        if at != 0 {
                            self.input(at, "scroll", last == 0);
                            last = at;
                        }
                    }
                    if last == 0 {
                        return Err(no_click(by_x.abs().max(by_y.abs())));
                    }
                    self.until(&o, last, presents)?;
                }
                "wheel" => {
                    let d = opts(1);
                    let o = opts(2);
                    let (x, y, t) = if s(0).is_empty() {
                        let w = host::window_state(main).ok_or_else(|| Error::msg("the app's window closed"))?;
                        let (x, y) = self.poster.client_point(main, w.client.0 as f64 / w.scale() / 2.0, w.client.1 as f64 / w.scale() / 2.0)?;
                        (x, y, self.poster.target(&self.pids.borrow(), x, y)?)
                    } else {
                        let (tree, i) = self.find(&parse_sel(&sel(0))?, o["window"].as_str(), timeout(&o))?;
                        self.aim(&tree, i)?
                    };
                    self.park(x, y)?;
                    self.input(self.poster.move_to(t, 0)?, "pointer", false);
                    self.rest(Duration::ZERO, Duration::from_millis(self.frame_ms(x, y)), presents)?;
                    let (dx, dy) = (d["dx"].as_f64().unwrap_or(0.0), d["dy"].as_f64().unwrap_or(0.0));
                    let at = self.poster.wheel(t, dx, dy)?;
                    if at == 0 {
                        return Err(no_click(dx.abs().max(dy.abs())));
                    }
                    self.input(at, "scroll", true);
                    self.until(&o, at, presents)?;
                }
                "act" => {
                    let o = opts(2);
                    let (_, i) = self.find(&parse_sel(&sel(0))?, o["window"].as_str(), timeout(&o))?;
                    // `{ value: 0.2 }` is as good as `{ value: "0.2" }` (a number was dropped as "").
                    let value = match &o["value"] {
                        Value::Number(n) => Some(n.to_string()),
                        v => v.as_str().map(str::to_string),
                    };
                    let action = Action::parse(&s(1), value.as_deref())?;
                    let at = self.a11y.act(i, &action)?;
                    self.input(at, "pattern", false);
                    if let Action::SetValue(want) = &action {
                        self.took(i, want, &sel(0))?;
                    }
                    self.until(&o, at, presents)?;
                }
                "query" => {
                    let o = opts(1);
                    let tree = self.snapshot(o["window"].as_str())?;
                    let sel = parse_sel(&sel(0))?;
                    let found: Vec<Value> = tree.find(&sel).into_iter().map(|i| tree.describe(i)).collect();
                    return Ok(Value::Array(found));
                }
                "waitFor" => {
                    let o = opts(1);
                    self.find(&parse_sel(&crate::driver::with_text(&sel(0), &o))?, o["window"].as_str(), timeout(&o))?;
                }
                "idle" => {
                    self.idle(host::now_ns(), timeout(&opts(0)), presents)?;
                }
                "frame" => {
                    self.frame(host::now_ns(), timeout(&opts(0)), presents)?;
                }
                "wait" => {
                    let ms = args.first().and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
                    std::thread::sleep(Duration::from_millis(ms as u64));
                }
                "window" => {
                    let o = opts(1);
                    let w = self.windows(o["window"].as_str())?[0];
                    self.poster.window(w, &s(0))?;
                }
                "snapshot" => {
                    let Some(dir) = pixels else { return Ok(Value::Bool(false)) };
                    let name: String = s(0).chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
                    let w = self.windows(opts(1)["window"].as_str())?[0];
                    let img = host::window_image(w).ok_or_else(|| Error::msg(format!("snapshot {name}: the window did not draw itself (PrintWindow)")))?;
                    std::fs::create_dir_all(dir)?;
                    crate::store::write_atomic(&dir.join(format!("{name}.png")), &crate::png::write(&img)?)?;
                }
                "goto" | "eval" => {
                    return Err(Error::usage(format!(
                        "page.{call}() is a page's: a native app is driven through its accessibility tree and its message loop (click, hover, move, drag, type, press, down, up, scroll, wheel, act, query, waitFor, idle, frame, wait, window, snapshot)"
                    )));
                }
                other => return Err(Error::msg(format!("unknown page call {other}"))),
            }
            Ok(Value::Bool(true))
        }
    }
}
