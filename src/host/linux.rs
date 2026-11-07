//! Linux calls behind `host`: `/proc` and `/sys` reads and system calls; nothing spawns.
//!
//! CPU time is exact everywhere it matters: a process's CPU clock (`clock_gettime` on its
//! `CPUCLOCK_SCHED` clock id, every thread live or exited, nanoseconds) and a thread's
//! `/proc/<tid>/schedstat`, never `/proc/<pid>/stat`'s 10 ms ticks. The "cycle" counter of
//! `ProcTimes::cycles` is therefore nanoseconds of CPU, and `host::tsc` is `now_ns`.

use super::{CacheGroup, Core, ProcInfo, ProcTimes, ThreadInfo, Topology};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn clock(id: libc::clockid_t) -> Option<u64> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    (unsafe { libc::clock_gettime(id, &mut ts) } == 0).then(|| ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

pub fn now_ns() -> u64 {
    clock(libc::CLOCK_MONOTONIC).unwrap_or(0)
}

/// CPU time of a whole process (every thread, live or exited) in ns, from its CPU clock:
/// `MAKE_PROCESS_CPUCLOCK(pid, CPUCLOCK_SCHED)`. Reading another process's clock needs no
/// permission (the kernel looks the process up for `clock_gettime` without a check).
/// One process's CPU ns (`super::process_cycles`).
pub fn process_cycles(pid: u32) -> Option<u64> {
    process_cpu_ns(pid)
}

fn process_cpu_ns(pid: u32) -> Option<u64> {
    clock(((!(pid as libc::clockid_t)) << 3) | 2)
}

pub fn thread_cycles() -> u64 {
    clock(libc::CLOCK_THREAD_CPUTIME_ID).unwrap_or(0)
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn tck() -> u64 {
    static T: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *T.get_or_init(|| (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64)
}

fn page() -> u64 {
    static P: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *P.get_or_init(|| (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(1) as u64)
}

/// One `/proc/<pid>/stat` line.
struct Stat {
    pid: u32,
    ppid: u32,
    comm: String,
    cpu_ns: u64,
    threads: u32,
    /// Start time after boot, in clock ticks: a parent's children start no earlier.
    start: u64,
    rss: u64,
    /// Exited (a zombie `Z`, or dead `X`/`x`), kept only until its parent's wait.
    gone: bool,
}

fn stat(pid: u32) -> Option<Stat> {
    let text = read(format!("/proc/{pid}/stat"))?;
    let (open, close) = (text.find('(')?, text.rfind(')')?);
    let comm = text[open + 1..close].to_string();
    let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
    let num = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let gone = matches!(f.first(), Some(&"Z" | &"X" | &"x"));
    Some(Stat { pid, ppid: num(1) as u32, comm, cpu_ns: (num(11) + num(12)) * 1_000_000_000 / tck(), threads: num(17) as u32, start: num(19), rss: num(21) * page(), gone })
}

fn pids() -> Vec<u32> {
    std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

/// A process's name: `comm`, or its executable's file name when `comm` was cut at 15 bytes.
fn name(s: &Stat) -> String {
    if s.comm.len() < 15 {
        return s.comm.clone();
    }
    exe_of(s.pid).and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| s.comm.clone())
}

/// Private memory: resident and swapped pages no other process maps (`smaps_rollup`).
fn private_bytes(pid: u32) -> u64 {
    let Some(text) = read(format!("/proc/{pid}/smaps_rollup")) else { return 0 };
    text.lines()
        .filter(|l| l.starts_with("Private_Clean:") || l.starts_with("Private_Dirty:") || l.starts_with("Swap:"))
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum::<u64>()
        * 1024
}

/// One process's memory by what holds it, from `smaps`: per mapping its resident pages, private ones
/// (clean, dirty and swapped, as `private_bytes` counts) and what backs it.
pub fn memory_map(pid: u32) -> Option<super::MemoryMap> {
    let text = read(format!("/proc/{pid}/smaps"))?;
    // (perms, path, resident, private resident, swapped) per mapping.
    let mut maps: Vec<(String, String, u64, u64, u64)> = Vec::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let first = f.next().unwrap_or("");
        if first.contains('-') && !first.ends_with(':') {
            let perms = f.next().unwrap_or("").to_string();
            let path = f.nth(3).map(|p| line[line.find(p).unwrap_or(0)..].to_string()).unwrap_or_default();
            maps.push((perms, path, 0, 0, 0));
        } else if let Some(m) = maps.last_mut() {
            let kb = f.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
            match first {
                "Rss:" => m.2 += kb,
                "Private_Clean:" | "Private_Dirty:" => m.3 += kb,
                "Swap:" => m.4 += kb,
                _ => {}
            }
        }
    }
    // A file with an executable mapping is a module: its other segments (data, relocations) are too.
    let modules: std::collections::HashSet<&str> = maps.iter().filter(|m| m.0.contains('x') && m.1.starts_with('/')).map(|m| m.1.as_str()).collect();
    let mut out = super::MemoryMap { pid, ..super::MemoryMap::default() };
    let gpu = |p: &str| ["/dev/dri/", "/dev/nvidia", "/dev/kgsl", "/dev/mali", "/dev/accel/"].iter().any(|d| p.starts_with(d));
    let mut private_resident = 0;
    for (perms, path, rss, resident, swap) in &maps {
        out.private += resident + swap;
        out.working_set += rss;
        private_resident += resident;
        let shared_mem = perms.ends_with('s') || ["/dev/shm/", "/memfd:", "/SYSV"].iter().any(|d| path.starts_with(d));
        match () {
            _ if gpu(path) => out.gpu_mapped += rss + swap,
            _ if shared_mem => out.shared += rss + swap,
            _ if modules.contains(path.as_str()) || path.starts_with("[v") => out.image += rss,
            _ if path.starts_with('/') => out.files += rss,
            _ => out.host += resident + swap,
        }
    }
    out.private_working_set = Some(private_resident);
    Some(out)
}

/// Indexes into `all` of the trees of `roots` (each root first, each process once), following only
/// children that started no earlier than their parent (a reused pid). An exited process (a zombie)
/// is in no tree: it runs nothing, and its children have left it.
fn tree_of(all: &[Stat], roots: &[u32]) -> Vec<usize> {
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in all.iter().enumerate() {
        if p.ppid != p.pid && !p.gone {
            children.entry(p.ppid).or_default().push(i);
        }
    }
    let mut seen = vec![false; all.len()];
    let mut out = Vec::new();
    for &root in roots {
        let Some(r) = all.iter().position(|p| p.pid == root && !p.gone) else { continue };
        if std::mem::replace(&mut seen[r], true) {
            continue;
        }
        out.push(r);
        let mut stack = vec![r];
        while let Some(i) = stack.pop() {
            for &j in children.get(&all[i].pid).map(Vec::as_slice).unwrap_or(&[]) {
                if all[j].start >= all[i].start && !std::mem::replace(&mut seen[j], true) {
                    out.push(j);
                    stack.push(j);
                }
            }
        }
    }
    out
}

fn all_stats() -> Vec<Stat> {
    pids().into_iter().filter_map(stat).collect()
}

pub fn process_list() -> Vec<(u32, u32, u64)> {
    all_stats().into_iter().map(|s| (s.pid, s.ppid, s.start)).collect()
}

pub fn process_trees(roots: &super::PickRoots) -> Vec<ProcTimes> {
    let all = all_stats();
    let roots = roots(&all.iter().map(|s| (s.pid, s.ppid, s.start)).collect::<Vec<_>>());
    tree_of(&all, &roots)
        .into_iter()
        .map(|i| {
            let s = &all[i];
            ProcTimes { pid: s.pid, parent: s.ppid, name: name(s), cpu_ns: s.cpu_ns, cycles: process_cpu_ns(s.pid).unwrap_or(0), private_bytes: private_bytes(s.pid), working_set: s.rss }
        })
        .collect()
}

/// The pids of `process_trees`, from the stat lines alone: no process's memory or name read.
pub fn tree_pids_of(roots: &super::PickRoots) -> Vec<u32> {
    let all = all_stats();
    let roots = roots(&all.iter().map(|s| (s.pid, s.ppid, s.start)).collect::<Vec<_>>());
    tree_of(&all, &roots).into_iter().map(|i| all[i].pid).collect()
}

pub fn tree_pids(root: u32) -> Vec<u32> {
    let all = all_stats();
    tree_of(&all, &[root]).into_iter().map(|i| all[i].pid).collect()
}

pub fn all_processes() -> Vec<ProcInfo> {
    all_stats()
        .into_iter()
        .map(|s| {
            // statm: resident − shared is the anonymous resident memory (cheaper than smaps_rollup for every process).
            let private = read(format!("/proc/{}/statm", s.pid))
                .map(|t| {
                    let v: Vec<u64> = t.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    v.get(1).copied().unwrap_or(0).saturating_sub(v.get(2).copied().unwrap_or(0)) * page()
                })
                .unwrap_or(0);
            ProcInfo { pid: s.pid, parent: s.ppid, name: name(&s), cpu_ns: s.cpu_ns, cycles: process_cpu_ns(s.pid).unwrap_or(0), private_bytes: private, threads: s.threads }
        })
        .collect()
}

/// `/proc/<tid>/schedstat`: (ns on a CPU, ns waiting to run, times switched in to run).
fn schedstat(path: &str) -> Option<(u64, u64, u64)> {
    let t = read(path)?;
    let mut it = t.split_whitespace().map(|x| x.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// Every thread of `pids`: times switched in to run (schedstat's run count, the same thing Windows
/// counts as context switches) and exact CPU ns; each process's IO bytes (`rchar`/`wchar`: every
/// byte its reads and writes moved, cached or not, as Windows' transfer counts).
pub fn threads(pids: &[u32]) -> (Vec<ThreadInfo>, Vec<(u32, u64, u64)>) {
    let (mut out, mut io) = (Vec::new(), Vec::new());
    for &pid in pids {
        if let Some(t) = read(format!("/proc/{pid}/io")) {
            let field = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
            io.push((pid, field("rchar:"), field("wchar:")));
        }
        for e in std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
            let Some(tid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let (cpu_ns, switches) = match schedstat(&format!("/proc/{pid}/task/{tid}/schedstat")) {
                Some((run, _, n)) => (run, n),
                None => {
                    let s = read(format!("/proc/{pid}/task/{tid}/status")).unwrap_or_default();
                    let n: u64 = s.lines().filter(|l| l.contains("ctxt_switches:")).filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).sum();
                    (0, n)
                }
            };
            out.push(ThreadInfo { pid, tid, switches: switches as u32, cpu_ns });
        }
    }
    out.sort_by_key(|t| (t.pid, t.tid));
    (out, io)
}

pub fn thread_cycles_of(tid: u32) -> Option<u64> {
    schedstat(&format!("/proc/{tid}/schedstat")).map(|s| s.0)
}

/// The module each thread started in, per tid. Linux keeps no start address: a thread's oldest
/// frames are at the top of its stack (with the C runtime's thread descriptor, which holds the
/// start routine, above them on glibc), so the first code address read down from the top that is
/// not in the C or C++ runtime's thread entry names its module. A process's main thread is its
/// executable's. Needs the thread's stack pointer (`/proc/<pid>/task/<tid>/syscall`) and
/// `process_vm_readv`: ptrace's access rule allows both for processes strobe started (and any of
/// this user's where `ptrace_scope` is 0); others are left unknown, as is a thread on a CPU now.
pub fn thread_modules(threads: &[(u32, u32)]) -> HashMap<u32, String> {
    const TOP: u64 = 32 * 1024;
    let runtime = |file: &str| ["libc.so", "libc-", "ld-linux", "ld-musl", "libpthread", "libstdc++", "libc++", "libgcc_s"].iter().any(|p| file.starts_with(p));
    let mut out = HashMap::new();
    let mut maps: HashMap<u32, Vec<(u64, u64, bool, String)>> = HashMap::new();
    for &(pid, tid) in threads {
        if tid == pid {
            if let Some(exe) = exe_of(pid) {
                out.insert(tid, exe.display().to_string());
            }
            continue;
        }
        let Some(sp) = read(format!("/proc/{pid}/task/{tid}/syscall")).and_then(|s| {
            let f: Vec<&str> = s.split_whitespace().collect();
            (f.len() >= 3).then(|| u64::from_str_radix(f[f.len() - 2].trim_start_matches("0x"), 16).ok()).flatten()
        }) else {
            continue;
        };
        let list = maps.entry(pid).or_insert_with(|| {
            read(format!("/proc/{pid}/maps"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    // `range perms offset dev inode<padding>path`: the path may hold spaces.
                    let mut f = l.splitn(6, ' ');
                    let (range, perms) = (f.next()?, f.next()?);
                    let path = f.nth(3).unwrap_or("").trim_start().to_string();
                    let (s, e) = range.split_once('-')?;
                    Some((u64::from_str_radix(s, 16).ok()?, u64::from_str_radix(e, 16).ok()?, perms.contains('x') && path.starts_with('/'), path))
                })
                .collect()
        });
        let Some(&(_, end, _, _)) = list.iter().find(|m| sp >= m.0 && sp < m.1) else { continue };
        let from = sp.max(end.saturating_sub(TOP)) & !7;
        let mut buf = vec![0u8; (end - from) as usize];
        let local = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
        let remote = libc::iovec { iov_base: from as *mut libc::c_void, iov_len: buf.len() };
        if unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) } != buf.len() as isize {
            continue;
        }
        let module = buf.as_chunks::<8>().0.iter().rev().find_map(|w| {
            let v = u64::from_ne_bytes(*w);
            let m = list.iter().find(|m| m.2 && v >= m.0 && v < m.1)?;
            (!runtime(m.3.rsplit('/').next().unwrap_or(&m.3))).then(|| m.3.clone())
        });
        if let Some(m) = module {
            out.insert(tid, m);
        }
    }
    out
}

pub fn thread_name(tid: u32) -> Option<String> {
    read(format!("/proc/{tid}/comm")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Idle time so far per logical CPU in ns (`/proc/stat`: idle + iowait, in 10 ms ticks).
pub fn idle_cycles(out: &mut Vec<u64>) -> bool {
    out.clear();
    let Some(text) = read("/proc/stat") else { return false };
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else { break };
        // `cpu  <user> …` is the total: its first field is a tick count, not a CPU number.
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let mut f = rest.split_whitespace();
        let Some(Ok(cpu)) = f.next().map(str::parse::<usize>) else { continue };
        let v: Vec<u64> = f.filter_map(|x| x.parse().ok()).collect();
        if out.len() <= cpu {
            out.resize(cpu + 1, 0);
        }
        out[cpu] = (v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0)) * 1_000_000_000 / tck();
    }
    !out.is_empty()
}

/// Hypervisor steal so far per logical CPU in ns (`/proc/stat`'s eighth field, in 10 ms ticks): time
/// the guest's CPU was ready while the hypervisor ran something else. Zero on bare metal, and on a
/// hypervisor that does not report it.
pub fn steal_ns(out: &mut Vec<u64>) -> bool {
    out.clear();
    let Some(text) = read("/proc/stat") else { return false };
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else { break };
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let mut f = rest.split_whitespace();
        let Some(Ok(cpu)) = f.next().map(str::parse::<usize>) else { continue };
        let Some(steal) = f.nth(7).and_then(|x| x.parse::<u64>().ok()) else { continue };
        if out.len() <= cpu {
            out.resize(cpu + 1, 0);
        }
        out[cpu] = steal * 1_000_000_000 / tck();
    }
    !out.is_empty()
}

pub fn system_busy(ms: u64) -> f64 {
    let read_total = || -> Option<(u64, u64)> {
        let t = read("/proc/stat")?;
        let v: Vec<u64> = t.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        let idle = v.get(3)? + v.get(4).copied().unwrap_or(0);
        Some((idle, v.iter().take(8).sum()))
    };
    let Some((i0, t0)) = read_total() else { return 0.0 };
    std::thread::sleep(std::time::Duration::from_millis(ms));
    let Some((i1, t1)) = read_total() else { return 0.0 };
    let total = t1.saturating_sub(t0) as f64;
    if total <= 0.0 { 0.0 } else { 1.0 - i1.saturating_sub(i0) as f64 / total }
}

/// Now in the clock ticks of `/proc/<pid>/stat`'s start time (since boot).
pub(super) fn boot_ticks() -> u64 {
    clock(libc::CLOCK_BOOTTIME).unwrap_or(0) / (1_000_000_000 / tck())
}

/// `0-7,16-23` or a hex mask `ff,000000ff` → a mask of the first 64 CPUs.
pub(super) fn cpu_list(s: &str) -> u64 {
    let mut mask = 0u64;
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
            for c in a..=b.min(63) {
                mask |= 1 << c;
            }
        }
    }
    mask
}

