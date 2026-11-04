//! Outside-in OS and GPU sampling (SPEC §8.5, §13.6) on one background thread, with timestamps
//! from `host::now_ns` (QPC). Each tick covers `(t_ns - dt_ns, t_ns]`.
//!
//! - per-CPU busy: idle cycle counts against the TSC, every tick (exact, a few µs)
//! - per-CPU frequency and utility: PDH `Processor Information`, once a second (its provider
//!   costs about 0.25 ms per collection)
//! - per-process GPU engines and memory: D3DKMT for the tracked PIDs (PDH engines for every
//!   process in `top`)
//! - device clocks, power, temperature, PCIe: NVML (`gpu.rs`)

use super::gpu::{self, Adapter, Device, DeviceSample, EngineUse, VramUse};
use crate::host;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default tick.
pub const INTERVAL: Duration = Duration::from_millis(100);
/// How often the costlier readings are taken: PDH frequency (~0.25 ms), NVML PCIe (~0.1 ms).
const SLOW_NS: u64 = 950_000_000;
/// The sampler re-reads which PIDs belong to the tree at most this often, and less often when
/// the system snapshot is expensive (it costs ~7 ms with 1,000 processes): see `run`.
const TREE_REFRESH: Duration = Duration::from_secs(1);
/// The tree refresh may use at most 1/500 of the sampler's time (0.2% of a core).
const TREE_SHARE: u32 = 500;
const MB: f64 = 1024.0 * 1024.0;

/// Everything one collection produced.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Tick {
    pub t_ns: u64,
    pub dt_ns: u64,
    /// Per logical CPU (group * 64 + number): % of the tick it was busy, from idle cycles.
    pub busy: Vec<f32>,
    pub gpu: Vec<EngineUse>,
    pub vram: Vec<VramUse>,
    /// Indexed like `Samples::devices`.
    pub devices: Vec<DeviceSample>,
    /// About once a second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freq: Option<Freq>,
}

/// PDH `Processor Information` per logical CPU, averaged over `(t_ns - dt_ns, t_ns]`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Freq {
    pub dt_ns: u64,
    /// `% Processor Performance`: average frequency against nominal; above 100 when boosting.
    pub perf: Vec<f32>,
    /// `% Processor Utility`: busy scaled by frequency (Task Manager's CPU); can exceed 100.
    pub util: Vec<f32>,
}

/// The collectors, reusable on any thread, and the adapter and engine name tables ticks index into.
pub struct Probe {
    cpus: Cpus,
    /// Engine time for every process (PDH); only with `all`.
    all: Option<gpu::Engines>,
    kmt: Option<gpu::Kmt>,
    nvml: Option<gpu::Nvml>,
    pub adapters: Vec<Adapter>,
    pub engines: Vec<String>,
    pub nominal_mhz: Option<f64>,
    /// With `all`: PIDs that had a GPU context at the last tick (sorted).
    pub present: Vec<u32>,
    last_ns: u64,
    slow_ns: u64,
}

impl Probe {
    /// Opens every source and takes the baseline collection rates need. GPU rows cover the
    /// PIDs given to `tick` (D3DKMT, cost scales with them); with `all`, engine rows cover every
    /// process instead (PDH, about 0.5 ms per tick) and memory comes from `vram`.
    pub fn new(all: bool, pids: &[u32]) -> Probe {
        let adapters = gpu::adapters();
        let mut engines = Vec::new();
        let kmt = gpu::Kmt::new(&adapters, &mut engines);
        let nvml = gpu::Nvml::load(&adapters);
        let all = if all { gpu::Engines::open() } else { None };
        let mut p = Probe { cpus: Cpus::open(), all, kmt, nvml, adapters, engines, nominal_mhz: None, present: Vec::new(), last_ns: 0, slow_ns: 0 };
        p.tick(pids);
        p
    }

