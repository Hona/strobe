//! What strobe starts and owns. A child starts on given CPUs without moving strobe (the child has the
//! mask before it runs any code, and strobe's own threads keep theirs), in a group strobe ends whole
//! (a job object on Windows, its own process group on Unix), and stamped with the folder it was
//! launched for, which whatever it starts inherits: what left the group (a detached service, a
//! double-forked daemon) still carries the stamp.

use std::path::Path;
use std::process::{Child, Command};

/// Every process strobe launches (an isolated app, a browser, a command target) carries the folder it
/// was launched for (its home or browser profile) in this variable, and whatever it starts inherits
/// it. Read from the environment only, never the command line: a person's shell or search whose
/// arguments name a strobe folder is not strobe's.
pub const HOME_VAR: &str = "STROBE_HOME";
/// And the strobe that launched it: `<pid>:<start time>`, so a reused pid is not that strobe.
pub const OWNER_VAR: &str = "STROBE_OWNER";

/// This strobe as `OWNER_VAR` names it.
pub fn owner_stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| format!("{}:{}", std::process::id(), super::start_time(std::process::id()).unwrap_or(0)))
}

/// A folder as a key: case and separators folded on Windows.
pub fn home_key(p: &Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) { s.to_lowercase().replace('/', "\\") } else { s }
}

/// A process's stamp: the folder it was launched for (`home_key`), and its strobe's pid and start time.
#[derive(Clone, Debug, PartialEq)]
pub struct Stamp {
    pub home: String,
    pub pid: u32,
    pub start: u64,
}

/// `<pid>:<start time>`, as `OWNER_VAR` and a folder's owner file hold it.
pub fn parse_owner(s: &str) -> Option<(u32, u64)> {
    let (p, t) = s.trim().split_once(':')?;
    Some((p.parse().ok()?, t.parse().ok()?))
}

/// A process's stamp, read from its environment now.
pub fn stamp_of(pid: u32) -> Option<Stamp> {
    let env = super::env_of(pid)?;
    // Windows reads variable names in any case.
    let var = |name: &str| env.iter().find_map(|e| e.split_once('=').filter(|(k, _)| if cfg!(windows) { k.eq_ignore_ascii_case(name) } else { *k == name }).map(|(_, v)| v.to_string()));
    let (owner, start) = parse_owner(&var(OWNER_VAR)?)?;
    Some(Stamp { home: home_key(Path::new(&var(HOME_VAR)?)), pid: owner, start })
}

/// Every running process strobe stamped, but this strobe: (pid, parent, stamp). A process's
/// environment is read once (a process is its pid and start time), so a scan reads only the
/// processes that started since the last one. A process that shows no stamp is read again for 2 s
/// after it was first seen: one that has just started may not have its environment yet (a Windows
/// process before its parameters are written, a Unix one between fork and exec).
pub fn stamped() -> Vec<(u32, u32, Stamp)> {
    stamped_in(&super::process_list())
}

/// `stamped` over a process list (pid, parent, start time) already taken.
pub(super) fn stamped_in(list: &[(u32, u32, u64)]) -> Vec<(u32, u32, Stamp)> {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    /// Each process's stamp (or none), and when it was first seen, by its pid and start time.
    type Seen = HashMap<(u32, u64), (Option<Stamp>, Instant)>;
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let me = std::process::id();
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let old = seen.take().unwrap_or_default();
    let mut now = HashMap::with_capacity(old.len());
    let mut out = Vec::new();
    for &(pid, parent, start) in list {
        if pid == 0 || pid == me {
            continue;
        }
        let entry = match old.get(&(pid, start)) {
            Some((None, first)) if first.elapsed() < Duration::from_secs(2) => (stamp_of(pid), *first),
            Some(e) => e.clone(),
            None => (stamp_of(pid), Instant::now()),
        };
        if let Some(s) = &entry.0 {
            out.push((pid, parent, s.clone()));
        }
        now.insert((pid, start), entry);
    }
    *seen = Some(now);
    out
}

/// The processes launched for `dir` (stamped with it), whoever launched them.
pub fn pids_in_home(dir: &Path) -> Vec<u32> {
    pids_in(&super::process_list(), dir)
}

/// `pids_in_home` over a process list already taken.
pub(super) fn pids_in(list: &[(u32, u32, u64)], dir: &Path) -> Vec<u32> {
    let k = home_key(dir);
    stamped_in(list).into_iter().filter(|(_, _, s)| s.home == k).map(|(p, _, _)| p).collect()
}