pub fn topology() -> Topology {
    let allowed = super::allowed();
    let all = allowed.mask;
    let mut groups: Vec<CacheGroup> = Vec::new();
    for cpu in super::bits(all) {
        for idx in std::fs::read_dir(format!("/sys/devices/system/cpu/cpu{cpu}/cache")).into_iter().flatten().flatten() {
            let d = idx.path();
            if read(d.join("level")).is_none_or(|l| l.trim() != "3") {
                continue;
            }
            let mask = read(d.join("shared_cpu_list")).map(|s| cpu_list(&s) & all).unwrap_or(0);
            let size = read(d.join("size")).unwrap_or_default();
            let size = size.trim();
            let kb = size.trim_end_matches(['K', 'M']).parse::<u32>().unwrap_or(0) * if size.ends_with('M') { 1024 } else { 1 };
            if mask != 0 && !groups.iter().any(|g| g.mask == mask) {
                groups.push(CacheGroup { mask, cpus: mask.count_ones(), l3_mb: kb / 1024 });
            }
        }
    }
    groups.sort_by_key(|g| g.mask.trailing_zeros());
    Topology { logical: allowed.count, allowed: all, limit: allowed.limit.clone(), groups, cores: cores(all) }
}

/// The CPUs this thread may use now (`sched_getaffinity`): the first 64 as a mask, how many in all,
/// and why the mask cannot name them all. `host::allowed` reads it once, before strobe narrows it.
pub(super) fn allowed() -> super::Allowed {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
        // More CPUs than a cpu_set_t holds: the online ones, with the same limit.
        let online = read("/sys/devices/system/cpu/online").unwrap_or_default();
        let mask = cpu_list(&online);
        return super::Allowed { mask, count: mask.count_ones(), limit: Some("more CPUs than strobe's affinity calls read: its CPU masks name the first 64".into()) };
    }
    let size = libc::CPU_SETSIZE as usize;
    let mask = (0..64).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).fold(0u64, |m, c| m | 1 << c);
    let count = (0..size).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).count() as u32;
    let limit = (count > mask.count_ones()).then(|| format!("strobe may use {count} CPUs, some numbered 64 or more: its CPU masks name the first 64"));
    super::Allowed { mask, count, limit }
}

