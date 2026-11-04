//! macOS GPU sampling (SPEC §13.6) from the IORegistry, with no permission and no helper:
//!
//! - per process: every GPU client (`IOAccelerator` children, one per Metal device a process opened)
//!   carries its creator (`IOUserClientCreator`: "pid 123, name") and `AppUsage`, the GPU time its
//!   command queues accumulated (`accumulatedGPUTime`, ns). The time each tick added is the
//!   process's GPU busy share, as Activity Monitor's GPU column. Apple GPUs report one engine: 3D.
//! - per device: the accelerator's `PerformanceStatistics` (`Device Utilization %`, `In use system memory`).
//!
//! Apple GPUs share the system's memory: there is no per-process VRAM (a process's GPU allocations are
//! in its physical footprint, `memory.app.private`), so `gpu.vram` is refused here, never 0.

use super::gpu::{Adapter, Device, DeviceSample, EngineUse, VramUse};
use crate::host::macos::{Cf, CFTypeRef, cf_array, cf_get, cf_i64, cf_string, cfstr};
use std::collections::HashMap;
use std::ffi::{CString, c_char};
use std::time::{Duration, Instant};

type Io = u32;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFTypeRef;
    fn IOServiceGetMatchingServices(main: u32, matching: CFTypeRef, iter: *mut Io) -> i32;
    fn IOIteratorNext(iter: Io) -> Io;
    fn IOObjectRelease(o: Io) -> i32;
    fn IORegistryEntryGetChildIterator(entry: Io, plane: *const c_char, iter: *mut Io) -> i32;
    fn IORegistryEntryCreateCFProperty(entry: Io, key: CFTypeRef, alloc: CFTypeRef, options: u32) -> CFTypeRef;
    fn IORegistryEntryGetRegistryEntryID(entry: Io, id: *mut u64) -> i32;
}

fn each(iter: Io, mut f: impl FnMut(Io)) {
    loop {
        let o = unsafe { IOIteratorNext(iter) };
        if o == 0 {
            break;
        }
        f(o);
        unsafe { IOObjectRelease(o) };
    }
    unsafe { IOObjectRelease(iter) };
}

/// Calls `f` with every IOAccelerator service.
fn accelerators(mut f: impl FnMut(Io)) {
    let name = CString::new("IOAccelerator").unwrap_or_default();
    let mut iter = 0;
    // IOServiceGetMatchingServices consumes the matching dictionary.
    if unsafe { IOServiceGetMatchingServices(0, IOServiceMatching(name.as_ptr()), &mut iter) } == 0 {
        each(iter, &mut f);
    }
}

fn property(entry: Io, key: &str) -> Option<Cf> {
    let k = cfstr(key);
    Cf::new(unsafe { IORegistryEntryCreateCFProperty(entry, k.0, std::ptr::null(), 0) })
}

fn entry_id(entry: Io) -> u64 {
    let mut id = 0;
    unsafe { IORegistryEntryGetRegistryEntryID(entry, &mut id) };
    id
}

/// Calls `f` with every client (child in the service plane) of an accelerator.
fn clients(acc: Io, f: impl FnMut(Io)) {
    let plane = CString::new("IOService").unwrap_or_default();
    let mut iter = 0;
    if unsafe { IORegistryEntryGetChildIterator(acc, plane.as_ptr(), &mut iter) } == 0 {
        each(iter, f);
    }
}

pub fn adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    accelerators(|acc| {
        let model = property(acc, "model").and_then(|m| cf_string(m.0)).unwrap_or_else(|| "Apple GPU".into());
        out.push(Adapter { luid: entry_id(acc), name: format!("{model} GPU"), bus: None, nodes: Vec::new(), apertures: Vec::new() });
    });
    out
}

/// `pid 123, WindowServer` → 123.
fn creator_pid(entry: Io) -> Option<u32> {
    let s = property(entry, "IOUserClientCreator").and_then(|c| cf_string(c.0))?;
    s.strip_prefix("pid ")?.split(',').next()?.trim().parse().ok()
}

