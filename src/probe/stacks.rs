//! Native CPU stacks of a process tree (attribution only, SPEC rule 3, §13.4), whichever sampler this
//! OS has: the kernel's sampled profile in the deep-mode helper (`kernel_profile`, `helper`) or
//! strobe's own suspend-and-unwind sampler (`suspend`) on Windows, perf events (`perf`) on Linux,
//! `/usr/bin/sample` (`sample`) on macOS. All of them give `Deep`, what a recording keeps
//! (`deep.json.gz`), symbolized when read (§12.2).

#[cfg(windows)]
use super::helper;
use serde::{Deserialize, Serialize};

/// Whether native stacks can be sampled here: the helper (Windows), or without it strobe's own
/// suspend-and-unwind sampler (Windows x64); perf events (Linux); `sample` (macOS).
pub fn stacks_available() -> bool {
    #[cfg(windows)]
    return helper::access().helper || cfg!(target_arch = "x86_64");
    #[cfg(not(windows))]
    return sampler_allowed();
}

/// Off Windows: whether strobe's own sampler may run, perf events for this user's processes
/// (Linux) or `/usr/bin/sample` (macOS).
#[cfg(not(windows))]
pub fn sampler_allowed() -> bool {
    #[cfg(target_os = "linux")]
    return super::perf::access().0;
    #[cfg(target_os = "macos")]
    return std::path::Path::new("/usr/bin/sample").is_file();
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

/// What a capture samples: a target's process set (`host::process_set`: its tree, every process
/// stamped with its home, and on Windows its job), read again as it samples, so a detached service
/// or a daemon the app starts outside its tree is sampled too. A target strobe did not launch (`pid:`)
/// has no home: its set is its tree.
#[derive(Clone, Debug)]
pub struct Targets {
    pub root: u32,
    pub home: Option<std::path::PathBuf>,
}

impl Targets {
    pub fn new(root: u32, home: Option<&std::path::Path>) -> Targets {
        Targets { root, home: home.map(std::path::Path::to_path_buf) }
    }

    pub fn pids(&self) -> Vec<u32> {
        crate::host::process_set(self.root, self.home.as_deref()).iter().map(|p| p.pid).collect()
    }
}

/// A sampled-stack session for a process set (attribution only): the kernel's sampled profile in
/// the helper, else (Windows) strobe's own sampler, which needs no elevation. Dropped without `stop`,
/// each ends its sampling (the helper when its client goes).
pub enum StackCapture {
    #[cfg(windows)]
    Helper(helper::Client),
    #[cfg(all(windows, target_arch = "x86_64"))]
    Suspend(super::suspend::Capture),
    #[cfg(target_os = "linux")]
    Perf(super::perf::Capture),
    #[cfg(target_os = "macos")]
    Sample(super::sample::Capture),
}

impl StackCapture {
    pub fn start(targets: Targets) -> crate::model::Result<StackCapture> {
        StackCapture::start_with(targets, false)
    }

    /// `unprivileged`: strobe's own sampler even when the helper is installed (Windows;
    /// `record pid:… --probes stacks-unprivileged`).
    pub fn start_with(targets: Targets, unprivileged: bool) -> crate::model::Result<StackCapture> {
        #[cfg(not(windows))]
        let _ = unprivileged;
        #[cfg(windows)]
        {
            if unprivileged || !helper::access().helper {
                // strobe's own sampler unwinds x64 frames only.
                #[cfg(target_arch = "x86_64")]
                return Ok(StackCapture::Suspend(super::suspend::Capture::start(targets)?));
                #[cfg(not(target_arch = "x86_64"))]
                return Err(crate::model::Error::msg(no_stacks()));
            }
            // The helper runs elevated in a process of its own, which holds no job of this strobe's
            // and reads no stamps for it: it gets the set as it is now, and follows its children.
            let mut client = helper::Client::connect()?;
            client.call(serde_json::json!({ "op": "stacks.start", "root": targets.root, "pids": targets.pids() }))?;
            Ok(StackCapture::Helper(client))
        }
        #[cfg(target_os = "linux")]
        return Ok(StackCapture::Perf(super::perf::Capture::start(targets)?));
        #[cfg(target_os = "macos")]
        return Ok(StackCapture::Sample(super::sample::Capture::start(targets)?));
    }

    /// Stops sampling; thread names and missing module lists come from the live processes.
    pub fn stop(self) -> crate::model::Result<Deep> {
        match self {
            #[cfg(windows)]
            StackCapture::Helper(mut c) => {
                let mut deep: Deep = serde_json::from_value(c.call(serde_json::json!({ "op": "stacks.stop" }))?)?;
                fill_from_processes(&mut deep);
                Ok(deep)
            }
            #[cfg(all(windows, target_arch = "x86_64"))]
            StackCapture::Suspend(s) => {
                let mut deep = s.stop()?;
                fill_from_processes(&mut deep);
                Ok(deep)
            }
            #[cfg(target_os = "linux")]
            StackCapture::Perf(p) => p.stop(),
            #[cfg(target_os = "macos")]
            StackCapture::Sample(s) => s.stop(),
        }
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
    /// Processes of the target's set the sampler gave no stacks of, with why (macOS `sample`: one
    /// macOS will not let it examine, such as a system XPC service doing the app's web content).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsampled: Vec<(u32, String)>,
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
