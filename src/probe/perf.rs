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

use crate::probe::stacks::{Deep, Module};
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

/// Opens a sampling event on every thread of the tree under `root` that has none yet.
fn follow(root: u32, kernel: bool, rings: &mut Vec<Ring>, opened: &mut HashSet<u32>) -> std::io::Result<()> {
    for pid in crate::host::tree_pids(root) {
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
    pub fn start(root: u32) -> Result<Capture> {
        if let Some(why) = refusal() {
            return Err(Error::msg(why));
        }
        let (_, kernel) = access();
        let pids = crate::host::tree_pids(root);
        if pids.is_empty() {
            return Err(Error::msg(format!("process {root} is not running")));
        }
        let mut rings: Vec<Ring> = Vec::new();
        let mut opened: HashSet<u32> = HashSet::new();
        if let Err(e) = follow(root, kernel, &mut rings, &mut opened) {
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
                        let _ = follow(root, kernel, &mut rings, &mut opened);
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

// Counting (SPEC §7.4 `cpu.instructions`): no samples, no interrupts, one counter per thread.

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
    /// The ticks between which its own events opened: a child started after `opened.1` inherited
    /// them, one started before `opened.0` did not.
    opened: (u64, u64),
    counting: Counting,
}

/// Every process strobe counts, by pid: snapshots of one process set share its groups, so a thread
/// is never counted twice and a group keeps counting between runs.
static GROUPS: Mutex<Option<HashMap<u32, Registered>>> = Mutex::new(None);

/// (ppid, start ticks, comm) of a live process.
fn proc_stat(pid: u32) -> Option<(u32, u64, String)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (text.find('(')?, text.rfind(')')?);
    let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
    if matches!(f.first(), Some(&"Z" | &"X" | &"x")) {
        return None;
    }
    Some((f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?, text[open + 1..close].to_string()))
}

/// Now, in the clock ticks of `/proc/<pid>/stat`'s start time (boot time).
fn boot_ticks() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    let hz = (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64;
    (ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64) / (1_000_000_000 / hz)
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

/// Opens a group on every thread of `pid`. Err when the machine has no hardware counter at all; a
/// process strobe may not count is `Ok(Failed)`.
fn open_group(pid: u32, name: &str) -> std::result::Result<Counting, Uncounted> {
    raise_fd_limit();
    let mut fds = Vec::new();
    let fail = |fds: &[i32], u: Uncounted| {
        for &fd in fds {
            unsafe { libc::close(fd) };
        }
        u
    };
    for e in std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
        let Some(tid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        match open_counter(tid) {
            Ok(fd) => fds.push(fd),
            Err(e) => match e.raw_os_error() {
                Some(libc::ESRCH) => {}
                Some(libc::ENOENT | libc::EOPNOTSUPP | libc::ENODEV) => {
                    return Err(fail(&fds, Uncounted::NoSource(format!("this CPU gives the OS no hardware instruction counter ({e}): a virtual machine without a virtual PMU counts none"))));
                }
                Some(libc::EACCES | libc::EPERM) => {
                    let why = format!("process {pid} ({name}) refuses perf events: it is not dumpable (a sandboxed process) or runs as another user");
                    return Ok(Counting::Failed(fail(&fds, Uncounted::Denied { why, fix: "count a process strobe may trace (your user's, dumpable), or give strobe CAP_PERFMON".into() })));
                }
                _ => return Ok(Counting::Failed(fail(&fds, Uncounted::Denied { why: format!("perf_event_open on process {pid} ({name}): {e}"), fix: "raise the open file limit (ulimit -n)".into() }))),
            },
        }
    }
    Ok(Counting::Own(Arc::new(Group { pid, name: name.to_string(), fds })))
}

/// The groups that count `pids`, each once, opening what is new: a process started by a counted
/// thread after its events opened counts in that group, any other gets its own. Err when one of
/// them cannot be counted, with why.
pub fn groups(pids: &[u32]) -> std::result::Result<Vec<Arc<Group>>, Uncounted> {
    if !access().0 {
        return Err(Uncounted::Denied {
            why: "perf events for your processes are off (kernel.perf_event_paranoid is above 2)".into(),
            fix: "sudo sysctl kernel.perf_event_paranoid=2 (Debian and Ubuntu ship 3), or give strobe CAP_PERFMON once: sudo setcap cap_perfmon+ep $(command -v strobe)".into(),
        });
    }
    let mut lock = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    let reg = lock.get_or_insert_with(HashMap::new);
    reg.retain(|pid, r| proc_stat(*pid).is_some_and(|(_, start, _)| start == r.start));
    let mut live: Vec<(u32, u32, u64, String)> = pids.iter().filter_map(|&p| proc_stat(p).map(|(pp, start, name)| (p, pp, start, name))).collect();
    // Parents first: whether a child inherited depends on its parent's registration.
    live.sort_by_key(|p| p.2);
    let mut out: Vec<Arc<Group>> = Vec::new();
    for (pid, ppid, start, name) in live {
        if reg.get(&pid).is_none_or(|r| r.start != start) {
            let parent = reg.get(&ppid).filter(|r| proc_stat(ppid).is_some_and(|(_, s, _)| s == r.start));
            let inherited = |counting| Registered { start, opened: (0, 0), counting };
            let r = match parent.map(|r| (&r.counting, r.opened)) {
                Some((Counting::In(g), _)) => inherited(Counting::In(g.clone())),
                Some((Counting::Own(g), (_, end))) if start > end => inherited(Counting::In(g.clone())),
                Some((Counting::Own(_), (begin, _))) if start >= begin => inherited(Counting::Failed(Uncounted::Partial(format!(
                    "process {pid} ({name}) started while strobe opened its parent's counters: strobe cannot tell whether they count it"
                )))),
                // Its parent's counters may count it, or may not: neither can be told apart.
                Some((Counting::Failed(u), _)) => inherited(Counting::Failed(u.clone())),
                _ => {
                    let begin = boot_ticks();
                    let counting = open_group(pid, &name)?;
                    Registered { start, opened: (begin, boot_ticks()), counting }
                }
            };
            reg.insert(pid, r);
        }
        add_group(reg, pid, &mut out)?;
    }
    Ok(out)
}

/// Adds `pid`'s group to `out` once, or fails with why it is not counted.
fn add_group(reg: &HashMap<u32, Registered>, pid: u32, out: &mut Vec<Arc<Group>>) -> std::result::Result<(), Uncounted> {
    let g = match reg.get(&pid).map(|r| &r.counting) {
        Some(Counting::Own(g) | Counting::In(g)) => g,
        Some(Counting::Failed(u)) => return Err(u.clone()),
        None => return Ok(()),
    };
    if !out.iter().any(|o| Arc::ptr_eq(o, g)) {
        out.push(g.clone());
    }
    Ok(())
}
