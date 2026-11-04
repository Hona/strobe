//! This machine: paths, the monotonic clock, processes, CPU topology, power and displays.

pub mod http;
pub mod paths;
pub mod power;
pub mod reserve;
#[cfg(windows)]
pub mod win;
#[cfg(target_os = "linux")]
pub mod dbus;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod x11;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(windows)]
use win as os;

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
        os::now_ns()
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
        os::session_locked()
    }
}

/// Turns the display back on if it went dark on idle (a zero-pixel mouse move on Windows).
pub fn wake_display() {
    #[cfg(windows)]
    win::wake_display();
}

/// The console display's state (0 off, 1 on, 2 dimmed) and how many times it changed so far; None
/// where it is not known.
pub fn display_state() -> Option<(u8, u64)> {
    #[cfg(windows)]
    {
        win::display_state()
    }
    #[cfg(not(windows))]
    {
        unix_display_state()
    }
}

/// While alive, keeps the machine awake and, while the session is unlocked, the display on (an
/// unattended bench must not let the display sleep under it), and counts how often it saw the session
/// locked, every 500 ms. A locked session's display may go dark as Windows decides: that is how a
/// locked machine runs for its users, and keeping a display lit for days of locked benches would be
/// strobe changing the machine. Benches track the display state per block instead (`display_state`)
/// and redo a pair whose blocks saw different states.
pub struct Awake {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    held: std::sync::Arc<std::sync::atomic::AtomicBool>,
    locked: std::sync::Arc<std::sync::atomic::AtomicU64>,
    unlocked: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Awake {
    /// Tracks the lock state from now on; keeps the machine awake only while `hold(true)`.
    pub fn start() -> Awake {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let locked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let unlocked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (s, h, l, u) = (stop.clone(), held.clone(), locked.clone(), unlocked.clone());
        std::thread::spawn(move || {
            let mut was: Option<(bool, bool)> = None;
            while !s.load(Ordering::Relaxed) {
                let locked = session_locked();
                if locked {
                    l.fetch_add(1, Ordering::Relaxed);
                } else {
                    u.fetch_add(1, Ordering::Relaxed);
                }
                let hold = h.load(Ordering::Relaxed);
                if was != Some((hold, locked)) {
                    #[cfg(any(windows, target_os = "macos"))]
                    os::keep_awake(hold, hold && !locked);
                    was = Some((hold, locked));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            #[cfg(any(windows, target_os = "macos"))]
            os::keep_awake(false, false);
        });
        Awake { stop, held, locked, unlocked }
    }

    /// Keeps the machine awake (and the display on while unlocked) or lets it sleep as the user's
    /// settings say: a bench holds it only while its blocks run, never while it waits (within 500 ms).
    pub fn hold(&self, on: bool) {
        self.held.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Lock observations so far: a block during which this changed saw the lock screen.
    pub fn locks(&self) -> u64 {
        self.locked.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Unlocked observations so far: a block run on a locked screen during which this changed saw it unlocked.
    pub fn unlocks(&self) -> u64 {
        self.unlocked.load(std::sync::atomic::Ordering::Relaxed)
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
        os::process_tree(root)
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

/// One thread from a system snapshot.
#[derive(Clone, Debug, Default)]
pub struct ThreadInfo {
    pub pid: u32,
    pub tid: u32,
    /// Context switches so far: each time the thread was switched in to run (it wraps at 2^32).
    pub switches: u32,
    /// Kernel + user time, in ns, in clock ticks.
    pub cpu_ns: u64,
}

/// The threads of `pids` (sorted) and each process's IO bytes so far (pid, read, written), from one
/// system snapshot.
pub fn threads(pids: &[u32]) -> (Vec<ThreadInfo>, Vec<(u32, u64, u64)>) {
    #[cfg(windows)]
    {
        win::threads(pids)
    }
    #[cfg(not(windows))]
    {
        os::threads(pids)
    }
}

/// CPU cycles charged to a thread so far (TSC units), or its name, when it can be opened.
pub fn thread_cycles_of(tid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        win::thread_cycles_of(tid)
    }
    #[cfg(not(windows))]
    {
        os::thread_cycles_of(tid)
    }
}

pub fn thread_name(tid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        win::thread_name(tid)
    }
    #[cfg(not(windows))]
    {
        os::thread_name(tid)
    }
}

/// A top-level window: its screen rect (x, y, width, height) and client size in physical pixels, its
/// DPI, and whether a person could see it (visible, not minimized, not cloaked by the compositor).
#[derive(Clone, Debug, Default, Serialize)]
pub struct TopWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub rect: (i32, i32, i32, i32),
    pub client: (i32, i32),
    pub visible: bool,
    pub iconic: bool,
    pub cloaked: bool,
    pub dpi: u32,
}

impl TopWindow {
    pub fn shown(&self) -> bool {
        self.visible && !self.iconic && !self.cloaked
    }

    pub fn scale(&self) -> f64 {
        self.dpi as f64 / 96.0
    }
}

/// The largest visible, unminimized top-level window of `pids` (Windows only).
pub fn main_window(pids: &[u32]) -> Option<TopWindow> {
    #[cfg(windows)]
    {
        win::main_window(pids)
    }
    #[cfg(not(windows))]
    {
        os::main_window(pids)
    }
}

/// The visible top-level windows of `pids`, front to back: handle, title, click-through.
pub fn top_windows(pids: &[u32]) -> Vec<(isize, String, bool)> {
    #[cfg(windows)]
    {
        win::top_windows(pids)
    }
    #[cfg(not(windows))]
    {
        let _ = pids;
        Vec::new()
    }
}

/// The thread that owns a window (its UI thread), while the window exists.
pub fn window_thread(hwnd: isize) -> Option<u32> {
    #[cfg(windows)]
    {
        let tid = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd as _, std::ptr::null_mut()) };
        (tid != 0).then_some(tid)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// A window's client area as it draws it now (Windows only).
pub fn window_image(hwnd: isize) -> Option<crate::png::Image> {
    #[cfg(windows)]
    {
        win::window_image(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// A debug C runtime the executable imports, when it imports one (Windows executables).
pub fn debug_runtime(exe: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        win::debug_runtime(exe)
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        None
    }
}

/// A window's state now; None once it is gone.
pub fn window_state(hwnd: isize) -> Option<TopWindow> {
    #[cfg(windows)]
    {
        win::window_state(hwnd)
    }
    #[cfg(not(windows))]
    {
        os::window_state(hwnd)
    }
}

/// Moves a window (and with `client`, sizes its client area in device-independent pixels), topmost
/// without focus.
pub fn place_window(w: &TopWindow, x: i32, y: i32, client: Option<(u32, u32)>) {
    #[cfg(windows)]
    win::place_window(w, x, y, client);
    #[cfg(not(windows))]
    os::place_window(w, x, y, client);
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
        os::all_processes()
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
        os::tree_pids(root)
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
        os::idle_cycles(out)
    }
}

/// The time stamp counter (x64), the unit of `ProcInfo::cycles`; None on other architectures.
pub fn tsc() -> Option<u64> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        #[allow(unused_unsafe)]
        Some(unsafe { std::arch::x86_64::_rdtsc() })
    }
    #[cfg(all(windows, not(target_arch = "x86_64")))]
    {
        None
    }
    #[cfg(not(windows))]
    {
        Some(now_ns())
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
        os::thread_cycles()
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
    #[cfg(not(windows))]
    unix_ctrl_c(&FLAG);
    &FLAG
}

/// The tree rooted at `root` plus every process running from `home` (an Electron app's detached
/// background service lives outside the tree but belongs to the target).
pub fn process_set(root: u32, home: Option<&Path>) -> Vec<ProcTimes> {
    let mut set = process_tree(root);
    if let Some(h) = home {
        for pid in running_under(h) {
            if !set.iter().any(|p| p.pid == pid) {
                set.extend(process_tree(pid));
            }
        }
    }
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
        os::topology()
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
        #[cfg(target_os = "linux")]
        return os::set_affinity(mask);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = mask;
            None
        }
    }
}

/// Restricts the calling thread to `mask`.
pub fn set_thread_affinity(mask: u64) {
    #[cfg(windows)]
    win::set_thread_affinity(mask);
    #[cfg(target_os = "linux")]
    os::set_thread_affinity(mask);
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = mask;
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

/// The active power plan's name, with Windows 11's power mode when one is set on it
/// ("Balanced · Best performance").
pub fn env_power_plan() -> String {
    #[cfg(windows)]
    {
        let plan = win::power_plan();
        match power::mode() {
            Some(m) if plan == "Balanced" => format!("{plan} · {m}"),
            _ => plan,
        }
    }
    #[cfg(not(windows))]
    {
        os::power_plan()
    }
}

/// A laptop running on its battery.
pub fn on_battery() -> bool {
    #[cfg(windows)]
    {
        win::on_battery()
    }
    #[cfg(not(windows))]
    {
        os::on_battery()
    }
}

pub fn env() -> Env {
    let topo = topology();
    #[cfg(windows)]
    let (os, cpu, power_plan, on_battery) = (win::os_name(), win::cpu_name(), env_power_plan(), win::on_battery());
    #[cfg(not(windows))]
    let (os, cpu, power_plan, on_battery) = (os::os_name(), os::cpu_name(), env_power_plan(), os::on_battery());
    #[cfg(windows)]
    let virtual_machine = win::virtual_machine();
    #[cfg(not(windows))]
    let virtual_machine = os::virtual_machine();
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
        os::system_busy(ms)
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
        os::window_pixels(pids)
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
        os::raise_windows(&tree_pids(root))
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
        os::refresh_at(x, y)
    }
}

/// This machine's name, lower case and without a domain: `COMPUTERNAME` on Windows, `gethostname`
/// elsewhere (shells keep `HOSTNAME` unexported, and macOS adds `.local`).
pub fn device_name() -> String {
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();
    #[cfg(not(windows))]
    let name = {
        let mut buf = [0u8; 256];
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if ok { String::from_utf8_lossy(&buf[..end]).to_string() } else { std::env::var("HOSTNAME").unwrap_or_default() }
    };
    name.split('.').next().unwrap_or("").to_lowercase()
}

/// The module each thread started in (its path), per tid, for `(pid, tid)` pairs (Windows only).
pub fn thread_modules(threads: &[(u32, u32)]) -> std::collections::HashMap<u32, String> {
    #[cfg(windows)]
    {
        win::thread_modules(threads)
    }
    #[cfg(not(windows))]
    {
        let _ = threads;
        Default::default()
    }
}

/// The display path a window shows on: its display target (the `VidPnTargetId` vertical blanks
/// carry), the adapter that scans it out (LUID), and its nominal refresh.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DisplayPath {
    pub device: String,
    pub target: u32,
    pub adapter_luid: u64,
    pub hz: f64,
}

/// The display path of the monitor a window is on (the nearest one).
pub fn display_of(hwnd: isize) -> Option<DisplayPath> {
    #[cfg(windows)]
    {
        win::display_of(hwnd)
    }
    #[cfg(not(windows))]
    {
        os::display_of(hwnd)
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

/// The executable path of a running process.
pub fn exe_of(pid: u32) -> Option<PathBuf> {
    os::exe_of(pid)
}

/// PIDs of running processes whose executable is exactly `exe`.
pub fn running(exe: &Path) -> Vec<u32> {
    os::running(exe)
}

/// PIDs of running processes whose executable lives under `dir` (on Linux also: whose working folder does).
pub fn running_under(dir: &Path) -> Vec<u32> {
    os::running_under(dir)
}

/// SIGINT sets `flag` instead of ending strobe.
#[cfg(unix)]
pub fn unix_ctrl_c(flag: &'static std::sync::atomic::AtomicBool) {
    use std::sync::atomic::{AtomicPtr, Ordering};
    static TARGET: AtomicPtr<std::sync::atomic::AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
    extern "C" fn handler(_: libc::c_int) {
        let flag = TARGET.load(Ordering::SeqCst);
        if !flag.is_null() {
            unsafe { (*flag).store(true, Ordering::SeqCst) };
        }
    }
    if TARGET.swap(flag as *const _ as *mut _, Ordering::SeqCst).is_null() {
        unsafe { libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t) };
    }
}

/// The main display on (1) or asleep (0), and how often that changed, from a poll every 500 ms that
/// starts on first use (macOS); None where the OS does not say (Linux).
#[cfg(unix)]
fn unix_display_state() -> Option<(u8, u64)> {
    #[cfg(target_os = "macos")]
    {
        use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
        static STATE: AtomicU8 = AtomicU8::new(u8::MAX);
        static CHANGES: AtomicU64 = AtomicU64::new(0);
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            STATE.store(macos::display_on() as u8, Ordering::Relaxed);
            std::thread::spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let v = macos::display_on() as u8;
                    if STATE.swap(v, Ordering::Relaxed) != v {
                        CHANGES.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        });
        Some((STATE.load(Ordering::Relaxed), CHANGES.load(Ordering::Relaxed)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 32-bit pixels (`stride` bytes per row) averaged into a 16×16 grid of 4-byte cells, sampling every
/// second pixel of each cell.
#[cfg(unix)]
pub fn grid16(bytes: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = vec![0u8; 16 * 16 * 4];
    for cy in 0..16 {
        for cx in 0..16 {
            let (x0, x1, y0, y1) = (cx * width / 16, (cx + 1) * width / 16, cy * height / 16, (cy + 1) * height / 16);
            let mut sum = [0u64; 4];
            let mut n = 0u64;
            for y in (y0..y1).step_by(2) {
                for x in (x0..x1).step_by(2) {
                    let at = y * stride + x * 4;
                    if let Some(px) = bytes.get(at..at + 4) {
                        for (s, p) in sum.iter_mut().zip(px) {
                            *s += *p as u64;
                        }
                        n += 1;
                    }
                }
            }
            for (k, s) in sum.iter().enumerate() {
                out[(cy * 16 + cx) * 4 + k] = (s / n.max(1)) as u8;
            }
        }
    }
    out
}