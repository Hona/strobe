//! Turning a block's trace into metric values (SPEC §7, §9.2).

use crate::engine;
use crate::model::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// A run or phase window between two `strobe:<token>:…:start|end` marks.
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub key: String,
    pub t0: i64,
    pub t1: i64,
    pub upid: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Frame {
    pub ts: i64,
    pub dur: i64,
    pub state: String,
    /// `frame_source:frame_sequence`: one vsync. Chromium can report a sequence more than once.
    pub seq: String,
    /// A FORKED report: the compositor's part of a frame whose main-thread part is pending.
    pub forked: bool,
}

/// One vsync's outcome after merging every report for its sequence.
#[derive(Clone, Debug)]
pub struct Vsync {
    pub ts: i64,
    pub cost: i64,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Presented,
    Partial,
    Dropped,
    NoUpdate,
}

/// Merges a window's frame reports into one outcome per vsync, in time order.
pub fn vsyncs(frames: &[Frame]) -> Vec<Vsync> {
    let mut by: BTreeMap<&str, Vec<&Frame>> = BTreeMap::new();
    for f in frames {
        by.entry(f.seq.as_str()).or_default().push(f);
    }
    let mut out: Vec<Vsync> = by
        .values()
        .map(|group| {
            let pick = |state: &str| group.iter().filter(|f| f.state == state).min_by_key(|f| f.forked);
            let ts = group.iter().map(|f| f.ts).min().unwrap_or(0);
            if let Some(f) = pick("STATE_PRESENTED_ALL") {
                Vsync { ts, cost: f.dur, outcome: Outcome::Presented }
            } else if let Some(f) = pick("STATE_PRESENTED_PARTIAL") {
                Vsync { ts, cost: f.dur, outcome: Outcome::Partial }
            } else if group.iter().any(|f| f.state == "STATE_DROPPED") {
                Vsync { ts, cost: 0, outcome: Outcome::Dropped }
            } else {
                Vsync { ts, cost: 0, outcome: Outcome::NoUpdate }
            }
        })
        .collect();
    out.sort_by_key(|v| v.ts);
    // A dropped vsync followed by one where the page wanted no update lost nothing on screen: no content was
    // waiting for it. A page drawing at 30 fps on a 165 Hz display asked for a frame between its updates and
    // Chromium reported the vsync it missed as dropped (BACKFILL), which read as 20% "choppier" for a page
    // whose updates came every 36.4 ms, as before.
    for i in 0..out.len().saturating_sub(1) {
        if out[i].outcome == Outcome::Dropped && out[i + 1].outcome == Outcome::NoUpdate {
            out[i].outcome = Outcome::NoUpdate;
        }
    }
    out
}

/// Gaps between consecutive presented frames while the page kept wanting updates (ms).
pub fn intervals(v: &[Vsync]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut last: Option<i64> = None;
    for s in v {
        match s.outcome {
            Outcome::Presented | Outcome::Partial => {
                let at = s.ts + s.cost;
                if let Some(prev) = last
                    && at > prev
                {
                    out.push((at - prev) as f64 / 1e6);
                }
                last = Some(at);
            }
            Outcome::NoUpdate => last = None,
            Outcome::Dropped => {}
        }
    }
    out
}

/// The vsync interval (ms): the median gap between consecutive sequence starts.
pub fn vsync_ms(v: &[Vsync]) -> Option<f64> {
    let gaps: Vec<f64> = v.windows(2).map(|w| (w[1].ts - w[0].ts) as f64 / 1e6).filter(|g| *g > 0.5 && *g < 50.0).collect();
    (gaps.len() >= 10).then(|| median(&gaps))
}

/// Everything strobe reads from one block's trace.
#[derive(Debug, Default, Serialize)]
pub struct BlockData {
    pub windows: Vec<Window>,
    /// Per window key.
    pub frames: BTreeMap<String, Vec<Frame>>,
    pub tasks: BTreeMap<String, Vec<(i64, i64)>>,
    pub latencies: BTreeMap<String, Vec<(i64, String)>>,
    /// Layout shifts per window key: (ts, score). Only in traces with the `loading` category.
    pub shifts: BTreeMap<String, Vec<(i64, f64)>>,
    /// Main-thread GC slice durations per window key (ns). Only in traces with `disabled-by-default-v8.gc`.
    pub gc: BTreeMap<String, Vec<i64>>,
    /// The GPU process's outermost shader and pipeline compile slices per window key (ns), on any of its
    /// threads. Only in traces with the Dawn categories.
    pub gpu_compile: BTreeMap<String, Vec<i64>>,
}

