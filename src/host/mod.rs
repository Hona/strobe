//! This machine: paths, the monotonic clock, processes, CPU topology, power and displays.

pub mod paths;
#[cfg(windows)]
pub mod win;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Monotonic nanoseconds (QPC on Windows).
pub fn now_ns() -> u64 {
    #[cfg(windows)]
    {
        win::qpc_ns()
    }
    #[cfg(not(windows))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

/// CPU time used so far by one process, in nanoseconds.
#[derive(Clone, Debug, Default)]
pub struct ProcTimes {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    pub cpu_ns: u64,
    pub private_bytes: u64,
}

/// Every process in the tree rooted at `root`, with CPU times.
pub fn process_tree(root: u32) -> Vec<ProcTimes> {
    #[cfg(windows)]
    {
        win::process_tree(root)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        Vec::new()
    }
}

/// One group of logical CPUs that shares an L3 cache (a CCD on Ryzen).
#[derive(Clone, Debug, Serialize)]
pub struct CacheGroup {
    pub mask: u64,
    pub cpus: u32,
    pub l3_mb: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Topology {
    pub logical: u32,
    pub groups: Vec<CacheGroup>,
}

impl Topology {
    /// The group with the largest L3 (3D V-Cache), when groups differ.
    pub fn vcache(&self) -> Option<&CacheGroup> {
        let max = self.groups.iter().map(|g| g.l3_mb).max()?;
        let min = self.groups.iter().map(|g| g.l3_mb).min()?;
        if max == min {
            return None;
        }
        self.groups.iter().find(|g| g.l3_mb == max)
    }
}

pub fn topology() -> Topology {
    #[cfg(windows)]
    {
        win::topology()
    }
    #[cfg(not(windows))]
    {
        let n = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
        Topology { logical: n, groups: Vec::new() }
    }
}

/// Restricts this process (and the children it starts afterwards) to `mask`; returns the old mask.
pub fn set_affinity(mask: u64) -> Option<u64> {
    #[cfg(windows)]
    {
        win::set_affinity(mask)
    }
    #[cfg(not(windows))]
    {
        let _ = mask;
        None
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Env {
    pub os: String,
    pub cpu: String,
    pub logical_cpus: u32,
    pub l3_groups: Vec<CacheGroup>,
    pub power_plan: String,
    pub on_battery: bool,
    pub strobe: String,
}

pub fn env() -> Env {
    let topo = topology();
    #[cfg(windows)]
    let (os, cpu, power_plan, on_battery) = (win::os_name(), win::cpu_name(), win::power_plan(), win::on_battery());
    #[cfg(not(windows))]
    let (os, cpu, power_plan, on_battery) = (std::env::consts::OS.to_string(), String::new(), String::new(), false);
    Env {
        os,
        cpu,
        logical_cpus: topo.logical,
        l3_groups: topo.groups,
        power_plan,
        on_battery,
        strobe: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Share of all CPUs busy over `ms`, 0..1.
pub fn system_busy(ms: u64) -> f64 {
    #[cfg(windows)]
    {
        win::system_busy(ms)
    }
    #[cfg(not(windows))]
    {
        let _ = ms;
        0.0
    }
}

/// Raises the visible windows of a process tree to topmost without activating them; returns how many.
pub fn raise_windows(root: u32) -> usize {
    #[cfg(windows)]
    {
        let pids: Vec<u32> = process_tree(root).iter().map(|p| p.pid).collect();
        win::raise_windows(&pids)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        0
    }
}

/// Refresh rate (Hz) of the display at a screen point.
pub fn refresh_at(x: i32, y: i32) -> Option<u32> {
    #[cfg(windows)]
    {
        win::refresh_at(x, y)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        None
    }
}

/// Finds an executable: on PATH, then in the given candidate paths.
pub fn find_exe(name: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    if let Some(found) = candidates.iter().find(|p| p.is_file()) {
        return Some(found.clone());
    }
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|p| p.join(&exe)).find(|p| p.is_file()))
}

/// Runs a command and returns stdout when it succeeds.
pub fn output(cmd: &mut Command) -> Option<String> {
    cmd.output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}
