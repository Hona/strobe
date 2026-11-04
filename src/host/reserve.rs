//! Keeps this user's other processes off a timed block's CPUs (SPEC §9.4, `bench --reserve-cpus`).
//! Agents, builds and editors left to the scheduler land on the CPUs strobe pinned the target to;
//! for the length of one block each of them gets a default CPU set without those CPUs, and gets its
//! own back after the block. A sweep every 200 ms catches processes started during the block (a
//! build spawning compilers): CPU sets are not inherited.
//!
//! Windows only. Process default CPU sets (not affinity masks): they need only
//! `PROCESS_SET_LIMITED_INFORMATION`, restore exactly (affinity changes overwrite every thread's own
//! mask, which nothing can put back), are never inherited, and on the machines checked hold as hard as
//! affinity (24 busy threads on a 16-CPU set stayed off the other CPUs). Threads that selected CPU
//! sets themselves, or set an affinity mask, keep theirs. What changes is journaled in
//! `<state>/reserve.json` first, so a strobe killed mid-block is undone by the next strobe that starts.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What one block's reservation did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    /// The CPUs kept for the target (a mask of processor group 0).
    pub cpus: u64,
    /// Processes moved off them, by executable name: how many of each.
    pub moved: BTreeMap<String, u32>,
    /// How many of those started while the block ran (moved by the sweep, up to 200 ms late).
    pub started: u32,
    /// This user's processes that could not be moved, by name, with why.
    pub failed: BTreeMap<String, String>,
    /// Moved back after the block. Others exited, or something else changed their CPU set since
    /// (theirs is kept).
    pub restored: u32,
    /// Sweeps of the process list, and the sweeper's own CPU while the block ran (% of one core; it
    /// runs on strobe's CPUs, not the target's).
    pub sweeps: u32,
    pub overhead_pct: f64,
    /// The first sweep, which moves what already runs before the block starts (ms).
    pub setup_ms: f64,
    pub wall_ms: f64,
}

impl Report {
    pub fn moved_total(&self) -> u32 {
        self.moved.values().sum()
    }
}

fn journal() -> std::path::PathBuf {
    super::paths::state_dir().join("reserve.json")
}

/// Undoes the reservation of a strobe that died mid-block (killed, crashed, power cut).
pub fn restore_stale() {
    #[cfg(windows)]
    imp::restore_stale();
}

/// A reservation in force; `finish` ends it.
pub struct Reserve {
    #[cfg(windows)]
    inner: imp::Reserve,
}

