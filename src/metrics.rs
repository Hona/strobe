//! Turning a block's trace into metric values (SPEC §7, §9.2).

use crate::engine;
use crate::model::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

pub mod produced;

/// A run's span with the stretches the bench file left out of it (`ctx.untimed`, SPEC §9.6), on
/// one clock (the host's, or a trace's): its timed totals, frames and inputs cover the rest.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Timed {
    pub t0: u64,
    pub t1: u64,
    /// Inside `[t0, t1]`, in order, not overlapping.
    pub untimed: Vec<(u64, u64)>,
}

impl Timed {
    pub fn whole(t0: u64, t1: u64) -> Timed {
        Timed { t0, t1, untimed: Vec::new() }
    }

    /// The stretches between the untimed ones, in order.
    pub fn segments(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut at = self.t0;
        for &(a, b) in &self.untimed {
            if a > at {
                out.push((at, a));
            }
            at = at.max(b);
        }
        if self.t1 > at {
            out.push((at, self.t1));
        }
        out
    }

    /// Whether `t` is in an untimed stretch.
    pub fn untimed_at(&self, t: u64) -> bool {
        self.untimed.iter().any(|&(a, b)| t >= a && t < b)
    }

    /// The untimed time inside `[a, b)`.
    pub fn untimed_in(&self, a: u64, b: u64) -> u64 {
        self.untimed.iter().map(|&(x, y)| y.min(b).saturating_sub(x.max(a))).sum()
    }

    /// The run's timed length (ns).
    pub fn ns(&self) -> u64 {
        self.t1.saturating_sub(self.t0).saturating_sub(self.untimed_in(self.t0, self.t1))
    }
}

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
    /// The renderer's and the raster/GPU side's stages of the report (ns, `STAGES_CPU`, `STAGES_GPU`),
    /// when the report carries them.
    pub split: Option<(i64, i64)>,
    /// The report's slice id in the trace.
    pub id: i64,
}

/// One vsync's outcome after merging every report for its sequence.
#[derive(Clone, Debug)]
pub struct Vsync {
    pub ts: i64,
    pub cost: i64,
    pub outcome: Outcome,
    /// The presented report's stage split (renderer, GPU process), ns.
    pub split: Option<(i64, i64)>,
    /// The presented report's slice id (-1 for none), to find its stages again.
    pub report: i64,
}

/// The stages of a frame report (Chromium's `PipelineReporter` children) on the renderer's side: its
/// compositor thread begins the frame, the main thread runs and commits, the compositor activates the
/// tree and submits the frame (or, with the display tree in the GPU process, its update).
pub const STAGES_RENDERER: [&str; 6] = ["BeginImplFrameToSendBeginMainFrame", "SendBeginMainFrameToCommit", "Commit", "Activation", "EndActivateToSubmitCompositorFrame", "EndActivateToSubmitUpdateDisplayTree"];
/// The GPU process's side: tiles raster between commit and activation, and the display compositor
/// draws to its swap (the children of the submit-to-presentation stage, by name on each platform).
/// Its waits (the frame in flight to the display compositor, its deadline, the buffer's latch and the
/// display's refresh after the swap) are in neither side.
pub const STAGES_RASTER: &str = "EndCommitToActivation";
pub const STAGES_VIZ_PARENT: [&str; 2] = ["SubmitCompositorFrameToPresentationCompositorFrame", "SubmitUpdateDisplayTreeToPresentationCompositorFrame"];
pub const STAGES_VIZ: [&str; 5] = ["StartDrawToSwapStart", "Swap", "SwapStartToSwapEnd", "SwapStartToBufferAvailable", "BufferAvailableToBufferReady"];
/// The share of presented frames whose reports must carry both sides for a block to split them.
pub const SPLIT_SHARE: f64 = 0.95;

fn sql_list(names: &[&str]) -> String {
    names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(",")
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Presented,
    Partial,
    Dropped,
    NoUpdate,
    /// Where an untimed stretch's frames were (`BlockData::leave_out_untimed`): no interval spans it,
    /// and it is no vsync.
    Untimed,
}

