//! Frame metrics from what the OS put on the screen (SPEC §7.2): OS presents alone (any process,
//! outside-in), or a toolkit's frame rows joined to them (begin, input and screen times).
//!
//! A frame is pending from its begin (a toolkit row's `begin_ns`; outside-in, the present call's
//! start) until its screen time. Between two consecutive frames of one surface (or swapchain) shown
//! at `a` and `b`, the refreshes in between were missed when the later frame was pending early
//! enough to make the refresh after `a` (pending before `a` + half a refresh): an animation that
//! fell behind. A later start is the app resting (or, outside-in, a frame the app itself started
//! late: only a toolkit's begin times can tell those apart), so the gap is neither an interval nor
//! a drop, as Chromium's `STATE_NO_UPDATE_DESIRED` resets intervals.

use super::{Present, Presents};
use serde_json::Value;
use std::collections::BTreeMap;

/// One toolkit frame row (native bridge v1 `frame_columns`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Row {
    pub seq: u64,
    pub surface: u32,
    pub tid: u32,
    pub begin_ns: u64,
    pub cpu_end_ns: u64,
    pub submit_ns: u64,
    pub present_ns: u64,
    pub input_ns: u64,
}

/// A run record's frame rows (`frame_columns`, `frames`), by column name.
pub fn rows_from_record(rec: &Value) -> Vec<Row> {
    let cols: Vec<&str> = rec["frame_columns"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let col = |name: &str| cols.iter().position(|c| *c == name);
    let idx = [col("seq"), col("surface"), col("tid"), col("begin_ns"), col("cpu_end_ns"), col("submit_ns"), col("present_ns"), col("input_ns")];
    rec["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let v = |i: usize| idx[i].and_then(|c| row[c].as_u64()).unwrap_or(0);
            Row { seq: v(0), surface: v(1) as u32, tid: v(2) as u32, begin_ns: v(3), cpu_end_ns: v(4), submit_ns: v(5), present_ns: v(6), input_ns: v(7) }
        })
        .collect()
}

/// Writes rows' `present_ns` back into a run record's `frames`.
pub fn fill_record(rec: &mut Value, rows: &[Row]) {
    let Some(at) = rec["frame_columns"].as_array().and_then(|c| c.iter().position(|x| x == "present_ns")) else { return };
    for (row, r) in rec["frames"].as_array_mut().into_iter().flatten().zip(rows) {
        if let Some(cell) = row.get_mut(at)
            && r.present_ns > 0
        {
            *cell = Value::from(r.present_ns);
        }
    }
}

/// A frame that reached the screen.
#[derive(Clone, Copy, Debug)]
pub struct Shown {
    /// One surface (toolkit rows) or swapchain (presents): gaps are taken within it.
    pub stream: u64,
    pub pending_ns: u64,
    pub screen_ns: u64,
    /// The frame's begin (toolkit rows only), for `frames.presented.*`.
    pub begin_ns: Option<u64>,
    pub input_ns: u64,
}

/// Slack around a row's present call when matching it to an OS present (the runtime logs its
/// start inside the call, and QPC reads on two threads differ by a few µs).
const JOIN_SLACK_NS: u64 = 500_000;

/// The OS present of each toolkit row: one of `pids`, started inside the row's present call
/// (`cpu_end_ns`..`submit_ns`), the presenting thread first. Fills `present_ns` with the screen
/// time of displayed ones and returns, per row, the matched present.
pub fn join<'a>(rows: &mut [Row], presents: &'a [Present], pids: &[u32]) -> Vec<Option<&'a Present>> {
    let mine: Vec<&Present> = presents.iter().filter(|p| pids.contains(&p.pid)).collect();
    let mut used = vec![false; mine.len()];
    let mut out = vec![None; rows.len()];
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].submit_ns);
    for i in order {
        let r = rows[i];
        if r.submit_ns == 0 {
            continue;
        }
        let lo = r.cpu_end_ns.min(r.submit_ns).saturating_sub(JOIN_SLACK_NS);
        let hi = r.submit_ns + JOIN_SLACK_NS;
        // Presents are sorted by start: only those inside the window are looked at.
        let from = mine.partition_point(|p| p.start_ns < lo);
        let pick = |same_thread: bool| {
            mine[from..]
                .iter()
                .enumerate()
                .take_while(|(_, p)| p.start_ns <= hi)
                .filter(|(j, p)| !used[from + j] && (!same_thread || p.tid == r.tid))
                .min_by_key(|(_, p)| p.start_ns.abs_diff(r.submit_ns))
                .map(|(j, _)| from + j)
        };
        if let Some(j) = pick(true).or_else(|| pick(false)) {
            used[j] = true;
            out[i] = Some(mine[j]);
            if mine[j].state == "displayed" && rows[i].present_ns == 0 {
                rows[i].present_ns = mine[j].screen_ns;
            }
        }
    }
    out
}

