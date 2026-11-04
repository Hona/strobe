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
//! - `frames`: presents (and a toolkit's frame rows) into frame metrics; no Windows API.
//!
//! Every timestamp is the QPC clock (`WNODE_HEADER.ClientContext = 1`) in nanoseconds, the clock of
//! `host::now_ns`, so presents and stacks line up with marks and OS samples without mapping.

pub mod frames;
#[cfg(windows)]
pub mod helper;
#[cfg(windows)]
pub mod present;
#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod stacks;

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

/// A present session: in this process when its token may, else in the helper.
pub enum PresentCapture {
    #[cfg(windows)]
    Local(present::Capture),
    #[cfg(windows)]
    Helper(helper::Client),
    #[cfg(not(windows))]
    None,
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
        return Err(crate::model::Error::msg("Linux gives no outside view of when another app's frames reach the screen (DRM page flips need root tracing and name no process; X11 Present and Wayland presentation-time report only to the app itself): presented frames need the toolkit's own present times"));
        #[cfg(target_os = "macos")]
        return Err(crate::model::Error::msg("macOS gives no unprivileged outside view of when another app's frames reach the screen (WindowServer's timing needs root tracing or Instruments): presented frames need the toolkit's own present times (Metal's presentedTime)"));
    }

    /// `in-process` or `helper`.
    pub fn via(&self) -> &'static str {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(_) => "in-process",
            #[cfg(windows)]
            PresentCapture::Helper(_) => "helper",
            #[cfg(not(windows))]
            PresentCapture::None => "",
        }
    }

    pub fn stop(self) -> crate::model::Result<Presents> {
        match self {
            #[cfg(windows)]
            PresentCapture::Local(c) => c.stop(),
            #[cfg(windows)]
            PresentCapture::Helper(mut c) => Ok(serde_json::from_value(c.call(serde_json::json!({ "op": "presents.stop" }))?)?),
            #[cfg(not(windows))]
            PresentCapture::None => Ok(Presents::default()),
        }
    }
}

/// A sampled-stack session for a process tree, in the helper (attribution only).
pub struct StackCapture {
    #[cfg(windows)]
    client: helper::Client,
    #[cfg(target_os = "linux")]
    perf: crate::probe::perf::Capture,
    #[cfg(target_os = "macos")]
    sample: crate::probe::sample::Capture,
}

impl StackCapture {
    pub fn start(root: u32) -> crate::model::Result<StackCapture> {
        #[cfg(windows)]
        {
            let mut client = helper::Client::connect()?;
            client.call(serde_json::json!({ "op": "stacks.start", "root": root }))?;
            Ok(StackCapture { client })
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
            let mut c = self.client;
            let mut deep: Deep = serde_json::from_value(c.call(serde_json::json!({ "op": "stacks.stop" }))?)?;
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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Module {
    pub pid: u32,
    pub base: u64,
    pub size: u64,
    /// A path this process can open (`C:\…`), translated from the kernel's device path.
    pub path: String,
}
