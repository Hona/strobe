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
/// processes that started since the last one; strobe's own children are read again until they show a
/// stamp (between fork and exec a child still has strobe's environment).
pub fn stamped() -> Vec<(u32, u32, Stamp)> {
    stamped_in(&super::process_list())
}

/// `stamped` over a process list (pid, parent, start time) already taken.
pub(super) fn stamped_in(list: &[(u32, u32, u64)]) -> Vec<(u32, u32, Stamp)> {
    use std::collections::HashMap;
    /// Each process's stamp (or none) by its pid and start time.
    type Seen = HashMap<(u32, u64), Option<Stamp>>;
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
        let stamp = match old.get(&(pid, start)) {
            Some(s) => s.clone(),
            None => stamp_of(pid),
        };
        if let Some(s) = &stamp {
            out.push((pid, parent, s.clone()));
        }
        if stamp.is_some() || parent != me {
            now.insert((pid, start), stamp);
        }
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

/// What one launch started, as one group: on Windows a job object that holds the child and
/// everything it starts but what breaks away (the job allows that, so no app fails for it), and
/// that the OS ends when strobe exits however it exits; on Unix the child's own process group, which
/// what it starts joins unless it makes its own (a daemon's `setsid`). Dropping it ends the group.
pub struct Group {
    #[cfg(windows)]
    job: Option<isize>,
    /// The group's id (its leader's pid) and the leader's start time.
    #[cfg(unix)]
    leader: Option<(u32, u64)>,
    /// The folder the launch was for: `group_pids` finds the group by it.
    #[cfg_attr(unix, allow(dead_code))]
    home: Option<String>,
}

/// Jobs by the folder their launch was for (Windows), for `group_pids`.
#[cfg(windows)]
static JOBS: std::sync::Mutex<Vec<(String, isize)>> = std::sync::Mutex::new(Vec::new());

impl Group {
    /// Ends every process of the group now.
    pub fn kill(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(job as _, 1) };
        }
        #[cfg(unix)]
        if let Some((pgid, start)) = self.leader {
            // While the leader runs (or waits to be reaped) the id is this group's; once it is gone, the
            // id names another group only when a new leader took the pid, with another start time.
            if super::start_time(pgid).is_none_or(|s| s == start) {
                unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
            }
        }
    }

    /// The group's processes now (Windows: the job's; Unix: none, the tree and the stamp find them).
    pub fn pids(&self) -> Vec<u32> {
        #[cfg(windows)]
        {
            self.job.map(job_pids).unwrap_or_default()
        }
        #[cfg(unix)]
        {
            Vec::new()
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            JOBS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, j)| *j != job);
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job as _) };
        }
    }
}

/// The processes of the groups launched for `dir` and still owned (Windows' jobs; none on Unix).
pub fn group_pids(dir: &Path) -> Vec<u32> {
    #[cfg(windows)]
    {
        let k = home_key(dir);
        let jobs: Vec<isize> = JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(h, _)| *h == k).map(|(_, j)| *j).collect();
        jobs.into_iter().flat_map(job_pids).collect()
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

/// `spawn_pinned` in a new `Group`, for a launch strobe ends whole when it is done with it; `home` is
/// the folder the launch is for (its stamp, set by the caller).
pub fn spawn_owned(cmd: &mut Command, cpus: Option<u64>, flags: u32, home: Option<&Path>) -> std::io::Result<(Child, Group)> {
    let home = home.map(home_key);
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        let job = unsafe {
            let j = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            let set = !j.is_null() && SetInformationJobObject(j, JobObjectExtendedLimitInformation, &limits as *const _ as *const _, std::mem::size_of_val(&limits) as u32) != 0;
            if !set && !j.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(j);
            }
            set.then_some(j as isize)
        };
        let mut group = Group { job, home };
        let (child, assigned) = windows(cmd, cpus, flags, job)?;
        if !assigned {
            // strobe's own job may refuse a nested one: the stamp and the tree still find everything.
            if let Some(j) = group.job.take() {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(j as _) };
            }
        }
        if let (Some(j), Some(h)) = (group.job, &group.home) {
            JOBS.lock().unwrap_or_else(|e| e.into_inner()).push((h.clone(), j));
        }
        Ok((child, group))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let _ = flags;
        cmd.process_group(0);
        pin(cmd, cpus);
        let child = cmd.spawn()?;
        let leader = super::start_time(child.id()).map(|s| (child.id(), s));
        Ok((child, Group { leader, home }))
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
