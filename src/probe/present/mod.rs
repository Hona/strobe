//! What reached the screen and when (SPEC §7.2), from outside the app, on every OS:
//!
//! - `windows`: an ETW real-time session (`probe::etw`) over the providers the display path logs
//!   (DXGI, DxgKrnl, Win32k, Dwm-Core), tracked the way PresentMon does into one record per present:
//!   displayed (with its screen time) or discarded. Needs admin or Performance Log Users; otherwise
//!   the deep-mode helper (`probe::helper`) runs it.
//! - `sck` (macOS) and `xpresent` (Linux X11): the same `Present` records from what those OSes show
//!   another app of a window's frames: ScreenCaptureKit's frame times, X Present's completion events.
//! - `frames`: presents into frame metrics; no OS API.
//!
//! Every timestamp is the host clock (`host::now_ns`: QPC on Windows, `WNODE_HEADER.ClientContext =
//! 1`; Mach absolute time on macOS; CLOCK_MONOTONIC on Linux), so presents line up with marks, native
//! stacks and OS samples without mapping.

pub mod frames;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod xpresent;

#[cfg(windows)]
use super::helper;
use serde::{Deserialize, Serialize};

/// One present, after the OS decided its fate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Present {
    pub pid: u32,
    /// The thread that called present (or the first one the OS saw working on it).
    pub tid: u32,
    /// The present call started (QPC ns).
    pub start_ns: u64,
    /// The present call returned; 0 when the runtime logs no stop (Vulkan without DXGI).
    pub stop_ns: u64,
    /// The GPU finished the frame's work, when known.
    pub ready_ns: u64,
    /// Scanout of the frame began: 0 when it never reached the screen.
    pub screen_ns: u64,
    pub swapchain: u64,
    pub sync_interval: i32,
    /// `Hardware_Legacy_Flip`, `Composed_Flip`, `Hardware_Independent_Flip`, `Hardware_Composed_Independent_Flip`, …
    pub mode: String,
    /// `displayed`, `discarded` (replaced or dropped before scanout), `lost` (tracking lost) or `failed`.
    pub state: String,
}

/// What a present session saw.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Presents {
    pub presents: Vec<Present>,
    /// Vertical blanks per display path: (QPC ns, VidPnTargetId).
    pub vsyncs: Vec<(u64, u32)>,
    pub events: u64,
    pub events_lost: u64,
    /// The consumer thread's own CPU time (cycle-exact), for the overhead budget.
    pub consumer_cpu_ms: Option<f64>,
    pub t0_ns: u64,
    pub t1_ns: u64,
    /// The source sees when a frame reached the screen, not when the app handed it over (macOS
    /// ScreenCaptureKit, X11 Present): each present's start is its screen time, so a gap between
    /// frames cannot be told from rest and intervals and drops are refused.
    #[serde(default)]
    pub no_begin: bool,
    /// The source sees frames that take one path only (X11: the Present extension): an app with
    /// none of them may still draw another way, so no frames is not zero frames.
    #[serde(default)]
    pub only: Option<String>,
    /// Windows the app excludes from screen capture (macOS `NSWindowSharingNone`), seen on screen
    /// and not streamed: what they draw is unseen. Each with when strobe first and last saw it there
    /// (host clock; the last is the first look that found it gone, so the span may be longer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unshared: Vec<Unshared>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Unshared {
    pub pid: u32,
    pub window: u64,
    pub first_ns: u64,
    pub last_ns: u64,
}

/// Whether this process may run the real-time present session itself, else why not.
#[derive(Clone, Debug, Serialize)]
pub struct Access {
    /// `admin` (elevated), `group` (Performance Log Users in this token), or `none`.
    pub token: &'static str,
    /// The user is in Performance Log Users but this logon predates it: sign out and in once.
    pub pending_sign_out: bool,
    /// The deep-mode helper is installed for this user.
    pub helper: bool,
}

impl Access {
    /// Present events in this process, or through the helper (Windows: Linux and macOS give none).
    pub fn presents(&self) -> bool {
        cfg!(windows) && (self.token != "none" || self.helper)
    }

