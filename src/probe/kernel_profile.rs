//! Native CPU samples from the kernel's sampled profile (attribution blocks only, SPEC rule 3): a
//! system logger session with the profile interrupt at 5 kHz (200 µs, as for JavaScript, §8.3),
//! a stack walk on every sample, image loads (with the rundown of images already loaded), and
//! process starts so a child of the target counts from its first sample.
//!
//! Needs SeSystemProfilePrivilege, which only administrators hold, so it runs in the helper (§13.4).
//! Samples of other processes are dropped as they arrive; the idle loop never leaves the kernel.

use super::etw::{self, Handler, Running};
use super::stacks::{Deep, Module};
use crate::model::{Error, Result};
use std::collections::{HashMap, HashSet};
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::core::GUID;

const PERFINFO: &str = "ce1dbfb4-137e-4da6-87b0-3f59aa102cbc";
const STACKWALK: &str = "def2fe46-7bd6-4b80-bd94-f57fe20d0ce3";
const IMAGE: &str = "2cb15d1d-5fc1-11d2-abe1-00a0c911f518";
const PROCESS: &str = "3d6fa8d0-fe05-11d0-9dda-00c04fd7ba7c";
const THREAD: &str = "3d6fa8d1-fe05-11d0-9dda-00c04fd7ba7c";
const FLAG_PROCESS: u32 = 0x1;
const FLAG_THREAD: u32 = 0x2;
const FLAG_IMAGE_LOAD: u32 = 0x4;
const FLAG_CSWITCH: u32 = 0x10;
const FLAG_DISPATCHER: u32 = 0x800;
const FLAG_PROFILE: u32 = 0x0100_0000;
const SAMPLED_PROFILE: u8 = 46;
const CSWITCH: u8 = 36;
const READY_THREAD: u8 = 50;
/// 200 µs in the profile interval's 100 ns units.
pub const INTERVAL_100NS: u32 = 2000;
/// Windows' own sampled-profile interval (1 ms), for when the current one cannot be read.
const DEFAULT_100NS: u32 = 10_000;

/// A sampled-stack session. Dropped without `stop` (an error, a client gone), its session stops and
/// the profile interval goes back all the same: fields drop in order, the session before the interval.
pub struct Capture {
    running: Running<Deep>,
    _raised: Raised,
    t0: u64,
}

/// The system-wide sampled-profile interval, raised for a capture and put back when this drops.
struct Raised(u32);

impl Raised {
    fn to(v: u32) -> Result<Raised> {
        let old = interval().unwrap_or(DEFAULT_100NS);
        set_interval(v)?;
        Ok(Raised(old))
    }
}

/// Puts the interval back only while it is still strobe's and no other strobe samples at it (that
/// capture's weights assume 200 µs; it puts it back when it ends). An interval read while another
/// strobe had raised it goes back to Windows' own.
impl Drop for Raised {
    fn drop(&mut self) {
        if interval() == Some(INTERVAL_100NS) && !etw::sessions().iter().any(|n| n.starts_with("strobe-stacks-")) {
            let _ = set_interval(if self.0 == INTERVAL_100NS { DEFAULT_100NS } else { self.0 });
        }
    }
}

