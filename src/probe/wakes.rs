//! Thread wakes and IO of a process tree (SPEC §13.6): two system snapshots around a run give each
//! thread's context switches (each one the thread being switched in to run: a wake), its CPU cycles,
//! and each process's IO bytes. Nothing is opened per sample except the threads, for their cycles.
//! Beside them, what the OS counts per process beyond time (SPEC §7.5 `energy.cpu`,
//! `cpu.instructions`, `cpu.idle_wakes`), or why it cannot.

use crate::host;
use crate::model::{Refusal, ValidatorId};
use serde::Serialize;
use std::collections::BTreeMap;

/// One moment of the tree.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    at_ns: u64,
    tsc: Option<u64>,
    /// (pid, tid) → (context switches, cycles).
    threads: BTreeMap<(u32, u32), (u32, Option<u64>)>,
    /// pid → (read, written) bytes.
    io: BTreeMap<u32, (u64, u64)>,
    counters: Counters,
}

impl Snapshot {
    /// The threads and IO of `pids` now.
    pub fn take(pids: &[u32]) -> Snapshot {
        Snapshot::take_since(wall_us(), pids, None)
    }

    /// The same, with the counters of `pids`, which were listed at `listed_us` (`wall_us`, read before
    /// listing them: a process started after it was not listed and counts from its start); `perf`
    /// counts instructions on Linux.
    pub fn take_since(listed_us: u64, pids: &[u32], perf: Option<&Perf>) -> Snapshot {
        let mut sorted = pids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let (threads, io) = host::threads(&sorted);
        Snapshot {
            at_ns: host::now_ns(),
            tsc: host::tsc(),
            threads: threads.into_iter().map(|t| ((t.pid, t.tid), (t.switches, host::thread_cycles_of(t.tid)))).collect(),
            io: io.into_iter().map(|(p, r, w)| (p, (r, w))).collect(),
            counters: Counters::take(listed_us, &sorted, perf),
        }
    }

