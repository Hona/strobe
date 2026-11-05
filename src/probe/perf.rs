//! Native CPU samples on Linux (attribution blocks only, SPEC rule 3): the kernel's `perf_event_open`
//! in strobe's own process, no `perf` binary.
//!
//! - One software `cpu-clock` event per thread of the target tree (on-CPU time, so a sample is a
//!   thread that was running, as the Windows profile interrupt), timestamps on `CLOCK_MONOTONIC`
//!   (`host::now_ns`). An inherited event cannot have a ring buffer of its own, so threads and
//!   processes that start later are found every 50 ms and sampled from then.
//! - Each sample copies the user registers and the top of the user stack, and strobe unwinds it with
//!   the modules' DWARF unwind tables (`.eh_frame`, `framehop`): release builds rarely keep frame
//!   pointers, which is what a kernel frame-pointer walk would need. Kernel frames come from the
//!   kernel's own callchain when this process may see them.
//! - Needs `perf_event_paranoid` ≤ 2 for the user's own processes (Debian ships 3), or CAP_PERFMON;
//!   kernel frames need ≤ 1 or CAP_PERFMON. `strobe status` names the one setting to change.

use crate::probe::stacks::{Deep, Module, Targets};
use crate::model::{Error, Result};
use framehop::{ExplicitModuleSectionInfo, Unwinder};
use object::{Object, ObjectSection, ObjectSegment};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// 1 ms of a thread's CPU time per sample (1 kHz): each sample carries a stack copy.
pub const INTERVAL_NS: u64 = 1_000_000;
/// User stack bytes copied per sample: deep enough for most stacks; a deeper one ends early.
const STACK_BYTES: u32 = 16 * 1024;

#[cfg(target_arch = "x86_64")]
const REGS: u64 = (1 << 6) | (1 << 7) | (1 << 8); // bp, sp, ip
#[cfg(target_arch = "aarch64")]
const REGS: u64 = (1 << 29) | (1 << 30) | (1 << 31) | (1 << 32); // fp, lr, sp, pc

const KERNEL: u64 = 0xFFFF_8000_0000_0000;

#[repr(C)]
#[derive(Default)]
struct Attr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved2: u16,
    aux_sample_size: u32,
    reserved3: u32,
    sig_data: u64,
    config3: u64,
}

const _: () = assert!(std::mem::size_of::<Attr>() == 136);

const SAMPLE_IP: u64 = 1;
const SAMPLE_TID: u64 = 1 << 1;
const SAMPLE_TIME: u64 = 1 << 2;
const SAMPLE_CALLCHAIN: u64 = 1 << 5;
const SAMPLE_CPU: u64 = 1 << 7;
const SAMPLE_REGS_USER: u64 = 1 << 12;
const SAMPLE_STACK_USER: u64 = 1 << 13;

/// Whether this process may sample its user's processes, and see kernel frames.
pub fn access() -> (bool, bool) {
    let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").ok().and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(2);
    let privileged = unsafe { libc::geteuid() } == 0 || has_perfmon();
    (privileged || paranoid <= 2, privileged || paranoid <= 1)
}

/// CAP_PERFMON (38) or CAP_SYS_ADMIN (21) in this process's effective set.
fn has_perfmon() -> bool {
    let caps = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("CapEff:").map(|v| u64::from_str_radix(v.trim(), 16).unwrap_or(0)))).unwrap_or(0);
    caps & ((1 << 38) | (1 << 21)) != 0
}

/// Why sampling is refused, when it is.
pub fn refusal() -> Option<String> {
    (!access().0).then(|| "native stacks need perf events for your processes: sudo sysctl kernel.perf_event_paranoid=2 (Debian and Ubuntu ship 3), or give strobe CAP_PERFMON once: sudo setcap cap_perfmon+ep $(command -v strobe)".into())
}

struct Ring {
    fd: i32,
    base: *mut u8,
    len: usize,
}

unsafe impl Send for Ring {}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut _, self.len);
            libc::close(self.fd);
        }
    }
}

fn open(tid: u32, kernel: bool, pages: usize) -> std::io::Result<Ring> {
    let mut a = Attr { kind: 1, size: 136, config: 0, sample_period: INTERVAL_NS, ..Attr::default() }; // PERF_TYPE_SOFTWARE, CPU_CLOCK
    a.sample_type = SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_REGS_USER | SAMPLE_STACK_USER | if kernel { SAMPLE_CALLCHAIN } else { 0 };
    // exclude_hv, sample_id_all, exclude_callchain_user, use_clockid; exclude_kernel without access. Enabled at once.
    a.flags = (1 << 6) | (1 << 18) | (1 << 22) | (1 << 25) | if kernel { 0 } else { 1 << 5 };
    a.wakeup = 1;
    a.sample_regs_user = REGS;
    a.sample_stack_user = STACK_BYTES;
    a.clockid = libc::CLOCK_MONOTONIC;
    // PERF_FLAG_FD_CLOEXEC
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &a as *const Attr, tid as libc::pid_t, -1, -1, 8 as libc::c_ulong) } as i32;
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let len = page * (1 + pages);
    let base = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if base == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(Ring { fd, base: base as *mut u8, len })
}

/// One raw sample, before unwinding.
struct Raw {
    ns: u64,
    pid: u32,
    tid: u32,
    cpu: u16,
    kernel: Vec<u64>,
    regs: Vec<u64>,
    stack: Vec<u8>,
}