/// Physical cores of the CPUs in `all`, from each CPU's `thread_siblings_list` (a vCPU without
/// one, or listing only itself, is its own core). Core kinds: Intel hybrid CPUs list their
/// efficiency cores in `/sys/devices/cpu_atom/cpus`; elsewhere (Arm big.LITTLE) each kind has its
/// own `cpu_capacity`.
fn cores(all: u64) -> Vec<Core> {
    let cpu = |c: u32, f: &str| read(format!("/sys/devices/system/cpu/cpu{c}/{f}"));
    let atom = read("/sys/devices/cpu_atom/cpus").map(|s| cpu_list(&s)).filter(|&m| m != 0);
    let mut capacity = [None::<u32>; 64];
    for c in super::bits(all) {
        capacity[c as usize] = cpu(c, "cpu_capacity").and_then(|s| s.trim().parse().ok());
    }
    let mut kinds: Vec<u32> = capacity.iter().flatten().copied().collect();
    kinds.sort_unstable();
    kinds.dedup();
    let class = |c: u32| -> u8 {
        match (atom, capacity[c as usize]) {
            (Some(a), _) => ((a >> c) & 1 == 0) as u8,
            (None, Some(cap)) => kinds.iter().position(|&k| k == cap).unwrap_or(0) as u8,
            (None, None) => 0,
        }
    };
    let mut cores: Vec<Core> = Vec::new();
    for c in super::bits(all) {
        let siblings = cpu(c, "topology/thread_siblings_list").map(|s| cpu_list(&s) & all).filter(|m| (m >> c) & 1 == 1).unwrap_or(1 << c);
        if !cores.iter().any(|k| k.mask & siblings != 0) {
            cores.push(Core { mask: siblings, class: class(c) });
        }
    }
    cores
}

