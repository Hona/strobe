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

/// Slack around a row's present call when matching it to an OS present: the runtime logs the
/// present's start on the presenting thread inside the call, on the same QPC clock as the row, so
/// a few µs cover rounding. Wider would reach the next surface's or frame's present call.
const JOIN_SLACK_NS: u64 = 10_000;

/// Rows (or inputs) that matched no present beyond this share, with no events lost, refuse the
/// frame metrics: a join that misses that much is not following the app's presents.
const UNMATCHED_MAX: f64 = 0.05;

fn stream(p: &Present) -> u64 {
    (p.pid as u64) << 48 ^ p.swapchain
}

/// What a join saw: rows (or inputs) it tried, those that matched no present, and those with more
/// than one present that fit.
#[derive(Clone, Copy, Debug, Default)]
pub struct JoinStats {
    pub tried: usize,
    pub unmatched: usize,
    pub ambiguous: usize,
}

impl JoinStats {
    /// Why the joined frames cannot be defended, if they cannot: `AmbiguousPresent` when rows fit
    /// several presents, `PresentsUnmatched` when over 5% matched none while the session lost no
    /// events (with lost events, `PresentsLost` already says frame counts may be short).
    pub fn refusal(&self, events_lost: u64) -> Option<String> {
        if self.tried == 0 {
            return None;
        }
        let share = |n: usize| n as f64 / self.tried as f64;
        if share(self.ambiguous) > UNMATCHED_MAX {
            return Some(format!(
                "AmbiguousPresent: {} of {} frames fit more than one OS present (several surfaces presenting from one thread at once), so which reached the screen when is unknown",
                self.ambiguous, self.tried
            ));
        }
        if events_lost == 0 && share(self.unmatched) > UNMATCHED_MAX {
            return Some(format!(
                "PresentsUnmatched: {} of {} frames matched no OS present with no events lost: the present session is not following this app's presents",
                self.unmatched, self.tried
            ));
        }
        None
    }
}

/// The OS present of each toolkit row: one of `pids`, started inside the row's present call
/// (`cpu_end_ns`..`submit_ns`, within `JOIN_SLACK_NS`) on the row's own thread, the one nearest the
/// call's start. Each surface keeps to the swapchain it first matched, and a swapchain to its
/// surface. Another thread's present counts only when the rows have a single surface (a driver
/// that presents from its own thread). Fills `present_ns` with the screen time of displayed ones
/// and returns, per row, the matched present.
pub fn join<'a>(rows: &mut [Row], presents: &'a [Present], pids: &[u32]) -> (Vec<Option<&'a Present>>, JoinStats) {
    let mine: Vec<&Present> = presents.iter().filter(|p| pids.contains(&p.pid)).collect();
    let mut used = vec![false; mine.len()];
    let mut out = vec![None; rows.len()];
    let mut stats = JoinStats::default();
    let single = rows.iter().map(|r| r.surface).collect::<std::collections::BTreeSet<_>>().len() <= 1;
    let mut chain_of: BTreeMap<u32, u64> = BTreeMap::new();
    let mut surface_of: BTreeMap<u64, u32> = BTreeMap::new();
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].cpu_end_ns.min(rows[i].submit_ns));
    for i in order {
        let r = rows[i];
        if r.submit_ns == 0 {
            continue;
        }
        stats.tried += 1;
        let start = r.cpu_end_ns.min(r.submit_ns);
        let lo = start.saturating_sub(JOIN_SLACK_NS);
        let hi = r.submit_ns + JOIN_SLACK_NS;
        // Presents are sorted by start: only those inside the window are looked at.
        let from = mine.partition_point(|p| p.start_ns < lo);
        let fits = |same_thread: bool| -> Vec<usize> {
            mine[from..]
                .iter()
                .enumerate()
                .take_while(|(_, p)| p.start_ns <= hi)
                .filter(|(j, p)| {
                    !used[from + j]
                        && (!same_thread || p.tid == r.tid)
                        && chain_of.get(&r.surface).is_none_or(|c| *c == stream(p))
                        && surface_of.get(&stream(p)).is_none_or(|s| *s == r.surface)
                })
                .map(|(j, _)| from + j)
                .collect()
        };
        let mut found = fits(true);
        if found.is_empty() && single {
            found = fits(false);
        }
        // Nearest to the call's start: a present started later belongs to a later call.
        let Some(&j) = found.iter().min_by_key(|&&j| mine[j].start_ns.abs_diff(start)) else {
            stats.unmatched += 1;
            continue;
        };
        if found.len() > 1 && !chain_of.contains_key(&r.surface) {
            stats.ambiguous += 1;
            continue;
        }
        used[j] = true;
        chain_of.insert(r.surface, stream(mine[j]));
        surface_of.insert(stream(mine[j]), r.surface);
        out[i] = Some(mine[j]);
        if mine[j].state == "displayed" && rows[i].present_ns == 0 {
            rows[i].present_ns = mine[j].screen_ns;
        }
    }
    (out, stats)
}