impl Ring {
    /// Every complete record since the last call; the reader hands the space back to the kernel.
    fn drain(&self, kernel: bool, out: &mut Vec<Raw>, lost: &mut u64) {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let meta = self.base;
        let head = unsafe { std::ptr::read_volatile(meta.add(1024) as *const u64) };
        std::sync::atomic::fence(Ordering::Acquire);
        let tail_ptr = unsafe { meta.add(1032) as *mut u64 };
        let mut tail = unsafe { std::ptr::read_volatile(tail_ptr) };
        let data = unsafe { meta.add(page) };
        let size = (self.len - page) as u64;
        let byte = |off: u64| unsafe { *data.add((off % size) as usize) };
        let read = |off: u64, n: usize| -> Vec<u8> { (0..n as u64).map(|i| byte(off + i)).collect() };
        let u64_at = |off: u64| u64::from_le_bytes(read(off, 8).try_into().unwrap_or_default());
        while tail + 8 <= head {
            let hdr = read(tail, 8);
            let kind = u32::from_le_bytes(hdr[0..4].try_into().unwrap_or_default());
            let len = u16::from_le_bytes(hdr[6..8].try_into().unwrap_or_default()) as u64;
            if len == 0 {
                break;
            }
            match kind {
                9 => {
                    // PERF_RECORD_SAMPLE
                    let body = read(tail + 8, len as usize - 8);
                    if let Some(r) = parse(&body, kernel) {
                        out.push(r);
                    }
                }
                2 => *lost += u64_at(tail + 16), // PERF_RECORD_LOST: id, lost
                _ => {}
            }
            tail += len;
        }
        std::sync::atomic::fence(Ordering::Release);
        unsafe { std::ptr::write_volatile(tail_ptr, tail) };
    }
}

fn parse(b: &[u8], kernel: bool) -> Option<Raw> {
    let mut at = 0usize;
    let mut u64n = || -> Option<u64> {
        let v = u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        Some(v)
    };
    let _ip = u64n()?;
    let pt = u64n()?;
    let ns = u64n()?;
    let cpu = u64n()? as u32 as u16;
    let mut kframes = Vec::new();
    if kernel {
        let n = u64n()? as usize;
        for _ in 0..n.min(512) {
            let f = u64n()?;
            // Context markers (PERF_CONTEXT_*) are the top 4095 values.
            if (KERNEL..0xFFFF_FFFF_FFFF_F000).contains(&f) {
                kframes.push(f);
            }
        }
    }
    let abi = u64n()?;
    let mut regs = Vec::new();
    if abi != 0 {
        for _ in 0..REGS.count_ones() {
            regs.push(u64n()?);
        }
    }
    let size = u64n()? as usize;
    let stack = if size > 0 {
        let s = b.get(at..at + size)?.to_vec();
        at += size;
        let dyn_size = u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?) as usize;
        s[..dyn_size.min(size)].to_vec()
    } else {
        Vec::new()
    };
    Some(Raw { ns, pid: (pt & 0xffff_ffff) as u32, tid: (pt >> 32) as u32, cpu, kernel: kframes, regs, stack })
}

/// One executable mapping of a file in a process.
#[derive(Clone)]
struct Map {
    start: u64,
    end: u64,
    offset: u64,
    path: String,
}

fn maps(pid: u32) -> Vec<Map> {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (range, perms, offset) = (f.next()?, f.next()?, f.next()?);
            let path = f.nth(2).map(str::to_string).unwrap_or_default();
            if !path.starts_with('/') {
                return None;
            }
            let (s, e) = range.split_once('-')?;
            let _ = perms;
            Some(Map { start: u64::from_str_radix(s, 16).ok()?, end: u64::from_str_radix(e, 16).ok()?, offset: u64::from_str_radix(offset, 16).ok()?, path })
        })
        .collect()
}

/// The modules of a process as recordings keep them: one per file, from its lowest mapping (file
/// offset 0, so an address minus `base` is an offset from the file's first segment) to its end.
fn modules_of(pid: u32, list: &[Map]) -> Vec<Module> {
    let mut by: HashMap<&str, (u64, u64)> = HashMap::new();
    for m in list {
        let e = by.entry(&m.path).or_insert((u64::MAX, 0));
        if m.offset == 0 {
            e.0 = e.0.min(m.start);
        }
        e.1 = e.1.max(m.end);
    }
    by.into_iter().filter(|(_, (b, e))| *b != u64::MAX && e > b).map(|(p, (b, e))| Module { pid, base: b, size: e - b, path: p.to_string() }).collect()
}

type Data = Arc<[u8]>;

type Section = Option<(std::ops::Range<u64>, Data)>;

/// A file's unwind sections, loaded once per path: its first segment's address and the sections.
#[derive(Clone)]
struct Sections {
    base: u64,
    text: Section,
    eh_frame: Section,
    eh_frame_hdr: Section,
    debug_frame: Section,
    got: Section,
}

fn sections(path: &str) -> Option<Sections> {
    let bytes = std::fs::read(path).ok()?;
    let obj = object::File::parse(&*bytes).ok()?;
    let base = obj.segments().map(|s| s.address()).min().unwrap_or(0);
    let sect = |name: &str| obj.section_by_name(name).and_then(|s| Some((s.address()..s.address() + s.size(), Data::from(s.data().ok()?))));
    Some(Sections { base, text: sect(".text"), eh_frame: sect(".eh_frame"), eh_frame_hdr: sect(".eh_frame_hdr"), debug_frame: sect(".debug_frame"), got: sect(".got") })
}

impl Sections {
    fn info(&self) -> ExplicitModuleSectionInfo<Data> {
        let (text_svma, text) = self.text.clone().unzip();
        let (eh_frame_svma, eh_frame) = self.eh_frame.clone().unzip();
        let (eh_frame_hdr_svma, eh_frame_hdr) = self.eh_frame_hdr.clone().unzip();
        ExplicitModuleSectionInfo {
            base_svma: self.base,
            text_svma,
            text,
            eh_frame_svma,
            eh_frame,
            eh_frame_hdr_svma,
            eh_frame_hdr,
            debug_frame: self.debug_frame.clone().map(|d| d.1),
            got_svma: self.got.clone().map(|g| g.0),
            ..Default::default()
        }
    }
}

/// One process's address space for unwinding.
struct Space {
    unwinder: framehop::UnwinderNative<Data, framehop::MayAllocateDuringUnwind>,
    maps: Vec<Map>,
    added: HashSet<String>,
    refreshed: std::time::Instant,
}