impl Capture {
    /// Samples the target's process set (`roots`, which the client read: its root and what left its
    /// tree) with each one's tree and the children any of them start meanwhile.
    pub fn start(roots: &[u32], waits: bool) -> Result<Capture> {
        enable_privilege("SeSystemProfilePrivilege")?;
        // A helper that died while sampling left its session running and the interval raised: the
        // session is stopped and the interval put back first, so it is not taken for the system's.
        etw::reap();
        let pids: HashSet<u32> = roots.iter().flat_map(|&p| crate::host::tree_pids(p)).collect();
        if pids.is_empty() {
            return Err(Error::msg(format!("none of the target's processes runs ({roots:?})")));
        }
        let raised = Raised::to(INTERVAL_100NS)?;
        // Asked for, context switches and wake-ups with stacks too, for off-CPU waits (SPEC §11.1 `waits`).
        let cfg = etw::Config {
            kind: "stacks",
            kernel: FLAG_PROCESS | FLAG_IMAGE_LOAD | FLAG_PROFILE | if waits { FLAG_THREAD | FLAG_CSWITCH | FLAG_DISPATCHER } else { 0 },
            providers: Vec::new(),
            buffer_kb: 1024,
            buffers: (64, 512),
            flush_ms: 0,
        };
        let t0 = crate::host::now_ns();
        let setup = |h: CONTROLTRACE_HANDLE| -> Result<()> {
            let ids = [
                CLASSIC_EVENT_ID { EventGuid: etw::guid(PERFINFO), Type: SAMPLED_PROFILE, Reserved: [0; 7] },
                CLASSIC_EVENT_ID { EventGuid: etw::guid(THREAD), Type: CSWITCH, Reserved: [0; 7] },
                CLASSIC_EVENT_ID { EventGuid: etw::guid(THREAD), Type: READY_THREAD, Reserved: [0; 7] },
            ];
            let ids = &ids[..if waits { 3 } else { 1 }];
            let rc = unsafe { TraceSetInformation(h, TraceStackTracingInfo, ids.as_ptr() as *const _, std::mem::size_of_val(ids) as u32) };
            if rc != 0 {
                return Err(Error::msg(format!("stack walking: {}", etw::win_error(rc))));
            }
            Ok(())
        };
        let running = Running::start(cfg, setup, move || Sampler::new(pids, waits))?;
        Ok(Capture { running, _raised: raised, t0 })
    }

    pub fn stop(self) -> Result<Deep> {
        let t1 = crate::host::now_ns();
        let Capture { running, _raised: raised, t0 } = self;
        let result = running.stop();
        drop(raised);
        let (mut deep, stats) = result?;
        deep.t0_ns = t0;
        deep.t1_ns = t1;
        deep.events_lost = stats.events_lost + stats.buffers_lost;
        // One session carries both: what it lost may be switches or wake-ups as well as samples.
        if let Some(w) = deep.waits.as_mut() {
            w.events_lost = deep.events_lost;
        }
        Ok(deep)
    }
}

/// After a dead strobe's sampled-stack session was stopped and none remains (`etw::reap`): the
/// interval it raised goes back to Windows' own, unless something else has changed it since.
pub fn reset_interval() {
    if interval() == Some(INTERVAL_100NS) {
        let _ = set_interval(DEFAULT_100NS);
    }
}

/// The system's sampled-profile interval (100 ns units).
fn interval() -> Option<u32> {
    let mut v = TRACE_PROFILE_INTERVAL { Source: 0, Interval: 0 };
    let mut len = 0u32;
    let rc = unsafe { TraceQueryInformation(CONTROLTRACE_HANDLE { Value: 0 }, TraceSampledProfileIntervalInfo, &mut v as *mut _ as *mut _, std::mem::size_of_val(&v) as u32, &mut len) };
    (rc == 0 && v.Interval > 0).then_some(v.Interval)
}

fn set_interval(v: u32) -> Result<()> {
    let i = TRACE_PROFILE_INTERVAL { Source: 0, Interval: v };
    let rc = unsafe { TraceSetInformation(CONTROLTRACE_HANDLE { Value: 0 }, TraceSampledProfileIntervalInfo, &i as *const _ as *const _, std::mem::size_of_val(&i) as u32) };
    if rc != 0 {
        return Err(Error::msg(format!("set the profile interval: {}", etw::win_error(rc))));
    }
    Ok(())
}

pub fn enable_privilege(name: &str) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LUID};
    use windows_sys::Win32::Security::{AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return Err(Error::msg("open this process's token"));
        }
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        let ok = LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), &mut luid) != 0 && {
            let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
            AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) != 0 && GetLastError() == 0
        };
        CloseHandle(token);
        if !ok {
            return Err(Error::msg(format!("{name} is not held: sampled stacks need an administrator token (the deep-mode helper)")));
        }
    }
    Ok(())
}