    /// Nothing yet, at `at_ns` (a tree about to start): a span from it counts every thread from zero.
    pub fn empty(at_ns: u64, tsc: Option<u64>) -> Snapshot {
        Snapshot { at_ns, tsc, counters: Counters { at_ns, ..Counters::default() }, ..Snapshot::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }
}

/// What the tree did between two snapshots.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Span {
    pub ms: f64,
    pub switches: u64,
    pub io_read: u64,
    pub io_write: u64,
    pub threads: Vec<ThreadSpan>,
    #[serde(skip)]
    pub counted: Counted,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ThreadSpan {
    pub pid: u32,
    pub tid: u32,
    pub switches: u64,
    /// CPU time from the thread's cycle counter over the span; None where it could not be read.
    pub cpu_ms: Option<f64>,
}

impl Span {
    pub fn wakes_per_s(&self) -> f64 {
        if self.ms > 0.0 { self.switches as f64 / (self.ms / 1e3) } else { f64::NAN }
    }
}

/// From `a` to `b`. A thread or process that started in between counts from zero; one that exited in
/// between is gone from `b`, and its last wakes with it, so the count is a lower bound when threads exit.
pub fn span(a: &Snapshot, b: &Snapshot) -> Span {
    let ms = b.at_ns.saturating_sub(a.at_ns) as f64 / 1e6;
    // Cycles to ms with the time stamp counter's rate over the span.
    let per_ms = match (a.tsc, b.tsc) {
        (Some(x), Some(y)) if y > x && ms > 0.0 => Some((y - x) as f64 / ms),
        _ => None,
    };
    let mut out = Span { ms, counted: counted(&a.counters, &b.counters), ..Span::default() };
    for (&(pid, tid), &(switches, cycles)) in &b.threads {
        let (s0, c0) = a.threads.get(&(pid, tid)).copied().unwrap_or((0, Some(0)));
        let n = switches.wrapping_sub(s0) as u64;
        out.switches += n;
        let cpu_ms = match (cycles, c0, per_ms) {
            (Some(c), Some(c0), Some(r)) => Some(c.saturating_sub(c0) as f64 / r),
            _ => None,
        };
        out.threads.push(ThreadSpan { pid, tid, switches: n, cpu_ms });
    }
    for (pid, &(r, w)) in &b.io {
        let (r0, w0) = a.io.get(pid).copied().unwrap_or((0, 0));
        out.io_read += r.saturating_sub(r0);
        out.io_write += w.saturating_sub(w0);
    }
    out
}

/// Every second: each thread's CPU and wakes over a whole launch, threads that exited included
/// (`query <rec> threads --timeline`), so work that starts late and stops again (a driver compiling
/// shaders 25 s after the launch) shows when it ran, not only as a per-run total.
pub const TIMELINE_EVERY: std::time::Duration = std::time::Duration::from_millis(1000);

/// One thread on a timeline: when it first and last showed, its name and the module it started in,
/// read while it ran (a thread that exited has neither later).
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct TimelineThread {
    pub pid: u32,
    pub tid: u32,
    pub name: Option<String>,
    pub module: Option<String>,
    pub first_ns: u64,
    pub last_ns: u64,
}

/// What a timeline sampler saw. `cells` are (tick, thread, CPU ms, wakes) for the ticks a thread ran
/// in, CPU from the thread's kernel and user times (Windows counts them in clock ticks of 15.6 ms:
/// within a few percent over a second); tick `i` covers `(ticks[i - 1], ticks[i]]` (tick 0 from `t0_ns`). A thread's CPU between its
/// last sighting and its exit is not in it (at most one interval).
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct TimelineData {
    pub t0_ns: u64,
    pub interval_ms: u64,
    pub ticks: Vec<u64>,
    pub threads: Vec<TimelineThread>,
    pub cells: Vec<(u32, u32, f32, u32)>,
    /// The sampler thread's own CPU (rule 2).
    pub cpu_ms: Option<f64>,
    /// Spans to show on it (name, start, end), when the recording keeps no runs of its own.
    #[serde(default)]
    pub marks: Vec<(String, u64, u64)>,
}

/// Samples a process set's threads on a background thread until `stop`.
pub struct Timeline {
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<TimelineData>,
}

impl Timeline {
    /// `pids` names the processes at each tick (a tree and what runs from its home).
    pub fn start(pids: impl Fn() -> Vec<u32> + Send + 'static) -> Timeline {
        let (stop, rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("strobe-timeline".into())
            .spawn(move || {
                let (k0, tsc0, ns0) = (host::thread_cycles(), host::tsc(), host::now_ns());
                // One system snapshot a tick, its threads' context switches and CPU time (no handle per
                // thread: the cycle counters cost more than the snapshot). Which processes: read again
                // every few ticks (finding what runs from a home opens every process).
                type Threads = BTreeMap<(u32, u32), (u32, u64)>;
                let read = |set: &[u32]| -> (u64, Threads) {
                    let mut sorted = set.to_vec();
                    sorted.sort_unstable();
                    sorted.dedup();
                    let at = host::now_ns();
                    (at, host::threads(&sorted).0.into_iter().map(|t| ((t.pid, t.tid), (t.switches, t.cpu_ns))).collect())
                };
                let mut set = pids();
                let (t0, mut prev) = read(&set);
                let mut out = TimelineData { t0_ns: t0, interval_ms: TIMELINE_EVERY.as_millis() as u64, ..TimelineData::default() };
                let mut index: BTreeMap<(u32, u32), u32> = BTreeMap::new();
                let mut seen = |out: &mut TimelineData, at: u64, threads: &Threads| {
                    let mut new = Vec::new();
                    for &(pid, tid) in threads.keys() {
                        let i = *index.entry((pid, tid)).or_insert_with(|| {
                            new.push((pid, tid));
                            out.threads.push(TimelineThread { pid, tid, name: host::thread_name(tid), first_ns: at, ..TimelineThread::default() });
                            out.threads.len() as u32 - 1
                        });
                        out.threads[i as usize].last_ns = at;
                    }
                    if !new.is_empty() {
                        let modules = host::thread_modules(&new);
                        for t in out.threads.iter_mut().filter(|t| t.module.is_none()) {
                            t.module = modules.get(&t.tid).cloned();
                        }
                    }
                    index.clone()
                };
                seen(&mut out, t0, &prev);
                loop {
                    if !matches!(rx.recv_timeout(TIMELINE_EVERY), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                        break;
                    }
                    let tick = out.ticks.len() as u32;
                    if tick % 5 == 4 {
                        set = pids();
                    }
                    let (at, now) = read(&set);
                    let index = seen(&mut out, at, &now);
                    for (key, &(switches, cpu_ns)) in &now {
                        // A thread born in the tick counts from zero.
                        let (s0, c0) = prev.get(key).copied().unwrap_or((0, 0));
                        let (wakes, cpu) = (switches.wrapping_sub(s0), cpu_ns.saturating_sub(c0) as f64 / 1e6);
                        if wakes > 0 || cpu > 0.0 {
                            out.cells.push((tick, index[key], cpu as f32, wakes));
                        }
                    }
                    out.ticks.push(at);
                    prev = now;
                }
                let hz = tsc0.zip(host::tsc()).map(|(a, b)| (b - a) as f64 / ((host::now_ns() - ns0) as f64 / 1e9)).filter(|&hz| hz > 0.0 && k0 > 0);
                out.cpu_ms = hz.map(|hz| (host::thread_cycles() - k0) as f64 / hz * 1e3);
                out
            })
            .expect("spawn the timeline thread");
        Timeline { stop, thread }
    }

    pub fn stop(self) -> TimelineData {
        let _ = self.stop.send(());
        self.thread.join().unwrap_or_default()
    }
}

/// Who a thread belongs to, by the module it started in (on macOS, a process row: its executable):
/// `driver` (a device driver's user-mode part, such as a GPU driver's worker threads), `os` (the
/// OS's own: its thread pool, audio, RPC, IPC; a pool thread may run the app's callbacks), else
/// `app`. Unknown without a module.
///
/// - driver: Windows' driver store; macOS's kernel extension bundles (`/System/Library/Extensions`,
///   where the GPU drivers' user-mode parts live, and `/Library/Extensions`); Linux's DRI drivers
///   (`*_dri.so`, a `dri/` folder), Vulkan drivers and loader (`libvulkan*`), Mesa's (`libgallium*`,
///   `libEGL_mesa`, `libGLX_mesa`), NVIDIA's (`libnvidia*`, `libcuda*`, `lib*_nvidia`) and `libdrm*`.
/// - os: the Windows folder; macOS's `/System`, `/usr/lib` libraries and `/usr/libexec`; Linux's C
///   runtime and the session's service clients (GLib's and D-Bus's workers, PulseAudio, PipeWire,
///   ALSA, JACK, systemd).
pub fn origin(module: &str) -> &'static str {
    let m = module.to_lowercase().replace('\\', "/");
    let file = m.rsplit('/').next().unwrap_or(&m);
    let windows = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase().replace('\\', "/");
    let linux_driver = file.contains("_dri.so")
        || m.contains("/dri/")
        || ["libvulkan", "libgallium", "libegl_mesa", "libglx_mesa", "libnvidia", "libcuda", "libegl_nvidia", "libglx_nvidia", "libdrm"].iter().any(|p| file.starts_with(p));
    let linux_os = ["libc.so", "libc-", "ld-linux", "ld-musl", "libpthread", "libglib-2.0", "libgio-2.0", "libgthread", "libdbus-1", "libpulse", "libpipewire", "libasound", "libjack", "libsystemd"]
        .iter()
        .any(|p| file.starts_with(p));
    if m.contains("/driverstore/") || m.starts_with("/system/library/extensions/") || m.starts_with("/library/extensions/") || linux_driver {
        "driver"
    } else if m.starts_with(&windows) || m.starts_with("/system/") || m.starts_with("/usr/libexec/") || (m.starts_with("/usr/lib/") && file.ends_with(".dylib")) || linux_os {
        "os"
    } else {
        "app"
    }
}

/// The module each thread of `spans` started in, while its process still runs.
pub fn modules(spans: &[Span]) -> BTreeMap<u32, String> {
    let threads: Vec<(u32, u32)> = spans.iter().flat_map(|s| s.threads.iter().map(|t| (t.pid, t.tid))).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    host::thread_modules(&threads).into_iter().collect()
}

/// Per thread over several spans (the measured runs): wakes per second and CPU ms per run, busiest
/// waker first, with the thread's name when it has one, the module it started in and that module's
/// `origin`.
pub fn by_thread(spans: &[Span], names: &BTreeMap<u32, String>, modules: &BTreeMap<u32, String>) -> Vec<serde_json::Value> {
    let total_ms: f64 = spans.iter().map(|s| s.ms).sum();
    let mut acc: BTreeMap<(u32, u32), (u64, f64, bool)> = BTreeMap::new();
    for s in spans {
        for t in &s.threads {
            let e = acc.entry((t.pid, t.tid)).or_insert((0, 0.0, true));
            e.0 += t.switches;
            match t.cpu_ms {
                Some(c) => e.1 += c,
                None => e.2 = false,
            }
        }
    }
    let runs = spans.len().max(1) as f64;
    let mut rows: Vec<serde_json::Value> = acc
        .into_iter()
        .map(|((pid, tid), (n, cpu, known))| {
            // macOS's per-process row (tid 0) is its process: its executable says whose it is.
            let exe = (tid == 0).then(|| host::exe_of(pid).map(|p| p.display().to_string())).flatten();
            let module = exe.as_ref().or_else(|| modules.get(&tid));
            // macOS counts wakes per process (the tid 0 row): its threads' own wakes are unknown, not 0.
            let unknown = cfg!(target_os = "macos") && tid != 0;
            serde_json::json!({
                "pid": pid, "tid": tid, "name": names.get(&tid), "switches": if unknown { None } else { Some(n) },
                "module": module.map(|m| m.rsplit(['\\', '/']).next().unwrap_or(m)), "origin": module.map(|m| origin(m)),
                "wakes_per_s": if unknown { None } else if total_ms > 0.0 { Some(n as f64 / (total_ms / 1e3)) } else { Some(f64::NAN) },
                "cpu_ms_per_run": if known { Some(cpu / runs) } else { None },
            })
        })
        .collect();
    rows.sort_by(|a, b| b["wakes_per_s"].as_f64().unwrap_or(0.0).total_cmp(&a["wakes_per_s"].as_f64().unwrap_or(0.0)));
    rows
}

/// The counter metrics (SPEC §7.5), in `Counted::counts` order: instructions per run, CPU energy per
/// run (mJ), package idle wakeups per second.
pub const COUNTER_METRICS: [&str; 3] = ["cpu.instructions", "energy.cpu", "cpu.idle_wakes"];

/// Why a counter metric has no value: typed, so its refusal (`NoCounter`) names the cause.
#[derive(Clone, Debug, PartialEq)]
pub enum Uncounted {
    /// This OS or CPU keeps no such count per process.
    NoSource(String),
    /// The count exists, but strobe may not read it here.
    Denied { why: String, fix: String },
    /// Part of the run went uncounted: a process that joined or left the set during it, counters
    /// the CPU shared with another user of them.
    Partial(String),
    /// Not opened: Linux opens perf events only in counter blocks (SPEC §7.5).
    NotOpened,
}

impl Uncounted {
    /// The refusal of a metric (its name is the refusal's key, so metrics refused for one cause read
    /// as one line).
    fn refusal(&self) -> Refusal {
        let r = |why: &str| Refusal::new(ValidatorId::NoCounter, why);
        match self {
            Uncounted::NoSource(why) => r(why).fix("compare it on an OS that counts it per process (SPEC §7.5), or pick cpu.work"),
            Uncounted::Denied { why, fix } => r(why).fix(fix.clone()),
            Uncounted::Partial(why) => r(why).fix("keep the set of the app's processes the same through a run (warm up what it starts)"),
            Uncounted::NotOpened => r("not counted in this block: Linux counts instructions in counter blocks").fix("declare it in a bench case"),
        }
    }
}

/// One counting source at a snapshot: a process (macOS), or a perf counting group (Linux: a process
/// and every thread and process its threads started after strobe opened them).
#[derive(Clone, Debug)]
struct Source {
    id: u64,
    pid: u32,
    name: String,
    /// Instructions, CPU energy (nJ), package idle wakeups, and (Linux) the ns the counters were
    /// enabled and running: a gap between the two is the CPU sharing them (multiplexing).
    counts: [u64; 5],
    /// macOS: when the process started (µs since the epoch), from which the kernel counts it.
    born_us: Option<u64>,
    #[cfg(target_os = "linux")]
    group: std::sync::Arc<super::perf::Group>,
}

/// The counts of a process set at one moment, or why a metric is not counted at all.
#[derive(Clone, Debug, Default)]
pub struct Counters {
    at_ns: u64,
    wall_us: u64,
    sources: Vec<Source>,
    missing: [Option<Uncounted>; 3],
}

/// What a span counted, per `COUNTER_METRICS`, over `ms`.
#[derive(Clone, Debug)]
pub struct Counted {
    pub ms: f64,
    pub counts: [Result<u64, Uncounted>; 3],
}

impl Default for Counted {
    fn default() -> Self {
        Counted { ms: 0.0, counts: std::array::from_fn(|_| Err(Uncounted::NotOpened)) }
    }
}

#[cfg(windows)]
const WINDOWS_NONE: &str = "Windows counts no instructions, CPU energy or package idle wakeups per process that a user's process can read: the kernel trace's PMC counters on context switches would give instructions (strobe does not collect them yet), and its energy figures (E3) are a model's estimates, not measurements";

/// Now on the wall clock (µs since the epoch), as macOS dates a process's start: read it before
/// listing the processes a snapshot counts (`Snapshot::take_since`, `Counters::take`).
pub fn wall_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

/// The perf events of one block or recording (Linux, SPEC §7.5), closed when it is dropped. Only a
/// counter block holds one: timing blocks never open them. Elsewhere nothing to hold.
#[derive(Default)]
pub struct Perf {
    #[cfg(target_os = "linux")]
    groups: super::perf::Groups,
}

impl Perf {
    pub fn new() -> Perf {
        Perf::default()
    }
}

impl Counters {
    /// The counts of `pids` now, listed at `listed_us` (`wall_us`). `perf` counts instructions on
    /// Linux; without it they are not opened.
    pub fn take(listed_us: u64, pids: &[u32], perf: Option<&Perf>) -> Counters {
        let mut c = Counters { at_ns: host::now_ns(), wall_us: listed_us, ..Counters::default() };
        #[cfg(windows)]
        {
            let _ = (pids, perf);
            c.missing = std::array::from_fn(|_| Some(Uncounted::NoSource(WINDOWS_NONE.into())));
        }
        #[cfg(target_os = "macos")]
        {
            let _ = perf;
            if !host::macos::energy_counters() {
                c.missing[1] = Some(Uncounted::NoSource("this Mac's CPU has no energy counters: macOS measures a process's CPU energy only on Apple silicon".into()));
            }
            for &pid in pids {
                match host::macos::counts(pid) {
                    Ok(k) => c.sources.push(Source {
                        id: (pid as u64) << 32 | (k.start_us & 0xffff_ffff),
                        pid,
                        name: k.name,
                        counts: [k.instructions, k.energy_nj, k.idle_wakes, 0, 0],
                        born_us: Some(k.start_us),
                    }),
                    // Gone: a span that held it says so.
                    Err(libc::ESRCH) => {}
                    Err(libc::EINVAL) => c.missing = std::array::from_fn(|_| Some(Uncounted::NoSource("this macOS gives no rusage v6 (macOS 13 or later does)".into()))),
                    Err(e) => {
                        let why = format!("proc_pid_rusage refused process {pid}: {}", std::io::Error::from_raw_os_error(e));
                        c.missing = std::array::from_fn(|_| Some(Uncounted::Denied { why: why.clone(), fix: "measure an app that runs as your user".into() }));
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            c.missing[1] = Some(Uncounted::NoSource(
                "Linux gives no per-process CPU energy: RAPL counts a whole package (every process on it, and strobe), so no share of it is the app's, and strobe never estimates one from CPU time".into(),
            ));
            c.missing[2] = Some(Uncounted::NoSource("Linux counts no package idle wakeups per process".into()));
            if let Some(perf) = perf {
                match perf.groups.read(pids) {
                    Ok(groups) => {
                        for g in groups {
                            match g.read() {
                                Some((n, enabled, running)) => {
                                    c.sources.push(Source { id: std::sync::Arc::as_ptr(&g) as u64, pid: g.pid, name: g.name.clone(), counts: [n, 0, 0, enabled, running], born_us: None, group: g })
                                }
                                None => c.missing[0] = Some(Uncounted::Partial(format!("the counters of process {} ({}) could not be read", g.pid, g.name))),
                            }
                        }
                    }
                    Err(u) => c.missing[0] = Some(u),
                }
            } else {
                c.missing[0] = Some(Uncounted::NotOpened);
            }
        }
        c
    }
}

/// What the sources counted from `a` to `b`. A source new in `b` counts from zero only when the OS
/// counts it from its start and it started after `a` (macOS); one that left the set is read once
/// more where its counters stay open (Linux). Any other is part of the run uncounted, and so is a
/// counter the CPU multiplexed: refused, never scaled or left short.
pub fn counted(a: &Counters, b: &Counters) -> Counted {
    let ms = b.at_ns.saturating_sub(a.at_ns) as f64 / 1e6;
    let mut sum = [0u64; 5];
    let mut partial: Option<String> = None;
    let add = |sum: &mut [u64; 5], to: &[u64; 5], from: &[u64; 5]| {
        for ((s, t), f) in sum.iter_mut().zip(to).zip(from) {
            *s += t.saturating_sub(*f);
        }
    };
    let before: std::collections::HashMap<u64, &Source> = a.sources.iter().map(|s| (s.id, s)).collect();
    for s in &b.sources {
        match before.get(&s.id) {
            Some(s0) => add(&mut sum, &s.counts, &s0.counts),
            None if s.born_us.is_some_and(|t| t >= a.wall_us) => add(&mut sum, &s.counts, &[0; 5]),
            None => partial = Some(format!("process {} ({}) joined the target's processes during the run, uncounted before", s.pid, s.name)),
        }
    }
    let after: std::collections::HashSet<u64> = b.sources.iter().map(|s| s.id).collect();
    for s0 in a.sources.iter().filter(|s| !after.contains(&s.id)) {
        #[cfg(target_os = "linux")]
        if let Some((n, enabled, running)) = s0.group.read() {
            add(&mut sum, &[n, 0, 0, enabled, running], &s0.counts);
            continue;
        }
        partial = Some(format!("process {} ({}) exited during the run, and its last counts went with it", s0.pid, s0.name));
    }
    if sum[3] > sum[4] {
        partial = Some(format!("the CPU's counters were shared with another perf user for {:.1}% of the time (multiplexed): the count missed part of the run", 100.0 * (sum[3] - sum[4]) as f64 / sum[3] as f64));
    }
    Counted {
        ms,
        counts: std::array::from_fn(|i| match (&b.missing[i], &a.missing[i], &partial) {
            (Some(u), _, _) | (None, Some(u), _) => Err(u.clone()),
            (None, None, Some(why)) => Err(Uncounted::Partial(why.clone())),
            (None, None, None) => Ok(sum[i]),
        }),
    }
}

/// Whether a series' declared metrics (its no-worse ones included) need Linux's perf events: they
/// cost the target at each of its context switches, so they open only in counter blocks of a series
/// that declares `cpu.instructions` (macOS counts always, at no cost to the target).
pub fn wants_perf<'a>(metrics: impl IntoIterator<Item = &'a String>) -> bool {
    metrics.into_iter().any(|m| m == "cpu.instructions")
}

/// The counter metrics one span counted, in their units (instructions, mJ, per second).
pub fn counter_values(c: &Counted) -> Vec<(&'static str, f64)> {
    COUNTER_METRICS
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let n = *c.counts[i].as_ref().ok()? as f64;
            let v = match i {
                0 => n,
                1 => n / 1e6,
                _ if c.ms > 0.0 => n / (c.ms / 1e3),
                _ => return None,
            };
            Some((*m, v))
        })
        .collect()
}

/// The counter metrics of a block's measured runs: a metric every run counted gets its values per
/// run; one the OS cannot count here, or that some run counted only in part, is refused
/// (`NoCounter`) when the series declares it, and otherwise left out.
pub fn counter_metrics<'a>(runs: &[(String, &Counted)], declared: impl IntoIterator<Item = &'a String>) -> (BTreeMap<String, BTreeMap<String, f64>>, BTreeMap<String, Refusal>) {
    let declared: Vec<&String> = declared.into_iter().collect();
    let (mut values, mut refused) = (BTreeMap::new(), BTreeMap::new());
    if runs.is_empty() {
        return (values, refused);
    }
    for (i, metric) in COUNTER_METRICS.iter().enumerate() {
        match runs.iter().find_map(|(_, c)| c.counts[i].as_ref().err()) {
            // A timing block on Linux: the series' counter blocks count it.
            Some(Uncounted::NotOpened) => {}
            Some(u) => {
                if declared.iter().any(|m| m == metric) {
                    refused.insert(metric.to_string(), u.refusal());
                }
            }
            None => {
                let per: BTreeMap<String, f64> = runs.iter().filter_map(|(k, c)| Some((k.clone(), counter_values(c).into_iter().find(|(m, _)| m == metric)?.1))).collect();
                values.insert(metric.to_string(), per);
            }
        }
    }
    (values, refused)
}