struct Unwind {
    spaces: HashMap<u32, Space>,
    files: HashMap<String, Option<Sections>>,
    cache: framehop::CacheNative<framehop::MayAllocateDuringUnwind>,
}

impl Unwind {
    /// Makes sure `pid`'s modules are known, re-reading its maps when `ip` is in none of them.
    fn refresh(&mut self, pid: u32, ip: u64) {
        let fresh = !self.spaces.contains_key(&pid);
        let stale = self.spaces.get(&pid).is_some_and(|s| !s.maps.iter().any(|m| ip >= m.start && ip < m.end) && s.refreshed.elapsed().as_millis() > 200);
        if fresh || stale {
            let list = maps(pid);
            let s = self.spaces.entry(pid).or_insert_with(|| Space { unwinder: framehop::UnwinderNative::new(), maps: Vec::new(), added: HashSet::new(), refreshed: std::time::Instant::now() });
            for m in modules_of(pid, &list) {
                if s.added.contains(&m.path) {
                    continue;
                }
                // The file's first segment (`base_svma`) sits at the module's base.
                if let Some(sec) = self.files.entry(m.path.clone()).or_insert_with(|| sections(&m.path)) {
                    s.unwinder.add_module(framehop::Module::new(m.path.clone(), m.base..m.base + m.size, m.base, sec.info()));
                }
                s.added.insert(m.path);
            }
            s.maps = list;
            s.refreshed = std::time::Instant::now();
        }
    }

    /// Return addresses, innermost first (the sampled instruction first).
    fn frames(&mut self, r: &Raw) -> Vec<u64> {
        if r.regs.len() != REGS.count_ones() as usize {
            return Vec::new();
        }
        #[cfg(target_arch = "x86_64")]
        let (pc, regs, sp) = (r.regs[2], framehop::x86_64::UnwindRegsX86_64::new(r.regs[2], r.regs[1], r.regs[0]), r.regs[1]);
        #[cfg(target_arch = "aarch64")]
        let (pc, regs, sp) = (r.regs[3], framehop::aarch64::UnwindRegsAarch64::new(r.regs[1], r.regs[2], r.regs[0]), r.regs[2]);
        let stack = &r.stack;
        let mut read = |addr: u64| -> std::result::Result<u64, ()> {
            let off = addr.checked_sub(sp).ok_or(())? as usize;
            stack.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default())).ok_or(())
        };
        self.refresh(r.pid, pc);
        let Some(space) = self.spaces.get_mut(&r.pid) else { return Vec::new() };
        let cache = &mut self.cache;
        let mut out = Vec::new();
        let mut it = space.unwinder.iter_frames(pc, regs, cache, &mut read);
        while let Ok(Some(f)) = it.next() {
            out.push(f.address());
            if out.len() >= 256 {
                break;
            }
        }
        out
    }
}