/// The session's consumer: keeps the samples of `pids` (and their children) and the images they use,
/// and, asked for, their threads' off-CPU waits.
pub struct Sampler {
    /// Whether the session has context switches and wake-ups (`waits`).
    waits_on: bool,
    guids: [GUID; 5],
    pids: HashSet<u32>,
    /// (stack walk timestamp, thread) → sample index: a sample taken in kernel mode gets its user
    /// part in a second stack walk event when the thread returns to user mode.
    pending: HashMap<(u64, u32), usize>,
    samples: Vec<(u64, u32, u32, u16, Vec<u64>)>,
    images: Vec<Module>,
    seen: HashSet<(u32, u64)>,
    freq: u64,
    /// With waits: the target's threads as their events arrived (to keep only their records).
    threads_now: HashMap<u32, u32>,
    /// With waits: each CPU's last SampledProfile (time, thread). A stack walk follows the event it
    /// walked for on the same CPU, so only one that matches it starts a sample.
    last_profile: HashMap<u16, (u64, u32)>,
    /// With waits: thread starts and ends, context switches and wake-ups, as they arrived. Real-time
    /// ETW delivers each CPU's buffers on their own, out of order between CPUs: sorted by time before
    /// they are paired.
    sched: Vec<Sched>,
    /// Stack walks of context switches into the target's threads (the new thread's: where it
    /// waited) and of wake-ups of them (the readying thread's), by (time, thread).
    switch_walks: HashMap<(u64, u32), Vec<u64>>,
    ready_walks: HashMap<(u64, u32), Vec<u64>>,
    /// (time, thread) of the CSwitch and ReadyThread events whose stack walks are kept.
    switch_keys: HashSet<(u64, u32)>,
    ready_keys: HashSet<(u64, u32)>,
}

/// A scheduler record, raw time first.
enum Sched {
    Thread { ts: u64, start: bool, pid: u32, tid: u32 },
    Switch { ts: u64, new: u32, old: u32, reason: u8, state: u8 },
    Ready { ts: u64, woken: u32, pid: u32, tid: u32 },
}

impl Sched {
    fn ts(&self) -> u64 {
        match self {
            Sched::Thread { ts, .. } | Sched::Switch { ts, .. } | Sched::Ready { ts, .. } => *ts,
        }
    }
}

const KERNEL: u64 = 0xFFFF_8000_0000_0000;
/// KTHREAD_STATE Terminated: a thread switched out for good opens no wait.
const TERMINATED: u8 = 4;

impl Sampler {
    pub fn new(pids: HashSet<u32>, waits: bool) -> Sampler {
        Sampler {
            waits_on: waits,
            guids: [etw::guid(STACKWALK), etw::guid(IMAGE), etw::guid(PROCESS), etw::guid(THREAD), etw::guid(PERFINFO)],
            pids,
            pending: HashMap::new(),
            samples: Vec::with_capacity(1 << 16),
            images: Vec::new(),
            seen: HashSet::new(),
            freq: etw::qpc_freq(),
            threads_now: HashMap::new(),
            last_profile: HashMap::new(),
            sched: Vec::new(),
            switch_walks: HashMap::new(),
            ready_walks: HashMap::new(),
            switch_keys: HashSet::new(),
            ready_keys: HashSet::new(),
        }
    }

    /// A profile sample's stack walk (or a part of one).
    fn sample(&mut self, e: &EVENT_RECORD, ts: u64, pid: u32, tid: u32, frames: Vec<u64>) {
        match self.pending.get(&(ts, tid)) {
            Some(&i) => merge(&mut self.samples[i].4, frames),
            None => {
                let cpu = unsafe { e.BufferContext.Anonymous.ProcessorIndex };
                self.pending.insert((ts, tid), self.samples.len());
                self.samples.push((ts, pid, tid, cpu, frames));
            }
        }
    }
}

/// A stack walk's frames added to what an earlier part of the same walk gave: kernel frames are
/// innermost, so they go first whichever part came first.
fn merge(into: &mut Vec<u64>, frames: Vec<u64>) {
    if frames.first().is_some_and(|f| *f >= KERNEL) && into.first().is_some_and(|f| *f < KERNEL) {
        let mut merged = frames;
        merged.append(into);
        *into = merged;
    } else {
        into.extend(frames);
    }
}

