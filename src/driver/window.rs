//! `page.window` sizing (SPEC §9.6): maximize, restore and resize a target's window through the OS, on
//! native apps and headed pages alike. A size is the content's (a native window's client area, a page's
//! viewport), in device-independent pixels: strobe adds the window's own part, measured as it goes (the
//! frame less the content), so a browser's toolbar or a title bar is no part of what a bench asks for.

use crate::host::{self, Placement};
use crate::model::{Error, Refusal, Result, ValidatorId};
use serde_json::Value;
use std::cell::Cell;
use std::time::{Duration, Instant};

/// A size within this many DIP of the one asked for is that size (a toolkit rounds to its own pixels).
const NEAR_DIP: f64 = 2.0;

/// How far a frame may reach past the work area (DIP): Windows' frames carry invisible resize borders
/// that lie outside it, even maximized.
const SLACK_DIP: f64 = if cfg!(windows) { 8.0 } else { 1.0 };

/// The longest strobe waits for the window to take a size and its content to hold still.
const SETTLE: Duration = Duration::from_secs(1);

/// What `page.window(action)` asks of a window's size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sizing {
    Maximize,
    /// Back to the size and state run 1 started from (before run 1: before the block's first sizing).
    Restore,
    /// The content to this size (DIP).
    Resize(f64, f64),
}

impl Sizing {
    /// The sizing `action` names; None for the other window actions (`show`, `hide`, …, native only).
    pub fn parse(action: &Value) -> Result<Option<Sizing>> {
        let bad = || Error::usage(format!("page.window({action}): \"maximize\", \"restore\" or {{ resize: {{ width, height }} }} (device-independent pixels of the content: the viewport on a page, the client area on a native app); native apps also take \"show\", \"hide\", \"minimize\", \"focus\" and \"close\""));
        match action {
            Value::String(s) if s == "maximize" => Ok(Some(Sizing::Maximize)),
            Value::String(s) if s == "restore" => Ok(Some(Sizing::Restore)),
            Value::String(_) => Ok(None),
            Value::Object(o) => {
                let r = o.get("resize").filter(|_| o.len() == 1).ok_or_else(bad)?;
                let side = |k: &str| r.get(k).and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 1.0 && *v <= 16384.0);
                match (side("width"), side("height")) {
                    (Some(w), Some(h)) => Ok(Some(Sizing::Resize(w.round(), h.round()))),
                    _ => Err(bad()),
                }
            }
            _ => Err(bad()),
        }
    }

    fn name(&self) -> String {
        match self {
            Sizing::Maximize => "\"maximize\"".into(),
            Sizing::Restore => "\"restore\"".into(),
            Sizing::Resize(w, h) => format!("{{ resize: {w}×{h} }}"),
        }
    }
}

/// What a run starts from in its window's size: the frame's size (DIP) and whether it is maximized.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Start {
    size: (f64, f64),
    maximized: bool,
}

impl Start {
    pub fn of(hwnd: isize) -> Option<Start> {
        host::window_placement(hwnd).map(|p| Start { size: (p.frame[2] / p.per_dip, p.frame[3] / p.per_dip), maximized: p.maximized })
    }

    fn near(&self, o: &Start) -> bool {
        self.maximized == o.maximized && (self.size.0 - o.size.0).abs() <= NEAR_DIP && (self.size.1 - o.size.1).abs() <= NEAR_DIP
    }

    fn describe(&self) -> String {
        format!("{}{:.0}×{:.0}", if self.maximized { "maximized, " } else { "" }, self.size.0, self.size.1)
    }
}

/// A window as `restore` puts it back: its place and its content (DIP).
#[derive(Clone, Copy, Debug)]
struct Saved {
    place: Placement,
    content: (f64, f64),
}

/// One block's sizing of one window: where `restore` goes back to, and what every run starts from.
#[derive(Default)]
pub struct Sizer {
    /// The window the block sized (one per block), once it did.
    window: Cell<Option<isize>>,
    /// Before the first sizing since run 1 started (before it, the block's first sizing): what
    /// `restore` goes back to, so a size setup chose is the one runs restore.
    saved: Cell<Option<Saved>>,
    /// The window as the first run started (`check`), or as the driver first saw it before then.
    first: Cell<Option<Start>>,
}