    /// GPU memory of `pids` (sorted) now, for a probe opened with `all`.
    pub fn vram(&mut self, pids: &[u32]) -> Vec<VramUse> {
        let mut out = Vec::new();
        if let Some(k) = self.kmt.as_mut() {
            k.read(pids, 0, false, &mut Vec::new(), &mut out);
        }
        out
    }

    /// Collects once. `pids` (sorted) are the processes whose GPU rows are read, unless `all`.
    pub fn tick(&mut self, pids: &[u32]) -> Tick {
        let t = host::now_ns();
        let dt = if self.last_ns == 0 { 0 } else { t - self.last_ns };
        self.last_ns = t;
        let slow = self.slow_ns == 0 || t - self.slow_ns >= SLOW_NS;
        let slow_dt = if self.slow_ns == 0 { 0 } else { t - self.slow_ns };
        if slow {
            self.slow_ns = t;
        }
        let mut tick = Tick { t_ns: t, dt_ns: dt, ..Tick::default() };
        tick.busy = self.cpus.busy();
        if slow {
            tick.freq = self.cpus.freq(slow_dt, &mut self.nominal_mhz);
        }
        match (self.all.as_mut(), self.kmt.as_mut()) {
            (Some(e), _) => {
                self.present.clear();
                e.read(&mut self.adapters, &mut self.engines, &mut tick.gpu, &mut self.present);
            }
            (None, Some(k)) => k.read(pids, dt, true, &mut tick.gpu, &mut tick.vram),
            (None, None) => {}
        }
        if let Some(n) = self.nvml.as_mut() {
            n.sample(t, slow, &mut tick.devices);
        }
        tick
    }

    pub fn gpu_available(&self) -> bool {
        self.kmt.is_some()
    }

    /// PIDs whose GPU statistics could not be opened (another user, protected).
    pub fn gpu_unreadable(&self) -> &[u32] {
        self.kmt.as_ref().map(|k| k.unreadable.as_slice()).unwrap_or(&[])
    }

    pub fn devices(&self) -> &[Device] {
        self.nvml.as_ref().map(|n| n.devices.as_slice()).unwrap_or(&[])
    }
}

struct Cpus {
    idle: Vec<u64>,
    prev: Vec<u64>,
    tsc: u64,
    pdh: Option<CpuPdh>,
}

struct CpuPdh {
    q: pdh::Query,
    util: isize,
    perf: isize,
    freq: isize,
}

impl Cpus {
    fn open() -> Cpus {
        let pdh = pdh::Query::open().and_then(|q| {
            Some(CpuPdh {
                util: q.add(r"\Processor Information(*)\% Processor Utility")?,
                perf: q.add(r"\Processor Information(*)\% Processor Performance")?,
                freq: q.add(r"\Processor Information(*)\Processor Frequency")?,
                q,
            })
        });
        Cpus { idle: Vec::new(), prev: Vec::new(), tsc: 0, pdh }
    }

    /// Busy % per CPU since the last call: 1 - Δidle cycles / ΔTSC. Empty on the first call.
    fn busy(&mut self) -> Vec<f32> {
        let ok = host::idle_cycles(&mut self.idle);
        let Some(tsc) = host::tsc().filter(|_| ok) else { return Vec::new() };
        let span = tsc.saturating_sub(self.tsc) as f64;
        let busy = if self.tsc > 0 && span > 0.0 && self.prev.len() == self.idle.len() {
            self.idle.iter().zip(&self.prev).map(|(i, p)| (100.0 * (1.0 - i.saturating_sub(*p) as f64 / span)).clamp(0.0, 100.0) as f32).collect()
        } else {
            Vec::new()
        };
        std::mem::swap(&mut self.idle, &mut self.prev);
        self.tsc = tsc;
        busy
    }

