//! Linux GPU sampling (SPEC §13.6): per process from the kernel's DRM client statistics
//! (`/proc/<pid>/fdinfo/<fd>` of each open DRM file: `drm-engine-<name>` busy ns, `drm-*-<region>`
//! memory), the standard every DRM driver with client stats prints (amdgpu, i915, xe, msm, panfrost,
//! v3d, nouveau with recent kernels). The proprietary NVIDIA driver prints none: its processes read as
//! unreadable, never 0. Per device: amdgpu's `gpu_busy_percent` and VRAM counters from sysfs.

use super::gpu::{Adapter, Device, DeviceSample, EngineUse, VramUse};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// `0000:03:00.0` → a number to tell adapters apart (domain, bus, device, function).
fn pci_id(addr: &str) -> u64 {
    addr.split([':', '.']).filter_map(|p| u64::from_str_radix(p, 16).ok()).fold(0, |a, p| (a << 16) | p)
}

/// The DRM cards (`/sys/class/drm/cardN`): driver, PCI address and vendor:device ids.
pub fn adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    let mut cards: Vec<_> = std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("card") && !n.contains('-')).collect();
    cards.sort();
    for card in cards {
        let dev = std::path::Path::new("/sys/class/drm").join(&card).join("device");
        let driver = std::fs::read_link(dev.join("driver")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
        let addr = std::fs::canonicalize(&dev).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
        let id = |f: &str| std::fs::read_to_string(dev.join(f)).map(|s| s.trim().trim_start_matches("0x").to_string()).unwrap_or_default();
        let name = format!("{driver} {addr} ({}:{})", id("vendor"), id("device"));
        out.push(Adapter { luid: pci_id(&addr), name, bus: None, nodes: Vec::new(), apertures: Vec::new() });
    }
    out
}

/// The kernel's engine names as Windows' engine types, so `gpu.process.3d` means the same thing.
fn engine_type(name: &str) -> String {
    match name {
        "gfx" | "render" | "rcs" | "graphics" | "gpu" | "3d" | "fragment" | "vertex-tiler" => "3D".into(),
        "compute" | "ccs" => "Compute".into(),
        "copy" | "bcs" | "dma" | "sdma" => "Copy".into(),
        "dec" | "vcs" | "video" | "vcn" | "uvd" => "VideoDecode".into(),
        "enc" | "vecs" | "vce" | "video-enhance" => "VideoEncode".into(),
        other => other.to_string(),
    }
}

/// One DRM client's counters at a tick.
#[derive(Default)]
struct Client {
    pid: u32,
    pdev: String,
    engines: Vec<(String, u64)>,
    local: u64,
    system: u64,
}

fn parse(text: &str, pid: u32) -> Option<(String, Client)> {
    let mut c = Client { pid, ..Client::default() };
    let mut id = None;
    let (mut resident, mut total, mut legacy): (HashMap<String, u64>, HashMap<String, u64>, HashMap<String, u64>) = Default::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let num = || v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok());
        let bytes = || num().map(|n| n * if v.ends_with("KiB") { 1024 } else if v.ends_with("MiB") { 1 << 20 } else { 1 });
        if k == "drm-client-id" {
            id = Some(v.to_string());
        } else if k == "drm-pdev" {
            c.pdev = v.to_string();
        } else if let Some(e) = k.strip_prefix("drm-engine-") {
            if !e.starts_with("capacity-")
                && let Some(n) = num()
            {
                c.engines.push((e.to_string(), n));
            }
        } else if let Some(r) = k.strip_prefix("drm-resident-") {
            resident.insert(r.to_string(), bytes()?);
        } else if let Some(r) = k.strip_prefix("drm-total-") {
            total.insert(r.to_string(), bytes()?);
        } else if let Some(r) = k.strip_prefix("drm-memory-") {
            legacy.insert(r.to_string(), bytes()?);
        }
    }
    // Resident where the driver says, else committed, else the older per-region counters (amdgpu).
    let regions = if !resident.is_empty() { resident } else if !total.is_empty() { total } else { legacy };
    for (r, b) in regions {
        if r.starts_with("vram") || r.starts_with("local") || r.starts_with("stolen") {
            c.local += b;
        } else if r.starts_with("gtt") || r.starts_with("system") || r == "cpu" {
            c.system += b;
        }
    }
    Some((id?, c))
}