/// A thread's state and wait reason as CSwitch gives them for the thread that left its CPU.
fn wait_kind(state: u8, reason: u8) -> (crate::probe::stacks::WaitKind, &'static str) {
    use crate::probe::stacks::WaitKind::*;
    // KTHREAD_STATE: Ready 1, Standby 3, DeferredReady 7 (preempted, still runnable); Waiting 5.
    const REASONS: [&str; 41] = [
        "Executive", "FreePage", "PageIn", "PoolAllocation", "DelayExecution", "Suspended", "UserRequest", "WrExecutive", "WrFreePage", "WrPageIn", "WrPoolAllocation", "WrDelayExecution", "WrSuspended",
        "WrUserRequest", "WrEventPair", "WrQueue", "WrLpcReceive", "WrLpcReply", "WrVirtualMemory", "WrPageOut", "WrRendezvous", "WrKeyedEvent", "WrTerminated", "WrProcessInUse", "WrCpuRateControl",
        "WrCalloutStack", "WrKernel", "WrResource", "WrPushLock", "WrMutex", "WrQuantumEnd", "WrDispatchInt", "WrPreempted", "WrYieldExecution", "WrFastMutex", "WrGuardedMutex", "WrRundown",
        "WrAlertByThreadId", "WrDeferredPreempt", "WrPhysicalFault", "WrIoRing",
    ];
    let word = REASONS.get(reason as usize).copied().unwrap_or("other");
    let kind = match state {
        1 | 3 | 7 => Preempted,
        5 if matches!(reason, 1 | 2 | 8 | 9 | 18 | 19 | 39) => Io,
        5 => Blocked,
        _ => Other,
    };
    (kind, if kind == Preempted { "Ready" } else { word })
}

fn u32_at(d: &[u8], at: usize) -> u32 {
    d.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
}