impl Sizer {
    /// Whether the block sized the window: only then is each run's start held to the first's.
    pub fn commanded(&self) -> bool {
        self.window.get().is_some()
    }

    /// The window the block sized, once it did.
    pub fn sized(&self) -> Option<isize> {
        self.window.get()
    }

    /// What runs start from until run 1 says (a driver opened during run 1 sees run 1's start).
    pub fn reference(&self, now: Option<Start>) {
        if self.first.get().is_none() {
            self.first.set(now);
        }
    }

    /// Before run `run` (1-based; `after`: where a run after the last would start): the first records
    /// the window's size, every later one, once the block sized the window, must match it (within 2
    /// DIP, and maximized as it was), else `ResetIncomplete` (only when `on`: the block checks what runs
    /// start from). Run 1's start is also where `restore` goes back to from then on. `now`: the window
    /// as it is.
    pub fn check(&self, run: usize, after: bool, on: bool, now: Option<Start>) -> Result<()> {
        if run == 1 && !after {
            self.saved.set(None);
        }
        let Some(now) = now else { return Ok(()) };
        let first = match self.first.get() {
            Some(f) if run != 1 || after => f,
            _ => {
                self.first.set(Some(now));
                return Ok(());
            }
        };
        if !on || !self.commanded() || now.near(&first) {
            return Ok(());
        }
        let when = if after { format!("a run after the last (run {}) would start", run.saturating_sub(1)) } else { format!("run {run} started") };
        Err(Refusal::new(ValidatorId::ResetIncomplete, format!("{when} with the window {} where the first run started with it {}: the case sized it (page.window) and did not size it back", now.describe(), first.describe()))
            .fix("size the window back in the case's reset (page.window(\"restore\") puts it as it was when run 1 started), or set resetCheck: false for runs that build on each other on purpose")
            .into())
    }