/// (parent, start ticks, name) of a live process.
pub fn parent_start_name(pid: u32) -> Option<(u32, u64, String)> {
    stat(pid).filter(|s| !s.gone).map(|s| (s.ppid, s.start, s.comm))
}

/// Whether `affinity_of`/`set_affinity_of` may act on the process: it runs as this user (its real
/// or effective uid is strobe's effective one), as `sched_setaffinity` requires without CAP_SYS_NICE.
pub(super) fn same_user(pid: u32) -> bool {
    let me = unsafe { libc::geteuid() };
    read(format!("/proc/{pid}/status")).and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:")).map(|u| u.split_whitespace().take(2).any(|x| x.parse::<u32>().ok() == Some(me)))).unwrap_or(false)
}

/// The thread ids of a process.
pub(super) fn tids(pid: u32) -> Vec<u32> {
    std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

/// Every pid in `/proc` (no other read).
pub(super) fn all_pids() -> Vec<u32> {
    pids()
}

pub(super) fn affinity_of(tid: libc::pid_t) -> Option<u64> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
        return None;
    }
    Some((0..64).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).fold(0u64, |m, c| m | 1 << c))
}

pub(super) fn set_affinity_of(tid: libc::pid_t, mask: u64) -> bool {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for c in 0..64 {
        if mask & (1 << c) != 0 {
            unsafe { libc::CPU_SET(c, &mut set) };
        }
    }
    unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 }
}

