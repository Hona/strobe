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

/// Per thread over several spans (the measured runs): wakes per second and CPU ms per run, busiest
/// waker first, with the thread's name when it has one.
pub fn by_thread(spans: &[Span], names: &BTreeMap<u32, String>) -> Vec<serde_json::Value> {
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
            serde_json::json!({
                "pid": pid, "tid": tid, "name": names.get(&tid), "switches": n,
                "wakes_per_s": if total_ms > 0.0 { n as f64 / (total_ms / 1e3) } else { f64::NAN },
                "cpu_ms_per_run": if known { Some(cpu / runs) } else { None },
            })
        })
        .collect();
    rows.sort_by(|a, b| b["wakes_per_s"].as_f64().unwrap_or(0.0).total_cmp(&a["wakes_per_s"].as_f64().unwrap_or(0.0)));
    rows
}