/// A child strobe launched, with everything it starts as one group: on Windows a job object that
/// holds the child and all it starts but what breaks away (the job allows that, so no app fails for
/// it), which the OS ends when strobe exits however it exits; on Unix the child's own process group,
/// which what it starts joins unless it makes its own (a daemon's `setsid`).
///
/// The child is never reaped before its group is ended (`exited` reads its status without reaping
/// it), so on Unix the group's id, the child's pid, cannot name another process when strobe ends the
/// group. Dropping it ends it all (`end`).
pub struct Owned {
    child: Child,
    /// Taken by `end`.
    group: Option<Group>,
}

struct Group {
    #[cfg(windows)]
    job: Option<isize>,
    /// The group's id: the child's pid.
    #[cfg(unix)]
    pgid: u32,
}

/// Jobs by the folder their launch was for (Windows), for `group_pids`.
#[cfg(windows)]
static JOBS: std::sync::Mutex<Vec<(String, isize)>> = std::sync::Mutex::new(Vec::new());

impl Owned {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The child's output pipes, when it was started with them.
    pub fn output(&mut self) -> (Option<std::process::ChildStdout>, Option<std::process::ChildStderr>) {
        (self.child.stdout.take(), self.child.stderr.take())
    }

    /// The child's exit code once it has exited (-1: ended by a signal). On Unix the child stays a
    /// zombie (`waitid` with `WNOWAIT`): its pid stays its own until `end` reaps it.
    pub fn exited(&mut self) -> Option<i32> {
        #[cfg(windows)]
        {
            self.child.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1))
        }
        #[cfg(unix)]
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let r = libc::waitid(libc::P_PID, self.child.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT);
            if r != 0 || info.si_pid() == 0 {
                return None;
            }
            Some(if info.si_code == libc::CLD_EXITED { info.si_status() } else { -1 })
        }
    }

    /// Ends the child's tree (read first: ending the group orphans it), its group, then the child,
    /// and reaps it. Idempotent.
    pub fn end(&mut self) {
        let Some(group) = self.group.take() else { return };
        super::kill_tree(self.child.id());
        #[cfg(windows)]
        if let Some(job) = group.job {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
            jobs.retain(|(_, j)| *j != job);
            unsafe {
                TerminateJobObject(job as _, 1);
                windows_sys::Win32::Foundation::CloseHandle(job as _);
            }
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(group.pgid as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.end();
    }
}

/// What the OS counted for a command that ran to its exit, with what it started (SPEC §9.6 `cli()`).
/// Each field is None where this OS does not count it for processes that have ended.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub code: i32,
    /// When strobe saw it exit, before anything was read.
    pub exited: Option<std::time::Instant>,
    /// CPU time (user and kernel) of the command and what it started (ns).
    pub cpu_ns: Option<u64>,
    /// How the CPU time was counted, said beside it: exact, or the part read from clock ticks.
    pub cpu_how: &'static str,
    /// Context switches (voluntary and involuntary) of its threads.
    pub switches: Option<u64>,
    /// The most memory one of its processes held at once (bytes): Unix its peak resident set
    /// (`ru_maxrss`), Windows its peak commit (`PeakProcessMemoryUsed`).
    pub peak_bytes: Option<u64>,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    /// Processes still running when the command exited (pid, name): strobe ended them, and what they
    /// did is not the command's whole work.
    pub left: Vec<(u32, String)>,
}