impl BlockData {
    /// Relabels runs before `first` as warmup (`…:run:<i>` → `…:warm:<i>`, phases included): a block
    /// decides where warmup ended only after the runs, from the work they did.
    pub fn warm_below(&mut self, first: usize) {
        let demote = |key: &str| -> String {
            let Some((head, rest)) = key.split_once(":run:") else { return key.to_string() };
            let index = rest.split(':').next().and_then(|i| i.parse::<usize>().ok());
            if index.is_some_and(|i| i < first) { format!("{head}:warm:{rest}") } else { key.to_string() }
        };
        for w in &mut self.windows {
            w.key = demote(&w.key);
        }
        self.frames = std::mem::take(&mut self.frames).into_iter().map(|(k, v)| (demote(&k), v)).collect();
        self.tasks = std::mem::take(&mut self.tasks).into_iter().map(|(k, v)| (demote(&k), v)).collect();
        self.latencies = std::mem::take(&mut self.latencies).into_iter().map(|(k, v)| (demote(&k), v)).collect();
        self.shifts = std::mem::take(&mut self.shifts).into_iter().map(|(k, v)| (demote(&k), v)).collect();
        self.gc = std::mem::take(&mut self.gc).into_iter().map(|(k, v)| (demote(&k), v)).collect();
        self.gpu_compile = std::mem::take(&mut self.gpu_compile).into_iter().map(|(k, v)| (demote(&k), v)).collect();
    }

    /// GPU-process compile time of each run window (ms), 0 for a run that compiled nothing.
    pub fn gpu_compile_per_run(&self, prefix: &str) -> BTreeMap<String, f64> {
        self.windows
            .iter()
            .filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:"))
            .map(|w| (w.key.rsplit_once(&format!(":{prefix}:")).map_or(w.key.clone(), |(_, i)| format!("{prefix}:{i}")), self.gpu_compile.get(&w.key).map_or(0.0, |s| s.iter().sum::<i64>() as f64 / 1e6)))
            .collect()
    }

    /// Main-thread GC time of each run window (ms), 0 for a run without a collection.
    pub fn gc_per_run(&self, prefix: &str) -> BTreeMap<String, f64> {
        self.windows
            .iter()
            .filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:"))
            .map(|w| (w.key.rsplit_once(&format!(":{prefix}:")).map_or(w.key.clone(), |(_, i)| format!("{prefix}:{i}")), self.gc.get(&w.key).map_or(0.0, |s| s.iter().sum::<i64>() as f64 / 1e6)))
            .collect()
    }

    /// The summed layout-shift score of each run window, 0 for a run without shifts.
    pub fn shift_per_run(&self, prefix: &str) -> BTreeMap<String, f64> {
        self.windows
            .iter()
            .filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:"))
            .map(|w| (w.key.rsplit_once(&format!(":{prefix}:")).map_or(w.key.clone(), |(_, i)| format!("{prefix}:{i}")), self.shifts.get(&w.key).map_or(0.0, |s| s.iter().map(|x| x.1).sum())))
            .collect()
    }
}