/// Every thread of this process (children started afterwards inherit it); 0 is every CPU strobe
/// may use (`host::allowed`: `available_parallelism` counts the mask strobe narrowed).
pub fn set_affinity(mask: u64) -> Option<u64> {
    let old = affinity_of(0)?;
    let all = super::allowed().mask;
    let target = if mask == 0 { all } else { mask & all };
    if target == 0 {
        return None;
    }
    let mut ok = false;
    for e in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
        if let Some(tid) = e.file_name().to_str().and_then(|s| s.parse::<libc::pid_t>().ok()) {
            ok |= set_affinity_of(tid, target);
        }
    }
    ok.then_some(old)
}

pub fn set_thread_affinity(mask: u64) {
    set_affinity_of(0, mask);
}

pub fn exe_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Why a process (0: strobe itself) runs at background priority, if it does: `SCHED_IDLE` or nice 19,
/// both inherited by what it starts.
pub fn background_qos(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(if pid == 0 { "/proc/self/stat".to_string() } else { format!("/proc/{pid}/stat") }).ok()?;
    // Fields after the command's closing parenthesis start at field 3 (state): nice is 19, policy 41.
    let f: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let nice: i32 = f.get(16)?.parse().ok()?;
    let policy: u32 = f.get(38).and_then(|p| p.parse().ok()).unwrap_or(0);
    if policy == libc::SCHED_IDLE as u32 {
        Some("SCHED_IDLE".into())
    } else if nice >= 19 {
        Some(format!("nice {nice}"))
    } else {
        None
    }
}

