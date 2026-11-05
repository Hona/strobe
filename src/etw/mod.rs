//! Event Tracing for Windows (SPEC §7.2, §13.4): what reached the screen and when, from the OS's own
//! present events, and native CPU stacks from the kernel's sampled profile.
//!
//! - `present`: real-time session over the providers the display path logs (DXGI, DxgKrnl, Win32k,
//!   Dwm-Core), tracked the way PresentMon does into one record per present: displayed (with its
//!   screen time) or discarded. Needs admin or Performance Log Users; otherwise the helper.
//! - `stacks`: the kernel's sampled profile with stack walks and image loads, for attribution
//!   blocks only. Needs SeSystemProfilePrivilege (administrators), so it runs in the helper.
//! - `helper`: the elevated helper (§13.4): a scheduled task the user's strobe starts on demand,
//!   reachable only over a pipe whose ACL names that user, doing a fixed set of operations.
//! - `frames`: presents into frame metrics; no OS API.
//! - `sck` (macOS) and `xpresent` (Linux X11): the same `Present` records from what those OSes show
//!   another app of a window's frames: ScreenCaptureKit's frame times, X Present's completion events.
//!
//! Every timestamp is the host clock (`host::now_ns`: QPC on Windows, `WNODE_HEADER.ClientContext =
//! 1`; Mach absolute time on macOS; CLOCK_MONOTONIC on Linux), so presents and stacks line up with
//! marks and OS samples without mapping.

pub mod frames;
#[cfg(windows)]
pub mod helper;
#[cfg(windows)]
pub mod present;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod stacks;
#[cfg(target_os = "linux")]
pub mod xpresent;

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
    // Off Windows `helper` means native stacks are available: perf events for this user (Linux),
    // `/usr/bin/sample` (macOS).
    #[cfg(target_os = "linux")]
    {
        Access { token: "none", pending_sign_out: false, helper: crate::probe::perf::access().0 }
    }
    #[cfg(target_os = "macos")]
    {
        Access { token: "none", pending_sign_out: false, helper: std::path::Path::new("/usr/bin/sample").is_file() }
    }
}

/// Presents as a running session completes them, for waits while it runs (`until: "frame"`, idle):
/// (pid, start, screen time or 0 when it never reached the screen), the newest 65,536.
#[derive(Default)]
pub struct Live {
    presents: std::sync::Mutex<std::collections::VecDeque<(u32, u64, u64)>>,
    cv: std::sync::Condvar,
}

/// What `Live` says about some processes: the first present that reached the screen after a time
/// (0 when none yet), and the start of their latest present of any fate (0 when none).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct LiveView {
    pub next_start_ns: u64,
    pub next_screen_ns: u64,
    pub last_start_ns: u64,
}

impl Live {
    pub fn push(&self, pid: u32, start: u64, screen: u64) {
        let mut q = self.presents.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back((pid, start, screen));
        if q.len() > 65_536 {
            q.pop_front();
        }
        self.cv.notify_all();
    }

