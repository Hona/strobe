//! This machine: paths, the monotonic clock, processes, CPU topology, power and displays.

pub mod http;
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

/// Whether the user's session shows the lock screen: it covers every window, so nothing the target
/// draws reaches the display and Chromium treats its windows as hidden.
pub fn session_locked() -> bool {
    #[cfg(windows)]
    {
        win::session_locked()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Turns the display back on if it went dark on idle (a zero-pixel mouse move on Windows).
pub fn wake_display() {
    #[cfg(windows)]
    win::wake_display();
}

/// While alive, keeps the machine awake and, while the session is unlocked, the display on (an
/// unattended bench must not let the display sleep under it; a locked screen may go dark while the
/// bench waits), and counts how often it saw the session locked, every 500 ms.
pub struct Awake {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    locked: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Awake {
    pub fn start() -> Awake {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let locked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (s, l) = (stop.clone(), locked.clone());
        std::thread::spawn(move || {
            let mut was: Option<bool> = None;
            while !s.load(Ordering::Relaxed) {
                let locked = session_locked();
                if locked {
                    l.fetch_add(1, Ordering::Relaxed);
                }
                if was != Some(locked) {
                    #[cfg(windows)]
                    win::keep_awake(true, !locked);
                    was = Some(locked);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            #[cfg(windows)]
            win::keep_awake(false, false);
        });
        Awake { stop, locked }
    }

    /// Lock observations so far: a block during which this changed saw the lock screen.
    pub fn locks(&self) -> u64 {
        self.locked.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// CPU time used so far by one process, in nanoseconds.
#[derive(Clone, Debug, Default)]
pub struct ProcTimes {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    pub cpu_ns: u64,
    /// CPU cycles charged so far (TSC units on x64): exact, where `cpu_ns` moves in clock ticks.
    pub cycles: u64,
    pub private_bytes: u64,
    pub working_set: u64,
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

/// One process from a system snapshot.
#[derive(Clone, Debug, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    /// Kernel + user time, in ns; the OS updates it per clock tick (15.6 ms on Windows).
    pub cpu_ns: u64,
    /// CPU cycles charged at context switches (TSC units on x64), exact where `cpu_ns` is coarse.
    pub cycles: u64,
    pub private_bytes: u64,
    pub threads: u32,
}

/// Every process on the machine, from one system snapshot (no process is opened, so protected
/// processes are included).
pub fn all_processes() -> Vec<ProcInfo> {
    #[cfg(windows)]
    {
        win::all_processes()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// PIDs of the tree rooted at `root` (root first), from one system snapshot. Children created
/// before their parent (a reused parent PID) are not followed.
pub fn tree_pids(root: u32) -> Vec<u32> {
    #[cfg(windows)]
    {
        win::tree_pids(root)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        Vec::new()
    }
}

/// Idle cycles (TSC units) so far per logical CPU; a CPU's busy share over an interval is
/// `1 - Δidle / Δtsc`. False when unavailable.
pub fn idle_cycles(out: &mut Vec<u64>) -> bool {
    #[cfg(windows)]
    {
        win::idle_cycles(out)
    }
    #[cfg(not(windows))]
    {
        out.clear();
        false
    }
}

/// The time stamp counter (x64), the unit of `ProcInfo::cycles`; None on other architectures.
pub fn tsc() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        #[allow(unused_unsafe)]
        Some(unsafe { std::arch::x86_64::_rdtsc() })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// CPU cycles charged so far to the calling thread (TSC units on x64; see `tsc`). Exact, where
/// thread times move in 15.6 ms clock ticks.
pub fn thread_cycles() -> u64 {
    #[cfg(windows)]
    {
        win::thread_cycles()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Turns on ANSI escape handling for stdout; false when stdout is not a console that has it.
pub fn enable_ansi() -> bool {
    #[cfg(windows)]
    {
        win::enable_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Installs a Ctrl+C handler (once) and returns the flag it sets instead of killing the process.
pub fn ctrl_c() -> &'static std::sync::atomic::AtomicBool {
    static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    #[cfg(windows)]
    win::on_ctrl_c(&FLAG);
    &FLAG
}

/// The tree rooted at `root` plus every process running from `home` (an Electron app's detached
/// background service lives outside the tree but belongs to the target).
pub fn process_set(root: u32, home: Option<&Path>) -> Vec<ProcTimes> {
    let mut set = process_tree(root);
    #[cfg(windows)]
    if let Some(h) = home {
        for pid in win::running_under(h) {
            if !set.iter().any(|p| p.pid == pid) {
                set.extend(process_tree(pid));
            }
        }
    }
    #[cfg(not(windows))]
    let _ = home;
    set.sort_by_key(|p| p.pid);
    set.dedup_by_key(|p| p.pid);
    set
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
    /// The hypervisor, when this is a virtual machine.
    pub virtual_machine: Option<String>,
    pub strobe: String,
}

pub fn env() -> Env {
    let topo = topology();
    #[cfg(windows)]
    let (os, cpu, power_plan, on_battery) = (win::os_name(), win::cpu_name(), win::power_plan(), win::on_battery());
    #[cfg(not(windows))]
    let (os, cpu, power_plan, on_battery) = (std::env::consts::OS.to_string(), String::new(), String::new(), false);
    #[cfg(windows)]
    let virtual_machine = win::virtual_machine();
    #[cfg(not(windows))]
    let virtual_machine = None;
    Env { os, cpu, logical_cpus: topo.logical, l3_groups: topo.groups, power_plan, on_battery, virtual_machine, strobe: env!("CARGO_PKG_VERSION").to_string() }
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
/// The largest visible window of these processes, shrunk to a 16×16 grid of averaged BGRA pixels read
/// from the screen: what a person sees of it, for "painted" and "settled" (Windows only).
pub fn window_pixels(pids: &[u32]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        win::window_pixels(pids)
    }
    #[cfg(not(windows))]
    {
        let _ = pids;
        None
    }
}

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