pub struct Kmt {
    adapters: Vec<(u64, u16)>,
    /// pid → its DRM file descriptors and when they were listed (re-listed every second).
    fds: HashMap<u32, (Vec<u32>, Instant)>,
    /// (client id, engine) → busy ns at the last tick.
    prev: HashMap<(String, String), u64>,
    names: HashMap<String, u16>,
    started: bool,
    pub unreadable: Vec<u32>,
}

/// The cards (`luid`s, as `adapters` names them) whose device also has a render node
/// (`/sys/class/drm/renderD*`), matched by the device's canonical path: only those run a process's
/// GPU work. A display-only device (a virtual machine's framebuffer such as bochs-drm or hyperv_drm,
/// simpledrm) has a card and no render node, and prints no engine time; vgem has a render node and
/// no GPU behind it.
fn rendering_cards() -> Vec<u64> {
    let entries: Vec<(String, std::path::PathBuf)> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().to_string_lossy().to_string(), std::fs::canonicalize(e.path().join("device")).ok()?)))
        .collect();
    let driver = |dev: &std::path::Path| std::fs::read_link(dev.join("driver")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
    let renders: Vec<&std::path::PathBuf> = entries.iter().filter(|(n, d)| n.starts_with("renderD") && driver(d) != "vgem").map(|(_, d)| d).collect();
    entries
        .iter()
        .filter(|(n, d)| n.starts_with("card") && !n.contains('-') && renders.contains(&d))
        .filter_map(|(_, d)| d.file_name().map(|f| pci_id(&f.to_string_lossy())))
        .collect()
}

impl Kmt {
    /// None without a device that renders: nothing to read, and listing every process's file
    /// descriptors each second for DRM files cost the OS sampler about 0.2% of a core on vps, whose
    /// only device is a display-only one.
    pub fn new(adapters: &[Adapter], names: &mut Vec<String>) -> Option<Kmt> {
        let rendering = rendering_cards();
        if !adapters.iter().any(|a| rendering.contains(&a.luid)) {
            return None;
        }
        let mut map = HashMap::new();
        for t in ["3D", "Compute", "Copy", "VideoDecode", "VideoEncode"] {
            map.insert(t.to_string(), super::gpu::intern(names, t));
        }
        Some(Kmt { adapters: adapters.iter().enumerate().map(|(i, a)| (a.luid, i as u16)).collect(), fds: HashMap::new(), prev: HashMap::new(), names: map, started: false, unreadable: Vec::new() })
    }

    fn drm_fds(&mut self, pid: u32) -> Option<Vec<u32>> {
        if let Some((fds, at)) = self.fds.get(&pid)
            && at.elapsed() < Duration::from_secs(1)
        {
            return Some(fds.clone());
        }
        let rd = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
        let mut nvidia = false;
        let fds: Vec<u32> = rd
            .flatten()
            .filter(|e| {
                let t = std::fs::read_link(e.path()).unwrap_or_default();
                nvidia |= t.to_string_lossy().starts_with("/dev/nvidia");
                t.starts_with("/dev/dri/")
            })
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        // The proprietary NVIDIA driver's devices print no client statistics: this process's GPU use is unknown.
        if nvidia && !self.unreadable.contains(&pid) {
            self.unreadable.push(pid);
        }
        self.fds.insert(pid, (fds.clone(), Instant::now()));
        Some(fds)
    }