/// Opens a sampling event on every thread of the target's process set that has none yet.
fn follow(targets: &Targets, kernel: bool, rings: &mut Vec<Ring>, opened: &mut HashSet<u32>) -> std::io::Result<()> {
    for pid in targets.pids() {
        for e in std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
            let Some(tid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if !opened.insert(tid) {
                continue;
            }
            // A big buffer first; the unprivileged mlock budget (perf_event_mlock_kb) may only allow small ones.
            match open(tid, kernel, 128).or_else(|_| open(tid, kernel, 16)) {
                Ok(r) => rings.push(r),
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// A running sampler: the reader thread drains every ring and unwinds as samples arrive. It owns
/// the rings, so its end closes every perf event; a capture dropped without `stop` ends it too.
pub struct Capture {
    /// RUN, then STOP (drain the rings and make the result) or DROP (end at once, nothing kept).
    state: Arc<AtomicU8>,
    thread: Option<std::thread::JoinHandle<Result<Deep>>>,
}

/// How long `stop` waits for the reader to unwind what the rings still hold and name the threads.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const RUN: u8 = 0;
const STOP: u8 = 1;
const DROP: u8 = 2;

impl Capture {
    pub fn start(targets: Targets) -> Result<Capture> {
        if let Some(why) = refusal() {
            return Err(Error::msg(why));
        }
        let (_, kernel) = access();
        if targets.pids().is_empty() {
            return Err(Error::msg(format!("process {} is not running", targets.root)));
        }
        let mut rings: Vec<Ring> = Vec::new();
        let mut opened: HashSet<u32> = HashSet::new();
        if let Err(e) = follow(&targets, kernel, &mut rings, &mut opened) {
            return Err(Error::msg(format!("perf_event_open: {e}")));
        }
        let state = Arc::new(AtomicU8::new(RUN));
        let flag = state.clone();
        let t0 = crate::host::now_ns();
        let thread = std::thread::Builder::new()
            .name("strobe-perf".into())
            .spawn(move || {
                let mut unwind = Unwind { spaces: HashMap::new(), files: HashMap::new(), cache: framehop::CacheNative::new() };
                let mut raw = Vec::new();
                let mut lost = 0u64;
                let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
                let mut deep = Deep { interval_us: INTERVAL_NS as f64 / 1e3, t0_ns: t0, ..Deep::default() };
                let mut seen_pids: HashSet<u32> = HashSet::new();
                let mut followed = std::time::Instant::now();
                loop {
                    let done = match flag.load(Ordering::Relaxed) {
                        RUN => false,
                        STOP => true,
                        _ => return Err(Error::msg("the capture was dropped")),
                    };
                    if followed.elapsed().as_millis() >= 50 {
                        let _ = follow(&targets, kernel, &mut rings, &mut opened);
                        followed = std::time::Instant::now();
                    }
                    let mut poll: Vec<libc::pollfd> = rings.iter().map(|r| libc::pollfd { fd: r.fd, events: libc::POLLIN, revents: 0 }).collect();
                    unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, 10) };
                    for r in &rings {
                        r.drain(kernel, &mut raw, &mut lost);
                    }
                    for r in raw.drain(..) {
                        if flag.load(Ordering::Relaxed) == DROP {
                            return Err(Error::msg("the capture was dropped"));
                        }
                        let mut frames = r.kernel.clone();
                        frames.extend(unwind.frames(&r));
                        if frames.is_empty() {
                            continue;
                        }
                        seen_pids.insert(r.pid);
                        let next = deep.stacks.len() as u32;
                        let at = *index.entry(frames.clone()).or_insert_with(|| {
                            deep.stacks.push(frames);
                            next
                        });
                        deep.samples.push((r.ns, r.pid, r.tid, r.cpu, at));
                    }
                    if done {
                        break;
                    }
                }
                drop(rings);
                deep.t1_ns = crate::host::now_ns();
                deep.events_lost = lost;
                let mut pids: Vec<u32> = seen_pids.into_iter().collect();
                pids.sort_unstable();
                for &pid in &pids {
                    let list = unwind.spaces.get(&pid).map(|s| s.maps.clone()).unwrap_or_else(|| maps(pid));
                    deep.modules.extend(modules_of(pid, &list));
                }
                if kernel && deep.stacks.iter().any(|s| s.first().is_some_and(|f| *f >= KERNEL)) {
                    deep.modules.push(Module { pid: 0, base: KERNEL, size: u64::MAX - KERNEL, path: "[kernel.kallsyms]".into() });
                }
                let tids: HashSet<u32> = deep.samples.iter().map(|s| s.2).collect();
                for tid in tids {
                    if let Some(n) = crate::host::thread_name(tid) {
                        deep.threads.insert(tid, n);
                    }
                }
                deep.pids = pids;
                Ok(deep)
            })
            .map_err(|e| Error::msg(format!("start the sampler thread: {e}")))?;
        Ok(Capture { state, thread: Some(thread) })
    }

    pub fn stop(mut self) -> Result<Deep> {
        self.state.store(STOP, Ordering::Relaxed);
        let thread = self.thread.take().ok_or_else(|| Error::msg("no sampler thread"))?;
        super::join_within(thread, STOP_WAIT).ok_or_else(|| Error::msg(format!("the sampler thread did not end within {} s", STOP_WAIT.as_secs())))?
    }
}

/// Dropped without `stop`: the reader ends at its next ring poll (10 ms) or sample, dropping the
/// rings (every perf event and mapping) without unwinding what they still hold.
impl Drop for Capture {
    fn drop(&mut self) {
        self.state.store(DROP, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            super::join_within(t, std::time::Duration::from_secs(2));
        }
    }
}

// Off-CPU waits (SPEC §11.1 `waits`, attribution only): the scheduler's own tracepoints.
//
// - Each thread of the target: `sched:sched_switch`, which fires in the thread that leaves its CPU,
//   sampled with its user registers and stack (where it waited) and the task state it left in, and
//   the event's context-switch records, whose switch-ins end each wait.
// - Each CPU: `sched:sched_waking`, filtered on the target's threads, which fires in whoever wakes one
//   (its stack: where it released the lock or set the event), or in an interrupt (a timer, a device).
// - Tracepoint samples with their raw fields need CAP_PERFMON (or root, or perf_event_paranoid -1),
//   and the event ids and field offsets come from tracefs.

use crate::probe::stacks::{Wait, WaitKind, Waits, Waker};
use crate::model::Refusal;

const SAMPLE_RAW: u64 = 1 << 10;
const RECORD_SWITCH: u32 = 14;
const MISC_SWITCH_OUT: u16 = 1 << 13;
/// `PERF_EVENT_IOC_SET_FILTER`: `_IOW('$', 6, char *)`.
const SET_FILTER: libc::c_ulong = 0x4008_2406;
/// Wake-up filters longer than this go unfiltered (and are filtered as they arrive).
const FILTER_MAX: usize = 3000;

/// A tracepoint's id and its fields' (offset, size) in its raw record.
struct Format {
    id: u64,
    fields: HashMap<String, (usize, usize)>,
}

fn format(event: &str) -> Option<Format> {
    let dir = ["/sys/kernel/tracing/events/sched", "/sys/kernel/debug/tracing/events/sched"].iter().map(|d| std::path::Path::new(d).join(event)).find(|d| d.join("id").is_file())?;
    let id = std::fs::read_to_string(dir.join("id")).ok()?.trim().parse().ok()?;
    let text = std::fs::read_to_string(dir.join("format")).ok()?;
    let mut fields = HashMap::new();
    for l in text.lines() {
        // `field:pid_t prev_pid;	offset:24;	size:4;	signed:1;`
        let Some(decl) = l.trim().strip_prefix("field:") else { continue };
        let mut parts = decl.split(';').map(str::trim);
        let name = parts.next().and_then(|d| d.split_whitespace().last()).map(|n| n.split('[').next().unwrap_or(n).to_string());
        let num = |key: &str, p: &str| p.strip_prefix(key).and_then(|v| v.parse::<usize>().ok());
        let rest: Vec<&str> = parts.collect();
        let off = rest.iter().find_map(|p| num("offset:", p));
        let size = rest.iter().find_map(|p| num("size:", p));
        if let (Some(n), Some(o), Some(s)) = (name, off, size) {
            fields.insert(n, (o, s));
        }
    }
    Some(Format { id, fields })
}

impl Format {
    fn read(&self, raw: &[u8], field: &str) -> Option<u64> {
        let &(off, size) = self.fields.get(field)?;
        let b = raw.get(off..off + size)?;
        Some(match size {
            1 => b[0] as u64,
            2 => u16::from_le_bytes([b[0], b[1]]) as u64,
            4 => u32::from_le_bytes(b.try_into().ok()?) as u64,
            8 => u64::from_le_bytes(b.try_into().ok()?),
            _ => return None,
        })
    }
}

/// Why waits cannot be read here, when they cannot.
fn waits_refusal() -> Option<Refusal> {
    let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").ok().and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(2);
    let privileged = unsafe { libc::geteuid() } == 0 || has_perfmon();
    if !privileged && paranoid > -1 {
        return Some(crate::probe::stacks::no_waits(
            "off-CPU waits need the scheduler's tracepoints (sched_switch, sched_waking) with their raw fields, which perf gives only with CAP_PERFMON or root (or perf_event_paranoid -1)",
            "give strobe CAP_PERFMON once: sudo setcap cap_perfmon+ep $(command -v strobe)",
        ));
    }
    None
}

/// A tracepoint sampled on every hit: `pid` and `cpu` as `perf_event_open` takes them. `switches`
/// adds the event's context-switch records (per-thread events only).
fn open_tracepoint(config: u64, pid: i32, cpu: i32, switches: bool, pages: usize) -> std::io::Result<Ring> {
    let mut a = Attr { kind: 2, size: 136, config, sample_period: 1, ..Attr::default() }; // PERF_TYPE_TRACEPOINT
    a.sample_type = SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_RAW | SAMPLE_REGS_USER | SAMPLE_STACK_USER;
    // exclude_hv, sample_id_all, use_clockid; per thread the context-switch records, per CPU disabled
    // until its filter is set (unfiltered, it samples every wake-up on the machine).
    a.flags = (1 << 6) | (1 << 18) | (1 << 25) | if switches { 1 << 26 } else { 1 };
    a.wakeup = 1;
    a.sample_regs_user = REGS;
    a.sample_stack_user = STACK_BYTES;
    a.clockid = libc::CLOCK_MONOTONIC;
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &a as *const Attr, pid as libc::pid_t, cpu, -1, 8 as libc::c_ulong) } as i32;
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let len = page * (1 + pages);
    let base = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if base == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(Ring { fd, base: base as *mut u8, len })
}