/// How long a target must have presented nothing before an input for its next present to be the
/// input's: a frame already under way when the input came may not hold it.
pub fn quiet_ns(refresh_ms: f64) -> u64 {
    (2.0 * refresh_ms * 1e6) as u64
}

/// The longest an input waits for its frame: past this its present is something else's.
const INPUT_HORIZON_NS: u64 = 1_000_000_000;

/// The frame that showed each input (an input strobe sent, at `input_ns` on the host clock): the
/// first present of `pids` started after it that reached the screen, when the target presented
/// nothing for `quiet_ns` before it (at rest, so that present is the input's). An input the target
/// was busy drawing through gets `None` and is not counted; one with no present within a second
/// counts as unmatched (it changed nothing on screen, or its present was missed).
pub fn join_inputs<'a>(inputs: &[u64], presents: &'a [Present], pids: &[u32], quiet_ns: u64) -> (Vec<Option<&'a Present>>, JoinStats) {
    let mine: Vec<&Present> = presents.iter().filter(|p| pids.contains(&p.pid)).collect();
    let mut stats = JoinStats::default();
    let out = inputs
        .iter()
        .map(|&t| {
            let at = mine.partition_point(|p| p.start_ns < t);
            if at > 0 && mine[at - 1].start_ns + quiet_ns > t {
                return None;
            }
            stats.tried += 1;
            let shown = mine[at..].iter().take_while(|p| p.start_ns <= t + INPUT_HORIZON_NS).find(|p| p.state == "displayed").copied();
            if shown.is_none() {
                stats.unmatched += 1;
            }
            shown
        })
        .collect();
    (out, stats)
}

/// The refresh period (ms) of the display the target shows on (`display`, its window's display
/// path): the median spacing of that path's vertical blanks in `[t0, t1]`, or its nominal refresh
/// when it has too few blanks or they disagree with it by over 10% (another adapter's path of
/// the same id). Without a display path, only when every path with blanks runs at one rate: the
/// busiest of several displays is not the one the target is on.
pub fn refresh_ms(vsyncs: &[(u64, u32)], t0: u64, t1: u64, display: Option<&crate::host::DisplayPath>) -> Option<f64> {
    let mut by: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for &(t, target) in vsyncs.iter().filter(|v| v.0 >= t0 && v.0 <= t1) {
        by.entry(target).or_default().push(t);
    }
    let period = |ts: &mut Vec<u64>| -> Option<f64> {
        // A driver may log one blank twice (`VSyncDPC` and its multi-plane event): gaps under
        // 0.5 ms are not refreshes.
        ts.sort_unstable();
        let mut gaps: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]) as f64 / 1e6).filter(|g| *g > 0.5 && *g < 50.0).collect();
        if gaps.len() < 10 {
            return None;
        }
        gaps.sort_by(|a, b| a.total_cmp(b));
        Some(gaps[gaps.len() / 2])
    };
    let periods: BTreeMap<u32, f64> = by.into_iter().filter_map(|(k, mut ts)| Some((k, period(&mut ts)?))).collect();
    if let Some(d) = display {
        let nominal = (d.hz > 1.0).then(|| 1000.0 / d.hz);
        return match (periods.get(&d.target), nominal) {
            (Some(&p), Some(n)) if (p - n).abs() > n * 0.1 => Some(n),
            (Some(&p), _) => Some(p),
            (None, n) => n,
        };
    }
    let first = *periods.values().next()?;
    periods.values().all(|p| (p - first).abs() <= first * 0.02).then_some(first)
}