    /// Collects the PDH counters; the reading covers the `dt` since the last collection.
    fn freq(&mut self, dt: u64, nominal: &mut Option<f64>) -> Option<Freq> {
        #[cfg(target_os = "linux")]
        if self.pdh.is_none() {
            return linux_freq(dt, nominal);
        }
        let p = self.pdh.as_mut()?;
        p.q.collect();
        let mut f = Freq { dt_ns: dt, ..Freq::default() };
        let flags = pdh::NOSCALE | pdh::NOCAP100;
        for (counter, out) in [(p.perf, &mut f.perf), (p.util, &mut f.util)] {
            p.q.each(counter, flags, |n, v| {
                if let Some(i) = cpu_index(n) {
                    if out.len() <= i {
                        out.resize(i + 1, 0.0);
                    }
                    out[i] = v as f32;
                }
            });
        }
        p.q.each(p.freq, flags, |n, v| {
            if cpu_index(n).is_some() && v > 0.0 {
                *nominal = Some(v);
            }
        });
        (dt > 0).then_some(f)
    }
}

/// Linux: each CPU's average frequency over the last interval the kernel measured
/// (`cpuinfo_avg_freq`, from the APERF/MPERF counters, kernel 6.13+) against its base frequency. Older
/// kernels only give an instantaneous `scaling_cur_freq`, which is not an average: no reading then.
#[cfg(target_os = "linux")]
fn linux_freq(dt: u64, nominal: &mut Option<f64>) -> Option<Freq> {
    let read = |cpu: usize, f: &str| std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/{f}")).ok().and_then(|s| s.trim().parse::<f64>().ok());
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut f = Freq { dt_ns: dt, ..Freq::default() };
    for cpu in 0..n {
        let (avg, base) = (read(cpu, "cpuinfo_avg_freq")?, read(cpu, "base_frequency").or_else(|| read(cpu, "cpuinfo_max_freq"))?);
        *nominal = Some(base / 1000.0);
        f.perf.push((100.0 * avg / base) as f32);
    }
    (dt > 0).then_some(f)
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
    /// Samples the tree rooted at `root_pid` every 100 ms (0: no process rows, only CPUs and
    /// devices). Returns after the baseline collection, so ticks cover everything from the
    /// return on.
    pub fn start(root_pid: u32) -> Sampler {
        Sampler::every(root_pid, INTERVAL)
    }

    pub fn every(root_pid: u32, interval: Duration) -> Sampler {
        let (stop, stop_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("strobe-sampler".into()).spawn(move || run(root_pid, interval, stop_rx, ready)).expect("spawn the sampler thread");
        let _ = ready_rx.recv();
        Sampler { stop, thread }
    }

    /// Takes one last tick (so windows up to now are covered; skipped when the previous one is
    /// under 5 ms old) and returns everything.
    pub fn stop(self) -> Samples {
        let _ = self.stop.send(());
        self.thread.join().unwrap_or_default()
    }
}

fn run(root: u32, interval: Duration, stop: mpsc::Receiver<()>, ready: mpsc::Sender<()>) -> Samples {
    let (k0, tsc0, ns0) = (host::thread_cycles(), host::tsc(), host::now_ns());
    let tree = || {
        let mut p = if root == 0 { Vec::new() } else { host::tree_pids(root) };
        p.sort_unstable();
        p
    };
    let started = Instant::now();
    let mut pids = tree();
    let mut refresh = TREE_REFRESH.max(started.elapsed() * TREE_SHARE);
    let mut probe = Probe::new(false, &pids);
    let mut refreshed = Instant::now();
    let setup = host::thread_cycles() - k0;
    let _ = ready.send(());
    let mut ticks = Vec::with_capacity(1024);
    let mut last = Instant::now();
    let mut next = last + interval;
    loop {
        let done = !matches!(stop.recv_timeout(next.saturating_duration_since(Instant::now())), Err(mpsc::RecvTimeoutError::Timeout));
        // A stop right after a tick leaves nothing worth a reading (rates over a few µs).
        if done && last.elapsed() < Duration::from_millis(5) {
            break;
        }
        if root != 0 && refreshed.elapsed() >= refresh {
            let t = Instant::now();
            pids = tree();
            refresh = TREE_REFRESH.max(t.elapsed() * TREE_SHARE);
            refreshed = Instant::now();
        }
        ticks.push(probe.tick(&pids));
        last = Instant::now();
        if done {
            break;
        }
        next += interval;
        let now = Instant::now();
        if next < now {
            next = now + interval;
        }
    }
    // Thread cycles are TSC units: convert with the TSC rate measured over the run.
    let spent = host::thread_cycles() - k0 - setup;
    let hz = tsc0.zip(host::tsc()).map(|(a, b)| (b - a) as f64 / ((host::now_ns() - ns0) as f64 / 1e9)).filter(|&hz| hz > 0.0 && k0 > 0);
    Samples {
        cpu_ms: hz.map(|hz| spent as f64 / hz * 1e3),
        setup_cpu_ms: hz.map(|hz| setup as f64 / hz * 1e3),
        tree_refresh_ms: (root != 0).then_some(refresh.as_secs_f64() * 1e3),
        root,
        interval_ms: interval.as_millis() as u64,
        nominal_mhz: probe.nominal_mhz,
        gpu_available: probe.gpu_available(),
        gpu_unreadable: probe.gpu_unreadable().to_vec(),
        devices: probe.devices().to_vec(),
        engines: std::mem::take(&mut probe.engines),
        adapters: std::mem::take(&mut probe.adapters),
        ticks,
    }
}

/// What a sampler collected. Ticks index into `adapters`, `engines` and `devices`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Samples {
    /// Root PID of the tracked tree; 0 = none (CPUs and devices only).
    pub root: u32,
    pub interval_ms: u64,
    /// Nominal (base) CPU frequency; a CPU's MHz is `nominal_mhz * perf / 100`.
    pub nominal_mhz: Option<f64>,
    /// False when no per-process GPU source opened (then GPU fields are None, not 0).
    pub gpu_available: bool,
    /// Tree PIDs whose GPU statistics could not be opened: GPU sums exclude them.
    pub gpu_unreadable: Vec<u32>,
    pub adapters: Vec<Adapter>,
    pub engines: Vec<String>,
    pub devices: Vec<Device>,
    pub ticks: Vec<Tick>,
    /// The sampler thread's own CPU time while sampling (rule 2), excluding setup; cycle-exact.
    /// None where thread cycles cannot be converted to time (no TSC).
    pub cpu_ms: Option<f64>,
    /// CPU time to open PDH, enumerate adapters and initialize NVML.
    pub setup_cpu_ms: Option<f64>,
    /// How often the tree's PIDs were re-read at the end (1 s, longer when the system snapshot
    /// is expensive). A child is tracked from the next refresh after it starts; its GPU engine
    /// time still counts from its start.
    pub tree_refresh_ms: Option<f64>,
}

