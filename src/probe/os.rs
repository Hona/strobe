//! Outside-in OS and GPU sampling (SPEC §8.5, §13.6): per-core load and frequency from PDH
//! `Processor Information`, plus `gpu.rs`, on one background thread with QPC timestamps
//! (`host::now_ns`). PDH rate counters average over the time between two collections, so each
//! tick covers `(t_ns - dt_ns, t_ns]`.

use super::gpu::{self, Adapter, Device, DeviceSample, EngineUse, VramUse};
use crate::host;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default tick for every signal.
pub const INTERVAL: Duration = Duration::from_millis(100);
/// How often the sampler re-reads which PIDs belong to the tree.
const TREE_REFRESH: Duration = Duration::from_secs(1);
const MB: f64 = 1024.0 * 1024.0;

/// One logical CPU over one tick, in %.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Core {
    /// `% Processor Time`: share of the tick the CPU was busy, 0..100.
    pub busy: f32,
    /// `% Processor Utility`: busy scaled by frequency against nominal (Task Manager's CPU); can exceed 100.
    pub util: f32,
    /// `% Processor Performance`: average frequency against nominal; above 100 when boosting.
    pub perf: f32,
}

/// Everything one collection produced. Process rows only for the PIDs the caller keeps.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Tick {
    pub t_ns: u64,
    pub dt_ns: u64,
    /// Indexed by logical CPU (group * 64 + number).
    pub cores: Vec<Core>,
    pub gpu: Vec<EngineUse>,
    pub vram: Vec<VramUse>,
    /// Indexed like `Samples::devices`.
    pub devices: Vec<DeviceSample>,
}

/// The collectors, reusable on any thread: one PDH query for CPU and GPU counters, NVML, and
/// the adapter and engine name tables the ticks index into.
pub struct Probe {
    q: Option<pdh::Query>,
    cpus: Option<Cpus>,
    gpu: Option<gpu::Engines>,
    nvml: Option<gpu::Nvml>,
    pub adapters: Vec<Adapter>,
    pub nominal_mhz: Option<f64>,
    last_ns: u64,
}

impl Probe {
    /// Opens every source and takes the baseline collection the rate counters need.
    pub fn new() -> Probe {
        let q = pdh::Query::open();
        let cpus = q.as_ref().and_then(Cpus::add);
        let gpu = q.as_ref().and_then(gpu::Engines::add);
        let adapters = gpu::adapters();
        let nvml = gpu::Nvml::load(&adapters);
        let mut p = Probe { q, cpus, gpu, nvml, adapters, nominal_mhz: None, last_ns: 0 };
        p.tick(&|_| false);
        p
    }

    /// Collects once. `keep` picks the PIDs whose GPU rows are kept.
    pub fn tick(&mut self, keep: &dyn Fn(u32) -> bool) -> Tick {
        if let Some(q) = &self.q {
            q.collect();
        }
        let t = host::now_ns();
        let mut tick = Tick { t_ns: t, dt_ns: if self.last_ns == 0 { 0 } else { t - self.last_ns }, ..Tick::default() };
        self.last_ns = t;
        if let (Some(q), Some(c)) = (self.q.as_mut(), &self.cpus) {
            c.read(q, &mut tick.cores, &mut self.nominal_mhz);
        }
        if let (Some(q), Some(g)) = (self.q.as_mut(), self.gpu.as_mut()) {
            g.read(q, keep, &mut self.adapters, &mut tick.gpu, &mut tick.vram);
        }
        if let Some(n) = self.nvml.as_mut() {
            n.sample(t, &mut tick.devices);
        }
        tick
    }

    pub fn gpu_available(&self) -> bool {
        self.gpu.is_some()
    }

    pub fn engines(&self) -> &[String] {
        self.gpu.as_ref().map(|g| g.names.as_slice()).unwrap_or(&[])
    }

    pub fn devices(&self) -> &[Device] {
        self.nvml.as_ref().map(|n| n.devices.as_slice()).unwrap_or(&[])
    }
}

impl Default for Probe {
    fn default() -> Self {
        Probe::new()
    }
}

struct Cpus {
    busy: isize,
    util: isize,
    perf: isize,
    freq: isize,
}

impl Cpus {
    fn add(q: &pdh::Query) -> Option<Cpus> {
        Some(Cpus {
            busy: q.add(r"\Processor Information(*)\% Processor Time")?,
            util: q.add(r"\Processor Information(*)\% Processor Utility")?,
            perf: q.add(r"\Processor Information(*)\% Processor Performance")?,
            freq: q.add(r"\Processor Information(*)\Processor Frequency")?,
        })
    }