impl Owned {
    /// Waits for the child to exit, reads what the OS counted for it and everything it started, ends
    /// what is left of its group, and reaps it. On Unix the child stays a zombie while it is read
    /// (its pid, the group's id, is not reused), and only processes its tree waited for count: one it
    /// detached (a double fork) that ended before it is in no count.
    pub fn run_to_exit(mut self) -> std::io::Result<Usage> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{
                JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAndIoAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject,
            };
            use windows_sys::Win32::System::Threading::GetProcessTimes;
            use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;
            let t0 = super::tsc();
            let w0 = std::time::Instant::now();
            let status = self.child.wait()?;
            let exited = std::time::Instant::now();
            let wall_ns = exited.duration_since(w0).as_nanos() as u64;
            let h = self.child.as_raw_handle();
            let job = self.group.as_ref().and_then(|g| g.job);
            let mut usage = Usage { code: status.code().unwrap_or(-1), exited: Some(exited), cpu_how: "exact", ..Usage::default() };
            // The child's own CPU from its cycle counter (exact); what it started from the job's
            // clock-tick accounting, the only count Windows keeps of processes that ended.
            let (mut cycles, mut child_100ns) = (0u64, None);
            unsafe {
                QueryProcessCycleTime(h, &mut cycles);
                let (mut c, mut e, mut k, mut u) = (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
                if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
                    let t = |f: windows_sys::Win32::Foundation::FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
                    child_100ns = Some(t(k) + t(u));
                }
            }
            // The TSC's rate over the wait, as the script host's CPU is read (cycles are TSC units).
            let hz = t0.zip(super::tsc()).filter(|(a, b)| b > a && wall_ns > 0).map(|(a, b)| (b - a) as f64 / (wall_ns as f64 / 1e9));
            let child_ns = hz.filter(|_| cycles > 0).map(|hz| (cycles as f64 / hz * 1e9) as u64);
            if let Some(j) = job {
                let mut acct: JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
                let ok = unsafe { QueryInformationJobObject(j as _, JobObjectBasicAndIoAccountingInformation, &mut acct as *mut _ as *mut _, std::mem::size_of_val(&acct) as u32, std::ptr::null_mut()) } != 0;
                if ok {
                    let job_100ns = (acct.BasicInfo.TotalUserTime + acct.BasicInfo.TotalKernelTime).max(0) as u64;
                    let others_ns = job_100ns.saturating_sub(child_100ns.unwrap_or(0)) * 100;
                    usage.cpu_ns = match child_ns {
                        Some(c) => Some(c + others_ns),
                        None => Some(job_100ns * 100),
                    };
                    usage.cpu_how = match (child_ns.is_some(), acct.BasicInfo.TotalProcesses > 1) {
                        (true, false) => "exact",
                        (true, true) => "the command's process exact, the processes it started from clock ticks",
                        (false, _) => "clock ticks",
                    };
                    usage.read_bytes = Some(acct.IoInfo.ReadTransferCount);
                    usage.write_bytes = Some(acct.IoInfo.WriteTransferCount);
                    if acct.BasicInfo.ActiveProcesses > 0 {
                        usage.left = job_pids(j).into_iter().map(|p| (p, super::exe_of(p).and_then(|e| e.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default())).collect();
                    }
                }
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
                if unsafe { QueryInformationJobObject(j as _, JobObjectExtendedLimitInformation, &mut limits as *mut _ as *mut _, std::mem::size_of_val(&limits) as u32, std::ptr::null_mut()) } != 0 {
                    usage.peak_bytes = Some(limits.PeakProcessMemoryUsed as u64);
                }
            } else {
                // No job (strobe's own job refused a nested one): the child alone, and what it started is
                // in no count.
                usage.cpu_ns = child_ns.or(child_100ns.map(|t| t * 100));
                usage.cpu_how = "the command's process only (no job to count what it started)";
            }
            self.end();
            Ok(usage)
        }
        #[cfg(unix)]
        unsafe {
            let pid = self.child.id();
            let mut info: libc::siginfo_t = std::mem::zeroed();
            // Exited, not reaped: a zombie keeps its pid and the group's id while it is read.
            while libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) != 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let exited = std::time::Instant::now();
            let mut usage = Usage { code: if info.si_code == libc::CLD_EXITED { info.si_status() } else { -1 }, exited: Some(exited), cpu_how: "exact", ..Usage::default() };
            #[cfg(target_os = "linux")]
            {
                // The zombie's IO: its own and what its children (reaped by it) did.
                if let Ok(io) = std::fs::read_to_string(format!("/proc/{pid}/io")) {
                    let field = |k: &str| io.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse::<u64>().ok());
                    usage.read_bytes = field("rchar:");
                    usage.write_bytes = field("wchar:");
                }
            }
            #[cfg(target_os = "macos")]
            {
                // Disk bytes of the process alone: its reaped children's are in no count, so with
                // children (their CPU time shows) IO is unknown.
                let mut ri: libc::rusage_info_v4 = std::mem::zeroed();
                if libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V4, &mut ri as *mut _ as *mut libc::rusage_info_t) == 0 && ri.ri_child_user_time + ri.ri_child_system_time == 0 {
                    usage.read_bytes = Some(ri.ri_diskio_bytesread);
                    usage.write_bytes = Some(ri.ri_diskio_byteswritten);
                }
            }
            usage.left = group_members(pid).into_iter().filter(|(p, _)| *p != pid).collect();
            if !usage.left.is_empty() {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let mut status = 0;
            let mut ru: libc::rusage = std::mem::zeroed();
            while libc::wait4(pid as libc::pid_t, &mut status, 0, &mut ru) < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    return Err(std::io::Error::last_os_error());
                }
            }
            // Reaped here: nothing is left to end, and the group's id may name another process now.
            self.group = None;
            let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1_000;
            usage.cpu_ns = Some(tv(ru.ru_utime) + tv(ru.ru_stime));
            usage.switches = Some(ru.ru_nvcsw as u64 + ru.ru_nivcsw as u64);
            // Linux counts kilobytes, macOS bytes.
            usage.peak_bytes = Some(if cfg!(target_os = "macos") { ru.ru_maxrss as u64 } else { ru.ru_maxrss as u64 * 1024 });
            Ok(usage)
        }
    }
}

