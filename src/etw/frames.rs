//! Frame metrics from what the OS put on the screen (SPEC §7.2): the presents of a process tree,
//! and the latency of the inputs strobe sent it to the presents that showed them (§7.4).
//!
//! A frame is pending from its present call's start until its screen time. Between two consecutive
//! frames of one swapchain shown at `a` and `b`, the refreshes in between were missed when the later
//! frame was pending early enough to make the refresh after `a` (pending before `a` + half a
//! refresh): an animation that fell behind. A later start is the app resting (or a frame the app
//! itself started late: only a toolkit's own begin times could tell those apart), so the gap is
//! neither an interval nor a drop, as Chromium's `STATE_NO_UPDATE_DESIRED` resets intervals.

use super::{Present, Presents};
use std::collections::BTreeMap;

/// A frame that reached the screen.
#[derive(Clone, Copy, Debug)]
pub struct Shown {
    /// The swapchain: gaps are taken within it.
    pub stream: u64,
    pub pending_ns: u64,
    pub screen_ns: u64,
}

/// Inputs that showed no frame beyond this share, with no events lost, refuse their latency: the
/// session is not following the app's presents.
const UNMATCHED_MAX: f64 = 0.05;

fn stream(p: &Present) -> u64 {
    (p.pid as u64) << 48 ^ p.swapchain
}