impl Ring {
    /// Every complete record since the last call as (type, misc, body), handed back to the kernel.
    fn records(&self, out: &mut Vec<(u32, u16, Vec<u8>)>, lost: &mut u64) {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let meta = self.base;
        let head = unsafe { std::ptr::read_volatile(meta.add(1024) as *const u64) };
        std::sync::atomic::fence(Ordering::Acquire);
        let tail_ptr = unsafe { meta.add(1032) as *mut u64 };
        let mut tail = unsafe { std::ptr::read_volatile(tail_ptr) };
        let data = unsafe { meta.add(page) };
        let size = (self.len - page) as u64;
        let read = |off: u64, n: usize| -> Vec<u8> { (0..n as u64).map(|i| unsafe { *data.add(((off + i) % size) as usize) }).collect() };
        while tail + 8 <= head {
            let hdr = read(tail, 8);
            let kind = u32::from_le_bytes(hdr[0..4].try_into().unwrap_or_default());
            let misc = u16::from_le_bytes(hdr[4..6].try_into().unwrap_or_default());
            let len = u16::from_le_bytes(hdr[6..8].try_into().unwrap_or_default()) as u64;
            if len == 0 {
                break;
            }
            let body = read(tail + 8, len as usize - 8);
            if kind == 2 {
                *lost += body.get(8..16).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()));
            } else {
                out.push((kind, misc, body));
            }
            tail += len;
        }
        std::sync::atomic::fence(Ordering::Release);
        unsafe { std::ptr::write_volatile(tail_ptr, tail) };
    }
}

/// A tracepoint sample of `open_tracepoint`'s layout: the thread, its time and CPU, the raw record,
/// and its user registers and stack.
fn parse_tracepoint(b: &[u8]) -> Option<(Raw, Vec<u8>)> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(at..at + n)?;
        at += n;
        Some(s)
    };
    let u64_of = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap_or_default());
    let pt = u64_of(take(8)?);
    let ns = u64_of(take(8)?);
    let cpu = u64_of(take(8)?) as u32 as u16;
    let raw_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let raw = take(raw_len)?.to_vec();
    let abi = u64_of(take(8)?);
    let mut regs = Vec::new();
    if abi != 0 {
        for _ in 0..REGS.count_ones() {
            regs.push(u64_of(take(8)?));
        }
    }
    let size = u64_of(take(8)?) as usize;
    let stack = if size > 0 {
        let s = take(size)?.to_vec();
        let dyn_size = u64_of(take(8)?) as usize;
        s[..dyn_size.min(size)].to_vec()
    } else {
        Vec::new()
    };
    Some((Raw { ns, pid: (pt & 0xffff_ffff) as u32, tid: (pt >> 32) as u32, cpu, kernel: Vec::new(), regs, stack }, raw))
}

/// The online CPUs (`/sys/devices/system/cpu/online`: `0-3,6`).
fn online_cpus() -> Vec<i32> {
    let text = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => out.extend(a.parse::<i32>().unwrap_or(0)..=b.parse::<i32>().unwrap_or(-1)),
            None => out.extend(part.parse::<i32>().ok()),
        }
    }
    out
}

/// A running waits capture: its thread follows the target's threads, drains the rings and unwinds as
/// events arrive. It owns every event and ring, so its end (`stop`, or a drop) closes them.
pub struct WaitCapture {
    state: Arc<AtomicU8>,
    thread: Option<std::thread::JoinHandle<Result<Waits>>>,
}