fn u64_at(d: &[u8], at: usize) -> u64 {
    d.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

impl Handler for Sampler {
    type Out = Deep;

    fn event(&mut self, e: &EVENT_RECORD) {
        let g = &e.EventHeader.ProviderId;
        let op = e.EventHeader.EventDescriptor.Opcode;
        let d = etw::payload(e);
        if etw::same(g, &self.guids[0]) && op == 32 {
            let (ts, pid, tid) = (u64_at(d, 0), u32_at(d, 8), u32_at(d, 12));
            let frames = || -> Vec<u64> { d[16.min(d.len())..].as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect() };
            if !self.waits_on {
                // Only the sampled profile walks stacks.
                if self.pids.contains(&pid) {
                    self.sample(e, ts, pid, tid, frames());
                }
                return;
            }
            // Each stack walk goes with the event it walked for (logged just before it, on its CPU;
            // a kernel-mode sample's user part later, under the same time): a sample only when that
            // was a SampledProfile, so a switch's or a wake-up's stack is never one.
            let cpu = unsafe { e.BufferContext.Anonymous.ProcessorIndex };
            if self.ready_keys.contains(&(ts, tid)) {
                merge(self.ready_walks.entry((ts, tid)).or_default(), frames());
            } else if self.switch_keys.contains(&(ts, tid)) {
                if self.pids.contains(&pid) {
                    merge(self.switch_walks.entry((ts, tid)).or_default(), frames());
                }
            } else if self.pids.contains(&pid) && (self.pending.contains_key(&(ts, tid)) || self.last_profile.get(&cpu) == Some(&(ts, tid))) {
                self.sample(e, ts, pid, tid, frames());
            }
        } else if etw::same(g, &self.guids[4]) && op == SAMPLED_PROFILE {
            if self.waits_on {
                // InstructionPointer (pointer-sized), then ThreadId.
                let ptr = if e.EventHeader.Flags as u32 & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 { 4 } else { 8 };
                let cpu = unsafe { e.BufferContext.Anonymous.ProcessorIndex };
                self.last_profile.insert(cpu, (e.EventHeader.TimeStamp as u64, u32_at(d, ptr)));
            }
        } else if etw::same(g, &self.guids[1]) && matches!(op, 10 | 3 | 4) {
            let (base, size, pid) = (u64_at(d, 0), u64_at(d, 8), u32_at(d, 16));
            if (pid != 0 && !self.pids.contains(&pid)) || !self.seen.insert((pid, base)) {
                return;
            }
            let name: Vec<u16> = d.get(56..).unwrap_or(&[]).as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|c| *c != 0).collect();
            self.images.push(Module { pid, base, size, path: String::from_utf16_lossy(&name) });
        } else if etw::same(g, &self.guids[2]) && matches!(op, 1 | 3) {
            let (pid, parent) = (u32_at(d, 8), u32_at(d, 12));
            if self.pids.contains(&parent) {
                self.pids.insert(pid);
            }
        } else if etw::same(g, &self.guids[3]) && self.waits_on {
            let ts = e.EventHeader.TimeStamp as u64;
            match op {
                // Thread start and rundown (1, 3), end and its rundown (2, 4): ProcessId, TThreadId.
                1..=4 => {
                    let (pid, tid) = (u32_at(d, 0), u32_at(d, 4));
                    if self.pids.contains(&pid) {
                        let start = matches!(op, 1 | 3);
                        if start {
                            self.threads_now.insert(tid, pid);
                        }
                        self.sched.push(Sched::Thread { ts, start, pid, tid });
                    }
                }
                CSWITCH => {
                    let (new, old) = (u32_at(d, 0), u32_at(d, 4));
                    let (reason, state) = (d.get(12).copied().unwrap_or(0), d.get(14).copied().unwrap_or(0));
                    let (to_target, from_target) = (self.threads_now.contains_key(&new), self.threads_now.contains_key(&old));
                    if to_target {
                        self.switch_keys.insert((ts, new));
                    }
                    if to_target || from_target {
                        self.sched.push(Sched::Switch { ts, new, old, reason, state });
                    }
                }
                READY_THREAD => {
                    let woken = u32_at(d, 0);
                    if self.threads_now.contains_key(&woken) {
                        let (pid, tid) = (e.EventHeader.ProcessId, e.EventHeader.ThreadId);
                        self.ready_keys.insert((ts, tid));
                        self.sched.push(Sched::Ready { ts, woken, pid, tid });
                    }
                }
                _ => {}
            }
        }
    }

    fn finish(self) -> Deep {
        let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
        let mut stacks = Vec::new();
        let mut samples = Vec::with_capacity(self.samples.len());
        for (ts, pid, tid, cpu, frames) in self.samples {
            let next = stacks.len() as u32;
            let at = *index.entry(frames.clone()).or_insert_with(|| {
                stacks.push(frames);
                next
            });
            samples.push((etw::ns(ts, self.freq), pid, tid, cpu, at));
        }
        let devices = dos_devices();
        let mut modules: Vec<Module> = self.images.into_iter().map(|m| Module { path: dos_path(&m.path, &devices), ..m }).collect();
        if !modules.iter().any(|m| m.pid == 0) {
            modules.extend(drivers(&devices));
        }
        let mut pids: Vec<u32> = self.pids.into_iter().collect();
        pids.sort_unstable();
        let waits = self.waits_on.then(|| pair_waits(self.sched, self.switch_walks, self.ready_walks, modules.clone(), self.freq));
        Deep { interval_us: INTERVAL_100NS as f64 / 10.0, modules, stacks, samples, pids, waits, ..Deep::default() }
    }
}