/// Accumulated GPU ns of one client, summed over its queues.
fn gpu_time(entry: Io) -> Option<u64> {
    let usage = property(entry, "AppUsage")?;
    Some(cf_array(usage.0).into_iter().filter_map(|d| cf_i64(cf_get(d, "accumulatedGPUTime"))).map(|v| v.max(0) as u64).sum())
}

pub struct Kmt {
    adapters: Vec<(u64, u16)>,
    engine: u16,
    /// (accelerator id, client id) → creator pid, re-listed every second.
    owners: HashMap<(u64, u64), u32>,
    listed: Option<Instant>,
    prev: HashMap<(u64, u64), u64>,
    started: bool,
    pub unreadable: Vec<u32>,
}

impl Kmt {
    pub fn new(adapters: &[Adapter], names: &mut Vec<String>) -> Option<Kmt> {
        if adapters.is_empty() {
            return None;
        }
        let engine = super::gpu::intern(names, "3D");
        Some(Kmt { adapters: adapters.iter().enumerate().map(|(i, a)| (a.luid, i as u16)).collect(), engine, owners: HashMap::new(), listed: None, prev: HashMap::new(), started: false, unreadable: Vec::new() })
    }

    pub fn read(&mut self, pids: &[u32], dt_ns: u64, nodes: bool, gpu: &mut Vec<EngineUse>, _vram: &mut Vec<VramUse>) {
        let relist = self.listed.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if relist {
            self.owners.clear();
        }
        let mut now: HashMap<(u64, u64), u64> = HashMap::new();
        let owners = &mut self.owners;
        accelerators(|acc| {
            let a = entry_id(acc);
            clients(acc, |c| {
                let key = (a, entry_id(c));
                let pid = if relist {
                    match creator_pid(c) {
                        Some(p) => {
                            owners.insert(key, p);
                            p
                        }
                        None => return,
                    }
                } else {
                    match owners.get(&key) {
                        Some(&p) => p,
                        None => return,
                    }
                };
                if pids.binary_search(&pid).is_ok()
                    && let Some(t) = gpu_time(c)
                {
                    now.insert(key, t);
                }
            });
        });
        if relist {
            self.listed = Some(Instant::now());
        }
        for (key, &t) in &now {
            let base = match self.prev.get(key) {
                Some(&p) => p,
                None if self.started => 0,
                None => continue,
            };
            if nodes && t > base && dt_ns > 0 {
                let adapter = self.adapters.iter().find(|x| x.0 == key.0).map(|x| x.1).unwrap_or(0);
                let pid = self.owners.get(key).copied().unwrap_or(0);
                gpu.push(EngineUse { pid, adapter, engine: self.engine, pct: ((t - base) as f64 * 100.0 / dt_ns as f64) as f32 });
            }
        }
        self.prev = now;
        self.started = true;
        gpu.sort_unstable_by_key(|e| (e.pid, e.adapter, e.engine));
        gpu.dedup_by(|cur, prev| {
            let same = (cur.pid, cur.adapter, cur.engine) == (prev.pid, prev.adapter, prev.engine);
            if same {
                prev.pct += cur.pct;
            }
            same
        });
    }
}

/// The accelerator's own statistics, as a device.
pub struct Nvml {
    pub devices: Vec<Device>,
}

impl Nvml {
    pub fn load(adapters: &[Adapter]) -> Option<Nvml> {
        let devices: Vec<Device> = adapters.iter().enumerate().map(|(i, a)| Device { name: a.name.clone(), adapter: Some(i as u16), vram_total_mb: None }).collect();
        (!devices.is_empty()).then_some(Nvml { devices })
    }

    pub fn sample(&mut self, _t_ns: u64, _pcie: bool, out: &mut Vec<DeviceSample>) {
        out.clear();
        accelerators(|acc| {
            let Some(stats) = property(acc, "PerformanceStatistics") else { return };
            let get = |k: &str| cf_i64(cf_get(stats.0, k)).map(|v| v as f64);
            out.push(DeviceSample { gpu_pct: get("Device Utilization %"), vram_used_mb: get("In use system memory").map(|b| b / (1 << 20) as f64), ..DeviceSample::default() });
        });
    }
}