impl WaitCapture {
    pub fn start(targets: Targets) -> std::result::Result<WaitCapture, Refusal> {
        if let Some(r) = waits_refusal() {
            return Err(r);
        }
        let (Some(switch), Some(waking)) = (format("sched_switch"), format("sched_waking")) else {
            return Err(crate::probe::stacks::no_waits(
                "off-CPU waits need tracefs (the scheduler's event ids and fields under /sys/kernel/tracing), which strobe could not read",
                "mount tracefs (mount -t tracefs nodev /sys/kernel/tracing) and run strobe as a user that may read it",
            ));
        };
        let cpus = online_cpus();
        let mut wakers: Vec<Ring> = Vec::new();
        for &c in &cpus {
            match open_tracepoint(waking.id, -1, c, false, 64) {
                Ok(r) => wakers.push(r),
                Err(e) => return Err(crate::probe::stacks::no_waits(format!("sched_waking on CPU {c}: {e}"), "give strobe CAP_PERFMON (or run it as root)")),
            }
        }
        let state = Arc::new(AtomicU8::new(RUN));
        let flag = state.clone();
        let thread = std::thread::Builder::new()
            .name("strobe-waits".into())
            .spawn(move || waits_loop(targets, switch, waking, wakers, flag))
            .map_err(|e| crate::probe::stacks::no_waits(format!("start the waits thread: {e}"), "try again"))?;
        Ok(WaitCapture { state, thread: Some(thread) })
    }

    pub fn stop(mut self) -> std::result::Result<Waits, Refusal> {
        self.state.store(STOP, Ordering::Relaxed);
        let thread = self.thread.take().ok_or_else(|| crate::probe::stacks::no_waits("no waits thread", "try again"))?;
        match super::join_within(thread, STOP_WAIT) {
            Some(Ok(w)) => Ok(w),
            Some(Err(e)) => Err(crate::probe::stacks::no_waits(format!("the waits capture failed: {}", e.message), "try again")),
            None => Err(crate::probe::stacks::no_waits(format!("the waits thread did not end within {} s", STOP_WAIT.as_secs()), "try again")),
        }
    }
}

impl Drop for WaitCapture {
    fn drop(&mut self) {
        self.state.store(DROP, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            super::join_within(t, std::time::Duration::from_secs(2));
        }
    }
}

/// Linux task states as `sched_switch` reports the one a thread left its CPU in.
fn task_state(state: u64) -> (WaitKind, &'static str) {
    // Runnable (0), or the preempted marker the kernel adds above the states (`R+`).
    match state & 0x7f {
        0 => (WaitKind::Preempted, "R"),
        s if s & 2 != 0 => (WaitKind::Io, "D"),
        s if s & 1 != 0 => (WaitKind::Blocked, "S"),
        _ => (WaitKind::Other, "other"),
    }
}

fn waits_loop(targets: Targets, switch: Format, waking: Format, wakers: Vec<Ring>, flag: Arc<AtomicU8>) -> Result<Waits> {
    let mut unwind = Unwind { spaces: HashMap::new(), files: HashMap::new(), cache: framehop::CacheNative::new() };
    let mut rings: HashMap<u32, Ring> = HashMap::new();
    let mut out = Waits::default();
    let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
    let mut open: HashMap<u32, (u64, u32, WaitKind, &'static str, Option<u32>)> = HashMap::new();
    let mut woken: Vec<(u32, u64, Waker)> = Vec::new();
    let mut lost = 0u64;
    let mut filtered: Vec<u32> = Vec::new();
    let mut unfiltered = false;
    let mut followed: Option<std::time::Instant> = None;
    let mut buf = Vec::new();
    loop {
        let done = match flag.load(Ordering::Relaxed) {
            RUN => false,
            STOP => true,
            _ => return Err(Error::msg("the capture was dropped")),
        };
        if followed.is_none_or(|t| t.elapsed().as_millis() >= 50) && !done {
            for pid in targets.pids() {
                for tid in tids(pid) {
                    if let std::collections::hash_map::Entry::Vacant(e) = rings.entry(tid) {
                        match open_tracepoint(switch.id, tid as i32, -1, true, 64) {
                            Ok(r) => {
                                e.insert(r);
                            }
                            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {}
                            Err(err) => return Err(Error::msg(format!("sched_switch on thread {tid}: {err}"))),
                        }
                    }
                }
            }
            // The wake-up filter names the threads being followed.
            let mut now: Vec<u32> = rings.keys().copied().collect();
            now.sort_unstable();
            if now != filtered {
                let text = now.iter().map(|t| format!("pid == {t}")).collect::<Vec<_>>().join(" || ");
                unfiltered = text.len() > FILTER_MAX || text.is_empty();
                let c = std::ffi::CString::new(if unfiltered { "pid != 0".to_string() } else { text }).unwrap_or_default();
                for r in &wakers {
                    unsafe {
                        libc::ioctl(r.fd, SET_FILTER as _, c.as_ptr());
                        // PERF_EVENT_IOC_ENABLE: on from its first filter.
                        libc::ioctl(r.fd, 0x2400 as _, 0);
                    }
                }
                filtered = now;
            }
            followed = Some(std::time::Instant::now());
        }
        let mut poll: Vec<libc::pollfd> = rings.values().chain(wakers.iter()).map(|r| libc::pollfd { fd: r.fd, events: libc::POLLIN, revents: 0 }).collect();
        unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, 10) };
        for r in rings.values() {
            r.records(&mut buf, &mut lost);
            for (kind, misc, body) in buf.drain(..) {
                if kind == 9 {
                    let Some((raw, fields)) = parse_tracepoint(&body) else { continue };
                    let (kind, word) = task_state(switch.read(&fields, "prev_state").unwrap_or(0));
                    let frames = unwind.frames(&raw);
                    let stack = out.stack(&mut index, frames);
                    open.insert(raw.tid, (raw.ns, raw.pid, kind, word, stack));
                } else if kind == RECORD_SWITCH && misc & MISC_SWITCH_OUT == 0 {
                    // A switch-in: sample_id_all's pid, tid and time.
                    let (Some(pt), Some(ns)) = (body.get(0..8), body.get(8..16)) else { continue };
                    let tid = (u64::from_le_bytes(pt.try_into().unwrap_or_default()) >> 32) as u32;
                    let ns = u64::from_le_bytes(ns.try_into().unwrap_or_default());
                    if let Some((from, pid, kind, word, stack)) = open.remove(&tid).filter(|o| o.0 <= ns) {
                        out.waits.push(Wait { pid, tid, from_ns: from, to_ns: ns, ready_ns: None, kind, reason: word.to_string(), stack, waker: None });
                    }
                }
            }
        }
        for r in &wakers {
            r.records(&mut buf, &mut lost);
            for (kind, _, body) in buf.drain(..) {
                if kind != 9 {
                    continue;
                }
                let Some((raw, fields)) = parse_tracepoint(&body) else { continue };
                let Some(wakee) = waking.read(&fields, "pid").map(|p| p as u32) else { continue };
                if unfiltered && !rings.contains_key(&wakee) {
                    continue;
                }
                // In a hard or soft interrupt (a timer, a device), or the idle task: no thread woke it.
                let irq = waking.read(&fields, "common_flags").is_some_and(|f| f & (0x08 | 0x10) != 0) || raw.tid == 0;
                let stack = if irq { None } else { out.stack(&mut index, unwind.frames(&raw)) };
                woken.push((wakee, raw.ns, Waker { pid: raw.pid, tid: raw.tid, interrupt: irq, stack }));
            }
        }
        if done {
            break;
        }
    }
    drop(rings);
    drop(wakers);
    // Each wait's waker: the last wake-up of its thread inside it.
    woken.sort_by_key(|w| (w.0, w.1));
    for w in &mut out.waits {
        let lo = woken.partition_point(|x| (x.0, x.1) < (w.tid, w.from_ns));
        let hi = woken.partition_point(|x| (x.0, x.1) <= (w.tid, w.to_ns));
        if let Some((_, at, k)) = woken.get(lo..hi).and_then(|s| s.last()) {
            w.ready_ns = Some(*at);
            w.waker = Some(k.clone());
        }
    }
    let mut pids: Vec<u32> = out.waits.iter().flat_map(|w| std::iter::once(w.pid).chain(w.waker.as_ref().filter(|k| !k.interrupt).map(|k| k.pid))).collect();
    pids.sort_unstable();
    pids.dedup();
    for &pid in &pids {
        let list = unwind.spaces.get(&pid).map(|s| s.maps.clone()).unwrap_or_else(|| maps(pid));
        out.modules.extend(modules_of(pid, &list));
    }
    let mut named: Vec<u32> = out.waits.iter().flat_map(|w| std::iter::once(w.tid).chain(w.waker.as_ref().filter(|k| !k.interrupt).map(|k| k.tid))).collect();
    named.sort_unstable();
    named.dedup();
    for tid in named {
        if let Some(n) = crate::host::thread_name(tid) {
            out.threads.insert(tid, n);
        }
    }
    out.events_lost = lost;
    Ok(out)
}