pub fn read(trace: &Path, token: &str) -> Result<BlockData> {
    let sql = format!(
        r#"
create perfetto table _m as
  select s.name, s.ts, t.upid from slice s
  join thread_track tt on s.track_id = tt.id join thread t using(utid)
  where s.name glob 'strobe:{token}:*';
create perfetto table _w as
  select substr(a.name, 1, length(a.name) - 6) as k, a.ts as t0, b.ts as t1, a.upid as upid
  from _m a join _m b on b.name = substr(a.name, 1, length(a.name) - 6) || ':end'
  where a.name glob '*:start';
select 'win' as kind, k, t0 as a, t1 as b, upid as c, null as d from _w
union all
select 'frame', w.k, s.ts, s.dur,
  extract_arg(s.arg_set_id, 'frame_reporter.frame_source') || ':' || extract_arg(s.arg_set_id, 'frame_reporter.frame_sequence'),
  extract_arg(s.arg_set_id, 'frame_reporter.state') || '|' || coalesce(extract_arg(s.arg_set_id, 'frame_reporter.frame_type'), '')
  from slice s join track tr on s.track_id = tr.id
  join _w w on extract_arg(tr.dimension_arg_set_id, 'upid') = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name = 'PipelineReporter'
union all
select 'task', w.k, s.ts, s.dur, null, null
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name = 'RunTask' and s.depth = 0 and t.name = 'CrRendererMain'
union all
select 'lat', w.k, s.ts, s.dur, null, extract_arg(s.arg_set_id, 'event_latency.event_type')
  from slice s join track tr on s.track_id = tr.id
  join _w w on extract_arg(tr.dimension_arg_set_id, 'upid') = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name = 'EventLatency'
union all
select 'shift', w.k, s.ts, null, null,
  coalesce(extract_arg(s.arg_set_id, 'debug.data.weighted_score_delta'), extract_arg(s.arg_set_id, 'debug.data.score'))
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name = 'LayoutShift'
union all
select 'gc', w.k, s.ts, s.dur, null, null
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  left join slice p on p.id = s.parent_id
  where s.name glob 'V8.GC_*' and t.name = 'CrRendererMain' and (p.name is null or p.name not glob 'V8.GC_*')
union all
select 'gpuc', w.k, s.ts, s.dur, null, null
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid) join process pr using(upid)
  join _w w on s.ts >= w.t0 and s.ts < w.t1
  where pr.name glob 'GPU*' and s.category glob '*gpu.dawn*' and (s.name glob '*Compile*' or s.name glob '*APICreate*Pipeline*')
    and not exists (select 1 from ancestor_slice(s.id) a where a.name glob '*Compile*' or a.name glob '*APICreate*Pipeline*');
"#
    );
    let table = engine::query(trace, &sql)?;
    let mut data = BlockData::default();
    let num = |c: &Option<String>| c.as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) as i64;
    for row in &table.rows {
        let (kind, key) = (row[0].as_deref().unwrap_or(""), row[1].clone().unwrap_or_default());
        match kind {
            "win" => data.windows.push(Window { key, t0: num(&row[2]), t1: num(&row[3]), upid: num(&row[4]) }),
            "frame" => {
                let raw = row[5].clone().unwrap_or_default();
                let (state, kind) = raw.split_once('|').unwrap_or((raw.as_str(), ""));
                data.frames.entry(key).or_default().push(Frame { ts: num(&row[2]), dur: num(&row[3]), state: state.to_string(), seq: row[4].clone().unwrap_or_default(), forked: kind == "FORKED" });
            }
            "task" => data.tasks.entry(key).or_default().push((num(&row[2]), num(&row[3]))),
            "lat" => data.latencies.entry(key).or_default().push((num(&row[3]), row[5].clone().unwrap_or_default())),
            "shift" => data.shifts.entry(key).or_default().push((num(&row[2]), row[5].as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0))),
            "gc" => data.gc.entry(key).or_default().push(num(&row[3])),
            "gpuc" => data.gpu_compile.entry(key).or_default().push(num(&row[3])),
            _ => {}
        }
    }
    data.windows.sort_by_key(|w| w.t0);
    Ok(data)
}

const DISCRETE: [&str; 8] = ["MOUSE_PRESSED", "MOUSE_RELEASED", "KEY_PRESSED", "KEY_RELEASED", "TOUCH_PRESSED", "TOUCH_RELEASED", "GESTURE_TAP", "GESTURE_TAP_DOWN"];

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = (sorted.len() - 1) as f64 * p;
    let (lo, hi) = (i.floor() as usize, i.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (i - lo as f64)
}

pub fn median(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.total_cmp(b));
    percentile(&v, 0.5)
}