/// The waits, from the scheduler records sorted by time: a thread of the target switched out opens one
/// (not a terminated thread), its next switch-in ends it (the CSwitch's stack walk: where it
/// waited), and the last ReadyThread of it inside is its waker (pid 0, the idle process: an interrupt
/// or DPC, no thread). A thread's end drops what it had open.
fn pair_waits(mut sched: Vec<Sched>, mut switch_walks: HashMap<(u64, u32), Vec<u64>>, mut ready_walks: HashMap<(u64, u32), Vec<u64>>, modules: Vec<Module>, freq: u64) -> crate::probe::stacks::Waits {
    use crate::probe::stacks::{Wait, Waker, Waits};
    sched.sort_by_key(Sched::ts);
    let ns = |t: u64| etw::ns(t, freq);
    let mut out = Waits { modules, ..Default::default() };
    let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
    let mut threads: HashMap<u32, u32> = HashMap::new();
    let mut open: HashMap<u32, (u64, u32, crate::probe::stacks::WaitKind, &'static str)> = HashMap::new();
    let mut ready: HashMap<u32, (u64, u32, u32)> = HashMap::new();
    for s in sched {
        match s {
            Sched::Thread { start: true, pid, tid, .. } => {
                threads.insert(tid, pid);
            }
            Sched::Thread { start: false, tid, .. } => {
                threads.remove(&tid);
                open.remove(&tid);
                ready.remove(&tid);
            }
            Sched::Ready { ts, woken, pid, tid } => {
                ready.insert(woken, (ts, pid, tid));
            }
            Sched::Switch { ts, new, old, reason, state } => {
                if let Some(&pid) = threads.get(&old) {
                    if state == TERMINATED {
                        open.remove(&old);
                    } else {
                        let (kind, word) = wait_kind(state, reason);
                        open.insert(old, (ts, pid, kind, word));
                    }
                }
                if let Some((from, pid, kind, reason)) = open.remove(&new) {
                    let woke = ready.remove(&new).filter(|r| r.0 >= from && r.0 <= ts);
                    let stack = out.stack(&mut index, switch_walks.remove(&(ts, new)).unwrap_or_default());
                    let waker = woke.map(|(rts, wpid, wtid)| {
                        let interrupt = wpid == 0;
                        let stack = if interrupt { None } else { out.stack(&mut index, ready_walks.remove(&(rts, wtid)).unwrap_or_default()) };
                        Waker { pid: wpid, tid: wtid, interrupt, stack }
                    });
                    out.waits.push(Wait { pid, tid: new, from_ns: ns(from), to_ns: ns(ts), ready_ns: woke.map(|r| ns(r.0)), kind, reason: reason.to_string(), stack, waker });
                }
            }
        }
    }
    out
}

/// The kernel's images when the session's rundown named none (pid 0), from the loaded driver list.
fn drivers(devices: &[(String, String)]) -> Vec<Module> {
    use windows_sys::Win32::System::ProcessStatus::{K32EnumDeviceDrivers, K32GetDeviceDriverFileNameW};
    let mut bases = vec![std::ptr::null_mut(); 1024];
    let mut need = 0u32;
    if unsafe { K32EnumDeviceDrivers(bases.as_mut_ptr(), (bases.len() * 8) as u32, &mut need) } == 0 {
        return Vec::new();
    }
    bases
        .iter()
        .take((need as usize / 8).min(bases.len()))
        .filter(|b| !b.is_null())
        .filter_map(|&b| {
            let mut name = vec![0u16; 1024];
            let n = unsafe { K32GetDeviceDriverFileNameW(b, name.as_mut_ptr(), name.len() as u32) };
            let path = dos_path(&String::from_utf16_lossy(&name[..n as usize]), devices);
            let size = crate::symbol::pdb::image_size(std::path::Path::new(&path))?;
            Some(Module { pid: 0, base: b as u64, size: size as u64, path })
        })
        .collect()
}

/// `\Device\HarddiskVolume3` → `C:` for every drive letter.
fn dos_devices() -> Vec<(String, String)> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", letter as char);
        let w: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        let n = unsafe { QueryDosDeviceW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n > 0 {
            let end = buf.iter().position(|c| *c == 0).unwrap_or(0);
            out.push((String::from_utf16_lossy(&buf[..end]), drive));
        }
    }
    out
}

/// A kernel image path as a path this user can open.
pub fn dos_path(path: &str, devices: &[(String, String)]) -> String {
    let lower = path.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("\\systemroot\\") {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        return format!("{root}\\{}", &path[path.len() - rest.len()..]);
    }
    if let Some(rest) = path.strip_prefix("\\??\\") {
        return rest.to_string();
    }
    for (dev, drive) in devices {
        if lower.starts_with(&dev.to_ascii_lowercase()) && path[dev.len()..].starts_with('\\') {
            return format!("{drive}{}", &path[dev.len()..]);
        }
    }
    if path.starts_with('\\') && !path.starts_with("\\\\") {
        // `\Windows\System32\…` (no device): the system drive.
        let root = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        return format!("{root}{path}");
    }
    path.to_string()
}