/// Averages over a time window for the tracked tree, weighted by each reading's overlap with
/// the window. MB = 2^20 bytes.
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
    /// `3D` engine time of the tree on every adapter, summed over its processes, % of one engine.
    pub gpu_3d_pct: Option<f64>,
    /// Engine type → % of one engine on `gpu_adapter`.
    pub gpu_engines: BTreeMap<String, f64>,
    /// Adapter → engine type → %, every adapter the tree used.
    pub gpu_adapters: BTreeMap<String, BTreeMap<String, f64>>,
    /// The tree's dedicated GPU memory over all adapters, last value in the window.
    pub vram_dedicated_mb: Option<f64>,
    /// The tree's shared GPU memory over all adapters, last value in the window.
    pub vram_shared_mb: Option<f64>,
    /// Tree PIDs the GPU fields exclude because their statistics could not be opened (a warning).
    pub gpu_unreadable_pids: Vec<u32>,
    /// NVML averages for the device behind `gpu_adapter` (the first device when the tree used
    /// none); `vram_used_mb` is the last value.
    pub device: Option<DeviceAvg>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CoreAvg {
    pub cpu: u32,
    pub busy_pct: Option<f64>,
    pub util_pct: Option<f64>,
    pub perf_pct: Option<f64>,
    pub mhz: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceAvg {
    pub name: String,
    #[serde(flatten)]
    pub avg: DeviceSample,
}

/// Overlap of `(t - dt, t]` with `[t0, t1]`, in ns.
fn overlap(t: u64, dt: u64, t0: u64, t1: u64) -> f64 {
    t.min(t1).saturating_sub(t.saturating_sub(dt).max(t0)) as f64
}

fn add(acc: &mut Vec<(f64, f64)>, values: &[f32], w: f64) {
    if acc.len() < values.len() {
        acc.resize(values.len(), (0.0, 0.0));
    }
    for (a, &v) in acc.iter_mut().zip(values) {
        a.0 += w * v as f64;
        a.1 += w;
    }
}

impl Samples {
    pub fn window(&self, t0: u64, t1: u64) -> Summary {
        let mut s = Summary { t0_ns: t0, t1_ns: t1, ..Summary::default() };
        let mut total = 0.0;
        let (mut busy, mut perf, mut util) = (Vec::new(), Vec::new(), Vec::new());
        let mut engines: BTreeMap<(u16, u16), f64> = BTreeMap::new();
        let mut devices: Vec<[(f64, f64); 9]> = vec![[(0.0, 0.0); 9]; self.devices.len()];
        let mut last: Option<&Tick> = None;
        for t in &self.ticks {
            if let Some(f) = &t.freq {
                let w = overlap(t.t_ns, f.dt_ns, t0, t1);
                if w > 0.0 {
                    add(&mut perf, &f.perf, w);
                    add(&mut util, &f.util, w);
                }
            }
            let w = overlap(t.t_ns, t.dt_ns, t0, t1);
            if w <= 0.0 {
                continue;
            }
            s.ticks += 1;
            total += w;
            last = Some(t);
            add(&mut busy, &t.busy, w);
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
        let avg = |acc: &Vec<(f64, f64)>, i: usize| acc.get(i).filter(|a| a.1 > 0.0).map(|a| a.0 / a.1);
        s.cores = (0..busy.len().max(perf.len()))
            .map(|i| CoreAvg { cpu: i as u32, busy_pct: avg(&busy, i), util_pct: avg(&util, i), perf_pct: avg(&perf, i), mhz: self.nominal_mhz.zip(avg(&perf, i)).map(|(n, p)| n * p / 100.0) })
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
        let primary = by_adapter.iter().max_by(|a, b| a.1.0.total_cmp(&b.1.0)).map(|(&a, _)| a).or_else(|| dedicated.iter().filter(|d| *d.1 > 0).max_by_key(|d| *d.1).map(|(&a, _)| a));
        if self.gpu_available {
            s.gpu_adapter = primary.map(adapter_name);
            s.gpu_engines = primary.and_then(|p| by_adapter.get(&p)).map(|a| a.1.clone()).unwrap_or_default();
            // Every adapter: a process that draws on one GPU and computes on another uses both, and
            // which one is busiest in a window must not decide what is counted.
            s.gpu_3d_pct = Some(by_adapter.values().map(|a| a.1.get("3D").copied().unwrap_or(0.0)).sum());
            s.gpu_adapters = by_adapter.into_iter().map(|(a, (_, m))| (adapter_name(a), m)).collect();
            // Apple GPUs share system memory: no per-process VRAM to report (refused, never 0).
            if cfg!(not(target_os = "macos")) {
                s.vram_dedicated_mb = Some(last.vram.iter().map(|m| m.dedicated).sum::<u64>() as f64 / MB);
                s.vram_shared_mb = Some(last.vram.iter().map(|m| m.shared).sum::<u64>() as f64 / MB);
            }
            s.gpu_unreadable_pids = self.gpu_unreadable.clone();
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

/// A PDH query with a reusable buffer. Wildcard counters re-enumerate instances on every collection.
#[cfg(windows)]
pub mod pdh {
    use windows_sys::Win32::System::Performance::{
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
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
                rc = unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE | flags, &mut size, &mut count, self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W) };
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