/// The state of the frame `leave_out_untimed` puts where an untimed stretch's frames were.
const UNTIMED_STATE: &str = "STROBE_UNTIMED";

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
                Vsync { ts, cost: f.dur, outcome: Outcome::Presented, split: f.split, report: f.id }
            } else if let Some(f) = pick("STATE_PRESENTED_PARTIAL") {
                Vsync { ts, cost: f.dur, outcome: Outcome::Partial, split: f.split, report: f.id }
            } else if group.iter().any(|f| f.state == UNTIMED_STATE) {
                Vsync { ts, cost: 0, outcome: Outcome::Untimed, split: None, report: -1 }
            } else if group.iter().any(|f| f.state == "STATE_DROPPED") {
                Vsync { ts, cost: 0, outcome: Outcome::Dropped, split: None, report: -1 }
            } else {
                Vsync { ts, cost: 0, outcome: Outcome::NoUpdate, split: None, report: -1 }
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
            Outcome::NoUpdate | Outcome::Untimed => last = None,
            Outcome::Dropped => {}
        }
    }
    out
}

/// The vsync interval (ms): the median gap between consecutive sequence starts.
pub fn vsync_ms(v: &[Vsync]) -> Option<f64> {
    let gaps: Vec<f64> = v.windows(2).filter(|w| w.iter().all(|s| s.outcome != Outcome::Untimed)).map(|w| (w[1].ts - w[0].ts) as f64 / 1e6).filter(|g| *g > 0.5 && *g < 50.0).collect();
    (gaps.len() >= 10).then(|| median(&gaps))
}

/// Everything strobe reads from one block's trace.
#[derive(Debug, Default, Serialize)]
pub struct BlockData {
    pub windows: Vec<Window>,
    /// Per window key.
    pub frames: BTreeMap<String, Vec<Frame>>,
    pub tasks: BTreeMap<String, Vec<(i64, i64)>>,
    /// Input events' latencies: (event time, ns, event type).
    pub latencies: BTreeMap<String, Vec<(i64, i64, String)>>,
    /// Layout shifts per window key: (ts, score). Only in traces with the `loading` category.
    pub shifts: BTreeMap<String, Vec<(i64, f64)>>,
    /// Main-thread GC slices per window key (ts, ns). Only in traces with `disabled-by-default-v8.gc`.
    pub gc: BTreeMap<String, Vec<(i64, i64)>>,
    /// The GPU process's outermost shader and pipeline compile slices per window key (ts, ns), on any
    /// of its threads. Only in traces with the Dawn categories.
    pub gpu_compile: BTreeMap<String, Vec<(i64, i64)>>,
    /// The page's own `performance.measure()` spans inside each window: (name, start, ns). Marks are
    /// instants on the thread's own track; a measure is an async slice, of any length (0 too).
    pub spans: BTreeMap<String, Vec<(String, i64, i64)>>,
    /// Each run window's `ctx.untimed` stretches (trace clock), by the run's key: what `leave_out_untimed`
    /// took out of it and its phases.
    pub untimed: BTreeMap<String, Vec<(i64, i64)>>,
}

/// `[a, a + d)` without the stretches `gaps` (sorted), as (start, ns) pieces.
fn outside(a: i64, d: i64, gaps: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let (mut out, mut at, end) = (Vec::new(), a, a + d);
    for &(x, y) in gaps {
        if y <= at || x >= end {
            continue;
        }
        if x > at {
            out.push((at, x - at));
        }
        at = at.max(y);
    }
    if end > at || (d == 0 && !gaps.iter().any(|&(x, y)| a >= x && a < y)) {
        out.push((at, (end - at).max(0)));
    }
    out
}