/// A process's environment as it was started (`/proc/<pid>/environ`); None for a process of another
/// user.
pub fn env_of(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(bytes.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect())
}

/// When a running process started (clock ticks after boot), which with its pid names it uniquely.
/// None once it has exited (a zombie, or dead): only its parent's wait keeps the pid.
pub fn start_time(pid: u32) -> Option<u64> {
    let text = read(format!("/proc/{pid}/stat"))?;
    let f: Vec<&str> = text.rsplit_once(')')?.1.split_whitespace().collect();
    if matches!(f.first(), Some(&"Z" | &"X" | &"x")) {
        return None;
    }
    f.get(19)?.parse().ok()
}

/// PIDs of running processes whose executable is exactly `exe`.
pub fn running(exe: &Path) -> Vec<u32> {
    let want = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    pids().into_iter().filter(|&p| exe_of(p).is_some_and(|e| e == want)).collect()
}

pub fn os_name() -> String {
    let pretty = read("/etc/os-release").and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_string())).unwrap_or_else(|| "Linux".into());
    let kernel = read("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default();
    format!("{pretty} · kernel {kernel}")
}

pub fn cpu_name() -> String {
    let info = read("/proc/cpuinfo").unwrap_or_default();
    let field = |k: &str| info.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
    field("model name").or_else(|| field("Hardware")).or_else(|| read("/sys/firmware/devicetree/base/model").map(|s| s.trim_end_matches('\0').to_string())).unwrap_or_else(|| std::env::consts::ARCH.into())
}

/// The CPU frequency policy: cpufreq's governor (and the energy preference and platform profile when
/// the platform has them). Without cpufreq (most virtual machines) the OS sets no frequency policy.
pub fn power_plan() -> String {
    let cpu = "/sys/devices/system/cpu/cpu0/cpufreq";
    let Some(gov) = read(format!("{cpu}/scaling_governor")) else { return "no cpufreq".into() };
    let mut s = format!("governor {}", gov.trim());
    if let Some(epp) = read(format!("{cpu}/energy_performance_preference")) {
        s += &format!(" · {}", epp.trim());
    }
    if let Some(p) = read("/sys/firmware/acpi/platform_profile") {
        s += &format!(" · profile {}", p.trim());
    }
    s
}

pub fn on_battery() -> bool {
    let mut mains = None;
    let mut discharging = false;
    for e in std::fs::read_dir("/sys/class/power_supply").into_iter().flatten().flatten() {
        let p = e.path();
        match read(p.join("type")).as_deref().map(str::trim) {
            Some("Mains") => mains = Some(mains.unwrap_or(false) || read(p.join("online")).is_some_and(|o| o.trim() == "1")),
            Some("Battery") => discharging |= read(p.join("status")).is_some_and(|s| s.trim() == "Discharging"),
            _ => {}
        }
    }
    discharging || mains == Some(false)
}