/// Per run window: displayed frames, discarded presents, and input-to-screen latencies (ms).
#[derive(Default)]
pub struct RunFrames {
    pub shown: Vec<Shown>,
    pub discarded: usize,
    pub latencies: Vec<f64>,
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
            if r.input_ns > 0 && r.input_ns <= r.present_ns {
                out.latencies.push((r.present_ns - r.input_ns) as f64 / 1e6);
            }
        } else if m.is_some_and(|p| p.state == "discarded") {
            out.discarded += 1;
        }
    }
    out
}

/// One measured run of a native app: its span on the host clock, its toolkit's frame rows when it
/// reports them (their `present_ns` filled from the OS presents), and the times strobe sent it
/// inputs (host clock), which the OS presents after each show on screen.
#[derive(Default)]
pub struct RunWindow {
    pub t0: u64,
    pub t1: u64,
    pub rows: Option<Vec<Row>>,
    pub inputs: Vec<u64>,
}

/// A native app's frame metrics from OS presents, and why some are refused.
#[derive(Default)]
pub struct NativeFrames {
    pub values: BTreeMap<String, f64>,
    /// Every frame metric: the join of rows to presents cannot be defended.
    pub refused: Option<String>,
    /// `interaction.*`: strobe's inputs mostly matched no present.
    pub inputs_refused: Option<String>,
}

/// Frame metrics of a native app's runs from OS presents: its toolkit's rows joined to them when
/// it reports frames, else its processes' presents alone; `interaction.*` from strobe's own inputs
/// to the first present that showed each (`join_inputs`) when the runs have inputs, else from the
/// rows' input times. `display` is the target window's display path, for the refresh period.
pub fn native_values(p: &Presents, pids: &[u32], runs: &mut [RunWindow], display: Option<&crate::host::DisplayPath>) -> NativeFrames {
    let (t0, t1) = (runs.first().map_or(0, |r| r.t0), runs.last().map_or(0, |r| r.t1));
    let refresh = refresh_ms(&p.vsyncs, t0.saturating_sub(1_000_000_000), t1 + 1_000_000_000, display);
    let quiet = quiet_ns(refresh.unwrap_or(1000.0 / 60.0));
    let (mut rows_stats, mut input_stats) = (JoinStats::default(), JoinStats::default());
    let frames: Vec<RunFrames> = runs
        .iter_mut()
        .map(|r| {
            let mut f = match r.rows.as_mut().filter(|rows| !rows.is_empty()) {
                Some(rows) => {
                    let (matched, s) = join(rows, &p.presents, pids);
                    rows_stats.tried += s.tried;
                    rows_stats.unmatched += s.unmatched;
                    rows_stats.ambiguous += s.ambiguous;
                    from_rows(rows, &matched)
                }
                None => from_presents(&p.presents, pids, r.t0, r.t1),
            };
            if !r.inputs.is_empty() {
                let (shown, s) = join_inputs(&r.inputs, &p.presents, pids, quiet);
                input_stats.tried += s.tried;
                input_stats.unmatched += s.unmatched;
                f.latencies = r.inputs.iter().zip(shown).filter_map(|(t, p)| p.map(|p| p.screen_ns.saturating_sub(*t) as f64 / 1e6)).collect();
            }
            f
        })
        .collect();
    let mut out = NativeFrames { values: values(&frames, refresh), refused: rows_stats.refusal(p.events_lost), inputs_refused: None };
    if refresh.is_none() && !p.vsyncs.is_empty() && display.is_none() {
        out.refused = Some("NoPresentSource: several displays run at different refresh rates and the target's window is unknown, so its refresh period is too".into());
    }
    if input_stats.tried > 0 && p.events_lost == 0 && input_stats.unmatched as f64 / input_stats.tried as f64 > UNMATCHED_MAX {
        out.inputs_refused = Some(format!(
            "PresentsUnmatched: {} of {} inputs showed no frame within a second: they changed nothing on screen, or the app presents in a way the session does not follow",
            input_stats.unmatched, input_stats.tried
        ));
    }
    if out.refused.is_some() {
        out.values.clear();
    }
    out
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
        inputs.extend(run.latencies.iter().copied());
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