impl BlockData {
    /// Takes the `ctx.untimed` stretches (SPEC §9.6) out of the runs they were in, and out of those
    /// runs' phases: their windows are `…:untimed:<run>.<n>`. Frames begun in one leave, with a frame
    /// that wanted no update in their place (so no interval spans the stretch), as do input latencies
    /// of events sent in one and layout shifts there; tasks, GC, compiles and the page's spans keep
    /// only their parts outside them.
    fn leave_out_untimed(&mut self) {
        for w in self.windows.iter().filter(|w| w.key.contains(":untimed:")) {
            let Some((head, rest)) = w.key.split_once(":untimed:") else { continue };
            let run = rest.split('.').next().unwrap_or("");
            self.untimed.entry(format!("{head}:run:{run}")).or_default().push((w.t0, w.t1));
        }
        self.windows.retain(|w| !w.key.contains(":untimed:"));
        let run_or_phase = |k: &String| !k.contains(":untimed:");
        self.frames.retain(|k, _| run_or_phase(k));
        self.tasks.retain(|k, _| run_or_phase(k));
        self.latencies.retain(|k, _| run_or_phase(k));
        self.shifts.retain(|k, _| run_or_phase(k));
        self.gc.retain(|k, _| run_or_phase(k));
        self.gpu_compile.retain(|k, _| run_or_phase(k));
        self.spans.retain(|k, _| run_or_phase(k));
        for gaps in self.untimed.values_mut() {
            gaps.sort_unstable();
        }
        let gaps_of = |untimed: &BTreeMap<String, Vec<(i64, i64)>>, key: &str| -> Option<Vec<(i64, i64)>> { untimed.get(key.split(":phase:").next().unwrap_or(key)).cloned() };
        let inside = |gaps: &[(i64, i64)], t: i64| gaps.iter().any(|&(a, b)| t >= a && t < b);
        for (k, frames) in self.frames.iter_mut() {
            let Some(gaps) = gaps_of(&self.untimed, k) else { continue };
            frames.retain(|f| !inside(&gaps, f.ts));
            frames.extend(gaps.iter().map(|&(a, _)| Frame { ts: a, state: UNTIMED_STATE.into(), seq: format!("untimed:{a}"), id: -1, ..Frame::default() }));
        }
        for (k, v) in self.latencies.iter_mut() {
            if let Some(gaps) = gaps_of(&self.untimed, k) {
                v.retain(|l| !inside(&gaps, l.0));
            }
        }
        for (k, v) in self.shifts.iter_mut() {
            if let Some(gaps) = gaps_of(&self.untimed, k) {
                v.retain(|s| !inside(&gaps, s.0));
            }
        }
        for map in [&mut self.tasks, &mut self.gc, &mut self.gpu_compile] {
            for (k, v) in map.iter_mut() {
                if let Some(gaps) = gaps_of(&self.untimed, k) {
                    *v = v.iter().flat_map(|&(a, d)| outside(a, d, &gaps)).collect();
                }
            }
        }
        for (k, v) in self.spans.iter_mut() {
            if let Some(gaps) = gaps_of(&self.untimed, k) {
                *v = v.iter().flat_map(|(n, a, d)| outside(*a, *d, &gaps).into_iter().map(move |(a, d)| (n.clone(), a, d))).collect();
            }
        }
    }