/// How long a target must have presented nothing before an input for its next present to be the
/// input's: a frame already under way when the input came may not hold it.
pub fn quiet_ns(refresh_ms: f64) -> u64 {
    (2.0 * refresh_ms * 1e6) as u64
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

/// Per run window: displayed frames and discarded presents.
#[derive(Default)]
pub struct RunFrames {
    pub shown: Vec<Shown>,
    pub discarded: usize,
}

/// The displayed and discarded presents of `pids` started in `[t0, t1)`.
pub fn from_presents(presents: &[Present], pids: &[u32], t0: u64, t1: u64) -> RunFrames {
    let mut out = RunFrames::default();
    for p in presents.iter().filter(|p| pids.contains(&p.pid) && p.start_ns >= t0 && p.start_ns < t1) {
        match p.state.as_str() {
            "displayed" => out.shown.push(Shown { stream: stream(p), pending_ns: p.start_ns, screen_ns: p.screen_ns }),
            "discarded" => out.discarded += 1,
            _ => {}
        }
    }
    out
}

/// A native app's frame metrics from OS presents, and why they are refused when they are: all of
/// them (`refused`) or some (`partial`, by metric).
#[derive(Default)]
pub struct NativeFrames {
    pub values: BTreeMap<String, f64>,
    pub refused: Option<String>,
    pub partial: BTreeMap<String, String>,
}

/// Metrics that need when a frame was handed over, not only when it was shown.
const NEED_BEGIN: &[&str] = &["frames.interval.p50", "frames.interval.p95", "frames.interval.p99", "frames.interval.max", "frames.dropped", "frames.dropped.pct"];

/// Frame metrics of a native app's measured runs (their spans on the host clock) from its
/// processes' presents. `display` is the target window's display path, for the refresh period.
pub fn native_values(p: &Presents, pids: &[u32], runs: &[(u64, u64)], display: Option<&crate::host::DisplayPath>) -> NativeFrames {
    let (t0, t1) = (runs.first().map_or(0, |r| r.0), runs.last().map_or(0, |r| r.1));
    let refresh = refresh_ms(&p.vsyncs, t0.saturating_sub(1_000_000_000), t1 + 1_000_000_000, display);
    let frames: Vec<RunFrames> = runs.iter().map(|r| from_presents(&p.presents, pids, r.0, r.1)).collect();
    let mut out = NativeFrames { values: values(&frames, refresh), ..NativeFrames::default() };
    if refresh.is_none() && !p.vsyncs.is_empty() && display.is_none() {
        out.refused = Some("NoPresentSource: several displays run at different refresh rates and the target's window is unknown, so its refresh period is too".into());
        out.values.clear();
    }
    if let Some(only) = &p.only
        && !p.presents.iter().any(|x| pids.contains(&x.pid) && x.start_ns >= t0 && x.start_ns < t1)
    {
        out.refused = Some(format!("NoPresentSource: the app showed no frame through {only} in the runs: it may draw another way, which strobe does not see"));
        out.values.clear();
    }
    if p.no_begin {
        for m in NEED_BEGIN {
            out.values.remove(*m);
            out.partial.insert(
                m.to_string(),
                format!("NoPresentSource: {m}: this OS shows strobe when a frame reached the screen, not when the app handed it over, so a gap between frames cannot be told from the app resting"),
            );
        }
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

/// Frame metrics over measured runs: interval percentiles pool every run's frames, counts
/// are the median per run (as `metrics::block_values` does for Chromium's frames). Nothing without
/// a refresh period (no vertical blanks: no display).
pub fn values(runs: &[RunFrames], refresh_ms: Option<f64>) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let Some(r) = refresh_ms else { return out };
    let refresh_ns = r * 1e6;
    let mut intervals = Vec::new();
    let (mut counts, mut dropped, mut discarded) = (Vec::new(), Vec::new(), Vec::new());
    let (mut shown_all, mut dropped_all) = (0usize, 0usize);
    for run in runs {
        let (missed, iv) = gaps(&run.shown, refresh_ns);
        intervals.extend(iv);
        counts.push(run.shown.len() as f64);
        dropped.push((missed + run.discarded) as f64);
        discarded.push(run.discarded as f64);
        shown_all += run.shown.len();
        dropped_all += missed + run.discarded;
    }
    intervals.sort_by(|a, b| a.total_cmp(b));
    if !intervals.is_empty() {
        for (name, p) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)] {
            out.insert(format!("frames.interval.{name}"), pct(&intervals, p));
        }
        out.insert("frames.interval.max".into(), *intervals.last().unwrap_or(&0.0));
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

/// An input strobe gave a native app: when it was posted (host clock), and whether it is a
/// discrete one (a click's release, a key press) or the first wheel step of a scroll.
#[derive(Clone, Copy, Debug)]
pub struct Sent {
    pub t_ns: u64,
    pub scroll: bool,
}

/// Input latencies from outside the app (SPEC §7.4), in ms: discrete inputs for `interaction.*`,
/// gesture-starting wheel steps for `scroll.latency.*`. An input counts when the app changed its
/// accessibility tree within a second after it (`changes`, host clock: a press highlight or a caret
/// blink changes none, so such an input never claims an unrelated later frame), and the app was at
/// rest when it came (no present started in the `quiet` before it); its latency is then to the
/// screen time of the first present of `pids` that started after it and reached the screen, within
/// a second. Nothing carries over between inputs.
#[derive(Clone, Debug, Default)]
pub struct Reactions {
    pub interaction: Vec<f64>,
    pub scroll: Vec<f64>,
    /// Inputs that changed the tree, per kind: (discrete, scroll).
    pub tried: (usize, usize),
    /// Of those, the app was already drawing when they came.
    pub busy: (usize, usize),
    /// Of those, no present reached the screen within a second.
    pub unshown: (usize, usize),
    /// Inputs after which the tree did not change.
    pub unchanged: usize,
}

/// The longest an input waits for its frame: past this its present is something else's.
const HORIZON_NS: u64 = 1_000_000_000;

pub fn reactions(sent: &[Sent], changes: &[u64], presents: &[Present], pids: &[u32], quiet_ns: u64) -> Reactions {
    let mine: Vec<&Present> = presents.iter().filter(|p| pids.contains(&p.pid) && p.state != "lost" && p.state != "failed").collect();
    let mut out = Reactions::default();
    for s in sent {
        let changed = changes.iter().any(|&c| c > s.t_ns && c <= s.t_ns + HORIZON_NS);
        if !changed {
            out.unchanged += 1;
            continue;
        }
        let k = usize::from(s.scroll);
        let bump = |t: &mut (usize, usize)| if k == 1 { t.1 += 1 } else { t.0 += 1 };
        bump(&mut out.tried);
        let at = mine.partition_point(|p| p.start_ns <= s.t_ns);
        if at > 0 && mine[at - 1].start_ns + quiet_ns > s.t_ns {
            bump(&mut out.busy);
            continue;
        }
        match mine[at..].iter().take_while(|p| p.start_ns <= s.t_ns + HORIZON_NS).find(|p| p.state == "displayed" && p.screen_ns > s.t_ns) {
            Some(p) => {
                let ms = (p.screen_ns - s.t_ns) as f64 / 1e6;
                if s.scroll { out.scroll.push(ms) } else { out.interaction.push(ms) }
            }
            None => bump(&mut out.unshown),
        }
    }
    out
}

impl Reactions {
    /// Values pooled over a block's runs, and why a kind is refused when it is: more than half of
    /// its inputs came while the app was drawing (`AmbiguousInteraction`), over 5% showed no frame
    /// while the session lost no events (`PresentsUnmatched`), or none reached a frame.
    pub fn values(&self, events_lost: u64, out: &mut BTreeMap<String, f64>, refused: &mut BTreeMap<String, String>) {
        for (prefix, count, v, tried, busy, unshown) in
            [("interaction", "interaction.n", &self.interaction, self.tried.0, self.busy.0, self.unshown.0), ("scroll.latency", "scroll.n", &self.scroll, self.tried.1, self.busy.1, self.unshown.1)]
        {
            let why = if tried == 0 {
                Some(if self.unchanged > 0 {
                    format!("NoPresentSource: {prefix}.*: none of the {} inputs changed the app's accessibility tree, so no frame can be shown to be their reaction", self.unchanged)
                } else {
                    format!("NoPresentSource: {prefix}.*: the runs sent no input of this kind")
                })
            } else if busy * 2 > tried {
                Some(format!("AmbiguousInteraction: {prefix}.*: the app was already drawing when {busy} of {tried} inputs came (an animation or a caret), so the next frame may not be their reaction"))
            } else if events_lost == 0 && unshown as f64 > UNMATCHED_MAX * (tried - busy) as f64 {
                Some(format!(
                    "PresentsUnmatched: {prefix}.*: {unshown} of {} inputs that changed the app's tree showed no frame within a second: the app presents in a way the session does not follow, or reacted only in its tree",
                    tried - busy
                ))
            } else if v.is_empty() {
                Some(format!("NoPresentSource: {prefix}.*: no frame reached the screen within a second of {unshown} inputs"))
            } else {
                None
            };
            match why {
                Some(w) => {
                    for k in [format!("{prefix}.p50"), format!("{prefix}.p95"), count.to_string()] {
                        refused.entry(k).or_insert_with(|| w.clone());
                    }
                }
                None => {
                    let mut v = v.clone();
                    v.sort_by(f64::total_cmp);
                    out.insert(count.to_string(), v.len() as f64);
                    out.insert(format!("{prefix}.p50"), pct(&v, 0.5));
                    out.insert(format!("{prefix}.p95"), pct(&v, 0.95));
                }
            }
        }
    }
}

/// Runs in which the app presented faster than the display refreshes (more presents started than
/// refreshes plus ten): a toolkit spinning unpaced, whose frames describe no person's screen.
pub fn unpaced(presents: &[Present], pids: &[u32], runs: &[(u64, u64)], refresh_ms: f64) -> Option<String> {
    for &(t0, t1) in runs {
        let n = presents.iter().filter(|p| pids.contains(&p.pid) && p.start_ns >= t0 && p.start_ns < t1).count();
        let refreshes = (t1.saturating_sub(t0) as f64 / 1e6 / refresh_ms).ceil() as usize;
        if n > refreshes + 10 {
            return Some(format!("UnpacedSpin: the app presented {n} frames in a run of {refreshes} refreshes: it draws without waiting for the display"));
        }
    }
    None
}
