//! Thread wakes and IO of a process tree (SPEC §13.6): two system snapshots around a run give each
//! thread's context switches (each one the thread being switched in to run: a wake), its CPU cycles,
//! and each process's IO bytes. Nothing is opened per sample except the threads, for their cycles.

use crate::host;
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
}

impl Snapshot {
    /// The threads and IO of `pids` now.
    pub fn take(pids: &[u32]) -> Snapshot {
        let mut sorted = pids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let (threads, io) = host::threads(&sorted);
        Snapshot {
            at_ns: host::now_ns(),
            tsc: host::tsc(),
            threads: threads.into_iter().map(|t| ((t.pid, t.tid), (t.switches, host::thread_cycles_of(t.tid)))).collect(),
            io: io.into_iter().map(|(p, r, w)| (p, (r, w))).collect(),
        }
    }

    /// Nothing yet, at `at_ns` (a tree about to start): a span from it counts every thread from zero.
    pub fn empty(at_ns: u64, tsc: Option<u64>) -> Snapshot {
        Snapshot { at_ns, tsc, ..Snapshot::default() }
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
    let mut out = Span { ms, ..Span::default() };
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

/// Who a thread belongs to, by the module it started in: `driver` (a device driver's user-mode part,
/// from the driver store, such as a GPU driver's worker threads), `os` (the OS's own: its thread pool,
/// audio, RPC; a pool thread may run the app's callbacks), else `app`. Unknown without a module.
pub fn origin(module: &str) -> &'static str {
    let m = module.to_lowercase().replace('/', "\\");
    let windows = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase();
    if m.contains("\\driverstore\\") {
        "driver"
    } else if m.starts_with(&windows) {
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
            let module = modules.get(&tid);
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