/// The processes in process group `pgid` now (pid, name).
#[cfg(unix)]
fn group_members(pgid: u32) -> Vec<(u32, String)> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            // `pid (comm) state ppid pgrp …`: the name may hold spaces and parentheses.
            let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
            let pgrp = stat[close + 1..].split_whitespace().nth(2).and_then(|s| s.parse::<u32>().ok());
            if pgrp == Some(pgid) {
                out.push((pid, stat[open + 1..close].to_string()));
            }
        }
        out
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0i32; 4096];
        // PROC_PGRP_ONLY (2), which the libc crate does not name.
        let n = unsafe { libc::proc_listpids(2, pgid, buf.as_mut_ptr() as *mut libc::c_void, (buf.len() * 4) as i32) };
        buf.truncate((n.max(0) as usize / 4).min(buf.len()));
        buf.into_iter().filter(|&p| p > 0).map(|p| (p as u32, super::exe_of(p as u32).and_then(|e| e.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default())).collect()
    }
}

/// The processes of the groups launched for `dir` and still owned (Windows' jobs; none on Unix: the
/// tree and the stamp find them there).
pub fn group_pids(dir: &Path) -> Vec<u32> {
    #[cfg(windows)]
    {
        let k = home_key(dir);
        // Read under the lock: `Owned::end` closes a job only after taking it out of the list.
        let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter().filter(|(h, _)| *h == k).flat_map(|(_, j)| job_pids(*j)).collect()
    }
    #[cfg(unix)]
    {
        let _ = dir;
        Vec::new()
    }
}

/// Spawns `cmd` restricted to the CPUs in `cpus` (None: it inherits strobe's mask, as any child does).
/// strobe's own affinity never changes. macOS has no CPU affinity: there the child runs anywhere.
/// `flags` are Windows process creation flags (`CREATE_NO_WINDOW`, …), ignored elsewhere: a `Command`
/// keeps only the last flags set on it, so they go through here rather than on `cmd`.
pub fn spawn_pinned(cmd: &mut Command, cpus: Option<u64>, flags: u32) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        windows(cmd, cpus, flags, None).map(|(child, _)| child)
    }
    #[cfg(not(windows))]
    {
        let _ = flags;
        pin(cmd, cpus);
        cmd.spawn()
    }
}

/// `spawn_pinned` as an `Owned` launch, which strobe ends whole when it is done with it; `home` is the
/// folder the launch is for (its stamp, set by the caller).
pub fn spawn_owned(cmd: &mut Command, cpus: Option<u64>, flags: u32, home: Option<&Path>) -> std::io::Result<Owned> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        let job = unsafe {
            let j = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            let set = !j.is_null() && SetInformationJobObject(j, JobObjectExtendedLimitInformation, &limits as *const _ as *const _, std::mem::size_of_val(&limits) as u32) != 0;
            if !set && !j.is_null() {
                CloseHandle(j);
            }
            set.then_some(j as isize)
        };
        let (child, assigned) = match windows(cmd, cpus, flags, job) {
            Ok(x) => x,
            Err(e) => {
                if let Some(j) = job {
                    unsafe { CloseHandle(j as _) };
                }
                return Err(e);
            }
        };
        // strobe's own job may refuse a nested one: the stamp and the tree still find everything.
        let job = match (job, assigned) {
            (Some(j), false) => {
                unsafe { CloseHandle(j as _) };
                None
            }
            (j, _) => j,
        };
        if let (Some(j), Some(h)) = (job, home) {
            JOBS.lock().unwrap_or_else(|e| e.into_inner()).push((home_key(h), j));
        }
        Ok(Owned { child, group: Some(Group { job }) })
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let _ = (flags, home);
        cmd.process_group(0);
        pin(cmd, cpus);
        let child = cmd.spawn()?;
        let pgid = child.id();
        Ok(Owned { child, group: Some(Group { pgid }) })
    }
}