    /// Reads every PID in `pids`: memory, and with `nodes` engine time over the `dt_ns` tick. A
    /// client present at the first read gets a baseline; a later one counts from zero.
    pub fn read(&mut self, pids: &[u32], dt_ns: u64, nodes: bool, gpu: &mut Vec<EngineUse>, vram: &mut Vec<VramUse>) {
        self.fds.retain(|p, _| pids.binary_search(p).is_ok());
        let mut clients: HashMap<String, Client> = HashMap::new();
        for &pid in pids {
            let Some(fds) = self.drm_fds(pid) else {
                if !self.unreadable.contains(&pid) && std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    self.unreadable.push(pid);
                }
                continue;
            };
            let mut stats = false;
            for &fd in &fds {
                let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else { continue };
                if let Some((id, c)) = parse(&text, pid) {
                    stats |= !c.engines.is_empty();
                    // A client shared by several fds or processes (after fork) counts once, for the first.
                    clients.entry(id).or_insert(c);
                }
            }
            // A GPU device open without client statistics (a driver that prints none, such as a virtual
            // GPU's): its use is unknown, not zero.
            if !fds.is_empty() && !stats && !self.unreadable.contains(&pid) {
                self.unreadable.push(pid);
            }
        }
        let adapter = |pdev: &str| self.adapters.iter().find(|a| a.0 == pci_id(pdev)).or(self.adapters.first()).map(|a| a.1).unwrap_or(0);
        let mut seen = std::collections::HashSet::new();
        for (id, c) in &clients {
            let a = adapter(&c.pdev);
            if c.local + c.system > 0 {
                vram.push(VramUse { pid: c.pid, adapter: a, dedicated: c.local, shared: c.system });
            }
            for (e, ns) in &c.engines {
                let key = (id.clone(), e.clone());
                seen.insert(key.clone());
                let prev = self.prev.insert(key, *ns);
                let base = match prev {
                    Some(p) => p,
                    None if self.started => 0,
                    None => continue,
                };
                if nodes && *ns > base && dt_ns > 0 {
                    let ty = engine_type(e);
                    let engine = match self.names.get(&ty) {
                        Some(&i) => i,
                        None => continue,
                    };
                    gpu.push(EngineUse { pid: c.pid, adapter: a, engine, pct: ((ns - base) as f64 * 100.0 / dt_ns as f64) as f32 });
                }
            }
        }
        self.prev.retain(|k, _| seen.contains(k));
        self.started = true;
        vram.sort_unstable_by_key(|v| (v.pid, v.adapter));
        vram.dedup_by(|cur, prev| {
            let same = (cur.pid, cur.adapter) == (prev.pid, prev.adapter);
            if same {
                prev.dedicated += cur.dedicated;
                prev.shared += cur.shared;
            }
            same
        });
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

/// Device counters from sysfs where the driver has them (amdgpu: busy %, VRAM used and total).
pub struct Nvml {
    pub devices: Vec<Device>,
    paths: Vec<std::path::PathBuf>,
}

impl Nvml {
    pub fn load(adapters: &[Adapter]) -> Option<Nvml> {
        let mut devices = Vec::new();
        let mut paths = Vec::new();
        for (i, a) in adapters.iter().enumerate() {
            let card = std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten().map(|e| e.path().join("device")).find(|d| std::fs::canonicalize(d).ok().and_then(|p| p.file_name().map(|n| pci_id(&n.to_string_lossy()))) == Some(a.luid));
            let Some(dev) = card.filter(|d| d.join("gpu_busy_percent").is_file()) else { continue };
            let total = std::fs::read_to_string(dev.join("mem_info_vram_total")).ok().and_then(|s| s.trim().parse::<f64>().ok()).map(|b| b / (1 << 20) as f64);
            devices.push(Device { name: a.name.clone(), adapter: Some(i as u16), vram_total_mb: total });
            paths.push(dev);
        }
        (!devices.is_empty()).then_some(Nvml { devices, paths })
    }

    pub fn sample(&mut self, _t_ns: u64, _pcie: bool, out: &mut Vec<DeviceSample>) {
        out.clear();
        for p in &self.paths {
            let read = |f: &str| std::fs::read_to_string(p.join(f)).ok().and_then(|s| s.trim().parse::<f64>().ok());
            out.push(DeviceSample { gpu_pct: read("gpu_busy_percent"), mem_pct: read("mem_busy_percent"), vram_used_mb: read("mem_info_vram_used").map(|b| b / (1 << 20) as f64), ..DeviceSample::default() });
        }
    }
}