// Counting (SPEC §7.5 `cpu.instructions`): no samples, no interrupts, one counter per thread.

use crate::probe::wakes::Uncounted;
use std::sync::Mutex;

/// One process's instruction counters: a counting event (user-mode instructions) with `inherit` on
/// each thread it had when strobe first saw it, so the threads and processes those threads start
/// later count in them too, live or exited. The events close with the last snapshot holding it.
#[derive(Debug)]
pub struct Group {
    pub pid: u32,
    pub name: String,
    fds: Vec<i32>,
}

impl Drop for Group {
    fn drop(&mut self) {
        for &fd in &self.fds {
            unsafe { libc::close(fd) };
        }
    }
}

impl Group {
    /// (instructions, ns enabled, ns running) so far, over every event and what it inherited.
    pub fn read(&self) -> Option<(u64, u64, u64)> {
        let mut total = (0u64, 0u64, 0u64);
        for &fd in &self.fds {
            let mut v = [0u64; 3];
            if unsafe { libc::read(fd, v.as_mut_ptr() as *mut libc::c_void, 24) } != 24 {
                return None;
            }
            total = (total.0 + v[0], total.1 + v[1], total.2 + v[2]);
        }
        Some(total)
    }
}

enum Counting {
    /// Its own events.
    Own(Arc<Group>),
    /// Started by a counted thread after its events opened: it counts in that group.
    In(Arc<Group>),
    Failed(Uncounted),
}

struct Registered {
    /// Start time (clock ticks after boot), which with the pid names the process.
    start: u64,
    /// The processes (pid, start) alive before the call that registered it opened its groups, and
    /// once they had. For a process with its own events: a child in the first did not inherit
    /// them, one in neither did, and one only in the second started while they opened, so nobody
    /// can tell.
    before: Arc<HashSet<(u32, u64)>>,
    after: Arc<HashSet<(u32, u64)>>,
    counting: Counting,
}

/// The perf events of one block or recording (SPEC §7.5): every process it counts, by pid, so the
/// snapshots of its runs share one group per process (never a thread twice) and a group keeps
/// counting between runs. Dropped with the block: every event it opened closes once no snapshot
/// holds its group, and nothing stays open on a target after strobe is done with it.
#[derive(Default)]
pub struct Groups(Mutex<HashMap<u32, Registered>>);

/// Every live process now, as (pid, start).
fn alive() -> HashSet<(u32, u64)> {
    crate::host::linux::process_list().into_iter().map(|(pid, _, start)| (pid, start)).collect()
}

/// A counting event on one thread: PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS, read with the
/// times enabled and running, inherited, user mode only (what `perf_event_paranoid` 2 allows, the
/// same everywhere), enabled at once.
fn open_counter(tid: u32) -> std::io::Result<i32> {
    let a = Attr { kind: 0, size: 136, config: 1, read_format: 1 | 2, flags: (1 << 1) | (1 << 5) | (1 << 6), ..Attr::default() };
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &a as *const Attr, tid as libc::pid_t, -1, -1, 8 as libc::c_ulong) } as i32;
    if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(fd) }
}

/// One counter per thread can pass the default soft limit of 1024 descriptors: raised to the hard
/// limit once.
fn raise_fd_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let mut r: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 && r.rlim_cur < r.rlim_max {
            r.rlim_cur = r.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
        }
    });
}