/// Linux: the CPU mask set between fork and exec. macOS has no CPU affinity.
#[cfg(not(windows))]
fn pin(cmd: &mut Command, cpus: Option<u64>) {
    #[cfg(target_os = "linux")]
    if let Some(mask) = cpus {
        use std::os::unix::process::CommandExt;
        // Every CPU the machine has, not strobe's own set: strobe may sit on the other CPUs (a
        // bench keeps it on the runner's), and a count from available_parallelism names the
        // lowest CPUs whatever strobe's set is.
        let configured = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) }.clamp(1, 64) as u32;
        let all = if configured >= 64 { u64::MAX } else { (1u64 << configured) - 1 };
        let target = if mask == 0 { all } else { mask & all };
        if target != 0 {
            let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
            for c in (0..64).filter(|c| target >> c & 1 == 1) {
                unsafe { libc::CPU_SET(c, &mut set) };
            }
            // Between fork and exec only async-signal-safe calls: the set is built before.
            unsafe {
                cmd.pre_exec(move || {
                    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
                    Ok(())
                });
            }
        }
    }
    #[cfg(target_os = "macos")]
    let _ = (cmd, cpus);
}

/// The processes in a job now (`JobObjectBasicProcessIdList`).
#[cfg(windows)]
fn job_pids(job: isize) -> Vec<u32> {
    use windows_sys::Win32::System::JobObjects::{JobObjectBasicProcessIdList, QueryInformationJobObject};
    // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts (assigned, listed), then the ids, pointer-sized.
    let head = 8 / size_of::<usize>();
    let mut n = 64usize;
    loop {
        let mut buf = vec![0usize; head + n];
        let ok = unsafe { QueryInformationJobObject(job as _, JobObjectBasicProcessIdList, buf.as_mut_ptr() as *mut _, (buf.len() * size_of::<usize>()) as u32, std::ptr::null_mut()) } != 0;
        let counts = buf.as_ptr() as *const u32;
        let (assigned, listed) = unsafe { (*counts as usize, *counts.add(1) as usize) };
        if ok {
            return buf[head..head + listed.min(n)].iter().map(|p| *p as u32).collect();
        }
        // Too small: the call still says how many there are.
        if assigned <= n || n >= 65536 {
            return Vec::new();
        }
        n = assigned + 16;
    }
}

/// ntdll's `NtResumeProcess`: resumes every thread of a process.
#[cfg(windows)]
fn nt_resume() -> Option<unsafe extern "system" fn(*mut std::ffi::c_void) -> i32> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type Resume = unsafe extern "system" fn(*mut std::ffi::c_void) -> i32;
    static F: std::sync::OnceLock<Option<Resume>> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        let f = unsafe { GetProcAddress(GetModuleHandleW(ntdll.as_ptr()), c"NtResumeProcess".as_ptr() as *const u8) }?;
        Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Resume>(f) })
    })
}

/// Created suspended, put in `job` and given the mask, then resumed: its first instruction already
/// runs there. Returns whether the job took it.
#[cfg(windows)]
fn windows(cmd: &mut Command, cpus: Option<u64>, flags: u32, job: Option<isize>) -> std::io::Result<(Child, bool)> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, GetProcessAffinityMask, OpenThread, ResumeThread, SetProcessAffinityMask, THREAD_SUSPEND_RESUME};
    if cpus.is_none() && job.is_none() {
        return Ok((cmd.creation_flags(flags).spawn()?, false));
    }
    let mut child = cmd.creation_flags(flags | CREATE_SUSPENDED).spawn()?;
    let h = child.as_raw_handle();
    unsafe {
        let assigned = job.is_some_and(|j| AssignProcessToJobObject(j as _, h) != 0);
        if let Some(mask) = cpus {
            let (mut proc_mask, mut sys_mask) = (0usize, 0usize);
            if GetProcessAffinityMask(h, &mut proc_mask, &mut sys_mask) != 0 {
                let target = if mask == 0 { sys_mask } else { mask as usize & sys_mask };
                if target != 0 {
                    SetProcessAffinityMask(h, target);
                }
            }
        }
        // A process created suspended has its one thread: resume it. `NtResumeProcess` takes
        // microseconds, where a system-wide thread snapshot to find that thread took milliseconds,
        // which a launch's time would count.
        let mut resumed = nt_resume().is_some_and(|resume| resume(h) >= 0);
        let snap = if resumed { INVALID_HANDLE_VALUE } else { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap != INVALID_HANDLE_VALUE {
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut ok = Thread32First(snap, &mut e) != 0;
            while ok {
                if e.th32OwnerProcessID == child.id() {
                    let t = OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID);
                    if !t.is_null() {
                        resumed |= ResumeThread(t) != u32::MAX;
                        CloseHandle(t);
                    }
                }
                ok = Thread32Next(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        if !resumed {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("could not resume the child"));
        }
        Ok((child, assigned))
    }
}