    fn read(&self, q: &mut pdh::Query, cores: &mut Vec<Core>, nominal: &mut Option<f64>) {
        q.each(self.busy, pdh::NOSCALE, |n, v| {
            if let Some(i) = cpu_index(n) {
                slot(cores, i).busy = v as f32;
            }
        });
        q.each(self.util, pdh::NOSCALE | pdh::NOCAP100, |n, v| {
            if let Some(i) = cpu_index(n) {
                slot(cores, i).util = v as f32;
            }
        });
        q.each(self.perf, pdh::NOSCALE | pdh::NOCAP100, |n, v| {
            if let Some(i) = cpu_index(n) {
                slot(cores, i).perf = v as f32;
            }
        });
        q.each(self.freq, pdh::NOSCALE | pdh::NOCAP100, |n, v| {
            if cpu_index(n).is_some() && v > 0.0 {
                *nominal = Some(v);
            }
        });
    }
}

fn slot(cores: &mut Vec<Core>, i: usize) -> &mut Core {
    if cores.len() <= i {
        cores.resize(i + 1, Core::default());
    }
    &mut cores[i]
}

/// `0,12` → 12 (group * 64 + number); `_Total` and `0,_Total` → None.
fn cpu_index(name: &[u16]) -> Option<usize> {
    let comma = name.iter().position(|&c| c == b',' as u16)?;
    let num = |s: &[u16]| -> Option<usize> {
        if s.is_empty() || !s.iter().all(|&c| (b'0' as u16..=b'9' as u16).contains(&c)) {
            return None;
        }
        Some(s.iter().fold(0, |a, &c| a * 10 + (c - b'0' as u16) as usize))
    };
    Some(num(&name[..comma])? * 64 + num(&name[comma + 1..])?)
}

/// Samples a process tree on a background thread until `stop`.
pub struct Sampler {
    stop: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<Samples>,
}

impl Sampler {
    /// Samples the tree rooted at `root_pid` (0: every process) every 100 ms. Returns after the
    /// baseline collection, so ticks cover everything from the return on.
    pub fn start(root_pid: u32) -> Sampler {
        Sampler::every(root_pid, INTERVAL)
    }

    pub fn every(root_pid: u32, interval: Duration) -> Sampler {
        let (stop, stop_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("strobe-sampler".into())
            .spawn(move || run(root_pid, interval, stop_rx, ready))
            .expect("spawn the sampler thread");
        let _ = ready_rx.recv();
        Sampler { stop, thread }
    }

    /// Takes one last tick (so windows up to now are covered) and returns everything.
    pub fn stop(self) -> Samples {
        let _ = self.stop.send(());
        self.thread.join().unwrap_or_default()
    }
}

fn run(root: u32, interval: Duration, stop: mpsc::Receiver<()>, ready: mpsc::Sender<()>) -> Samples {
    let cpu0 = host::thread_cpu_ns();
    let tree = || {
        let mut p = host::tree_pids(root);
        p.sort_unstable();
        p
    };
    let mut probe = Probe::new();
    let mut pids = if root == 0 { Vec::new() } else { tree() };
    let mut refreshed = Instant::now();
    let setup_ns = host::thread_cpu_ns() - cpu0;
    let _ = ready.send(());
    let mut ticks = Vec::with_capacity(1024);
    let mut next = Instant::now() + interval;
    loop {
        let done = !matches!(stop.recv_timeout(next.saturating_duration_since(Instant::now())), Err(mpsc::RecvTimeoutError::Timeout));
        if root != 0 && refreshed.elapsed() >= TREE_REFRESH {
            pids = tree();
            refreshed = Instant::now();
        }
        ticks.push(probe.tick(&|pid| root == 0 || pids.binary_search(&pid).is_ok()));
        if done {
            break;
        }
        next += interval;
        let now = Instant::now();
        if next < now {
            next = now + interval;
        }
    }
    Samples {
        root,
        interval_ms: interval.as_millis() as u64,
        nominal_mhz: probe.nominal_mhz,
        gpu_available: probe.gpu_available(),
        engines: probe.engines().to_vec(),
        devices: probe.devices().to_vec(),
        adapters: probe.adapters,
        ticks,
        cpu_ms: (host::thread_cpu_ns() - cpu0 - setup_ns) as f64 / 1e6,
        setup_cpu_ms: setup_ns as f64 / 1e6,
    }
}

/// What a sampler collected. Ticks index into `adapters`, `engines` and `devices`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Samples {
    /// Root PID of the tracked tree; 0 = every process.
    pub root: u32,
    pub interval_ms: u64,
    /// Nominal (base) CPU frequency; a core's MHz is `nominal_mhz * perf / 100`.
    pub nominal_mhz: Option<f64>,
    /// False when the PDH GPU counters are missing (then GPU fields are None, not 0).
    pub gpu_available: bool,
    pub adapters: Vec<Adapter>,
    pub engines: Vec<String>,
    pub devices: Vec<Device>,
    pub ticks: Vec<Tick>,
    /// The sampler thread's own CPU time while sampling (rule 2), excluding setup.
    pub cpu_ms: f64,
    /// CPU time to open PDH, enumerate adapters and initialize NVML.
    pub setup_cpu_ms: f64,
}

