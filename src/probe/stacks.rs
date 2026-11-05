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
    /// With why there are no waits: only the helper's kernel session sees context switches.
    #[cfg(windows)]
    Suspend(super::suspend::Capture, crate::model::Refusal),
    /// With the off-CPU waits capture, or why there is none.
    #[cfg(target_os = "linux")]
    Perf(super::perf::Capture, std::result::Result<super::perf::WaitCapture, crate::model::Refusal>),
    /// With whether waits were asked for (macOS has no source for them).
    #[cfg(target_os = "macos")]
    Sample(super::sample::Capture, bool),
}

/// Why a capture that did not ask for waits has none.
fn not_asked() -> crate::model::Refusal {
    no_waits(
        "this capture did not record off-CPU waits: the scheduler's events cost the target at every context switch, so only a capture that asks takes them",
        "record with --probes waits (record pid:<n>, a native app's record, or bench --why always)",
    )
}

impl StackCapture {
    /// `waits`: the target's off-CPU waits too (`--probes waits`), which cost it at every context
    /// switch; their capture failing refuses the waits, never the stacks.
    pub fn start(targets: Targets, waits: bool) -> crate::model::Result<StackCapture> {
        StackCapture::start_with(targets, false, waits)
    }

    /// `unprivileged`: strobe's own sampler even when the helper is installed (Windows;
    /// `record pid:… --probes stacks-unprivileged`).
    pub fn start_with(targets: Targets, unprivileged: bool, waits: bool) -> crate::model::Result<StackCapture> {
        #[cfg(not(windows))]
        let _ = unprivileged;
        #[cfg(windows)]
        {
            let fix = "strobe up installs this build's deep helper (one UAC prompt)";
            if unprivileged || !helper::access().helper {
                let why = match (waits, unprivileged) {
                    (false, _) => not_asked(),
                    (true, true) => no_waits("off-CPU waits need the deep helper's kernel session (context switches and wake-ups); this capture asked for strobe's own sampler", fix),
                    (true, false) => no_waits("off-CPU waits need the deep helper's kernel session (context switches and wake-ups), and it is not installed", fix),
                };
                return Ok(StackCapture::Suspend(super::suspend::Capture::start(targets.clone())?, why));
            }
            // The helper runs elevated in a process of its own, which holds no job of this strobe's
            // and reads no stamps for it: it gets the set as it is now, and follows its children.
            let mut client = match helper::Client::connect() {
                Ok(c) => c,
                // Another build's helper (its operations may mean other things): stacks from strobe's
                // own sampler, and waits refused until `strobe up` installs this build's.
                Err(e) if !helper::helper_current() => {
                    let why = if waits { no_waits(format!("off-CPU waits need this build's deep helper: {}", e.message), fix) } else { not_asked() };
                    return Ok(StackCapture::Suspend(super::suspend::Capture::start(targets)?, why));
                }
                Err(e) => return Err(e),
            };
            client.call(serde_json::json!({ "op": "stacks.start", "root": targets.root, "pids": targets.pids(), "waits": waits }))?;
            Ok(StackCapture::Helper(client))
        }
        #[cfg(target_os = "linux")]
        {
            let waits = if waits { super::perf::WaitCapture::start(targets.clone()) } else { Err(not_asked()) };
            Ok(StackCapture::Perf(super::perf::Capture::start(targets)?, waits))
        }
        #[cfg(target_os = "macos")]
        {
            Ok(StackCapture::Sample(super::sample::Capture::start(targets)?, waits))
        }
    }

    /// Stops sampling; thread names and missing module lists come from the live processes.
    pub fn stop(self) -> crate::model::Result<Deep> {
        match self {
            #[cfg(windows)]
            StackCapture::Helper(mut c) => {
                let mut deep: Deep = serde_json::from_value(c.call(serde_json::json!({ "op": "stacks.stop" }))?)?;
                if deep.waits.is_none() && deep.waits_refused.is_none() {
                    deep.waits_refused = Some(not_asked());
                }
                fill_from_processes(&mut deep);
                Ok(deep)
            }
            #[cfg(windows)]
            StackCapture::Suspend(s, why) => {
                let mut deep = s.stop()?;
                deep.waits_refused = Some(why);
                fill_from_processes(&mut deep);
                Ok(deep)
            }
            #[cfg(target_os = "linux")]
            StackCapture::Perf(p, waits) => {
                let mut deep = p.stop()?;
                match waits.and_then(|w| w.stop()) {
                    Ok(w) => deep.waits = Some(w),
                    Err(why) => deep.waits_refused = Some(why),
                }
                Ok(deep)
            }
            #[cfg(target_os = "macos")]
            StackCapture::Sample(s, waits) => {
                let mut deep = s.stop()?;
                deep.waits_refused = Some(if waits {
                    no_waits(
                        "macOS gives an unprivileged process no scheduler events of another (context switches and wake-ups need kdebug tracing, which needs root)",
                        "read waits on Linux or Windows; on macOS, query hotspots shows where threads ran",
                    )
                } else {
                    not_asked()
                });
                Ok(deep)
            }
        }
    }
}