impl Reserve {
    /// Moves this user's other processes off `cpus` until `finish`. Err says why nothing moved.
    pub fn start(cpus: u64) -> Result<Reserve, String> {
        #[cfg(windows)]
        {
            imp::Reserve::start(cpus).map(|inner| Reserve { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = cpus;
            Err("not built on this OS yet".into())
        }
    }

    /// Puts every moved process back and says what happened.
    pub fn finish(self) -> Report {
        #[cfg(windows)]
        {
            self.inner.finish()
        }
        #[cfg(not(windows))]
        {
            Report::default()
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{Report, journal};
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{EqualSid, GetLengthSid, GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessAffinityMask, GetProcessDefaultCpuSets, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_LIMITED_INFORMATION,
        SetProcessDefaultCpuSets,
    };

    const SWEEP: Duration = Duration::from_millis(200);

    struct Moved {
        handle: HANDLE,
        pid: u32,
        create: i64,
        original: Vec<u32>,
        set: Vec<u32>,
    }

    // Process handles are plain kernel handles, usable from any thread.
    unsafe impl Send for Moved {}

    pub struct Reserve {
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<Report>>,
    }

    impl Reserve {
        pub fn start(cpus: u64) -> Result<Reserve, String> {
            let allowed = allowed_sets(cpus);
            if allowed.is_empty() {
                return Err("no CPU outside the target's to move other processes to".into());
            }
            let stop = Arc::new(AtomicBool::new(false));
            let (tx, rx) = std::sync::mpsc::channel();
            let flag = stop.clone();
            let worker = std::thread::Builder::new().name("strobe-reserve".into()).spawn(move || run(cpus, allowed, flag, tx)).map_err(|e| e.to_string())?;
            // The block starts once the first sweep moved what already runs.
            let _ = rx.recv_timeout(Duration::from_secs(10));
            Ok(Reserve { stop, worker: Some(worker) })
        }

        pub fn finish(mut self) -> Report {
            self.stop.store(true, Ordering::Relaxed);
            let w = self.worker.take();
            if let Some(w) = &w {
                w.thread().unpark();
            }
            w.and_then(|w| w.join().ok()).unwrap_or_default()
        }
    }

    impl Drop for Reserve {
        fn drop(&mut self) {
            // A block that failed still puts everything back.
            self.stop.store(true, Ordering::Relaxed);
            if let Some(w) = self.worker.take() {
                w.thread().unpark();
                let _ = w.join();
            }
        }
    }

    /// CPU set ids of every CPU outside `cpus` (group 0's mask; other groups are all outside).
    fn allowed_sets(cpus: u64) -> Vec<u32> {
        let mut out = Vec::new();
        unsafe {
            let mut len = 0u32;
            GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, GetCurrentProcess(), 0);
            if len == 0 {
                return out;
            }
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetSystemCpuSetInformation(buf.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION, len, &mut len, GetCurrentProcess(), 0) == 0 {
                return out;
            }
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            while off + 8 <= len as usize {
                let info = &*(base.add(off) as *const SYSTEM_CPU_SET_INFORMATION);
                let set = info.Anonymous.CpuSet;
                let reserved = set.Group == 0 && set.LogicalProcessorIndex < 64 && cpus >> set.LogicalProcessorIndex & 1 == 1;
                if !reserved {
                    out.push(set.Id);
                }
                if info.Size == 0 {
                    break;
                }
                off += info.Size as usize;
            }
        }
        out
    }

    fn default_sets(h: HANDLE) -> Option<Vec<u32>> {
        let mut ids = vec![0u32; 64];
        let mut n = 0u32;
        unsafe {
            if GetProcessDefaultCpuSets(h, ids.as_mut_ptr(), ids.len() as u32, &mut n) == 0 {
                if GetLastError() != 122 {
                    return None;
                }
                ids.resize(n as usize, 0);
                if GetProcessDefaultCpuSets(h, ids.as_mut_ptr(), ids.len() as u32, &mut n) == 0 {
                    return None;
                }
            }
        }
        ids.truncate(n as usize);
        Some(ids)
    }

    fn set_sets(h: HANDLE, ids: &[u32]) -> bool {
        unsafe { SetProcessDefaultCpuSets(h, if ids.is_empty() { std::ptr::null() } else { ids.as_ptr() }, ids.len() as u32) != 0 }
    }

    /// The token user's SID of a process, as bytes.
    fn user_of(h: HANDLE) -> Option<Vec<u8>> {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(h, TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut buf = vec![0u64; 32];
            let mut len = 0u32;
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut len) != 0;
            CloseHandle(token);
            if !ok {
                return None;
            }
            let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let n = GetLengthSid(sid) as usize;
            Some(std::slice::from_raw_parts(sid as *const u8, n).to_vec())
        }
    }

    fn same_sid(a: &[u8], b: &[u8]) -> bool {
        unsafe { EqualSid(a.as_ptr() as PSID, b.as_ptr() as PSID) != 0 }
    }

    fn creation(h: HANDLE) -> i64 {
        use windows_sys::Win32::Foundation::FILETIME;
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = unsafe { std::mem::zeroed() };
        unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
        ((c.dwHighDateTime as u64) << 32 | c.dwLowDateTime as u64) as i64
    }

    enum Verdict {
        /// Not this user's, protected, or already off the CPUs (or deliberately on them): left alone.
        Skip,
        Failed(String),
        Move(Moved),
    }

    fn consider(pid: u32, create: i64, cpus: u64, allowed: &[u32], me: &[u8]) -> Verdict {
        unsafe {
            let q = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if q.is_null() {
                return Verdict::Skip;
            }
            let ours = user_of(q).is_some_and(|sid| same_sid(&sid, me));
            let (mut mask, mut system) = (0usize, 0usize);
            let has_mask = GetProcessAffinityMask(q, &mut mask, &mut system) != 0;
            CloseHandle(q);
            if !ours {
                return Verdict::Skip;
            }
            let mask = mask as u64;
            // Pinned to the target's CPUs (strobe's own launches) or kept off them already.
            if has_mask && (mask & !cpus == 0 || mask & cpus == 0) {
                return Verdict::Skip;
            }
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return Verdict::Failed("elevated: an unelevated strobe cannot change them".into());
            }
            if creation(h) != create {
                CloseHandle(h);
                return Verdict::Skip;
            }
            let Some(original) = default_sets(h) else {
                CloseHandle(h);
                return Verdict::Failed("its CPU set could not be read".into());
            };
            let set: Vec<u32> = if original.is_empty() { allowed.to_vec() } else { original.iter().copied().filter(|id| allowed.contains(id)).collect() };
            if set.is_empty() {
                CloseHandle(h);
                return Verdict::Failed("its own CPU set holds only the target's CPUs".into());
            }
            if set == original {
                CloseHandle(h);
                return Verdict::Skip;
            }
            Verdict::Move(Moved { handle: h, pid, create, original, set })
        }
    }

    fn write_journal(moved: &[Moved], batch: &[(Moved, String)]) {
        let entries: Vec<Value> = moved.iter().chain(batch.iter().map(|(m, _)| m)).map(|m| json!({ "pid": m.pid, "create": m.create, "original": m.original, "set": m.set })).collect();
        let _ = std::fs::write(journal(), json!({ "pid": std::process::id(), "entries": entries }).to_string());
    }

    fn run(cpus: u64, allowed: Vec<u32>, stop: Arc<AtomicBool>, ready: std::sync::mpsc::Sender<()>) -> Report {
        let started = Instant::now();
        let (mut c0, mut t0) = (0, 0);
        let mut report = Report { cpus, ..Report::default() };
        let me = user_of(unsafe { GetCurrentProcess() }).unwrap_or_default();
        let own = std::process::id();
        let mut session = 0u32;
        unsafe { windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(own, &mut session) };
        let mut seen: HashSet<(u32, i64)> = HashSet::new();
        let mut moved: Vec<Moved> = Vec::new();
        let mut first = true;
        loop {
            let procs = super::super::win::procs();
            if !first && procs.iter().all(|p| seen.contains(&(p.0, p.2))) {
                // Nothing started since the last sweep.
                report.sweeps += 1;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::park_timeout(SWEEP);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            // strobe's own tree: the target (pinned to these CPUs), the runner, servers.
            let mut tree: HashSet<u32> = HashSet::from([own]);
            let create_of: std::collections::HashMap<u32, i64> = procs.iter().map(|p| (p.0, p.2)).collect();
            loop {
                let before = tree.len();
                for p in &procs {
                    if !tree.contains(&p.0) && tree.contains(&p.1) && create_of.get(&p.1).is_some_and(|c| *c <= p.2) {
                        tree.insert(p.0);
                    }
                }
                if tree.len() == before {
                    break;
                }
            }
            let mut batch: Vec<(Moved, String)> = Vec::new();
            for (pid, _, create, sess, name) in &procs {
                if !seen.insert((*pid, *create)) || *pid <= 4 || *sess != session || tree.contains(pid) {
                    continue;
                }
                let lower = name.to_lowercase();
                if lower == "strobe.exe" || lower == "strobew.exe" {
                    continue;
                }
                match consider(*pid, *create, cpus, &allowed, &me) {
                    Verdict::Skip => {}
                    Verdict::Failed(why) => {
                        report.failed.insert(name.clone(), why);
                    }
                    Verdict::Move(m) => batch.push((m, name.clone())),
                }
            }
            if !batch.is_empty() {
                // Journal first: a strobe killed between the two still has these put back.
                write_journal(&moved, &batch);
                for (m, name) in batch {
                    if set_sets(m.handle, &m.set) {
                        *report.moved.entry(name).or_default() += 1;
                        if !first {
                            report.started += 1;
                        }
                        moved.push(m);
                    } else {
                        report.failed.insert(name, "the OS refused its CPU set".into());
                        unsafe { CloseHandle(m.handle) };
                    }
                }
            }
            report.sweeps += 1;
            if first {
                report.setup_ms = started.elapsed().as_secs_f64() * 1e3;
                (c0, t0) = (crate::host::thread_cycles(), crate::host::tsc().unwrap_or(0));
                let _ = ready.send(());
                first = false;
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::park_timeout(SWEEP);
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        let (c1, t1) = (crate::host::thread_cycles(), crate::host::tsc().unwrap_or(0));
        report.overhead_pct = if t1 > t0 { 100.0 * c1.saturating_sub(c0) as f64 / (t1 - t0) as f64 } else { 0.0 };
        for m in moved.drain(..) {
            // Only where the set is still strobe's: something else that changed it since keeps its own.
            if default_sets(m.handle).is_some_and(|now| now == m.set) && set_sets(m.handle, &m.original) {
                report.restored += 1;
            }
            unsafe { CloseHandle(m.handle) };
        }
        let _ = std::fs::remove_file(journal());
        report.wall_ms = started.elapsed().as_secs_f64() * 1e3;
        report
    }

    pub fn restore_stale() {
        let Some(v) = std::fs::read_to_string(journal()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
        let pid = v["pid"].as_u64().unwrap_or(0) as u32;
        if pid == std::process::id() || crate::host::exe_of(pid).and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())).is_some_and(|n| n.starts_with("strobe")) {
            return;
        }
        for e in v["entries"].as_array().into_iter().flatten() {
            let ids = |k: &str| e[k].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as u32)).collect::<Vec<u32>>()).unwrap_or_default();
            let (pid, create, original, set) = (e["pid"].as_u64().unwrap_or(0) as u32, e["create"].as_i64().unwrap_or(0), ids("original"), ids("set"));
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_LIMITED_INFORMATION, 0, pid);
                if h.is_null() {
                    continue;
                }
                if creation(h) == create && default_sets(h).is_some_and(|now| now == set) {
                    set_sets(h, &original);
                }
                CloseHandle(h);
            }
        }
        let _ = std::fs::remove_file(journal());
    }
}
