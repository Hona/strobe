//! `strobe top`: live per-core load by CCD, processes with CPU, memory, GPU and VRAM, and the
//! NVML device line (SPEC §5.9). The same collectors as the OS and GPU probes (`probe::os`).

use super::Output;
use crate::cli::{Args, duration_ms};
use crate::host::{self, Topology};
use crate::model::{Error, Result};
use crate::probe::gpu::DeviceSample;
use crate::probe::os::{Probe, Sampler, Samples, Summary, Tick};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Write};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const MB: f64 = 1024.0 * 1024.0;

pub fn run(args: &Args) -> Result<Output> {
    let interval = Duration::from_millis(args.get("interval").map(duration_ms).transpose()?.unwrap_or(1000).max(200));
    let target = args.get("target").map(|t| t.parse::<u32>().map_err(|_| Error::usage(format!("--target: not a PID: {t}")))).transpose()?;
    let limit = args.num("limit")?.map(|n| n.max(1.0) as usize).unwrap_or(20);
    let sort = args.get("sort").unwrap_or("cpu").to_string();
    if !["cpu", "gpu", "vram", "mem"].contains(&sort.as_str()) {
        return Err(Error::usage(format!("--sort: {sort}; use cpu, gpu, vram or mem")));
    }
    if args.json() && !args.has("once") {
        return Err(Error::usage("--json needs --once"));
    }
    if let Some(pid) = target
        && host::tree_pids(pid).is_empty()
    {
        return Err(Error::msg(format!("no process with PID {pid}")));
    }
    let mut top = Top::new(target, limit, sort);
    if args.has("once") {
        let sampler = target.map(Sampler::start);
        let t0 = host::now_ns();
        std::thread::sleep(interval);
        let frame = top.frame();
        let samples = sampler.map(Sampler::stop);
        let summary = samples.as_ref().map(|s| s.window(t0, host::now_ns()));
        let text = render(&frame, &top.topo, &top.cpu_name, samples.as_ref().zip(summary.as_ref()), false);
        let json = serde_json::json!({
            "frame": frame,
            "target": summary.as_ref().map(|s| serde_json::json!({
                "summary": s,
                "sampler": samples.as_ref().map(|x| serde_json::json!({ "interval_ms": x.interval_ms, "ticks": x.ticks.len(), "cpu_ms": x.cpu_ms, "setup_cpu_ms": x.setup_cpu_ms })),
            })),
        });
        return Ok(Output::new(text, json));
    }
    let stop = host::ctrl_c();
    let ansi = std::io::stdout().is_terminal() && host::enable_ansi();
    let mut out = std::io::stdout();
    if ansi {
        let _ = write!(out, "\x1b[?1049h\x1b[?25l");
    }
    let mut next = Instant::now() + interval;
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next {
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
            continue;
        }
        next = (next + interval).max(now + interval / 2);
        let frame = top.frame();
        let text = render(&frame, &top.topo, &top.cpu_name, None, true);
        let _ = if ansi { write!(out, "\x1b[H{}\x1b[J", text.replace('\n', "\x1b[K\n")) } else { writeln!(out, "{text}") };
        let _ = out.flush();
    }
    if ansi {
        let _ = write!(out, "\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
    Ok(Output::new(String::new(), Value::Null))
}

struct Top {
    probe: Probe,
    prev: HashMap<u32, u64>,
    prev_t: u64,
    target: Option<u32>,
    limit: usize,
    sort: String,
    topo: Topology,
    cpu_name: String,
}

#[derive(Serialize)]
struct Frame {
    elapsed_ms: f64,
    cores: Vec<CoreRow>,
    groups: Vec<GroupRow>,
    devices: Vec<DeviceRow>,
    /// Adapter → engine type → % of one engine, summed over every process.
    adapters: BTreeMap<String, BTreeMap<String, f64>>,
    /// Sorted by `sort`; the target's tree only with `--target`.
    processes: Vec<ProcRow>,
    sort: String,
    target: Option<u32>,
    total: Option<ProcRow>,
}

#[derive(Serialize)]
struct CoreRow {
    cpu: usize,
    busy_pct: f64,
    util_pct: f64,
    perf_pct: f64,
    mhz: Option<f64>,
}

#[derive(Serialize)]
struct GroupRow {
    name: String,
    cpus: Vec<usize>,
    l3_mb: u32,
    vcache: bool,
    busy_pct: f64,
    perf_pct: f64,
    mhz: Option<f64>,
}

#[derive(Serialize)]
struct DeviceRow {
    name: String,
    vram_total_mb: Option<f64>,
    #[serde(flatten)]
    sample: DeviceSample,
}

/// CPU in % of one core over the frame; GPU in % of one engine; MB = 2^20 bytes.
#[derive(Clone, Default, Serialize)]
struct ProcRow {
    pid: u32,
    parent: u32,
    name: String,
    cpu_pct: f64,
    private_mb: f64,
    gpu_3d_pct: f64,
    /// The busiest engine type on any adapter (Task Manager's GPU column).
    gpu_pct: f64,
    gpu_engine: String,
    vram_dedicated_mb: f64,
    vram_shared_mb: f64,
}

impl Top {
    fn new(target: Option<u32>, limit: usize, sort: String) -> Top {
        let probe = Probe::new();
        let prev = host::all_processes().into_iter().map(|p| (p.pid, p.cpu_ns)).collect();
        Top { probe, prev, prev_t: host::now_ns(), target, limit, sort, topo: host::topology(), cpu_name: host::env().cpu }
    }

    fn frame(&mut self) -> Frame {
        let tick = self.probe.tick(&|_| true);
        let procs = host::all_processes();
        let t = host::now_ns();
        let dt = t.saturating_sub(self.prev_t).max(1) as f64;
        self.prev_t = t;
        let tree = self.target.map(|pid| {
            let mut p = host::tree_pids(pid);
            p.sort_unstable();
            p
        });
        let engines = self.probe.engines();
        let mut gpu: HashMap<u32, HashMap<(u16, u16), f64>> = HashMap::new();
        for e in &tick.gpu {
            *gpu.entry(e.pid).or_default().entry((e.adapter, e.engine)).or_default() += e.pct as f64;
        }
        let mut vram: HashMap<u32, (u64, u64)> = HashMap::new();
        for m in &tick.vram {
            let v = vram.entry(m.pid).or_default();
            v.0 += m.dedicated;
            v.1 += m.shared;
        }
        let mut rows: Vec<ProcRow> = procs
            .iter()
            .filter(|p| p.pid != 0 && tree.as_ref().is_none_or(|t| t.binary_search(&p.pid).is_ok()))
            .map(|p| {
                let cpu = self.prev.get(&p.pid).map(|&c0| p.cpu_ns.saturating_sub(c0) as f64 / dt * 100.0).unwrap_or(0.0);
                let use_ = gpu.get(&p.pid);
                let engine_name = |e: u16| engines.get(e as usize).map(String::as_str).unwrap_or("");
                let gpu_3d = use_.map(|u| u.iter().filter(|((_, e), _)| engine_name(*e) == "3D").map(|(_, &v)| v).fold(0.0, f64::max)).unwrap_or(0.0);
                let busiest = use_.and_then(|u| u.iter().max_by(|a, b| a.1.total_cmp(b.1)));
                let (d, s) = vram.get(&p.pid).copied().unwrap_or_default();
                ProcRow {
                    pid: p.pid,
                    parent: p.parent,
                    name: p.name.clone(),
                    cpu_pct: cpu,
                    private_mb: p.private_bytes as f64 / MB,
                    gpu_3d_pct: gpu_3d,
                    gpu_pct: busiest.map(|b| *b.1).unwrap_or(0.0),
                    gpu_engine: busiest.map(|b| engine_name(b.0.1).to_string()).unwrap_or_default(),
                    vram_dedicated_mb: d as f64 / MB,
                    vram_shared_mb: s as f64 / MB,
                }
            })
            .collect();
        self.prev = procs.iter().map(|p| (p.pid, p.cpu_ns)).collect();
        let key = |r: &ProcRow| match self.sort.as_str() {
            "gpu" => r.gpu_pct,
            "vram" => r.vram_dedicated_mb,
            "mem" => r.private_mb,
            _ => r.cpu_pct,
        };
        rows.sort_by(|a, b| key(b).total_cmp(&key(a)));
        let total = tree.is_some().then(|| ProcRow {
            name: format!("tree of {}", self.target.unwrap_or(0)),
            pid: self.target.unwrap_or(0),
            cpu_pct: rows.iter().map(|r| r.cpu_pct).sum(),
            private_mb: rows.iter().map(|r| r.private_mb).sum(),
            gpu_3d_pct: rows.iter().map(|r| r.gpu_3d_pct).sum(),
            vram_dedicated_mb: rows.iter().map(|r| r.vram_dedicated_mb).sum(),
            vram_shared_mb: rows.iter().map(|r| r.vram_shared_mb).sum(),
            ..ProcRow::default()
        });
        if tree.is_none() {
            rows.truncate(self.limit);
        }
        let mut adapters: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        for e in &tick.gpu {
            let a = self.probe.adapters.get(e.adapter as usize).map(|a| a.name.clone()).unwrap_or_default();
            *adapters.entry(a).or_default().entry(engines.get(e.engine as usize).cloned().unwrap_or_default()).or_default() += e.pct as f64;
        }
        let (cores, groups) = self.cores(&tick);
        let devices = self.probe.devices().iter().zip(&tick.devices).map(|(d, s)| DeviceRow { name: d.name.clone(), vram_total_mb: d.vram_total_mb, sample: *s }).collect();
        Frame { elapsed_ms: dt / 1e6, cores, groups, devices, adapters, processes: rows, sort: self.sort.clone(), target: self.target, total }
    }

    fn cores(&self, tick: &Tick) -> (Vec<CoreRow>, Vec<GroupRow>) {
        let mhz = |perf: f64| self.probe.nominal_mhz.map(|n| n * perf / 100.0);
        let cores: Vec<CoreRow> = tick
            .cores
            .iter()
            .enumerate()
            .map(|(i, c)| CoreRow { cpu: i, busy_pct: c.busy as f64, util_pct: c.util as f64, perf_pct: c.perf as f64, mhz: mhz(c.perf as f64) })
            .collect();
        let vcache = self.topo.vcache().map(|g| g.mask);
        let mut groups: Vec<GroupRow> = self
            .topo
            .groups
            .iter()
            .enumerate()
            .map(|(i, g)| (format!("CCD{i}"), (0..64).filter(|b| g.mask & (1u64 << b) != 0).collect::<Vec<usize>>(), g.l3_mb, vcache == Some(g.mask)))
            .chain(std::iter::once(("CPUs".to_string(), Vec::new(), 0, false)))
            .map(|(name, cpus, l3_mb, vcache)| GroupRow { name, cpus, l3_mb, vcache, busy_pct: 0.0, perf_pct: 0.0, mhz: None })
            .collect();
        if groups.len() == 1 {
            groups[0].cpus = (0..cores.len()).collect();
        } else {
            groups.pop();
        }
        for g in &mut groups {
            g.cpus.retain(|&c| c < cores.len());
            let n = g.cpus.len().max(1) as f64;
            g.busy_pct = g.cpus.iter().map(|&c| cores[c].busy_pct).sum::<f64>() / n;
            g.perf_pct = g.cpus.iter().map(|&c| cores[c].perf_pct).sum::<f64>() / n;
            g.mhz = mhz(g.perf_pct);
        }
        (cores, groups)
    }
}

fn render(f: &Frame, topo: &Topology, cpu_name: &str, target: Option<(&Samples, &Summary)>, live: bool) -> String {
    let mut t = String::new();
    let busy = f.cores.iter().map(|c| c.busy_pct).sum::<f64>() / f.cores.len().max(1) as f64;
    t += &format!(
        "strobe top  {:.1} s  {}  {} logical CPUs  {:.0}% busy{}\n",
        f.elapsed_ms / 1000.0,
        cpu_name,
        topo.logical,
        busy,
        if live { "  (Ctrl+C quits)" } else { "" }
    );
    for g in &f.groups {
        let l3 = if g.l3_mb > 0 { format!("  {} MB L3", g.l3_mb) } else { String::new() };
        t += &format!(
            "\n{}{}{}  {} CPUs  busy {:.0}%  freq {:.0}%{}\n",
            g.name,
            l3,
            if g.vcache { "  V-Cache" } else { "" },
            g.cpus.len(),
            g.busy_pct,
            g.perf_pct,
            g.mhz.map(|m| format!(" ({:.2} GHz)", m / 1000.0)).unwrap_or_default()
        );
        for row in g.cpus.chunks(4) {
            let cells: Vec<String> = row
                .iter()
                .map(|&c| {
                    let core = &f.cores[c];
                    let fill = ((core.busy_pct / 100.0 * 8.0).round() as usize).min(8);
                    format!("{c:>3} {}{} {:>3.0}% {:>3.0}%", "#".repeat(fill), ".".repeat(8 - fill), core.busy_pct, core.perf_pct)
                })
                .collect();
            t += &format!("  {}\n", cells.join("    "));
        }
    }
    t += "      (per CPU: busy %, frequency % of nominal)\n";
    for d in &f.devices {
        let s = &d.sample;
        let mut parts: Vec<String> = Vec::new();
        if let Some(v) = s.gpu_pct {
            parts.push(format!("util {v:.0}%"));
        }
        if let Some(v) = s.mem_pct {
            parts.push(format!("mem {v:.0}%"));
        }
        if let (Some(g), Some(m)) = (s.gfx_mhz, s.mem_mhz) {
            parts.push(format!("{g:.0} / {m:.0} MHz"));
        }
        if let Some(v) = s.power_w {
            parts.push(format!("{v:.1} W"));
        }
        if let Some(v) = s.temp_c {
            parts.push(format!("{v:.0} C"));
        }
        if let (Some(tx), Some(rx)) = (s.pcie_tx_mbs, s.pcie_rx_mbs) {
            parts.push(format!("PCIe tx {tx:.1} rx {rx:.1} MB/s"));
        }
        if let Some(v) = s.vram_used_mb {
            parts.push(format!("VRAM {:.1}{} GB", v / 1024.0, d.vram_total_mb.map(|t| format!(" / {:.1}", t / 1024.0)).unwrap_or_default()));
        }
        t += &format!("\nGPU   {}  {}\n", d.name, parts.join("  "));
    }
    for (adapter, engines) in &f.adapters {
        let mut e: Vec<(&String, &f64)> = engines.iter().filter(|e| *e.1 >= 0.05).collect();
        e.sort_by(|a, b| b.1.total_cmp(a.1));
        if !e.is_empty() {
            let list: Vec<String> = e.iter().take(5).map(|(n, v)| format!("{n} {v:.1}%")).collect();
            t += &format!("      {adapter}: {}\n", list.join("  "));
        }
    }
    t += &format!(
        "\n{:>7}  {:<24} {:>7} {:>10} {:>7} {:>7}  {:<12} {:>10}\n",
        "PID", "NAME", "CPU", "PRIVATE", "3D", "GPU", "ENGINE", "VRAM"
    );
    let line = |r: &ProcRow| {
        let name: String = r.name.chars().take(24).collect();
        format!(
            "{:>7}  {:<24} {:>6.1}% {:>7.1} MB {:>6.1}% {:>6.1}%  {:<12} {:>7.1} MB\n",
            r.pid, name, r.cpu_pct, r.private_mb, r.gpu_3d_pct, r.gpu_pct, r.gpu_engine.chars().take(12).collect::<String>(), r.vram_dedicated_mb
        )
    };
    for r in &f.processes {
        t += &line(r);
    }
    if let Some(total) = &f.total {
        t += &line(total);
    }
    t += &format!("      (CPU: % of one core; 3D, GPU: % of one engine; sorted by {})\n", f.sort);
    if let Some((samples, s)) = target {
        let gpu = match (&s.gpu_adapter, s.gpu_3d_pct) {
            (Some(a), Some(v)) => format!("3D {v:.1}% on {a}"),
            (None, Some(_)) => "no GPU use".to_string(),
            _ => "GPU counters unavailable".to_string(),
        };
        t += &format!(
            "\ntarget tree  {} ticks over {:.1} s  {}  VRAM {:.1} MB dedicated, {:.1} MB shared\n",
            s.ticks,
            s.covered_ms / 1000.0,
            gpu,
            s.vram_dedicated_mb.unwrap_or(0.0),
            s.vram_shared_mb.unwrap_or(0.0)
        );
        let secs = s.covered_ms.max(1.0) / 1000.0;
        t += &format!(
            "sampler      every {} ms  {:.1} ms CPU ({:.3}% of one core), setup {:.1} ms\n",
            samples.interval_ms,
            samples.cpu_ms,
            samples.cpu_ms / 10.0 / secs,
            samples.setup_cpu_ms
        );
    }
    t
}