/// The hypervisor, when the CPU says it runs under one, named from the firmware strings.
pub fn virtual_machine() -> Option<String> {
    let flagged = read("/proc/cpuinfo").is_some_and(|t| t.lines().any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor")));
    let dmi = |f: &str| read(format!("/sys/class/dmi/id/{f}")).map(|s| s.trim().to_string()).unwrap_or_default();
    let text = format!("{} {}", dmi("sys_vendor"), dmi("product_name")).trim().to_string();
    let named = ["QEMU", "KVM", "VMware", "VirtualBox", "Xen", "Parallels", "Virtual Machine", "Bochs", "Google Compute Engine", "Amazon EC2", "Droplet", "OpenStack", "Hetzner"].iter().any(|m| text.contains(m));
    (flagged || named).then(|| if text.is_empty() { "hypervisor".into() } else { text })
}

pub fn on_ctrl_c(flag: &'static std::sync::atomic::AtomicBool) {
    super::unix_ctrl_c(flag);
}

/// The user's graphical session shows its lock screen: logind's `LockedHint` of the user's display
/// session, over the system bus (one connection kept open; the poll is two small messages). False
/// with no graphical session, or a locker that does not tell logind.
pub fn session_locked() -> bool {
    use std::sync::Mutex;
    static BUS: Mutex<Option<super::dbus::Conn>> = Mutex::new(None);
    let mut guard = BUS.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..2 {
        if guard.is_none() {
            *guard = super::dbus::Conn::system().ok();
        }
        let Some(bus) = guard.as_mut() else { return false };
        match locked_hint(bus) {
            Ok(v) => return v,
            Err(_) => *guard = None,
        }
    }
    false
}

/// What `keep_awake` holds: logind's idle inhibitor (a file descriptor: closing it releases it) and
/// the session's screen saver inhibition (a cookie, owned by the connection that took it).
type Inhibitors = (Option<std::os::fd::OwnedFd>, Option<(super::dbus::Conn, u32)>);
static AWAKE: std::sync::Mutex<Inhibitors> = std::sync::Mutex::new((None, None));

/// Keeps the machine from going idle when `system` (logind's `idle` inhibitor in block mode: no idle
/// suspend or idle action, while a person may still suspend it), and the display from blanking as
/// well when `display` (`org.freedesktop.ScreenSaver.Inhibit` on the session bus, where the desktop
/// has it). Released when false; a desktop without either keeps its own idle rules.
pub fn keep_awake(system: bool, display: bool) {
    use super::dbus::{Arg, Conn};
    let mut held = AWAKE.lock().unwrap_or_else(|e| e.into_inner());
    if system && held.0.is_none() {
        held.0 = Conn::system_with_fds().ok().and_then(|mut c| {
            let args = [Arg::Str("idle"), Arg::Str("strobe"), Arg::Str("a measurement is running"), Arg::Str("block")];
            c.call_fds("org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "Inhibit", &args).ok()?.1.into_iter().next()
        });
    } else if !system {
        held.0 = None;
    }
    if display && held.1.is_none() {
        held.1 = Conn::session().ok().and_then(|mut c| {
            let r = c.call("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver", "org.freedesktop.ScreenSaver", "Inhibit", &[Arg::Str("strobe"), Arg::Str("a measurement is running")]).ok()?;
            let cookie = r.first()?.as_i64()? as u32;
            Some((c, cookie))
        });
    } else if !display && let Some((mut c, cookie)) = held.1.take() {
        let _ = c.call("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver", "org.freedesktop.ScreenSaver", "UnInhibit", &[Arg::U32(cookie)]);
    }
}

fn locked_hint(bus: &mut super::dbus::Conn) -> std::io::Result<bool> {
    use super::dbus::Value;
    let display = bus.get_property("org.freedesktop.login1", "/org/freedesktop/login1/user/self", "org.freedesktop.login1.User", "Display")?;
    let path = match display {
        Value::Struct(f) => match f.get(1) {
            Some(Value::Str(p)) => p.clone(),
            _ => return Ok(false),
        },
        _ => return Ok(false),
    };
    if path == "/" {
        return Ok(false);
    }
    Ok(matches!(bus.get_property("org.freedesktop.login1", &path, "org.freedesktop.login1.Session", "LockedHint")?, Value::Bool(true)))
}

/// The active display outputs from the kernel's mode setting (DRM): position in the framebuffer, size
/// and the exact refresh (pixel clock over the mode's totals), with the driver that scans them out.
#[derive(Clone, Debug)]
pub struct Output {
    pub driver: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub hz: f64,
}

impl Output {
    /// A virtual GPU's output (a VM's console, a test device): nothing a person watches presents there.
    pub fn is_virtual(&self) -> bool {
        ["virtio_gpu", "bochs-drm", "bochs", "qxl", "vmwgfx", "cirrus", "simpledrm", "vkms", "hyperv_drm", "vboxvideo"].contains(&self.driver.as_str())
    }
}

#[repr(C)]
#[derive(Default)]
struct CardRes {
    fb_ptr: u64,
    crtc_ptr: u64,
    conn_ptr: u64,
    enc_ptr: u64,
    n_fbs: u32,
    n_crtcs: u32,
    n_conns: u32,
    n_encs: u32,
    min_w: u32,
    max_w: u32,
    min_h: u32,
    max_h: u32,
}

#[repr(C)]
struct ModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    kind: u32,
    name: [u8; 32],
}

#[repr(C)]
struct Crtc {
    set_conns_ptr: u64,
    n_conns: u32,
    id: u32,
    fb: u32,
    x: u32,
    y: u32,
    gamma: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

#[repr(C)]
struct Version {
    major: i32,
    minor: i32,
    patch: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

const _: () = assert!(std::mem::size_of::<CardRes>() == 64 && std::mem::size_of::<Crtc>() == 104);

fn iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | (0x64 << 8) | nr
}

pub fn outputs() -> Vec<Output> {
    use std::os::fd::AsRawFd;
    let mut out = Vec::new();
    for e in std::fs::read_dir("/dev/dri").into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().starts_with("card") {
            continue;
        }
        let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(e.path()).or_else(|_| std::fs::File::open(e.path())) else { continue };
        let fd = f.as_raw_fd();
        let mut name = [0u8; 64];
        let mut v = Version { major: 0, minor: 0, patch: 0, name_len: name.len(), name: name.as_mut_ptr(), date_len: 0, date: std::ptr::null_mut(), desc_len: 0, desc: std::ptr::null_mut() };
        let driver = if unsafe { libc::ioctl(fd, iowr(0x00, std::mem::size_of::<Version>()) as _, &mut v) } == 0 { String::from_utf8_lossy(&name[..v.name_len.min(64)]).to_string() } else { String::new() };
        let mut res = CardRes::default();
        if unsafe { libc::ioctl(fd, iowr(0xA0, 64) as _, &mut res) } != 0 || res.n_crtcs == 0 {
            continue;
        }
        let mut crtcs = vec![0u32; res.n_crtcs as usize];
        let mut res2 = CardRes { crtc_ptr: crtcs.as_mut_ptr() as u64, n_crtcs: res.n_crtcs, ..CardRes::default() };
        if unsafe { libc::ioctl(fd, iowr(0xA0, 64) as _, &mut res2) } != 0 {
            continue;
        }
        for &id in crtcs.iter().take(res2.n_crtcs.min(res.n_crtcs) as usize) {
            let mut c: Crtc = unsafe { std::mem::zeroed() };
            c.id = id;
            if unsafe { libc::ioctl(fd, iowr(0xA1, 104) as _, &mut c) } != 0 || c.mode_valid == 0 {
                continue;
            }
            let m = &c.mode;
            let mut total = m.htotal as f64 * m.vtotal as f64;
            if m.flags & 0x10 != 0 {
                total /= 2.0; // interlaced
            }
            if m.flags & 0x20 != 0 {
                total *= 2.0; // double scan
            }
            let hz = if total > 0.0 { m.clock as f64 * 1000.0 / total } else { m.vrefresh as f64 };
            out.push(Output { driver: driver.clone(), x: c.x as i32, y: c.y as i32, width: m.hdisplay as u32, height: m.vdisplay as u32, hz });
        }
    }
    out
}

/// The refresh of the output containing a framebuffer point, or the only output's.
pub fn refresh_at(x: i32, y: i32) -> Option<u32> {
    let outs = outputs();
    let hit = outs.iter().find(|o| x >= o.x && y >= o.y && x < o.x + o.width as i32 && y < o.y + o.height as i32);
    hit.or(if outs.len() == 1 { outs.first() } else { None }).map(|o| o.hz.round() as u32).filter(|&hz| hz > 1)
}

pub use super::x11::{main_window, place_window, raise_windows, window_pixels, window_state};

/// The output a window shows on, with its refresh (DRM): the one containing the window's centre.
pub fn display_of(hwnd: isize) -> Option<super::DisplayPath> {
    let w = window_state(hwnd)?;
    let (cx, cy) = (w.rect.0 + w.rect.2 / 2, w.rect.1 + w.rect.3 / 2);
    let outs = outputs();
    let o = outs.iter().find(|o| cx >= o.x && cy >= o.y && cx < o.x + o.width as i32 && cy < o.y + o.height as i32).or(if outs.len() == 1 { outs.first() } else { None })?;
    Some(super::DisplayPath { device: format!("{} {}x{}+{}+{}{}", o.driver, o.width, o.height, o.x, o.y, if o.is_virtual() { " (virtual)" } else { "" }), target: 0, adapter_luid: 0, hz: o.hz })
}