/// Thread names, and module lists for processes whose image loads the session missed.
#[cfg(windows)]
fn fill_from_processes(deep: &mut Deep) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
    let mut tids: std::collections::BTreeSet<u32> = deep.samples.iter().map(|s| s.2).collect();
    if let Some(w) = &deep.waits {
        tids.extend(w.waits.iter().flat_map(|x| std::iter::once(x.tid).chain(x.waker.as_ref().map(|k| k.tid))));
    }
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
    if let Some(w) = deep.waits.as_mut() {
        w.threads = deep.threads.clone();
        // The wakers' processes too: a waker outside the target has no image loads in the session.
        let pids: std::collections::BTreeSet<u32> = w.waits.iter().flat_map(|x| std::iter::once(x.pid).chain(x.waker.as_ref().filter(|k| !k.interrupt).map(|k| k.pid))).collect();
        for pid in pids {
            if !w.modules.iter().any(|m| m.pid == pid) {
                w.modules.extend(process_modules(pid));
            }
        }
    }
    let sampled: std::collections::BTreeSet<u32> = deep.samples.iter().map(|s| s.1).collect();
    for pid in sampled {
        if !deep.modules.iter().any(|m| m.pid == pid) {
            let found = process_modules(pid);
            deep.modules.extend(found);
        }
    }
}

/// The images loaded in a live process now (none when it is gone or strobe may not read it).
#[cfg(windows)]
fn process_modules(pid: u32) -> Vec<Module> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModulesEx, K32GetModuleFileNameExW, K32GetModuleInformation, LIST_MODULES_ALL, MODULEINFO};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    let mut out = Vec::new();
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            return out;
        }
        let mut mods = vec![std::ptr::null_mut(); 1024];
        let mut need = 0u32;
        if K32EnumProcessModulesEx(h, mods.as_mut_ptr(), (mods.len() * 8) as u32, &mut need, LIST_MODULES_ALL) != 0 {
            for &m in mods.iter().take((need as usize / 8).min(mods.len())) {
                let mut info: MODULEINFO = std::mem::zeroed();
                let mut name = vec![0u16; 1024];
                let n = K32GetModuleFileNameExW(h, m, name.as_mut_ptr(), name.len() as u32);
                if n > 0 && K32GetModuleInformation(h, m, &mut info, std::mem::size_of::<MODULEINFO>() as u32) != 0 {
                    out.push(Module { pid, base: info.lpBaseOfDll as u64, size: info.SizeOfImage as u64, path: String::from_utf16_lossy(&name[..n as usize]) });
                }
            }
        }
        CloseHandle(h);
    }
    out
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
    /// Off-CPU waits of the same threads over the same span (`query <rec> waits`), from the
    /// scheduler's events; or why there are none (`NoWaits`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waits: Option<Waits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waits_refused: Option<crate::model::Refusal>,
}

/// Why a thread was off its CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitKind {
    /// Waiting for something (a lock, an event, a timer, a socket): it gave the CPU up.
    Blocked,
    /// In an uninterruptible wait: disk IO, a page fault (Linux `D`, Windows page-in waits).
    Io,
    /// Runnable but switched out for another thread (preempted, its quantum over).
    Preempted,
    Other,
}

/// One stretch a thread of the target spent off its CPU: from its switch-out to its next switch-in.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wait {
    pub pid: u32,
    pub tid: u32,
    /// Host clock (ns), as native samples.
    pub from_ns: u64,
    pub to_ns: u64,
    /// When something made it runnable again (its wake-up), when the OS said: the rest of the wait
    /// it was queued for a CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_ns: Option<u64>,
    pub kind: WaitKind,
    /// The OS's own word for it (Linux the task state `S`, `D`; Windows the wait reason).
    pub reason: String,
    /// Where it waited: its stack when it switched out (Linux) or back in (Windows), index into `Waits::stacks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waker: Option<Waker>,
}

/// Who made a waiting thread runnable: a thread (with the stack it did it from), or an interrupt
/// (a timer, a device: no thread of anyone's).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Waker {
    pub pid: u32,
    pub tid: u32,
    pub interrupt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<u32>,
}

/// Off-CPU waits (attribution only, SPEC rule 3): the scheduler's switch and wake-up events of the
/// target's threads, with stacks (Linux perf tracepoints, Windows CSwitch and ReadyThread).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Waits {
    pub waits: Vec<Wait>,
    /// Unique stacks, innermost first.
    pub stacks: Vec<Vec<u64>>,
    /// Images of the waiting and waking processes (kernel under pid 0).
    pub modules: Vec<Module>,
    pub threads: std::collections::BTreeMap<u32, String>,
    /// Events the OS dropped (a full buffer): waits may be missing.
    pub events_lost: u64,
    /// Why wakers are not known, when they are not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_wakers: Option<String>,
}

impl Waits {
    /// Interns a stack, innermost first.
    pub fn stack(&mut self, index: &mut std::collections::HashMap<Vec<u64>, u32>, frames: Vec<u64>) -> Option<u32> {
        if frames.is_empty() {
            return None;
        }
        let next = self.stacks.len() as u32;
        Some(*index.entry(frames.clone()).or_insert_with(|| {
            self.stacks.push(frames);
            next
        }))
    }
}

/// The refusal for waits this capture cannot give (`NoWaits`).
pub fn no_waits(why: impl Into<String>, fix: impl Into<String>) -> crate::model::Refusal {
    crate::model::Refusal::new(crate::model::ValidatorId::NoWaits, why).fix(fix)
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