    /// The untimed time inside a run or phase window (ns).
    pub fn untimed_in(&self, w: &Window) -> i64 {
        let run = w.key.split(":phase:").next().unwrap_or(&w.key);
        self.untimed.get(run).map_or(0, |gaps| gaps.iter().map(|&(a, b)| (b.min(w.t1) - a.max(w.t0)).max(0)).sum())
    }

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
        self.spans = std::mem::take(&mut self.spans).into_iter().map(|(k, v)| (demote(&k), v)).collect();
        self.untimed = std::mem::take(&mut self.untimed).into_iter().map(|(k, v)| (demote(&k), v)).collect();
    }

    /// `span.<name>` of each run window (ms): the time its spans covered, overlapping ones merged (two
    /// overlapping measures of one name are one stretch of time, as nested GC slices count once), 0
    /// for a run without one. None when no run had one: the span never fired (`NoSpan`).
    pub fn span_per_run(&self, prefix: &str, name: &str) -> Option<BTreeMap<String, f64>> {
        let runs: Vec<&Window> = self.windows.iter().filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:")).collect();
        let spans = |w: &Window| -> Vec<(i64, i64)> { self.spans.get(&w.key).map(|s| s.iter().filter(|(n, _, _)| n == name).map(|&(_, ts, d)| (ts, ts + d)).collect()).unwrap_or_default() };
        if runs.iter().all(|w| spans(w).is_empty()) {
            return None;
        }
        let covered = |mut v: Vec<(i64, i64)>| -> i64 {
            v.sort_unstable();
            let (mut total, mut end) = (0i64, i64::MIN);
            for (a, b) in v {
                let a = a.max(end);
                if b > a {
                    total += b - a;
                }
                end = end.max(b);
            }
            total
        };
        Some(runs.iter().map(|w| (w.key.rsplit_once(&format!(":{prefix}:")).map_or(w.key.clone(), |(_, i)| format!("{prefix}:{i}")), covered(spans(w)) as f64 / 1e6)).collect())
    }

    /// GPU-process compile time of each run window (ms), 0 for a run that compiled nothing.
    pub fn gpu_compile_per_run(&self, prefix: &str) -> BTreeMap<String, f64> {
        self.windows
            .iter()
            .filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:"))
            .map(|w| {
                (
                    w.key.rsplit_once(&format!(":{prefix}:")).map_or(w.key.clone(), |(_, i)| format!("{prefix}:{i}")),
                    self.gpu_compile.get(&w.key).map_or(0.0, |s| s.iter().map(|x| x.1).sum::<i64>() as f64 / 1e6),
                )
            })
            .collect()
    }

    /// Main-thread GC time of each run window (ms), 0 for a run without a collection.
    pub fn gc_per_run(&self, prefix: &str) -> BTreeMap<String, f64> {
        self.windows
            .iter()
            .filter(|w| w.key.contains(&format!(":{prefix}:")) && !w.key.contains(":phase:"))
            .map(|w| {
                (
                    w.key.rsplit_once(&format!(":{prefix}:")).map_or(w.key.clone(), |(_, i)| format!("{prefix}:{i}")),
                    self.gc.get(&w.key).map_or(0.0, |s| s.iter().map(|x| x.1).sum::<i64>() as f64 / 1e6),
                )
            })
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
create perfetto table _st as
  select c.parent_id as id,
    sum(case when c.name in ({cpu}) then c.dur else 0 end) as cpu,
    sum(case when c.name in ({cpu}) then 1 else 0 end) as ncpu,
    sum(case when c.name = '{raster}' then c.dur else 0 end) as raster
  from slice c join slice p on p.id = c.parent_id
  where p.name = 'PipelineReporter' group by c.parent_id;
create perfetto table _sv as
  select v.parent_id as id, sum(g.dur) as gpu, count(*) as n
  from slice g join slice v on v.id = g.parent_id
  where v.name in ({viz_parent}) and g.name in ({gpu})
  group by v.parent_id;
select 'win' as kind, k, t0 as a, t1 as b, upid as c, null as d from _w
union all
select 'frame', w.k, s.ts, s.dur,
  extract_arg(s.arg_set_id, 'frame_reporter.frame_source') || ':' || extract_arg(s.arg_set_id, 'frame_reporter.frame_sequence'),
  extract_arg(s.arg_set_id, 'frame_reporter.state') || '|' || coalesce(extract_arg(s.arg_set_id, 'frame_reporter.frame_type'), '')
    || '|' || coalesce(case when st.ncpu > 0 and sv.n > 0 then st.cpu || ':' || (st.raster + sv.gpu) end, '') || '|' || s.id
  from slice s join track tr on s.track_id = tr.id
  join _w w on extract_arg(tr.dimension_arg_set_id, 'upid') = w.upid and s.ts >= w.t0 and s.ts < w.t1
  left join _st st on st.id = s.id left join _sv sv on sv.id = s.id
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
    and not exists (select 1 from ancestor_slice(s.id) a where a.name glob '*Compile*' or a.name glob '*APICreate*Pipeline*')
union all
select 'span', w.k, s.ts, s.dur, null, s.name
  from slice s join track tr on s.track_id = tr.id
  left join thread_track tt on tt.id = s.track_id left join thread t on t.utid = tt.utid
  join _w w on coalesce(t.upid, extract_arg(tr.dimension_arg_set_id, 'upid')) = w.upid and s.ts >= w.t0 and s.ts + s.dur <= w.t1
  where s.category glob '*blink.user_timing*' and s.dur >= 0 and s.name not glob 'strobe:*'
    and not (s.dur = 0 and tr.type = 'thread_track_event');
"#,
        cpu = sql_list(&STAGES_RENDERER),
        raster = STAGES_RASTER,
        viz_parent = sql_list(&STAGES_VIZ_PARENT),
        gpu = sql_list(&STAGES_VIZ),
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
                let mut parts = raw.splitn(4, '|');
                let (state, kind, split, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let split = split.split_once(':').and_then(|(c, g)| Some((c.parse::<f64>().ok()? as i64, g.parse::<f64>().ok()? as i64)));
                let id = id.parse::<i64>().unwrap_or(-1);
                data.frames.entry(key).or_default().push(Frame { ts: num(&row[2]), dur: num(&row[3]), state: state.to_string(), seq: row[4].clone().unwrap_or_default(), forked: kind == "FORKED", split, id });
            }
            "task" => data.tasks.entry(key).or_default().push((num(&row[2]), num(&row[3]))),
            "lat" => data.latencies.entry(key).or_default().push((num(&row[2]), num(&row[3]), row[5].clone().unwrap_or_default())),
            "shift" => data.shifts.entry(key).or_default().push((num(&row[2]), row[5].as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0))),
            "gc" => data.gc.entry(key).or_default().push((num(&row[2]), num(&row[3]))),
            "gpuc" => data.gpu_compile.entry(key).or_default().push((num(&row[2]), num(&row[3]))),
            "span" => data.spans.entry(key).or_default().push((row[5].clone().unwrap_or_default(), num(&row[2]), num(&row[3]))),
            _ => {}
        }
    }
    data.windows.sort_by_key(|w| w.t0);
    data.leave_out_untimed();
    Ok(data)
}