    /// Why present metrics are refused, when they are.
    pub fn refusal(&self) -> Option<String> {
        if self.presents() {
            return None;
        }
        Some(if self.pending_sign_out {
            "present events need a new logon: sign out of Windows and in once (strobe up added you to Performance Log Users)".into()
        } else {
            "present events need Performance Log Users or the deep-mode helper: run strobe up and approve its one UAC prompt".into()
        })
    }
}

pub fn access() -> Access {
    #[cfg(windows)]
    {
        helper::access()
    }
    // Off Windows there is no helper: `helper` says whether strobe's own native sampler may run.
    #[cfg(not(windows))]
    {
        Access { token: "none", pending_sign_out: false, helper: super::stacks::sampler_allowed() }
    }
}

/// Presents as a running session completes them, for waits while it runs (`until: "frame"`, idle):
/// (pid, start, screen time or 0 when it never reached the screen), the newest 65,536.
#[derive(Default)]
pub struct Live {
    presents: std::sync::Mutex<LiveQueue>,
    cv: std::sync::Condvar,
}

/// The presents, and each process's latest present (its start, or its screen time when later): a
/// wait asks for that between every sleep, and
/// finding it in the queue (65,536 presents after a few minutes of frames) cost strobe's main thread
/// most of its CPU inside a native block's runs on air.
#[derive(Default)]
struct LiveQueue {
    all: std::collections::VecDeque<(u32, u64, u64)>,
    last: std::collections::HashMap<u32, u64>,
}

/// What `Live` says about some processes: the first present that reached the screen after a time
/// (0 when none yet), and the latest one of theirs began or reached the screen, of any fate (0 when
/// none; the field keeps its name for the deep-mode helper's answers).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct LiveView {
    pub next_start_ns: u64,
    pub next_screen_ns: u64,
    pub last_start_ns: u64,
}

impl Live {
    pub fn push(&self, pid: u32, start: u64, screen: u64) {
        let mut q = self.presents.lock().unwrap_or_else(|e| e.into_inner());
        q.all.push_back((pid, start, screen));
        if q.all.len() > 65_536 {
            q.all.pop_front();
        }
        let last = q.last.entry(pid).or_default();
        *last = (*last).max(start.max(screen));
        self.cv.notify_all();
    }