/// Values of every metric for one block: frame metrics pool all measured runs' frames; other
/// metrics are the median over runs. `extra` holds per-run values measured outside the trace
/// (run wall time, CPU), keyed by metric then run key.
pub fn block_values(data: &BlockData, prefix: &str, extra: &BTreeMap<String, BTreeMap<String, f64>>) -> BTreeMap<String, f64> {
    let runs: Vec<&Window> = data.windows.iter().filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:")).collect();
    let mut out = BTreeMap::new();
    let mut frame_ms: Vec<f64> = Vec::new();
    let mut interval_ms: Vec<f64> = Vec::new();
    let mut vsync: Vec<f64> = Vec::new();
    let (mut presented_all, mut dropped_all) = (0usize, 0usize);
    let mut per_run: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    let mut discrete: Vec<f64> = Vec::new();
    let mut scroll: Vec<f64> = Vec::new();
    for w in &runs {
        let v = vsyncs(data.frames.get(&w.key).map(Vec::as_slice).unwrap_or(&[]));
        let presented: Vec<&Vsync> = v.iter().filter(|s| matches!(s.outcome, Outcome::Presented | Outcome::Partial)).collect();
        let dropped = v.iter().filter(|s| s.outcome == Outcome::Dropped).count();
        frame_ms.extend(presented.iter().map(|s| s.cost as f64 / 1e6));
        interval_ms.extend(intervals(&v));
        vsync.extend(vsync_ms(&v));
        presented_all += presented.len();
        dropped_all += dropped;
        per_run.entry("frames.presented.count").or_default().push(presented.len() as f64);
        per_run.entry("frames.dropped").or_default().push(dropped as f64);
        per_run.entry("frames.partial").or_default().push(v.iter().filter(|s| s.outcome == Outcome::Partial).count() as f64);
        // A lean trace has no RunTask slices: main.busy then comes from the Performance domain.
        if !data.tasks.is_empty() {
            let tasks = data.tasks.get(&w.key).map(Vec::as_slice).unwrap_or(&[]);
            let span = (w.t1 - w.t0).max(1) as f64;
            let busy: f64 = tasks.iter().map(|(ts, dur)| ((*ts + *dur).min(w.t1) - ts).max(0) as f64).sum();
            per_run.entry("main.busy").or_default().push(100.0 * busy / span);
            per_run.entry("main.tasks.long").or_default().push(tasks.iter().filter(|t| t.1 > 50_000_000).count() as f64);
        }
        for (dur, ty) in data.latencies.get(&w.key).map(Vec::as_slice).unwrap_or(&[]) {
            let ms = *dur as f64 / 1e6;
            if DISCRETE.contains(&ty.as_str()) {
                discrete.push(ms);
            } else if ty.contains("SCROLL_UPDATE") || ty == "MOUSE_WHEEL" {
                scroll.push(ms);
            }
        }
    }
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    if !frame_ms.is_empty() {
        for (name, p) in [("p50", 0.5), ("p90", 0.9), ("p95", 0.95), ("p99", 0.99)] {
            out.insert(format!("frames.presented.{name}"), percentile(&frame_ms, p));
        }
        out.insert("frames.presented.max".into(), *frame_ms.last().unwrap_or(&0.0));
    }
    interval_ms.sort_by(|a, b| a.total_cmp(b));
    if !interval_ms.is_empty() {
        for (name, p) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)] {
            out.insert(format!("frames.interval.{name}"), percentile(&interval_ms, p));
        }
        out.insert("frames.interval.max".into(), *interval_ms.last().unwrap_or(&0.0));
    }
    if presented_all + dropped_all > 0 {
        out.insert("frames.dropped.pct".into(), 100.0 * dropped_all as f64 / (presented_all + dropped_all) as f64);
    }
    if !vsync.is_empty() {
        out.insert("display.hz".into(), 1000.0 / median(&vsync));
    }
    out.insert("frames.n".into(), frame_ms.len() as f64);
    for (name, values) in per_run {
        out.insert(name.to_string(), median(&values));
    }
    discrete.sort_by(|a, b| a.total_cmp(b));
    if !discrete.is_empty() {
        out.insert("interaction.n".into(), discrete.len() as f64);
        out.insert("interaction.p50".into(), percentile(&discrete, 0.5));
        out.insert("interaction.p95".into(), percentile(&discrete, 0.95));
    }
    scroll.sort_by(|a, b| a.total_cmp(b));
    if !scroll.is_empty() {
        out.insert("scroll.n".into(), scroll.len() as f64);
    }
    if scroll.len() >= 20 {
        out.insert("scroll.latency.p95".into(), percentile(&scroll, 0.95));
    }
    for (metric, runs) in extra {
        let values: Vec<f64> = runs.values().copied().collect();
        out.insert(metric.clone(), median(&values));
    }
    if let Some(v) = out.get("cpu.app.total").copied() {
        out.insert("cpu.process.total".into(), v);
    }
    out
}

/// Frame costs per phase name, pooled over measured runs (for reports).
pub fn phase_frames(data: &BlockData) -> BTreeMap<String, Vec<f64>> {
    let mut out: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for w in data.windows.iter().filter(|w| w.key.contains(":phase:") && !w.key.contains(":warm:")) {
        let name = w.key.rsplit(':').next().unwrap_or("").to_string();
        let v = vsyncs(data.frames.get(&w.key).map(Vec::as_slice).unwrap_or(&[]));
        out.entry(name).or_default().extend(v.iter().filter(|s| matches!(s.outcome, Outcome::Presented | Outcome::Partial)).map(|s| s.cost as f64 / 1e6));
    }
    out
}

/// Every presented frame's cost in measured runs (ms), sorted; for reports.
pub fn run_frames(data: &BlockData) -> Vec<f64> {
    let mut out: Vec<f64> = data
        .frames
        .iter()
        .filter(|(k, _)| k.contains(":run:") && !k.contains(":phase:"))
        .flat_map(|(_, f)| vsyncs(f).into_iter().filter(|s| matches!(s.outcome, Outcome::Presented | Outcome::Partial)).map(|s| s.cost as f64 / 1e6).collect::<Vec<_>>())
        .collect();
    out.sort_by(|a, b| a.total_cmp(b));
    out
}

/// Every frame interval in measured runs (ms), sorted; for reports.
pub fn run_intervals(data: &BlockData) -> Vec<f64> {
    let mut out: Vec<f64> = data.frames.iter().filter(|(k, _)| k.contains(":run:") && !k.contains(":phase:")).flat_map(|(_, f)| intervals(&vsyncs(f))).collect();
    out.sort_by(|a, b| a.total_cmp(b));
    out
}