const DISCRETE: [&str; 8] = ["MOUSE_PRESSED", "MOUSE_RELEASED", "KEY_PRESSED", "KEY_RELEASED", "TOUCH_PRESSED", "TOUCH_RELEASED", "GESTURE_TAP", "GESTURE_TAP_DOWN"];
/// The pointer presses among them (`interaction.n.press`): Chromium times each event to the frame that
/// showed it, so a click is a press and a release there, each its own sample (SPEC §7.4).
const PRESSES: [&str; 3] = ["MOUSE_PRESSED", "TOUCH_PRESSED", "GESTURE_TAP_DOWN"];

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
    let mut presses = 0usize;
    let mut scroll: Vec<f64> = Vec::new();
    let (mut split_cpu, mut split_gpu, mut split_bad) = (Vec::new(), Vec::new(), 0usize);
    for w in &runs {
        let v = vsyncs(data.frames.get(&w.key).map(Vec::as_slice).unwrap_or(&[]));
        let presented: Vec<&Vsync> = v.iter().filter(|s| matches!(s.outcome, Outcome::Presented | Outcome::Partial)).collect();
        let dropped = v.iter().filter(|s| s.outcome == Outcome::Dropped).count();
        frame_ms.extend(presented.iter().map(|s| s.cost as f64 / 1e6));
        for (s, (c, g)) in presented.iter().filter_map(|s| Some((s, s.split?))) {
            // The stages are children of the report: together they never outlast it.
            if c < 0 || g < 0 || c + g > s.cost {
                split_bad += 1;
                continue;
            }
            split_cpu.push(c as f64 / 1e6);
            split_gpu.push(g as f64 / 1e6);
        }
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
            let span = (w.t1 - w.t0 - data.untimed_in(w)).max(1) as f64;
            let busy: f64 = tasks.iter().map(|(ts, dur)| ((*ts + *dur).min(w.t1) - ts).max(0) as f64).sum();
            per_run.entry("main.busy").or_default().push(100.0 * busy / span);
            per_run.entry("main.tasks.long").or_default().push(tasks.iter().filter(|t| t.1 > 50_000_000).count() as f64);
        }
        for (_, dur, ty) in data.latencies.get(&w.key).map(Vec::as_slice).unwrap_or(&[]) {
            let ms = *dur as f64 / 1e6;
            if DISCRETE.contains(&ty.as_str()) {
                discrete.push(ms);
                presses += usize::from(PRESSES.contains(&ty.as_str()));
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
    // Where presented frames' time went (SPEC §7.2): the renderer's stages and the GPU process's, from
    // frames whose reports carry both and keep the stages inside the frame; only when nearly all
    // presented frames do (a split of the few that carry stages would describe those few).
    out.insert("frames.split.n".into(), split_cpu.len() as f64);
    if split_bad > 0 {
        out.insert("frames.split.bad".into(), split_bad as f64);
    } else if !split_cpu.is_empty() && split_cpu.len() as f64 >= SPLIT_SHARE * frame_ms.len() as f64 {
        split_cpu.sort_by(|a, b| a.total_cmp(b));
        split_gpu.sort_by(|a, b| a.total_cmp(b));
        out.insert("frames.renderer.p95".into(), percentile(&split_cpu, 0.95));
        out.insert("frames.viz.p95".into(), percentile(&split_gpu, 0.95));
    }
    for (name, values) in per_run {
        out.insert(name.to_string(), median(&values));
    }
    discrete.sort_by(|a, b| a.total_cmp(b));
    if !discrete.is_empty() {
        out.insert("interaction.n".into(), discrete.len() as f64);
        out.insert("interaction.n.press".into(), presses as f64);
        out.insert("interaction.p50".into(), percentile(&discrete, 0.5));
        out.insert("interaction.p95".into(), percentile(&discrete, 0.95));
    }
    // As native targets give them: every percentile with its n, and too few updates for one refused
    // `TailTooShort` against its minimum n, not left out.
    scroll.sort_by(|a, b| a.total_cmp(b));
    if !scroll.is_empty() {
        out.insert("scroll.n".into(), scroll.len() as f64);
        out.insert("scroll.latency.p50".into(), percentile(&scroll, 0.5));
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

/// A frame-split metric (`frames.renderer.*`, `frames.viz.*`).
pub fn split_metric(m: &str) -> bool {
    m.starts_with("frames.renderer.") || m.starts_with("frames.viz.")
}

/// Why a page block has no value for a declared frame-split metric: its stages broke the frame's own
/// bounds (never split then), or too few presented frames' reports carried both sides.
pub fn split_refusal(metric: &str, values: &BTreeMap<String, f64>) -> crate::model::Refusal {
    use crate::model::{Refusal, ValidatorId};
    let (split, all) = (values.get("frames.split.n").copied().unwrap_or(0.0), values.get("frames.n").copied().unwrap_or(0.0));
    match values.get("frames.split.bad") {
        Some(bad) => Refusal::new(ValidatorId::NoPresentSource, format!("{metric}: {bad:.0} presented frames had stages that add up to more than the frame, so no frame is split")),
        None if all == 0.0 => Refusal::new(ValidatorId::NoPresentSource, format!("{metric}: no presented frame")),
        None => Refusal::new(
            ValidatorId::NoPresentSource,
            format!("{metric}: {split:.0} of {all:.0} presented frames carried both sides' pipeline stages (renderer and GPU process), under the {:.0}% a split needs", SPLIT_SHARE * 100.0),
        ),
    }
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