    /// Waits up to `wait` for a present of `pids` that started after `after` and reached the screen
    /// (`after` = `u64::MAX`: only the latest start, without reading the queue).
    pub fn view(&self, pids: &[u32], after: u64, wait: std::time::Duration) -> LiveView {
        let deadline = std::time::Instant::now() + wait;
        let mut q = self.presents.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            // From the newest back to presents that started over a second before `after`: they come
            // in as they complete, so one that started after `after` is behind those only if it
            // completed before a present that had started a second earlier.
            let recent = q.all.iter().rev().take_while(|p| p.1.saturating_add(1_000_000_000) > after);
            let next = if after == u64::MAX { None } else { recent.filter(|p| pids.contains(&p.0) && p.1 > after && p.2 > 0).min_by_key(|p| p.1).copied() };
            let last = pids.iter().filter_map(|p| q.last.get(p)).copied().max().unwrap_or(0);
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if next.is_some() || left.is_zero() {
                return LiveView { next_start_ns: next.map_or(0, |p| p.1), next_screen_ns: next.map_or(0, |p| p.2), last_start_ns: last };
            }
            q = self.cv.wait_timeout(q, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// A present session: in this process when its token may, else in the helper (Windows); a window's
/// frames through ScreenCaptureKit (macOS) or X Present (Linux X11).
pub enum PresentCapture {
    #[cfg(windows)]
    Local(windows::Capture),
    #[cfg(windows)]
    Helper(helper::Client),
    #[cfg(target_os = "macos")]
    Screen(sck::Capture),
    #[cfg(target_os = "linux")]
    X11(xpresent::Capture),
}

impl PresentCapture {
    pub fn start() -> crate::model::Result<PresentCapture> {
        #[cfg(windows)]
        {
            let a = access();
            let mut local_err = None;
            if a.token != "none" {
                match windows::Capture::start() {
                    Ok(c) => return Ok(PresentCapture::Local(c)),
                    Err(e) => local_err = Some(e),
                }
            }
            if a.helper {
                let mut c = helper::Client::connect()?;
                c.call(serde_json::json!({ "op": "presents.start" }))?;
                return Ok(PresentCapture::Helper(c));
            }
            Err(local_err.unwrap_or_else(|| crate::model::Error::msg(a.refusal().unwrap_or_default())))
        }
        #[cfg(target_os = "linux")]
        return Ok(PresentCapture::X11(xpresent::Capture::start()?));
        #[cfg(target_os = "macos")]
        return Ok(PresentCapture::Screen(sck::Capture::start()?));
    }

    /// `in-process`, `helper`, `screencapturekit` or `x11-present`.
    pub fn via(&self) -> &'static str {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(_) => "in-process",
            #[cfg(windows)]
            PresentCapture::Helper(_) => "helper",
            #[cfg(target_os = "macos")]
            PresentCapture::Screen(_) => "screencapturekit",
            #[cfg(target_os = "linux")]
            PresentCapture::X11(_) => "x11-present",
        }
    }

    /// Follows the windows of `pids`: the OS session (Windows) sees every process already; a
    /// window source (macOS, Linux) watches their windows from now on. Returns when it does.
    pub fn track(&mut self, pids: &[u32]) {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(_) | PresentCapture::Helper(_) => {
                let _ = pids;
            }
            #[cfg(target_os = "macos")]
            PresentCapture::Screen(c) => c.track(pids),
            #[cfg(target_os = "linux")]
            PresentCapture::X11(c) => c.track(pids),
        }
    }

    /// While the session runs: `Live::view` of `pids` after `after`, waiting up to `wait`.
    pub fn live(&mut self, pids: &[u32], after: u64, wait: std::time::Duration) -> crate::model::Result<LiveView> {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(c) => Ok(c.live.view(pids, after, wait)),
            #[cfg(windows)]
            PresentCapture::Helper(c) => Ok(serde_json::from_value(c.call(serde_json::json!({ "op": "presents.live", "pids": pids, "after_ns": after, "wait_ms": wait.as_millis() as u64 }))?)?),
            #[cfg(target_os = "macos")]
            PresentCapture::Screen(c) => {
                c.track(pids);
                Ok(c.live.view(pids, after, wait))
            }
            #[cfg(target_os = "linux")]
            PresentCapture::X11(c) => {
                c.track(pids);
                Ok(c.live.view(pids, after, wait))
            }
        }
    }

    /// The latest a present of `pids` began or reached the screen (0 when none), without looking for
    /// new windows (`track` reads the window list): what a settle polls every few milliseconds.
    pub fn last_ns(&mut self, pids: &[u32]) -> crate::model::Result<u64> {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(_) | PresentCapture::Helper(_) => Ok(self.live(pids, u64::MAX, std::time::Duration::ZERO)?.last_start_ns),
            #[cfg(target_os = "macos")]
            PresentCapture::Screen(c) => Ok(c.live.view(pids, u64::MAX, std::time::Duration::ZERO).last_start_ns),
            #[cfg(target_os = "linux")]
            PresentCapture::X11(c) => Ok(c.live.view(pids, u64::MAX, std::time::Duration::ZERO).last_start_ns),
        }
    }

    pub fn stop(self) -> crate::model::Result<Presents> {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(c) => c.stop(),
            #[cfg(windows)]
            PresentCapture::Helper(mut c) => Ok(serde_json::from_value(c.call(serde_json::json!({ "op": "presents.stop" }))?)?),
            #[cfg(target_os = "macos")]
            PresentCapture::Screen(c) => c.stop(),
            #[cfg(target_os = "linux")]
            PresentCapture::X11(c) => c.stop(),
        }
    }
}