    /// Waits up to `wait` for a present of `pids` that started after `after` and reached the screen.
    pub fn view(&self, pids: &[u32], after: u64, wait: std::time::Duration) -> LiveView {
        let deadline = std::time::Instant::now() + wait;
        let mut q = self.presents.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let mine = || q.iter().filter(|p| pids.contains(&p.0));
            let next = mine().filter(|p| p.1 > after && p.2 > 0).min_by_key(|p| p.1);
            let last = mine().map(|p| p.1).max().unwrap_or(0);
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
    Local(present::Capture),
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
                match present::Capture::start() {
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

/// Whether native stacks can be sampled here: the helper (Windows), or without it strobe's own
/// suspend-and-unwind sampler (Windows x64); perf events (Linux); `sample` (macOS).
pub fn stacks_available() -> bool {
    access().helper || cfg!(all(windows, target_arch = "x86_64"))
}

/// Why a recording has no native stacks.
pub fn no_stacks() -> &'static str {
    if cfg!(windows) {
        "native stacks need the deep helper (strobe up, SPEC §13.4), or an x64 build of strobe to sample them itself"
    } else if cfg!(target_os = "macos") {
        "native stacks need /usr/bin/sample"
    } else {
        "native stacks need perf_event_paranoid at 2 or less, or CAP_PERFMON"
    }
}

/// How samples were taken, when strobe took them itself without the helper (`probe::suspend`).
pub fn stacks_how(d: &Deep) -> String {
    match &d.suspend {
        Some(s) => format!(
            "; sampled without the deep helper by suspending each busy thread once per {:.0} ms of its CPU ({} threads; each stall {:.0} µs mean, {:.0} µs p99, {:.0} µs max; user frames only{})",
            d.interval_us / 1e3,
            s.threads,
            s.stall_us_mean,
            s.stall_us_p99,
            s.stall_us_max,
            if s.capped > 0 { format!("; {} over the rate cap", s.capped) } else { String::new() }
        ),
        None => String::new(),
    }
}

/// A sampled-stack session for a process tree (attribution only): the kernel's sampled profile in
/// the helper, else (Windows) strobe's own sampler, which needs no elevation.
pub struct StackCapture {
    #[cfg(windows)]
    client: Option<helper::Client>,
    #[cfg(windows)]
    suspend: Option<crate::probe::suspend::Capture>,
    #[cfg(target_os = "linux")]
    perf: crate::probe::perf::Capture,
    #[cfg(target_os = "macos")]
    sample: crate::probe::sample::Capture,
}

impl StackCapture {
    pub fn start(root: u32) -> crate::model::Result<StackCapture> {
        StackCapture::start_with(root, false)
    }

    /// `unprivileged`: strobe's own sampler even when the helper is installed (Windows;
    /// `record pid:… --probes stacks-unprivileged`).
    pub fn start_with(root: u32, unprivileged: bool) -> crate::model::Result<StackCapture> {
        #[cfg(not(windows))]
        let _ = unprivileged;
        #[cfg(windows)]
        {
            if unprivileged || !access().helper {
                return Ok(StackCapture { client: None, suspend: Some(crate::probe::suspend::Capture::start(root)?) });
            }
            let mut client = helper::Client::connect()?;
            client.call(serde_json::json!({ "op": "stacks.start", "root": root }))?;
            Ok(StackCapture { client: Some(client), suspend: None })
        }
        #[cfg(target_os = "linux")]
        return Ok(StackCapture { perf: crate::probe::perf::Capture::start(root)? });
        #[cfg(target_os = "macos")]
        return Ok(StackCapture { sample: crate::probe::sample::Capture::start(root)? });
    }

    /// Stops sampling; thread names and missing module lists come from the live processes.
    pub fn stop(self) -> crate::model::Result<Deep> {
        #[cfg(windows)]
        {
            let mut deep: Deep = match (self.client, self.suspend) {
                (Some(mut c), _) => serde_json::from_value(c.call(serde_json::json!({ "op": "stacks.stop" }))?)?,
                (None, Some(s)) => s.stop()?,
                (None, None) => return Err(crate::model::Error::msg("no stack session")),
            };
            fill_from_processes(&mut deep);
            Ok(deep)
        }
        #[cfg(target_os = "linux")]
        return self.perf.stop();
        #[cfg(target_os = "macos")]
        return self.sample.stop();
    }
}

/// Thread names, and module lists for processes whose image loads the session missed.
#[cfg(windows)]
fn fill_from_processes(deep: &mut Deep) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModulesEx, K32GetModuleFileNameExW, K32GetModuleInformation, LIST_MODULES_ALL, MODULEINFO};
    use windows_sys::Win32::System::Threading::{GetThreadDescription, OpenProcess, OpenThread, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, THREAD_QUERY_LIMITED_INFORMATION};
    let tids: std::collections::BTreeSet<u32> = deep.samples.iter().map(|s| s.2).collect();
    for tid in tids {
        unsafe {
            let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
            if h.is_null() {
                continue;
            }
            let mut text: *mut u16 = std::ptr::null_mut();
            if GetThreadDescription(h, &mut text) >= 0 && !text.is_null() {
                let mut n = 0;
                while *text.add(n) != 0 {
                    n += 1;
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
                if !name.is_empty() {
                    deep.threads.insert(tid, name);
                }
                windows_sys::Win32::Foundation::LocalFree(text as _);
            }
            CloseHandle(h);
        }
    }
    let sampled: std::collections::BTreeSet<u32> = deep.samples.iter().map(|s| s.1).collect();
    for pid in sampled {
        if deep.modules.iter().any(|m| m.pid == pid) {
            continue;
        }
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
            if h.is_null() {
                continue;
            }
            let mut mods = vec![std::ptr::null_mut(); 1024];
            let mut need = 0u32;
            if K32EnumProcessModulesEx(h, mods.as_mut_ptr(), (mods.len() * 8) as u32, &mut need, LIST_MODULES_ALL) != 0 {
                for &m in mods.iter().take((need as usize / 8).min(mods.len())) {
                    let mut info: MODULEINFO = std::mem::zeroed();
                    let mut name = vec![0u16; 1024];
                    let n = K32GetModuleFileNameExW(h, m, name.as_mut_ptr(), name.len() as u32);
                    if n > 0 && K32GetModuleInformation(h, m, &mut info, std::mem::size_of::<MODULEINFO>() as u32) != 0 {
                        deep.modules.push(Module { pid, base: info.lpBaseOfDll as u64, size: info.SizeOfImage as u64, path: String::from_utf16_lossy(&name[..n as usize]) });
                    }
                }
            }
            CloseHandle(h);
        }
    }
}

