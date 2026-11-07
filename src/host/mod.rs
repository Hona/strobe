//! This machine: paths, the monotonic clock, processes, CPU topology, power and displays.

pub(crate) mod adb;
pub mod build;
pub mod git;
pub mod http;
pub mod ios;
pub mod motion;
pub mod paths;
pub mod power;
pub mod reserve;
pub mod spawn;
pub mod tcp;
#[cfg(windows)]
pub mod cycle_clock;
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

pub use spawn::{spawn_owned, spawn_pinned};

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

/// Seconds since the last keyboard or mouse input in the session, where strobe uses it (macOS: it
/// takes a person's focus only after a minute without input).
pub fn person_idle_s() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        Some(os::user_idle_s())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Turns the display back on if it went dark on idle (a zero-pixel mouse move on Windows).
pub fn wake_display() {
    #[cfg(windows)]
    win::wake_display();
}

/// The processes that composite the screen, by executable name: Windows' desktop window manager,
/// macOS's window server, X servers and the common Wayland and X11 compositors.
pub const COMPOSITORS: &[&str] = &["dwm.exe", "WindowServer", "Xorg", "Xwayland", "gnome-shell", "kwin_wayland", "kwin_x11", "weston", "sway", "picom", "mutter"];

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
            // What this `Awake` asks for now: (machine awake, display on).
            let mut asked = (false, false);
            while !s.load(Ordering::Relaxed) {
                let locked = session_locked();
                if locked {
                    l.fetch_add(1, Ordering::Relaxed);
                } else {
                    u.fetch_add(1, Ordering::Relaxed);
                }
                let hold = h.load(Ordering::Relaxed);
                let now = (hold, hold && !locked);
                if now != asked {
                    Awake::ask(asked, now);
                    asked = now;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Awake::ask(asked, (false, false));
        });
        Awake { stop, held, locked, unlocked }
    }

    /// One `Awake` changes what it asks for. Windows keeps an execution state per thread, so each
    /// `Awake` thread holds its own. macOS's power assertions and Linux's inhibitors are one per
    /// process: they follow every live `Awake`'s asks together, so a short command that ends in the
    /// daemon never lets the machine sleep under a bench that still holds it.
    fn ask(old: (bool, bool), new: (bool, bool)) {
        #[cfg(windows)]
        {
            let _ = old;
            os::keep_awake(new.0, new.1);
        }
        #[cfg(not(windows))]
        {
            static ASKING: std::sync::Mutex<(u32, u32)> = std::sync::Mutex::new((0, 0));
            let mut n = ASKING.lock().unwrap_or_else(|e| e.into_inner());
            n.0 = n.0 + new.0 as u32 - old.0 as u32;
            n.1 = n.1 + new.1 as u32 - old.1 as u32;
            os::keep_awake(n.0 > 0, n.1 > 0);
        }
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
    /// CPU cycles charged so far (`tsc` units): exact, where `cpu_ns` moves in clock ticks.
    pub cycles: u64,
    pub private_bytes: u64,
    pub working_set: u64,
}

/// Every process in the tree rooted at `root`, with CPU times.
/// One process's CPU so far in `ProcTimes::cycles` units (`tsc` units: cycles on Windows, CPU ns
/// elsewhere), read without listing the system's processes: cheap enough at a run's two ends.
pub fn process_cycles(pid: u32) -> Option<u64> {
    os::process_cycles(pid)
}

pub fn process_tree(root: u32) -> Vec<ProcTimes> {
    process_trees(&[root])
}

/// Every process in the trees rooted at `roots` (each once), with CPU times, from one system
/// snapshot. A child is followed only when it started no earlier than its parent: a parent pid that
/// was reused names an older, unrelated process. Pid 0 roots nothing (the kernel's own).
pub fn process_trees(roots: &[u32]) -> Vec<ProcTimes> {
    trees(roots, &|_| Vec::new())
}

/// Picks root pids from a snapshot's process list (pid, parent, start time).
pub(crate) type PickRoots<'a> = dyn Fn(&[(u32, u32, u64)]) -> Vec<u32> + 'a;

/// `process_trees` of `roots` and of the roots `more` picks from the same snapshot.
fn trees(roots: &[u32], more: &PickRoots) -> Vec<ProcTimes> {
    let more = |list: &[(u32, u32, u64)]| -> Vec<u32> { roots.iter().copied().chain(more(list)).filter(|&r| r != 0).collect() };
    #[cfg(windows)]
    {
        win::process_trees(&more)
    }
    #[cfg(not(windows))]
    {
        os::process_trees(&more)
    }
}