    /// Does `s` to window `hwnd`, whose content (DIP) `content` reads, and waits up to a second for the
    /// window to take it and its content to hold still. A size that does not fit the display's work
    /// area with the window's own part refuses `DisplayTooSmall` before anything moves, as does one the
    /// OS clamped to the work area; a window that fits and still stays another size (the app's own
    /// least or most size) refuses `VariantMismatch`. Returns when strobe asked (host clock).
    pub fn apply(&self, hwnd: isize, s: Sizing, content: &dyn Fn() -> Result<(f64, f64)>) -> Result<u64> {
        let gone = || Error::msg("page.window(): the window closed");
        if let Some(other) = self.sized().filter(|w| *w != hwnd) {
            return Err(Error::usage(format!("page.window({}): a block sizes one window, and it sized another ({other}) first", s.name())));
        }
        let p0 = host::window_placement(hwnd).ok_or_else(gone)?;
        let c0 = content()?;
        self.window.set(Some(hwnd));
        if self.saved.get().is_none() {
            self.saved.set(Some(Saved { place: p0, content: c0 }));
        }
        let Saved { place: saved, content: saved_c } = self.saved.get().unwrap_or(Saved { place: p0, content: c0 });
        let k = p0.per_dip;
        let slack = SLACK_DIP * k;
        let asked = crate::host::now_ns();
        // The content a run should see once the window took it; maximized: the window's own state.
        let want = match s {
            Sizing::Maximize => {
                if !p0.maximized {
                    host::maximize_window(hwnd, p0.work)?;
                }
                None
            }
            Sizing::Restore if saved.maximized => {
                if !p0.maximized {
                    host::maximize_window(hwnd, p0.work)?;
                }
                None
            }
            Sizing::Restore => {
                host::restore_window(hwnd, saved.frame)?;
                Some(saved_c)
            }
            Sizing::Resize(w, h) => {
                let size = (p0.frame[2] + (w - c0.0) * k, p0.frame[3] + (h - c0.1) * k);
                let work = p0.work;
                if size.0 > work[2] + 2.0 * slack || size.1 > work[3] + slack {
                    return Err(too_small(s, size, (p0.frame[2] - c0.0 * k, p0.frame[3] - c0.1 * k), work, k));
                }
                host::set_window_frame(hwnd, inside(&p0, size))?;
                Some((w, h))
            }
        };
        let near = |c: (f64, f64), w: (f64, f64)| (c.0 - w.0).abs() <= NEAR_DIP && (c.1 - w.1).abs() <= NEAR_DIP;
        // Until the window took it and its content held still for 100 ms, at most `SETTLE`: the content
        // and the window's place then.
        let settle = || -> Result<((f64, f64), Placement)> {
            let deadline = Instant::now() + SETTLE;
            let (mut last, mut still) = (content()?, Instant::now());
            loop {
                std::thread::sleep(Duration::from_millis(20));
                let now = content()?;
                let placed = host::window_placement(hwnd).ok_or_else(gone)?;
                if now != last {
                    (last, still) = (now, Instant::now());
                }
                let took = match want {
                    Some(w) => near(now, w),
                    None => placed.maximized,
                };
                if (took && still.elapsed() >= Duration::from_millis(100)) || Instant::now() >= deadline {
                    return Ok((last, placed));
                }
            }
        };
        let clamped = |p: &Placement| p.frame[2] >= p.work[2] - SLACK_DIP * p.per_dip || p.frame[3] >= p.work[3] - SLACK_DIP * p.per_dip;
        let (mut last, mut placed) = settle()?;
        // A window's own part may change as it takes a size (a browser lays out its toolbars again):
        // measured once more and asked again, once, at the scale and in the work area it has now.
        if let Some(w) = want.filter(|w| !near(last, *w) && !clamped(&placed)) {
            let (f, k) = (placed.frame, placed.per_dip);
            host::set_window_frame(hwnd, inside(&placed, (f[2] + (w.0 - last.0) * k, f[3] + (w.1 - last.1) * k)))?;
            (last, placed) = settle()?;
        }
        match want {
            Some(w) if !near(last, w) => {
                let (size, k) = ((placed.frame[2], placed.frame[3]), placed.per_dip);
                if clamped(&placed) {
                    return Err(too_small(s, (size.0 + (w.0 - last.0) * k, size.1 + (w.1 - last.1) * k), (size.0 - last.0 * k, size.1 - last.1 * k), placed.work, k));
                }
                Err(Refusal::new(
                    ValidatorId::VariantMismatch,
                    format!("page.window({}): the content stayed {:.0}×{:.0}, not {:.0}×{:.0}, in a window that fits the display: the app keeps its own least or most size", s.name(), last.0, last.1, w.0, w.1),
                )
                .fix("ask for a size the app takes (its window's least and most sizes are its own), or measure at the size it keeps")
                .into())
            }
            None if !placed.maximized => Err(Refusal::new(ValidatorId::VariantMismatch, format!("page.window({}): the window did not maximize within a second: the app keeps its own size", s.name()))
                .fix("measure at the size the app keeps, or resize it ({ resize: { width, height } }) to a size it takes")
                .into()),
            _ => Ok(asked),
        }
    }
}

/// A frame of `size` (screen units) where `p`'s is, moved no more than it must to lie in its work area.
fn inside(p: &Placement, size: (f64, f64)) -> [f64; 4] {
    let (work, slack) = (p.work, SLACK_DIP * p.per_dip);
    let x = p.frame[0].min(work[0] + work[2] + slack - size.0).max(work[0] - slack);
    let y = p.frame[1].min(work[1] + work[3] + slack - size.1).max(work[1]);
    [x, y, size.0, size.1]
}

/// `DisplayTooSmall` for a window of `size` (screen units) whose own part is `chrome`.
fn too_small(s: Sizing, size: (f64, f64), chrome: (f64, f64), work: [f64; 4], k: f64) -> Error {
    let most = ((work[2] - chrome.0) / k + 2.0 * SLACK_DIP, (work[3] - chrome.1) / k + SLACK_DIP);
    Refusal::new(
        ValidatorId::DisplayTooSmall,
        format!(
            "page.window({}): the window would be {:.0}×{:.0} with its own part ({:.0}×{:.0}), larger than this display's work area {:.0}×{:.0}",
            s.name(),
            size.0 / k,
            size.1 / k,
            chrome.0 / k,
            chrome.1 / k,
            work[2] / k,
            work[3] / k
        ),
    )
    .fix(format!("ask for content no larger than {:.0}×{:.0} here, or run on a larger display", most.0.floor(), most.1.floor()))
    .into()
}