/// Native samples from the kernel's sampled profile, as the helper returns them and a recording
/// stores them (`deep.json.gz`), symbolized when read (SPEC §12.2).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Deep {
    pub interval_us: f64,
    pub t0_ns: u64,
    pub t1_ns: u64,
    /// Images of the tracked processes and of the kernel (pid 0).
    pub modules: Vec<Module>,
    /// Unique stacks, innermost frame first (kernel frames before user frames).
    pub stacks: Vec<Vec<u64>>,
    /// (QPC ns, pid, tid, cpu, stack index).
    pub samples: Vec<(u64, u32, u32, u16, u32)>,
    pub threads: std::collections::BTreeMap<u32, String>,
    pub pids: Vec<u32>,
    pub events_lost: u64,
    /// The samples carry no time (macOS `sample`): each is stamped with the capture's start, and
    /// they cover the whole capture.
    #[serde(default)]
    pub untimed: bool,
    /// Frames the sampling tool already named: (pid, address, function, file, line).
    #[serde(default)]
    pub located: Vec<(u32, u64, String, String, u32)>,
    /// Samples left out of `stacks`, by why (macOS: a thread waiting or inside the kernel).
    #[serde(default)]
    pub dropped: std::collections::BTreeMap<String, u64>,
    /// Sampled from strobe's own process without the helper (`probe::suspend`): what it cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<SuspendStats>,
}

/// What sampling stacks by suspending threads cost the target (`query hotspots` prints it).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SuspendStats {
    pub samples: u64,
    /// Threads sampled at least once.
    pub threads: usize,
    /// Each sample's stall of its thread: from the suspend to the resume (µs).
    pub stall_us_mean: f64,
    pub stall_us_p99: f64,
    pub stall_us_max: f64,
    /// Samples the rate cap left out.
    pub capped: u64,
    /// Threads that could not be opened for sampling (another user's, protected, 32-bit).
    pub unopened: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Module {
    pub pid: u32,
    pub base: u64,
    pub size: u64,
    /// A path this process can open (`C:\…`), translated from the kernel's device path.
    pub path: String,
}