/// The refresh period (ms) from vertical blanks in `[t0, t1]`: the median spacing on the display
/// path with the most of them.
pub fn refresh_ms(vsyncs: &[(u64, u32)], t0: u64, t1: u64) -> Option<f64> {
    let mut by: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for &(t, target) in vsyncs.iter().filter(|v| v.0 >= t0 && v.0 <= t1) {
        by.entry(target).or_default().push(t);
    }
    let ts = by.into_values().max_by_key(Vec::len)?;
    let mut gaps: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]) as f64 / 1e6).filter(|g| *g > 0.5 && *g < 50.0).collect();
    if gaps.len() < 10 {
        return None;
    }
    gaps.sort_by(|a, b| a.total_cmp(b));
    Some(gaps[gaps.len() / 2])
}

/// Per run window: displayed frames, discarded presents.
#[derive(Default)]
pub struct RunFrames {
    pub shown: Vec<Shown>,
    pub discarded: usize,
}

fn stream(p: &Present) -> u64 {
    (p.pid as u64) << 48 ^ p.swapchain
}

/// Outside-in: the displayed and discarded presents of `pids` started in `[t0, t1)`.
pub fn from_presents(presents: &[Present], pids: &[u32], t0: u64, t1: u64) -> RunFrames {
    let mut out = RunFrames::default();
    for p in presents.iter().filter(|p| pids.contains(&p.pid) && p.start_ns >= t0 && p.start_ns < t1) {
        match p.state.as_str() {
            "displayed" => out.shown.push(Shown { stream: stream(p), pending_ns: p.start_ns, screen_ns: p.screen_ns, begin_ns: None, input_ns: 0 }),
            "discarded" => out.discarded += 1,
            _ => {}
        }
    }
    out
}

/// From toolkit rows joined to presents (`join`): displayed rows and rows whose present was discarded.
pub fn from_rows(rows: &[Row], matched: &[Option<&Present>]) -> RunFrames {
    let mut out = RunFrames::default();
    for (r, m) in rows.iter().zip(matched) {
        if r.present_ns > 0 {
            out.shown.push(Shown { stream: r.surface as u64, pending_ns: r.begin_ns, screen_ns: r.present_ns, begin_ns: Some(r.begin_ns), input_ns: r.input_ns });
        } else if m.is_some_and(|p| p.state == "discarded") {
            out.discarded += 1;
        }
    }
    out
}

/// One measured run of a native app: its span on the host clock, and its toolkit's frame rows when
/// it has a bridge (their `present_ns` filled from the OS presents).
pub struct RunWindow {
    pub t0: u64,
    pub t1: u64,
    pub rows: Option<Vec<Row>>,
}

/// Frame metrics of a native app's runs from OS presents: its toolkit's rows joined to them when
/// it reports frames, else its processes' presents alone.
pub fn native_values(p: &Presents, pids: &[u32], runs: &mut [RunWindow]) -> BTreeMap<String, f64> {
    let (t0, t1) = (runs.first().map_or(0, |r| r.t0), runs.last().map_or(0, |r| r.t1));
    let frames: Vec<RunFrames> = runs
        .iter_mut()
        .map(|r| match r.rows.as_mut().filter(|rows| !rows.is_empty()) {
            Some(rows) => {
                let matched = join(rows, &p.presents, pids);
                from_rows(rows, &matched)
            }
            None => from_presents(&p.presents, pids, r.t0, r.t1),
        })
        .collect();
    values(&frames, refresh_ms(&p.vsyncs, t0.saturating_sub(1_000_000_000), t1 + 1_000_000_000))
}