/// Every process now: (pid, parent, start time in `start_time`'s units), the cheapest snapshot.
pub fn process_list() -> Vec<(u32, u32, u64)> {
    #[cfg(windows)]
    {
        win::process_list()
    }
    #[cfg(not(windows))]
    {
        os::process_list()
    }
}

/// Ends a running process and every process of its tree now: Windows terminates each (when its
/// creation time is still the snapshot's), Unix sends SIGKILL to the process group it leads (when it
/// runs and leads one) and to each process of the tree, children first.
pub fn kill_tree(pid: u32) {
    kill_launch(pid, None);
}

/// `kill_tree` of a launch strobe owns and has not reaped; `start` is its start time, read at the
/// launch: on macOS a launch that exited still roots what it left running (`macos::kill_set`).
pub(crate) fn kill_launch(pid: u32, start: Option<u64>) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = start;
        win::kill_tree(pid);
    }
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        let tree = macos::kill_set(pid, start);
        #[cfg(not(target_os = "macos"))]
        let tree = {
            let _ = start;
            tree_pids(pid)
        };
        if tree.is_empty() {
            return;
        }
        if unsafe { libc::getpgid(pid as i32) } == pid as i32 {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        for p in tree.iter().rev() {
            unsafe { libc::kill(*p as i32, libc::SIGKILL) };
        }
    }
}

/// One process's memory by what holds it (SPEC §8.6), in bytes. Windows walks its address space
/// (committed regions); Linux reads `smaps` (resident and swapped pages).
#[derive(Clone, Debug, Default, Serialize)]
pub struct MemoryMap {
    pub pid: u32,
    pub name: String,
    /// What `memory.app.private` reads: the commit charge (Windows), which includes commit held for
    /// the process's GPU allocations; private resident and swapped pages (Linux), device mappings too.
    pub private: u64,
    /// `memory.app.host`: memory the process allocated in its own address space (heap, stacks,
    /// anonymous mappings), without GPU memory mapped into it.
    pub host: u64,
    /// GPU memory mapped for the CPU: write-combined or uncached regions (Windows: a GPU driver maps
    /// the VRAM it allocates for the process this way, and Windows charges it as commit), GPU device
    /// mappings (Linux). `gpu.vram` and `gpu.shared` count the allocations themselves.
    pub gpu_mapped: u64,
    /// Shared memory sections (pagefile-backed on Windows, shmem and memfd on Linux).
    pub shared: u64,
    /// Mapped data files.
    pub files: u64,
    /// Executables and libraries: code and their data.
    pub image: u64,
    /// Windows: commit charged to the process with no private mapping in its address space (page
    /// tables, commit a driver holds for it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmapped: Option<u64>,
    pub working_set: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_working_set: Option<u64>,
}

/// `pid`'s memory by what holds it. None where it cannot be read: a process strobe cannot open, and
/// on macOS any other process (its regions need its task port, which only a debugger gets).
pub fn memory_map(pid: u32) -> Option<MemoryMap> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        os::memory_map(pid)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = pid;
        None
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
    /// CPU cycles charged at context switches (`tsc` units), exact where `cpu_ns` is coarse.
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

/// CPU cycles charged to a thread so far (`tsc` units), or its name, when it can be opened.
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

/// Linux: the proportional set size of these processes summed (`/proc/<pid>/smaps_rollup` `Pss`: a
/// page shared by n processes counts 1/n), so memory an app's processes share counts once
/// (`memory.app.pss`). None elsewhere, or when a process's could not be read (no partial sums).
pub fn pss_bytes(pids: &[u32]) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut total = 0u64;
        for pid in pids {
            let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
            let kb: u64 = text.lines().find_map(|l| l.strip_prefix("Pss:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())?;
            total += kb * 1024;
        }
        (!pids.is_empty()).then_some(total)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pids;
        None
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
    #[cfg(target_os = "linux")]
    {
        x11::top_windows(pids)
    }
    #[cfg(target_os = "macos")]
    {
        macos::top_windows(pids)
    }
}

/// The thread that owns a window (its UI thread), while the window exists. On Linux and macOS no
/// thread owns a window: the UI thread is the process's main thread, where AppKit must run and
/// X11 toolkits do.
pub fn window_thread(hwnd: isize) -> Option<u32> {
    #[cfg(windows)]
    {
        let tid = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd as _, std::ptr::null_mut()) };
        (tid != 0).then_some(tid)
    }
    #[cfg(target_os = "linux")]
    {
        window_state(hwnd).map(|w| w.pid).filter(|&p| p != 0)
    }
    #[cfg(target_os = "macos")]
    {
        macos::main_thread(window_state(hwnd)?.pid)
    }
}

