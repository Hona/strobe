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
pub use crate::metrics::Timed;
use std::collections::BTreeMap;

/// A frame that reached the screen.
#[derive(Clone, Copy, Debug)]
pub struct Shown {
    /// The swapchain: gaps are taken within it.
    pub stream: u64,
    pub pending_ns: u64,
    pub screen_ns: u64,
    /// The stretch of its run it began in (`Timed::segments`): gaps are taken within one, never across a
    /// stretch `ctx.untimed` left out.
    pub seg: usize,
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

/// Per run window: displayed frames, discarded presents, and presents whose way to the screen the OS's
/// events lost (Windows: a DirectComposition surface's, whose composition they do not show).
#[derive(Default)]
pub struct RunFrames {
    pub shown: Vec<Shown>,
    pub discarded: usize,
    pub lost: usize,
}

/// The displayed and discarded presents of `pids` started in `[t0, t1)`.
pub fn from_presents(presents: &[Present], pids: &[u32], t0: u64, t1: u64) -> RunFrames {
    from_timed(presents, pids, &Timed::whole(t0, t1))
}

/// The displayed and discarded presents of `pids` started in a run's timed stretches: a frame begun
/// inside a `ctx.untimed` stretch is none of the run's.
pub fn from_timed(presents: &[Present], pids: &[u32], run: &Timed) -> RunFrames {
    let mut out = RunFrames::default();
    for (seg, (t0, t1)) in run.segments().into_iter().enumerate() {
        for p in presents.iter().filter(|p| pids.contains(&p.pid) && p.start_ns >= t0 && p.start_ns < t1) {
            match p.state.as_str() {
                "displayed" => out.shown.push(Shown { stream: stream(p), pending_ns: p.start_ns, screen_ns: p.screen_ns, seg }),
                "discarded" => out.discarded += 1,
                "lost" => out.lost += 1,
                _ => {}
            }
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

/// Frame metrics of a native app's measured runs (their timed stretches on the host clock) from its
/// processes' presents. `display` is the target window's display path, for the refresh period.
pub fn native_values(p: &Presents, pids: &[u32], timed: &[Timed], display: Option<&crate::host::DisplayPath>) -> NativeFrames {
    let runs: Vec<(u64, u64)> = timed.iter().map(|r| (r.t0, r.t1)).collect();
    let (t0, t1) = (runs.first().map_or(0, |r| r.0), runs.last().map_or(0, |r| r.1));
    let refresh = refresh_ms(&p.vsyncs, t0.saturating_sub(1_000_000_000), t1 + 1_000_000_000, display);
    let frames: Vec<RunFrames> = timed.iter().map(|r| from_timed(&p.presents, pids, r)).collect();
    let mut out = NativeFrames { values: values(&frames, refresh), ..NativeFrames::default() };
    // A present whose way to the screen was lost may have shown a frame: counted with none, the frames
    // would read short (Firefox's DirectComposition surfaces on Windows read 0), so none are given.
    let lost: usize = frames.iter().map(|f| f.lost).sum();
    if lost > 0 {
        out.refused = Some(format!(
            "NoPresentSource: the OS's present events lost the way to the screen of {lost} of the app's presents in the measured runs (a DirectComposition surface's composition is not in them), so its frames would read short"
        ));
        out.values.clear();
        return out;
    }
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
    if let Some(why) = unshared(p, pids, &runs) {
        out.refused = Some(why);
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

/// Why frames are refused when the app showed a window it excludes from screen capture during a
/// measured run (`runs`, not the resets between them): what that window draws is unseen, and the
/// frames of its other windows would read as all of the app's.
pub fn unshared(p: &Presents, pids: &[u32], runs: &[(u64, u64)]) -> Option<String> {
    let mut ids: Vec<u64> = p.unshared.iter().filter(|u| pids.contains(&u.pid) && runs.iter().any(|r| u.first_ns <= r.1 && u.last_ns >= r.0)).map(|u| u.window).collect();
    ids.sort_unstable();
    ids.dedup();
    let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    match ids.len() {
        0 => None,
        1 => Some(format!("NoPresentSource: the app showed window {list}, which it excludes from screen capture (sharing type none), so what it draws cannot be seen; strobe streams no such window")),
        n => Some(format!("NoPresentSource: the app showed {n} windows ({list}) it excludes from screen capture (sharing type none), so what they draw cannot be seen; strobe streams no such window")),
    }
}

/// Missed refreshes and the intervals between frames shown while the target kept drawing.
fn gaps(shown: &[Shown], refresh_ns: f64) -> (usize, Vec<f64>) {
    let mut by: BTreeMap<(u64, usize), Vec<&Shown>> = BTreeMap::new();
    for s in shown {
        by.entry((s.stream, s.seg)).or_default().push(s);
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

/// An input strobe gave a target: when it was sent (host clock), and whether it is a discrete one (a
/// single click's release, a key press, a button's `up`) or the first wheel step of a scroll. A single
/// click's release carries when its press went out: a target that acted on the press is timed from it.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Sent {
    pub t_ns: u64,
    pub scroll: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub press: Option<u64>,
    /// The target's own word on whether it acted on the press: it changed between the press and the
    /// release, and not again after the release within a second. A page tells from its events' order
    /// (`CHANGES_SCRIPT`), exactly. None where it says nothing (an accessibility tree): then the
    /// changes' times decide (`reactions`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acted: Option<bool>,
    /// A key of `type(…, { burst: true })`: sent without waiting for the key before it to show, so no
    /// frame is its reaction alone (`Reactions::burst`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub burst: bool,
}

/// Why `interaction.*` is refused for a block whose runs typed in a burst (every engine and app).
pub fn burst_refusal(keys: usize) -> String {
    format!(
        "AmbiguousInteraction: interaction.*: the runs typed {keys} keys in a burst (type with burst: true), each sent before the one before it showed, so no frame is any one key's reaction; measure burst typing by cpu.work, main.work, ui.cpu, run.duration or a phase, or type without burst for each key's latency"
    )
}

/// The edge of an input its latency is timed from (`interaction.n.press` counts the presses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    /// The input as sent: a click's release, a key going down, a button's `up`, a scroll's first step.
    Sent,
    /// A single click's press: the target changed its model between the press and the release, so it
    /// acted on the press (tabs, menus and many native controls do), and the release has nothing left
    /// to show.
    Press,
}

/// A change of the target's model (its accessibility tree, a page's DOM) on the host clock: the
/// earliest and the latest it can have happened, and whether it is press feedback only (focus moving,
/// a control armed or pressed: what a release-acting button shows on the mouse press), which never
/// says a click acted on its press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Change(pub u64, pub u64, #[serde(default)] pub bool);

/// A change strobe heard of at `at` (an accessibility event): it happened by then, and how long before
/// is not known (events carry no source time). A provider may announce a change long after drawing it
/// (one toolkit's tree on macOS: 350–440 ms after its pane switch began to draw), so the event's time is
/// no bound on the reaction's frame.
pub fn notified(at: u64, feedback: bool) -> Change {
    Change(0, at, feedback)
}

/// Whether a change can have followed `t`: one with an earliest (a page's) when that is after `t` (a
/// change read a millisecond either side of a press may be the move's hover); an accessibility event
/// (`notified`, no earliest) when it was heard after `t`, as for any input.
fn after(c: &Change, t: u64) -> bool {
    if c.0 == 0 { c.1 > t } else { c.0 > t }
}

/// Input latencies from outside the target (SPEC §7.4), in ms: discrete inputs for `interaction.*`,
/// gesture-starting wheel steps for `scroll.latency.*`. An input counts when the target's model changed
/// within a second after it (`changes`: a press highlight or a caret blink changes none, so such an
/// input never claims an unrelated later frame), and the target was at rest when it was sent: no
/// present of `pids` begun before it reached the screen (or began, when it never did) in the `quiet`
/// before it, so a frame on its way at the send (a press's look, an animation's) is never taken for its
/// reaction, and every earlier input's reaction already on the screen (a key typed before the last
/// one showed shares its frame: its latency would be the last one's remainder). Its latency is then to the screen time of the first present that began after it and after
/// the earliest its change can have happened (a frame begun before the model changed cannot show the
/// change: a page's `:active` undone at the release is not an asynchronous reaction's frame; an
/// accessibility event bounds nothing, `notified`), and reached the screen, within a second. Inputs
/// claim changes in order, each the first one no earlier input claimed, so a change announced late
/// for one input is never another's too.
///
/// A single click is one sample from its press instead when it acted on its press (`pressed`): never
/// one from each edge.
#[derive(Clone, Debug, Default)]
pub struct Reactions {
    pub interaction: Vec<(f64, Edge)>,
    pub scroll: Vec<f64>,
    /// Inputs that changed the model, per kind: (discrete, scroll).
    pub tried: (usize, usize),
    /// Of those, the target was still drawing when they were sent, an earlier input's reaction was not
    /// on the screen yet, or (a press) an earlier input's change was still to come.
    pub busy: (usize, usize),
    /// Of those, no present reached the screen within a second.
    pub unshown: (usize, usize),
    /// Inputs after which the model did not change.
    pub unchanged: usize,
    /// Keys typed in a burst: `interaction.*` is refused when there are any.
    pub burst: usize,
}

/// The longest an input waits for its frame: past this its present is something else's.
pub const HORIZON_NS: u64 = 1_000_000_000;

/// What decides one input's sample.
enum Pick {
    /// From this edge, bounded by this change (its index), or by the edge alone.
    At(u64, Option<usize>, Edge),
    /// A press whose change (its index) may be an earlier input's announced late: no sample.
    Ambiguous(usize),
    Unchanged,
}

pub fn reactions(sent: &[Sent], changes: &[Change], presents: &[Present], pids: &[u32], quiet_ns: u64) -> Reactions {
    let mut mine: Vec<&Present> = presents.iter().filter(|p| pids.contains(&p.pid) && p.state != "lost" && p.state != "failed").collect();
    // Sources deliver presents as they complete, not as they began: the searches below need start order.
    mine.sort_by_key(|p| p.start_ns);
    let mut sent = sent.to_vec();
    sent.sort_by_key(|s| s.t_ns);
    // Changes in the order they are known to have happened by, each claimed by one input at most.
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|&i| changes[i].1);
    let mut claimed = vec![false; changes.len()];
    // Each earlier input: when it was sent, and the latest its claimed change happened by.
    let mut seen: Vec<(u64, Option<u64>)> = Vec::new();
    let mut out = Reactions::default();
    // The first change yet unclaimed that passes `ok`.
    let first = |claimed: &[bool], ok: &dyn Fn(&Change) -> bool| order.iter().copied().find(|&i| !claimed[i] && ok(&changes[i]));
    // The first frame of the target that began after `t` and no earlier than `bound`, and reached the screen.
    let frame = |t: u64, bound: u64| {
        let at = mine.partition_point(|p| p.start_ns <= t);
        mine[at..].iter().take_while(|p| p.start_ns <= t + HORIZON_NS).find(|p| p.state == "displayed" && p.screen_ns > t && p.start_ns >= bound).copied()
    };
    let drawing = |t: u64| {
        let at = mine.partition_point(|p| p.start_ns <= t);
        mine[..at].iter().rev().take_while(|p| p.start_ns + HORIZON_NS > t).any(|p| p.start_ns.max(p.screen_ns) + quiet_ns > t)
    };
    // When each earlier input's reaction reached the screen: an input sent before one did came while the
    // target still answered that one (its frame not yet begun, so `drawing` cannot see it), not at rest.
    let mut answered: Vec<u64> = Vec::new();
    out.burst = sent.iter().filter(|s| s.burst).count();
    for (k, s) in sent.iter().enumerate() {
        let next = sent.get(k + 1).map_or(u64::MAX, |n| n.press.unwrap_or(n.t_ns));
        // A click that acted on its press: the page says so (`acted`), else its first change after the
        // press that is no press feedback came before the release, and nothing changed again between
        // the release and the next input (a release-acting control with press feedback in its model is
        // the release's), and every earlier input's change came before this press (else the change may
        // be one of theirs, announced late: ambiguous). Its press is the sample only when a frame
        // showing the change began before the release: a reaction drawn after it is not charged the hold.
        let pressed = s.press.and_then(|p| {
            let i = match s.acted {
                Some(true) => first(&claimed, &|c| c.1 > p),
                Some(false) => return None,
                None => {
                    let i = first(&claimed, &|c| !c.2 && after(c, p)).filter(|&i| changes[i].1 < s.t_ns)?;
                    let again = order.iter().any(|&j| j != i && !claimed[j] && !changes[j].2 && changes[j].1 > s.t_ns && changes[j].1 < next.min(s.t_ns + HORIZON_NS));
                    if again {
                        return None;
                    }
                    if seen.iter().any(|&(at, by)| at + HORIZON_NS > p && by.is_none_or(|by| by >= p)) {
                        return Some(Pick::Ambiguous(i));
                    }
                    Some(i)
                }
            };
            let bound = i.map_or(p, |i| changes[i].0);
            if !drawing(p) && frame(p, bound).is_none_or(|f| f.start_ns >= s.t_ns) {
                return None;
            }
            Some(Pick::At(p, i, Edge::Press))
        });
        let pick = pressed.unwrap_or_else(|| match first(&claimed, &|c| c.1 > s.t_ns && c.1 <= s.t_ns + HORIZON_NS) {
            Some(i) => Pick::At(s.t_ns, Some(i), Edge::Sent),
            None => Pick::Unchanged,
        });
        let kind = usize::from(s.scroll);
        let bump = |x: &mut (usize, usize)| if kind == 1 { x.1 += 1 } else { x.0 += 1 };
        let (t, i, edge) = match pick {
            Pick::At(t, i, edge) => (t, i, edge),
            Pick::Ambiguous(i) => {
                claimed[i] = true;
                bump(&mut out.tried);
                bump(&mut out.busy);
                seen.push((s.t_ns, Some(changes[i].1)));
                continue;
            }
            Pick::Unchanged => {
                out.unchanged += 1;
                seen.push((s.t_ns, None));
                continue;
            }
        };
        if let Some(i) = i {
            claimed[i] = true;
        }
        seen.push((s.t_ns, i.map(|i| changes[i].1)));
        bump(&mut out.tried);
        let shown = frame(t, i.map_or(t, |i| changes[i].0));
        let behind = answered.iter().any(|&at| at > t);
        answered.extend(shown.map(|p| p.screen_ns));
        if drawing(t) || behind {
            bump(&mut out.busy);
            continue;
        }
        match shown {
            Some(p) => {
                let ms = (p.screen_ns - t) as f64 / 1e6;
                if s.scroll { out.scroll.push(ms) } else { out.interaction.push((ms, edge)) }
            }
            None => bump(&mut out.unshown),
        }
    }
    out
}

impl Reactions {
    /// Another run's, added.
    pub fn add(&mut self, o: Reactions) {
        self.interaction.extend(o.interaction);
        self.scroll.extend(o.scroll);
        self.tried = (self.tried.0 + o.tried.0, self.tried.1 + o.tried.1);
        self.busy = (self.busy.0 + o.busy.0, self.busy.1 + o.busy.1);
        self.unshown = (self.unshown.0 + o.unshown.0, self.unshown.1 + o.unshown.1);
        self.unchanged += o.unchanged;
        self.burst += o.burst;
    }

    /// The interaction samples timed from a click's press.
    pub fn presses(&self) -> usize {
        self.interaction.iter().filter(|x| x.1 == Edge::Press).count()
    }

    /// Values pooled over a block's runs, and why a kind is refused when it is: more than half of
    /// its inputs came while the app was drawing (`AmbiguousInteraction`), over 5% showed no frame
    /// while the session lost no events (`PresentsUnmatched`), or none reached a frame. Beside
    /// `interaction.n`, `interaction.n.press`: how many of its samples are a click's press.
    pub fn values(&self, events_lost: u64, out: &mut BTreeMap<String, f64>, refused: &mut BTreeMap<String, String>) {
        let interaction: Vec<f64> = self.interaction.iter().map(|x| x.0).collect();
        for (prefix, count, v, tried, busy, unshown) in
            [("interaction", "interaction.n", &interaction, self.tried.0, self.busy.0, self.unshown.0), ("scroll.latency", "scroll.n", &self.scroll, self.tried.1, self.busy.1, self.unshown.1)]
        {
            let why = if prefix == "interaction" && self.burst > 0 {
                Some(burst_refusal(self.burst))
            } else if tried == 0 {
                Some(if self.unchanged > 0 {
                    format!("NoPresentSource: {prefix}.*: none of the {} inputs changed the app's accessibility tree, so no frame can be shown to be their reaction", self.unchanged)
                } else {
                    format!("NoPresentSource: {prefix}.*: the runs sent no input of this kind")
                })
            } else if busy * 2 > tried {
                Some(format!("AmbiguousInteraction: {prefix}.*: the app was still drawing when {busy} of {tried} inputs were sent (an animation, a caret, or a press's own look still on its way after 300 ms), so the next frame may not be their reaction"))
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
                    if prefix == "interaction" {
                        out.insert("interaction.n.press".into(), self.presses() as f64);
                    }
                }
            }
        }
    }
}

/// The fastest rate (presents started per second) of `pids` in any of `runs`; None without presents.
pub fn present_rate(presents: &[Present], pids: &[u32], runs: &[(u64, u64)]) -> Option<f64> {
    runs.iter()
        .filter(|(t0, t1)| t1 > t0)
        .map(|&(t0, t1)| presents.iter().filter(|p| pids.contains(&p.pid) && p.start_ns >= t0 && p.start_ns < t1).count() as f64 / ((t1 - t0) as f64 / 1e9))
        .filter(|r| *r > 0.0)
        .max_by(f64::total_cmp)
}

/// `UnpacedSpin` as a warning: presents finishing far faster than the display refreshes (1.5 times
/// its rate), whatever the toolkit believes (composited while the session is locked, a display
/// asleep), with the rate and the refresh. `hz` from the vertical blanks, else the display's mode.
pub fn spin(presents: &[Present], pids: &[u32], runs: &[(u64, u64)], hz: Option<f64>) -> Option<(f64, f64)> {
    let hz = hz.filter(|hz| *hz > 0.0)?;
    present_rate(presents, pids, runs).filter(|fps| *fps > 1.5 * hz).map(|fps| (fps, hz))
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