/// Missed refreshes and the intervals between frames shown while the target kept drawing.
fn gaps(shown: &[Shown], refresh_ns: f64) -> (usize, Vec<f64>) {
    let mut by: BTreeMap<u64, Vec<&Shown>> = BTreeMap::new();
    for s in shown {
        by.entry(s.stream).or_default().push(s);
    }
    let (mut missed, mut intervals) = (0usize, Vec::new());
    for list in by.values_mut() {
        list.sort_by_key(|s| s.screen_ns);
        for w in list.windows(2) {
            let (a, b) = (w[0], w[1]);
            if b.screen_ns <= a.screen_ns || (b.pending_ns as f64) > a.screen_ns as f64 + refresh_ns / 2.0 {
                continue;
            }
            let gap = (b.screen_ns - a.screen_ns) as f64;
            intervals.push(gap / 1e6);
            missed += ((gap / refresh_ns).round() as usize).saturating_sub(1);
        }
    }
    (missed, intervals)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    crate::metrics::percentile(sorted, p)
}

/// Frame metrics over measured runs: interval and cost percentiles pool every run's frames, counts
/// are the median per run (as `metrics::block_values` does for Chromium's frames). Nothing without
/// a refresh period (no vertical blanks: no display).
pub fn values(runs: &[RunFrames], refresh_ms: Option<f64>) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let Some(r) = refresh_ms else { return out };
    let refresh_ns = r * 1e6;
    let (mut intervals, mut costs, mut inputs) = (Vec::new(), Vec::new(), Vec::new());
    let (mut counts, mut dropped, mut discarded) = (Vec::new(), Vec::new(), Vec::new());
    let (mut shown_all, mut dropped_all) = (0usize, 0usize);
    for run in runs {
        let (missed, iv) = gaps(&run.shown, refresh_ns);
        intervals.extend(iv);
        costs.extend(run.shown.iter().filter_map(|s| s.begin_ns.filter(|b| *b > 0 && *b <= s.screen_ns).map(|b| (s.screen_ns - b) as f64 / 1e6)));
        inputs.extend(run.shown.iter().filter(|s| s.input_ns > 0 && s.input_ns <= s.screen_ns).map(|s| (s.screen_ns - s.input_ns) as f64 / 1e6));
        counts.push(run.shown.len() as f64);
        dropped.push((missed + run.discarded) as f64);
        discarded.push(run.discarded as f64);
        shown_all += run.shown.len();
        dropped_all += missed + run.discarded;
    }
    let sort = |v: &mut Vec<f64>| v.sort_by(|a, b| a.total_cmp(b));
    sort(&mut intervals);
    sort(&mut costs);
    sort(&mut inputs);
    if !intervals.is_empty() {
        for (name, p) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)] {
            out.insert(format!("frames.interval.{name}"), pct(&intervals, p));
        }
        out.insert("frames.interval.max".into(), *intervals.last().unwrap_or(&0.0));
    }
    if !costs.is_empty() {
        for (name, p) in [("p50", 0.5), ("p90", 0.9), ("p95", 0.95), ("p99", 0.99)] {
            out.insert(format!("frames.presented.{name}"), pct(&costs, p));
        }
        out.insert("frames.presented.max".into(), *costs.last().unwrap_or(&0.0));
    }
    if !inputs.is_empty() {
        out.insert("interaction.n".into(), inputs.len() as f64);
        out.insert("interaction.p50".into(), pct(&inputs, 0.5));
        out.insert("interaction.p95".into(), pct(&inputs, 0.95));
    }
    out.insert("frames.n".into(), shown_all as f64);
    out.insert("frames.presented.count".into(), crate::metrics::median(&counts));
    out.insert("frames.dropped".into(), crate::metrics::median(&dropped));
    out.insert("frames.discarded".into(), crate::metrics::median(&discarded));
    if shown_all + dropped_all > 0 {
        out.insert("frames.dropped.pct".into(), 100.0 * dropped_all as f64 / (shown_all + dropped_all) as f64);
    }
    out.insert("display.hz".into(), 1000.0 / r);
    out
}