/// A window's client area as it draws it now.
pub fn window_image(hwnd: isize) -> Option<crate::png::Image> {
    #[cfg(windows)]
    {
        win::window_image(hwnd)
    }
    #[cfg(target_os = "linux")]
    {
        x11::window_image(hwnd)
    }
    #[cfg(target_os = "macos")]
    {
        macos::window_image(hwnd)
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

/// PIDs of the tree rooted at `root` (root first), from one system snapshot. Children that started
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

/// Idle cycles (`tsc` units) so far per logical CPU; a CPU's busy share over an interval is
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

/// The resolution of Linux's idle counters (`/proc/stat` counts in `USER_HZ` ticks, 100 a second):
/// a CPU's idle time over a short span is off by up to one.
pub const IDLE_TICK_NS: u64 = 10_000_000;

/// Hypervisor steal so far per logical CPU, in ns: a Linux guest reads it (`/proc/stat`); false on
/// bare metal and on other OSes, where a hypervisor's time away shows as nothing a guest can read.
pub fn steal_ns(out: &mut Vec<u64>) -> bool {
    #[cfg(target_os = "linux")]
    {
        static GUEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*GUEST.get_or_init(|| linux::virtual_machine().is_some()) {
            out.clear();
            return false;
        }
        linux::steal_ns(out)
    }
    #[cfg(not(target_os = "linux"))]
    {
        out.clear();
        false
    }
}

/// A clock in the unit of `ProcInfo::cycles`. Windows: the performance counter times the kernel's
/// cycle factor, or the time stamp counter where the kernel charges raw TSC cycles (x64); None where
/// its cycles are not time (`cycles_refused`, `cycle_clock`). ns elsewhere.
pub fn tsc() -> Option<u64> {
    #[cfg(windows)]
    {
        cycle_clock::read()
    }
    #[cfg(not(windows))]
    {
        Some(now_ns())
    }
}

/// Settles the cycle clock (`cycle_clock`) now: before strobe moves its own CPU affinity and before
/// any measured window opens, since its check pins a thread to every CPU (once per boot, then cached).
pub fn warm_cycle_clock() {
    #[cfg(windows)]
    cycle_clock::clock();
}

/// Why this machine's process and thread cycles cannot be read as time (`tsc` is None), so every
/// metric made of them is refused rather than read from 15.6 ms clock ticks. None where they can.
pub fn cycles_refused() -> Option<String> {
    #[cfg(windows)]
    {
        let os = if cfg!(target_arch = "aarch64") { "Windows on Arm" } else { "Windows" };
        cycle_clock::clock().unit.as_ref().err().map(|why| format!("{os}: {why}"))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The cycle clock in words, for `status`.
pub fn cycle_clock_text() -> String {
    #[cfg(windows)]
    {
        cycle_clock::describe()
    }
    #[cfg(target_os = "linux")]
    {
        "ns: the processes' and threads' CPU clocks".into()
    }
    #[cfg(target_os = "macos")]
    {
        "ns: Mach time charged at every context switch".into()
    }
}

/// CPU cycles charged so far to the calling thread (`tsc` units). Exact, where
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

/// The tree rooted at `root` plus everything launched for `home` and their trees: every process
/// stamped with it (an app's detached background service, a double-forked daemon: outside the tree,
/// but the target's) and, on Windows, every process in its launch's job. Sorted by pid. strobe's own
/// process is in no target's set (it launched the target, and samples itself apart), unless it is
/// the root.
pub fn process_set(root: u32, home: Option<&Path>) -> Vec<ProcTimes> {
    let mut set = trees(&[root], &|list| match home {
        Some(h) => spawn::pids_in(list, h).into_iter().chain(spawn::group_pids(h)).collect(),
        None => Vec::new(),
    });
    let me = std::process::id();
    set.retain(|p| p.pid != me || root == me);
    set.sort_by_key(|p| p.pid);
    set
}

/// The pids of `process_set`, sorted, for a reader that needs no more (the OS sampler's re-read of a
/// target's set). On Linux it reads only the stat lines: `process_set` also reads each process's
/// `smaps_rollup` for its private memory, which for a browser's dozen processes cost the sampler
/// about 20 ms a re-read on vps.
pub fn process_set_pids(root: u32, home: Option<&Path>) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let roots = |list: &[(u32, u32, u64)]| -> Vec<u32> {
            let more = match home {
                Some(h) => spawn::pids_in(list, h).into_iter().chain(spawn::group_pids(h)).collect(),
                None => Vec::new(),
            };
            std::iter::once(root).chain(more).filter(|&r| r != 0).collect()
        };
        let me = std::process::id();
        let mut set: Vec<u32> = linux::tree_pids_of(&roots).into_iter().filter(|&p| p != me || root == me).collect();
        set.sort_unstable();
        set
    }
    #[cfg(not(target_os = "linux"))]
    {
        process_set(root, home).into_iter().map(|p| p.pid).collect()
    }
}

/// One group of logical CPUs that shares an L3 cache (a CCD on Ryzen).
#[derive(Clone, Debug, Serialize)]
pub struct CacheGroup {
    pub mask: u64,
    pub cpus: u32,
    pub l3_mb: u32,
}

/// One physical core: its logical CPUs (SMT siblings) and how fast its kind is among this CPU's.
#[derive(Clone, Debug, Serialize)]
pub struct Core {
    pub mask: u64,
    /// 0 for the slowest kind of core; higher is faster. Equal on every core of a CPU with one kind,
    /// lower on the efficiency cores of a hybrid one.
    pub class: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct Topology {
    /// How many logical CPUs strobe may use (`allowed`).
    pub logical: u32,
    /// The CPUs strobe may use: its affinity when it started, before it narrowed its own
    /// (`OwnAffinity`), so cpusets, `taskset` and offline or non-contiguous CPUs count.
    pub allowed: u64,
    /// Why strobe's 64-bit CPU masks cannot name every CPU it may use (CPUs numbered 64 or more,
    /// more than one Windows processor group): `--pin auto` leaves the target unpinned then.
    pub limit: Option<String>,
    /// The L3 groups, within `allowed`.
    pub groups: Vec<CacheGroup>,
    /// The physical cores within `allowed`, by first CPU; none on macOS (no CPU affinity). Where
    /// the OS says nothing of SMT (most virtual machines' vCPUs) each logical CPU is its own core.
    pub cores: Vec<Core>,
}

/// What `--pin auto` picks on this machine (`Topology::auto_pin`).
#[derive(Clone, Debug, Serialize)]
pub struct AutoPin {
    /// The target's CPUs; None leaves it unpinned.
    pub target: Option<u64>,
    /// Every other allowed CPU: strobe's and the runner's (and where `--reserve-cpus` moves this
    /// user's other processes). Never empty when `target` is set.
    pub reserved: Option<u64>,
    /// What was picked and why, in one line.
    pub reason: String,
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

    /// Every CPU strobe may use (`allowed`).
    pub fn all(&self) -> u64 {
        self.allowed
    }

    /// Where `--pin auto` puts the target, with strobe and the runner on every other allowed CPU
    /// (SPEC §9.4). The target never shares a physical core with them and never mixes core kinds:
    /// - L3 groups that differ: the target on the largest (3D V-Cache).
    /// - Equal L3 groups: the target on the first, when it holds 4 or more cores of one kind.
    /// - Hybrid CPU: the target on the fastest kind of core when there are 4 or more of them,
    ///   strobe and the runner on the slower ones.
    /// - One kind, 4 or more cores: the last cores/4 (1 or 2) whole cores kept apart, the target on
    ///   the rest.
    /// - Fewer cores, CPUs the masks cannot name, or macOS (no CPU affinity): unpinned, naming the
    ///   `--pin` list that would pin anyway where there is one.
    pub fn auto_pin(&self) -> AutoPin {
        let unpinned = |why: String| AutoPin { target: None, reserved: None, reason: format!("unpinned ({why})") };
        if cfg!(target_os = "macos") {
            return unpinned("macOS has no CPU affinity: the OS places every thread".into());
        }
        if let Some(why) = &self.limit {
            return unpinned(why.clone());
        }
        let all = self.all();
        let cores: Vec<Core> = if self.cores.is_empty() { bits(all).map(|c| Core { mask: 1 << c, class: 0 }).collect() } else { self.cores.clone() };
        let split = |target: u64, what: String| -> Option<AutoPin> {
            let reserved = all & !target;
            (target & all != 0 && reserved != 0).then(|| AutoPin {
                target: Some(target & all),
                reserved: Some(reserved),
                reason: format!("{what} · strobe and the runner on CPUs {}", cpu_text(reserved)),
            })
        };
        if self.groups.len() > 1 {
            let pick = match self.vcache() {
                Some(v) => Some((v, "V-Cache".to_string())),
                None => {
                    let g = &self.groups[0];
                    let inside: Vec<&Core> = cores.iter().filter(|c| c.mask & !g.mask == 0).collect();
                    (inside.len() >= 4 && inside.iter().all(|c| c.class == inside[0].class)).then(|| (g, format!("first of {} equal L3 groups", self.groups.len())))
                }
            };
            if let Some(p) = pick.and_then(|(g, why)| {
                let i = self.groups.iter().position(|x| x.mask == g.mask).unwrap_or(0);
                split(g.mask, format!("CCD{i} ({} MB L3, {} CPUs) · {why}", g.l3_mb, g.cpus))
            }) {
                return p;
            }
        }
        let fastest = cores.iter().map(|c| c.class).max().unwrap_or(0);
        let fast: Vec<&Core> = cores.iter().filter(|c| c.class == fastest).collect();
        if fast.len() < cores.len() {
            let mask = fast.iter().fold(0u64, |m, c| m | c.mask);
            let n = fast.len();
            if n >= 4
                && let Some(p) = split(mask, format!("CPUs {} · the {n} fastest cores of {}", cpu_text(mask), cores.len()))
            {
                return p;
            }
            return unpinned(format!("{n} fastest cores, too few to keep the slower ones apart by default: --pin {} pins the target to them", cpu_text(mask)));
        }
        if cores.len() < 4 {
            let Some(last) = cores.iter().max_by_key(|c| c.mask.trailing_zeros()) else { return unpinned("no CPU known".into()) };
            let rest = all & !last.mask;
            if rest == 0 {
                return unpinned("1 core: nothing to keep apart".into());
            }
            return unpinned(format!("{} cores, too few to keep one apart by default: --pin {} pins the target there and leaves CPUs {} to strobe", cores.len(), cpu_text(rest), cpu_text(last.mask)));
        }
        let keep = (cores.len() / 4).clamp(1, 2);
        let mut order: Vec<&Core> = cores.iter().collect();
        order.sort_by_key(|c| std::cmp::Reverse(c.mask.trailing_zeros()));
        let kept = order.iter().take(keep).fold(0u64, |m, c| m | c.mask);
        let target = all & !kept;
        split(target, format!("CPUs {} · {keep} of {} cores kept apart", cpu_text(target), cores.len())).unwrap_or_else(|| unpinned("no core left for the target".into()))
    }
}

/// The CPUs strobe may use and what its masks cannot name, read once: the first call comes before
/// strobe narrows its own affinity (`set_affinity`, `set_thread_affinity` and `topology` read it).
pub(crate) struct Allowed {
    pub mask: u64,
    pub count: u32,
    pub limit: Option<String>,
}

pub(crate) fn allowed() -> &'static Allowed {
    static ALLOWED: std::sync::OnceLock<Allowed> = std::sync::OnceLock::new();
    ALLOWED.get_or_init(os::allowed)
}

/// The CPU numbers in a mask.
pub(crate) fn bits(mask: u64) -> impl Iterator<Item = u32> {
    (0..64).filter(move |c| (mask >> c) & 1 == 1)
}

/// A CPU mask as a list: `0-5,8,10-11`.
pub fn cpu_text(mask: u64) -> String {
    let mut parts = Vec::new();
    let mut c = 0u32;
    while c < 64 {
        if (mask >> c) & 1 == 0 {
            c += 1;
            continue;
        }
        let start = c;
        while c < 64 && (mask >> c) & 1 == 1 {
            c += 1;
        }
        parts.push(if c - 1 == start { start.to_string() } else { format!("{start}-{}", c - 1) });
    }
    parts.join(",")
}

pub fn topology() -> Topology {
    allowed();
    #[cfg(windows)]
    {
        win::topology()
    }
    #[cfg(not(windows))]
    {
        os::topology()
    }
}

/// Restricts this process (and the children it starts afterwards) to `mask` (0: every allowed CPU);
/// returns the old mask.
pub fn set_affinity(mask: u64) -> Option<u64> {
    allowed();
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
    allowed();
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
    /// The OS's own "reduce motion" setting (`motion::reduced_motion`), read only; None when unknown.
    pub reduced_motion: Option<bool>,
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
    let reduced_motion = motion::reduced_motion();
    Env { os, cpu, logical_cpus: topo.logical, l3_groups: topo.groups, power_plan, on_battery, virtual_machine, reduced_motion, strobe: env!("CARGO_PKG_VERSION").to_string() }
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

/// Raises the visible windows of these processes (a target's process set) to topmost without
/// activating them; returns how many.
pub fn raise_windows(pids: &[u32]) -> usize {
    #[cfg(windows)]
    {
        win::raise_windows(pids)
    }
    #[cfg(not(windows))]
    {
        os::raise_windows(pids)
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

/// A display, in device-independent pixels on the desktop (from the main display's top left): its
/// bounds, and its work area, what a window may cover (less the menu bar and the Dock on macOS, the
/// taskbar on Windows, panels the window manager keeps on X11).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct WorkArea {
    pub screen: [f64; 4],
    pub work: [f64; 4],
}

/// The `WorkArea` of the display holding the point `at` (a window's centre) now (the Dock or taskbar
/// may move), or None without a display.
pub fn work_area(at: (f64, f64)) -> Option<WorkArea> {
    #[cfg(windows)]
    {
        win::work_area(at)
    }
    #[cfg(target_os = "macos")]
    {
        macos::work_area(at)
    }
    #[cfg(target_os = "linux")]
    {
        x11::work_area(at)
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

/// The module each thread started in (its path), per tid, for `(pid, tid)` pairs (Windows: its start
/// address; Linux: read from the top of its stack; macOS keeps no per-thread start a user can read).
pub fn thread_modules(threads: &[(u32, u32)]) -> std::collections::HashMap<u32, String> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        os::thread_modules(threads)
    }
    #[cfg(target_os = "macos")]
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

/// The user's own Documents and Desktop folders, where an app saves without being asked to (an
/// onboarding project): Windows' known folders where the shell keeps them (OneDrive's folder backup
/// moves them out of the profile), elsewhere in the home.
pub fn user_folders() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        win::user_folders()
    }
    #[cfg(not(windows))]
    {
        vec![home().join("Documents"), home().join("Desktop")]
    }
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}

/// Why a process (None: strobe itself, whose priority what it launches inherits) runs at background
/// priority: macOS `taskpolicy -b`, Windows' idle priority class, efficiency mode or background mode,
/// Linux `SCHED_IDLE` or nice 19. There the OS keeps it on slow cores or behind everything else, so
/// its frames and input latencies describe no person's machine (SPEC §10.1 `BackgroundPriority`).
pub fn background_qos(pid: Option<u32>) -> Option<String> {
    os::background_qos(pid.unwrap_or(0))
}

/// macOS: why strobe's timers (and those of what it launches) fire late, when they do: a launchd job
/// whose ProcessType lets macOS coalesce them (SPEC §10.1 `TimerCoalescing`).
pub fn timer_coalescing() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        os::timer_coalescing().map(|(ms, job)| {
            format!("timers that wake about {ms:.0} ms late: it runs in a launchd job (its oldest ancestor: {job}) whose ProcessType lets macOS coalesce the timers of everything it starts")
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The executable path of a running process.
pub fn exe_of(pid: u32) -> Option<PathBuf> {
    os::exe_of(pid)
}

/// PIDs of running processes whose executable is exactly `exe`.
pub fn running(exe: &Path) -> Vec<u32> {
    os::running(exe)
}

/// A process's environment (`NAME=value`), never its arguments, where this user may read it.
pub fn env_of(pid: u32) -> Option<Vec<String>> {
    os::env_of(pid)
}

/// When a process started, in units of its OS (only compared for equality): with its pid it names
/// one process, since a reused pid starts later.
pub fn start_time(pid: u32) -> Option<u64> {
    os::start_time(pid)
}

/// SIGINT and SIGTERM set `flag` instead of ending strobe; a second one while the flag is still set
/// ends it as the signal would.
#[cfg(unix)]
pub fn unix_ctrl_c(flag: &'static std::sync::atomic::AtomicBool) {
    use std::sync::atomic::{AtomicPtr, Ordering};
    static TARGET: AtomicPtr<std::sync::atomic::AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
    extern "C" fn handler(sig: libc::c_int) {
        let flag = TARGET.load(Ordering::SeqCst);
        if !flag.is_null() && !unsafe { (*flag).swap(true, Ordering::SeqCst) } {
            return;
        }
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    if TARGET.swap(flag as *const _ as *mut _, Ordering::SeqCst).is_null() {
        unsafe {
            libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        }
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