fn tids(pid: u32) -> HashSet<u32> {
    std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

/// Opens a group on every thread of `pid`. A thread that started while they opened may have
/// inherited an event already (from a thread opened before it) or not: the threads are listed
/// before and after, and the group opens again (at most 3 times) until none started meanwhile.
/// Err when the machine has no hardware counter at all; a process strobe may not count is
/// `Ok(Failed)`.
fn open_group(pid: u32, name: &str) -> std::result::Result<Counting, Uncounted> {
    raise_fd_limit();
    let close = |fds: &[i32]| {
        for &fd in fds {
            unsafe { libc::close(fd) };
        }
    };
    for _ in 0..3 {
        let listed = tids(pid);
        let mut fds = Vec::new();
        for &tid in &listed {
            match open_counter(tid) {
                Ok(fd) => fds.push(fd),
                Err(e) => match e.raw_os_error() {
                    Some(libc::ESRCH) => {}
                    Some(libc::ENOENT | libc::EOPNOTSUPP | libc::ENODEV) => {
                        close(&fds);
                        return Err(Uncounted::NoSource(format!("this CPU gives the OS no hardware instruction counter ({e}): a virtual machine without a virtual PMU counts none")));
                    }
                    Some(libc::EACCES | libc::EPERM) => {
                        close(&fds);
                        let why = format!("process {pid} ({name}) refuses perf events: it is not dumpable (a sandboxed process) or runs as another user");
                        return Ok(Counting::Failed(Uncounted::Denied { why, fix: "count a process strobe may trace (your user's, dumpable), or give strobe CAP_PERFMON".into() }));
                    }
                    _ => {
                        close(&fds);
                        return Ok(Counting::Failed(Uncounted::Denied { why: format!("perf_event_open on process {pid} ({name}): {e}"), fix: "raise the open file limit (ulimit -n)".into() }));
                    }
                },
            }
        }
        if tids(pid).is_subset(&listed) {
            return Ok(Counting::Own(Arc::new(Group { pid, name: name.to_string(), fds })));
        }
        close(&fds);
    }
    Ok(Counting::Failed(Uncounted::Partial(format!("process {pid} ({name}) kept starting threads while strobe opened its counters: some might count twice or not at all"))))
}

impl Groups {
    /// The groups that count `pids`, each once, opening what is new. The first call gives every
    /// process its own group. Later, a new process counts in its parent's group when it started
    /// after that group opened; one whose parent is not counted (reparented to an init, or started
    /// outside the counted processes) may already count in a group strobe holds, or in none: it
    /// refuses. Err when one of them cannot be counted, with why.
    pub fn read(&self, pids: &[u32]) -> std::result::Result<Vec<Arc<Group>>, Uncounted> {
        if !access().0 {
            return Err(Uncounted::Denied {
                why: "perf events for your processes are off (kernel.perf_event_paranoid is above 2)".into(),
                fix: "sudo sysctl kernel.perf_event_paranoid=2 (Debian and Ubuntu ship 3), or give strobe CAP_PERFMON once: sudo setcap cap_perfmon+ep $(command -v strobe)".into(),
            });
        }
        let mut reg = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let first = reg.is_empty();
        let stat = crate::host::linux::parent_start_name;
        reg.retain(|pid, r| stat(*pid).is_some_and(|(_, start, _)| start == r.start));
        let mut live: Vec<(u32, u32, u64, String)> = pids.iter().filter_map(|&p| stat(p).map(|(pp, start, name)| (p, pp, start, name))).collect();
        // Parents first: whether a child inherited depends on its parent's registration.
        live.sort_by_key(|p| p.2);
        let fresh: Vec<(u32, u32, u64, String)> = live.iter().filter(|(pid, _, start, _)| reg.get(pid).is_none_or(|r| r.start != *start)).cloned().collect();
        if !fresh.is_empty() {
            let before = Arc::new(alive());
            let mut opened: Vec<u32> = Vec::new();
            for (pid, ppid, start, name) in fresh {
                let parent = reg.get(&ppid).filter(|r| stat(ppid).is_some_and(|(_, s, _)| s == r.start));
                let counting = match parent.map(|r| (&r.counting, &r.before, &r.after)) {
                    Some((Counting::In(g), _, _)) => Counting::In(g.clone()),
                    Some((Counting::Own(g), b, a)) if !b.contains(&(pid, start)) && !a.contains(&(pid, start)) => Counting::In(g.clone()),
                    Some((Counting::Own(_), b, _)) if !b.contains(&(pid, start)) => {
                        Counting::Failed(Uncounted::Partial(format!("process {pid} ({name}) started while strobe opened its parent's counters: strobe cannot tell whether they count it")))
                    }
                    // Its parent's counters may count it, or may not.
                    Some((Counting::Failed(u), _, _)) => Counting::Failed(u.clone()),
                    // Started before its parent's group opened, or the first look: a group of its own.
                    Some((Counting::Own(_), _, _)) => open_group(pid, &name)?,
                    None if first => open_group(pid, &name)?,
                    None => Counting::Failed(Uncounted::Partial(format!(
                        "process {pid} ({name}) joined the target's processes from outside the counted ones (its parent {ppid} is not counted): it may count in a group strobe holds, or in none"
                    ))),
                };
                if matches!(counting, Counting::Own(_)) {
                    opened.push(pid);
                }
                reg.insert(pid, Registered { start, before: before.clone(), after: before.clone(), counting });
            }
            if !opened.is_empty() {
                let after = Arc::new(alive());
                for pid in opened {
                    if let Some(r) = reg.get_mut(&pid) {
                        r.after = after.clone();
                    }
                }
            }
        }
        let mut out: Vec<Arc<Group>> = Vec::new();
        for (pid, ..) in &live {
            let g = match reg.get(pid).map(|r| &r.counting) {
                Some(Counting::Own(g) | Counting::In(g)) => g,
                Some(Counting::Failed(u)) => return Err(u.clone()),
                None => continue,
            };
            if !out.iter().any(|o| Arc::ptr_eq(o, g)) {
                out.push(g.clone());
            }
        }
        Ok(out)
    }
}