/// Averages over a time window for the tracked tree. % values are time-weighted by each tick's
/// overlap with the window. MB = 2^20 bytes.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub t0_ns: u64,
    pub t1_ns: u64,
    /// Ticks that overlap the window.
    pub ticks: usize,
    /// Window time the ticks cover.
    pub covered_ms: f64,
    pub cores: Vec<CoreAvg>,
    /// The adapter the tree used most (engine time, then dedicated memory).
    pub gpu_adapter: Option<String>,
    /// `3D` engine time of the tree on `gpu_adapter`, summed over its processes, % of one engine.
    pub gpu_3d_pct: Option<f64>,
    /// Engine type → % of one engine on `gpu_adapter`.
    pub gpu_engines: BTreeMap<String, f64>,
    /// Adapter → engine type → %, every adapter the tree used.
    pub gpu_adapters: BTreeMap<String, BTreeMap<String, f64>>,
    /// The tree's dedicated GPU memory over all adapters, last value in the window.
    pub vram_dedicated_mb: Option<f64>,
    /// The tree's shared GPU memory over all adapters, last value in the window.
    pub vram_shared_mb: Option<f64>,
    /// NVML averages for the device behind `gpu_adapter` (the first device when the tree used none).
    pub device: Option<DeviceAvg>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CoreAvg {
    pub cpu: u32,
    pub busy_pct: f64,
    pub util_pct: f64,
    pub perf_pct: f64,
    pub mhz: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceAvg {
    pub name: String,
    #[serde(flatten)]
    pub avg: DeviceSample,
}

impl Samples {
    pub fn window(&self, t0: u64, t1: u64) -> Summary {
        let mut s = Summary { t0_ns: t0, t1_ns: t1, ..Summary::default() };
        let mut total = 0.0;
        let mut cores: Vec<[f64; 3]> = Vec::new();
        let mut engines: BTreeMap<(u16, u16), f64> = BTreeMap::new();
        let mut devices: Vec<[(f64, f64); 9]> = vec![[(0.0, 0.0); 9]; self.devices.len()];
        let mut last: Option<&Tick> = None;
        for t in &self.ticks {
            let w = t.t_ns.min(t1).saturating_sub(t.t_ns.saturating_sub(t.dt_ns).max(t0)) as f64;
            if w <= 0.0 {
                continue;
            }
            s.ticks += 1;
            total += w;
            last = Some(t);
            if cores.len() < t.cores.len() {
                cores.resize(t.cores.len(), [0.0; 3]);
            }
            for (acc, c) in cores.iter_mut().zip(&t.cores) {
                acc[0] += w * c.busy as f64;
                acc[1] += w * c.util as f64;
                acc[2] += w * c.perf as f64;
            }
            for e in &t.gpu {
                *engines.entry((e.adapter, e.engine)).or_default() += w * e.pct as f64;
            }
            for (acc, d) in devices.iter_mut().zip(&t.devices) {
                for (a, v) in acc.iter_mut().zip(d.fields()) {
                    if let Some(v) = v {
                        a.0 += w * v;
                        a.1 += w;
                    }
                }
            }
        }
        let Some(last) = last else { return s };
        s.covered_ms = total / 1e6;
        s.cores = cores
            .iter()
            .enumerate()
            .map(|(i, a)| CoreAvg {
                cpu: i as u32,
                busy_pct: a[0] / total,
                util_pct: a[1] / total,
                perf_pct: a[2] / total,
                mhz: self.nominal_mhz.map(|n| n * a[2] / total / 100.0),
            })
            .collect();
        let adapter_name = |i: u16| self.adapters.get(i as usize).map(|a| a.name.clone()).unwrap_or_else(|| format!("adapter {i}"));
        let mut by_adapter: BTreeMap<u16, (f64, BTreeMap<String, f64>)> = BTreeMap::new();
        for (&(a, e), &sum) in &engines {
            let entry = by_adapter.entry(a).or_default();
            entry.0 += sum / total;
            *entry.1.entry(self.engines.get(e as usize).cloned().unwrap_or_default()).or_default() += sum / total;
        }
        let mut dedicated: BTreeMap<u16, u64> = BTreeMap::new();
        for m in &last.vram {
            *dedicated.entry(m.adapter).or_default() += m.dedicated;
        }
        let primary = by_adapter
            .iter()
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
            .map(|(&a, _)| a)
            .or_else(|| dedicated.iter().filter(|d| *d.1 > 0).max_by_key(|d| *d.1).map(|(&a, _)| a));
        if self.gpu_available {
            s.gpu_adapter = primary.map(adapter_name);
            s.gpu_engines = primary.and_then(|p| by_adapter.get(&p)).map(|a| a.1.clone()).unwrap_or_default();
            s.gpu_3d_pct = Some(s.gpu_engines.get("3D").copied().unwrap_or(0.0));
            s.gpu_adapters = by_adapter.into_iter().map(|(a, (_, m))| (adapter_name(a), m)).collect();
            s.vram_dedicated_mb = Some(last.vram.iter().map(|m| m.dedicated).sum::<u64>() as f64 / MB);
            s.vram_shared_mb = Some(last.vram.iter().map(|m| m.shared).sum::<u64>() as f64 / MB);
        }
        let device = match primary {
            Some(p) => self.devices.iter().position(|d| d.adapter == Some(p)),
            None => (!self.devices.is_empty()).then_some(0),
        };
        if let Some(i) = device {
            let mut f = devices[i].map(|(sum, w)| (w > 0.0).then(|| sum / w));
            f[8] = last.devices.get(i).and_then(|d| d.vram_used_mb);
            s.device = Some(DeviceAvg { name: self.devices[i].name.clone(), avg: DeviceSample::from_fields(f) });
        }
        s
    }
}

/// A PDH query with reusable buffers. Wildcard counters re-enumerate instances on every collection.
#[cfg(windows)]
pub mod pdh {
    use windows_sys::Win32::System::Performance::{
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
        PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    };

    pub const NOSCALE: u32 = 0x1000;
    pub const NOCAP100: u32 = 0x8000;

    pub struct Query {
        h: isize,
        buf: Vec<u64>,
    }

    impl Query {
        pub fn open() -> Option<Query> {
            let mut h = 0isize;
            (unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut h) } == 0).then(|| Query { h, buf: vec![0u64; 16 * 1024] })
        }

        pub fn add(&self, path: &str) -> Option<isize> {
            let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let mut c = 0isize;
            (unsafe { PdhAddEnglishCounterW(self.h, w.as_ptr(), 0, &mut c) } == 0).then_some(c)
        }

        pub fn collect(&self) -> bool {
            unsafe { PdhCollectQueryData(self.h) == 0 }
        }

        /// Calls `f(instance, value)` for every instance with valid data in the last collection.
        pub fn each(&mut self, counter: isize, flags: u32, mut f: impl FnMut(&[u16], f64)) {
            let mut count = 0u32;
            let mut rc = PDH_MORE_DATA;
            for _ in 0..3 {
                let mut size = (self.buf.len() * 8) as u32;
                rc = unsafe {
                    PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE | flags, &mut size, &mut count, self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W)
                };
                if rc != PDH_MORE_DATA {
                    break;
                }
                self.buf.resize((size as usize / 8 + 1) * 5 / 4, 0);
            }
            if rc != 0 {
                return;
            }
            let items = unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, count as usize) };
            for item in items {
                if item.FmtValue.CStatus > 1 || item.szName.is_null() {
                    continue;
                }
                let name = unsafe {
                    let mut n = 0usize;
                    while *item.szName.add(n) != 0 {
                        n += 1;
                    }
                    std::slice::from_raw_parts(item.szName, n)
                };
                f(name, unsafe { item.FmtValue.Anonymous.doubleValue });
            }
        }
    }

    impl Drop for Query {
        fn drop(&mut self) {
            unsafe { PdhCloseQuery(self.h) };
        }
    }
}

#[cfg(not(windows))]
pub mod pdh {
    pub const NOSCALE: u32 = 0x1000;
    pub const NOCAP100: u32 = 0x8000;

    pub struct Query;

    impl Query {
        pub fn open() -> Option<Query> {
            None
        }
        pub fn add(&self, _path: &str) -> Option<isize> {
            None
        }
        pub fn collect(&self) -> bool {
            false
        }
        pub fn each(&mut self, _counter: isize, _flags: u32, _f: impl FnMut(&[u16], f64)) {}
    }
}
