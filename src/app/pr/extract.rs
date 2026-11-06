//! What a result says, read for the report: each comparison's outcome, the pairs of a series, and helpers over the result.

use super::*;

// ---------- outcome of one comparison ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Out {
    Desc,
    Gated,
    Refused,
    Same,
    Unclear,
    Stopped,
    Saturated,
    Better,
    Worse,
    NoWorse,
    NoWorseSlower,
    AaFalse,
    NotRun,
}

pub(super) fn verdict(c: &Value) -> &str {
    c["verdict"].as_str().unwrap_or("")
}

pub(super) fn better_of(metric: &str) -> Better {
    model::metric(metric).map(|m| m.better).unwrap_or(Better::Lower)
}

pub(super) fn unit_of(metric: &str) -> &'static str {
    model::metric(metric).map(|m| m.unit).unwrap_or("")
}

pub(super) fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

pub(super) fn kind(c: &Value) -> VerdictKind {
    stats::kind_of(c)
}

/// `worse (>2%)`: head worse by more than a no-worse margin, not a directional `slower`.
pub(super) fn beyond_margin(c: &Value) -> bool {
    kind(c) == VerdictKind::Worse && c["noworse"].is_number()
}

/// Which way a no-worse interval also clears 0: worse (`no worse (≤2%, but slower)`), better
/// (`, faster`), or neither. An exact one says neither.
pub(super) fn noworse_side(c: &Value) -> Option<Out> {
    if c["exact"] == true {
        return None;
    }
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    let (w_lo, w_hi) = if better_of(c["metric"].as_str().unwrap_or("")) == Better::Lower { (lo, hi) } else { (-hi, -lo) };
    if w_hi < 0.0 {
        Some(Out::Better)
    } else if w_lo > 0.0 {
        Some(Out::Worse)
    } else {
        None
    }
}

/// `faster (small)`: decided, but the interval reaches inside the equivalence bound.
pub(super) fn small(c: &Value) -> bool {
    let (lo, hi, bound) = (f(&c["lo"]), f(&c["hi"]), f(&c["bound"]));
    c["exact"] != true && if hi < 0.0 { hi > -bound } else { lo < bound }
}

pub(super) fn outcome(c: &Value, aa: bool) -> Out {
    if verdict(c).is_empty() {
        return Out::Gated;
    }
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    match kind(c) {
        VerdictKind::Descriptive => Out::Desc,
        VerdictKind::Untested => Out::Gated,
        VerdictKind::Refused => Out::Refused,
        VerdictKind::Same => Out::Same,
        VerdictKind::Saturated => Out::Saturated,
        VerdictKind::Stopped => Out::Stopped,
        VerdictKind::NoWorse if noworse_side(c) == Some(Out::Worse) => Out::NoWorseSlower,
        VerdictKind::NoWorse => Out::NoWorse,
        VerdictKind::Worse if beyond_margin(c) => Out::Worse,
        VerdictKind::Unclear | VerdictKind::Underpowered => Out::Unclear,
        VerdictKind::Better | VerdictKind::Worse if !(hi < 0.0 || lo > 0.0) => Out::Unclear,
        VerdictKind::Better | VerdictKind::Worse if aa => Out::AaFalse,
        VerdictKind::Better | VerdictKind::Worse => {
            let improved = (hi < 0.0) == (better_of(c["metric"].as_str().unwrap_or("")) == Better::Lower);
            if improved { Out::Better } else { Out::Worse }
        }
    }
}

/// Whether a change is decided in a direction, so it is set in bold.
pub(super) fn marked(c: &Value, o: Out) -> bool {
    match o {
        Out::Better | Out::Worse | Out::AaFalse | Out::NoWorseSlower => true,
        // `no worse (≤2%, faster)`: within the margin, and the interval also clears 0 the good way.
        Out::NoWorse => noworse_side(c) == Some(Out::Better),
        _ => false,
    }
}

pub(super) fn abs_unit(unit: &str) -> &str {
    if unit == "%" { "pp" } else { unit }
}

/// The change as a table shows it: relative changes as percentages, others in the metric's unit.
pub(super) fn delta(c: &Value, x: f64) -> String {
    let unit = unit_of(c["metric"].as_str().unwrap_or(""));
    if c["relative"] == true { fmt::pct(x) } else { signed_disp(x, abs_unit(unit)) }
}

/// `−54% to −32%`, or `exact` for a counter that held one value per side.
pub(super) fn range(c: &Value) -> String {
    if c["exact"] == true {
        return "exact".into();
    }
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    if !lo.is_finite() || !hi.is_finite() {
        return "—".into();
    }
    let (a, b) = (delta(c, lo), delta(c, hi));
    if a == b { a.replace(' ', "\u{a0}") } else { format!("{a} to {b}").replace(' ', "\u{a0}") }
}

pub(super) fn value(c: &Value, side: &str) -> String {
    disp(f(&c[side]), unit_of(c["metric"].as_str().unwrap_or("")))
}

/// `7.05 → 3.90 ms`: both medians, the unit once when they share it.
pub(super) fn before_after(c: &Value) -> String {
    let (b, h) = (value(c, "base"), value(c, "head"));
    match (b.rsplit_once(' '), h.rsplit_once(' ')) {
        (Some((bn, bu)), Some((_, hu))) if bu == hu && bn.parse::<f64>().is_ok() => nb(&format!("{bn} → {h}")),
        _ => nb(&format!("{b} → {h}")),
    }
}

/// `7.10 → 3.89 ms`: before → this PR, the unit once when they share it; it may break after the arrow.
pub(super) fn arrow(b: &str, h: &str) -> String {
    match (b.rsplit_once(' '), h.rsplit_once(' ')) {
        (Some((bn, bu)), Some((_, hu))) if bu == hu && bn.parse::<f64>().is_ok() => format!("{bn}\u{a0}→ {}", nb(h)),
        _ => format!("{}\u{a0}→ {}", nb(b), nb(h)),
    }
}

pub(super) fn html_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Text for a table cell: no raw pipes, no stray HTML.
pub(super) fn cell(s: &str) -> String {
    html_esc(s).replace('|', "\\|")
}

pub(super) fn code(s: &str) -> String {
    format!("`{}`", s.replace('`', "'").replace('|', "\\|"))
}

/// A refused comparison's validator and why (`TailTooShort`, `51 frames < 200`); a result from before
/// comparisons kept their refusal is read from its verdict, `refused (TailTooShort): 51 frames < 200`.
pub(super) fn refusal(c: &Value) -> (String, String) {
    if let (Some(id), Some(why)) = (c["refusal"]["validator"].as_str(), c["refusal"]["summary"].as_str()) {
        return (id.to_string(), why.to_string());
    }
    let rest = verdict(c).trim_start_matches("refused").trim();
    let (id, why) = rest.split_once("): ").map(|(a, b)| (a.trim_start_matches('('), b)).unwrap_or((rest.trim_matches(['(', ')']), ""));
    (id.to_string(), why.to_string())
}

// ---------- the pairs of a series ----------

/// A pair's two blocks by pair number: (position in the series' blocks, block) per side.
pub(super) type Sides<'a> = (Option<(usize, &'a Value)>, Option<(usize, &'a Value)>);

#[derive(Clone, Copy)]
pub(super) struct Pair<'a> {
    pub(super) n: usize,
    pub(super) ab: bool,
    pub(super) base: &'a Value,
    pub(super) head: &'a Value,
}

pub(super) fn pairs(s: &Value) -> Vec<Pair<'_>> {
    let mut by: BTreeMap<u64, Sides> = BTreeMap::new();
    for (i, b) in s["blocks"].as_array().into_iter().flatten().enumerate() {
        let Some(p) = b["pair"].as_u64().filter(|p| *p < 1_000_000) else { continue };
        let e = by.entry(p).or_default();
        if b["variant"] == "base" {
            e.0 = Some((i, b));
        } else if b["variant"] == "head" {
            e.1 = Some((i, b));
        }
    }
    by.into_iter().filter_map(|(p, (b, h))| Some(Pair { n: p as usize + 1, ab: b?.0 < h?.0, base: b?.1, head: h?.1 })).collect()
}

pub(super) fn bv(b: &Value, m: &str) -> f64 {
    f(&b["values"][m])
}

/// Pairs in which head did better on `metric`, of those with both values.
pub(super) fn wins(ps: &[Pair], metric: &str) -> (usize, usize) {
    let lower = better_of(metric) == Better::Lower;
    let both: Vec<(f64, f64)> = ps.iter().map(|p| (bv(p.base, metric), bv(p.head, metric))).filter(|(a, b)| a.is_finite() && b.is_finite()).collect();
    (both.iter().filter(|(a, b)| if lower { b < a } else { b > a }).count(), both.len())
}

/// Past this many pairs a series' rows are its first and last `SHOWN_ENDS` and one summary row.
pub(super) const LONG_PAIRS: usize = 20;
pub(super) const SHOWN_ENDS: usize = 5;

/// Each pair's change on `m`, sorted, in the comparison's own scale (`ratio`: the comparison's
/// `relative`, which `stats::compare` sets only when every value on both sides is positive): a
/// fraction for a ratio comparison, else a difference in the metric's unit. Deciding it here from
/// the pairs instead once showed a pair with a zero head as −100% beside a comparison of differences.
pub(super) fn pair_changes(ps: &[&Pair], m: &str, ratio: bool) -> Vec<f64> {
    let both: Vec<(f64, f64)> = ps.iter().map(|p| (bv(p.base, m), bv(p.head, m))).filter(|(a, b)| a.is_finite() && b.is_finite()).collect();
    let mut v: Vec<f64> = both.iter().map(|(a, b)| if ratio { b / a - 1.0 } else { b - a }).collect();
    v.sort_by(f64::total_cmp);
    v
}

pub(super) fn show_change(x: f64, ratio: bool, m: &str) -> String {
    if ratio { fmt::pct(x) } else { signed_disp(x, abs_unit(unit_of(m))) }
}

/// The `q` quantile of sorted values, by the nearest rank.
pub(super) fn quantile(v: &[f64], q: f64) -> Option<f64> {
    (!v.is_empty()).then(|| v[((v.len() - 1) as f64 * q).round() as usize])
}

/// The Pairs table's row for the pairs a long series does not list: how many, their medians, and
/// the median of their own changes.
pub(super) fn summary_row(rest: &[&Pair], m: &str, ratio: bool, frames: bool, foreign: bool) -> String {
    let unit = unit_of(m);
    let med = |head: bool| {
        let mut v: Vec<f64> = rest.iter().map(|p| bv(if head { p.head } else { p.base }, m)).filter(|x| x.is_finite()).collect();
        v.sort_by(f64::total_cmp);
        quantile(&v, 0.5).unwrap_or(f64::NAN)
    };
    let ch = pair_changes(rest, m, ratio);
    let mid = quantile(&ch, 0.5).map(|x| format!("{} <sub>median</sub>", nb(&show_change(x, ratio, m)))).unwrap_or_else(|| "—".into());
    let mut row = format!("| … | {} more pairs: medians {} | {mid} |", rest.len(), arrow(&disp(med(false), unit), &disp(med(true), unit)));
    for on in [frames, foreign] {
        if on {
            row += " |";
        }
    }
    row + "\n"
}

pub(super) fn conf(c: &Value) -> String {
    let x = f(&c["confidence"]);
    if !x.is_finite() || x <= 0.0 {
        return String::new();
    }
    let p = x * 100.0;
    let t = if (p - p.round()).abs() < 0.05 { format!("{:.0}", p) } else { format!("{p:.1}") };
    if x < 0.945 { format!("{t}% range") } else { format!("{t}%") }
}

// ---------- helpers over the result ----------

/// A thread (or same-named threads) on one side: owner, wakes/s, CPU ms/run, how many.
pub(super) type Waker = (String, f64, f64, usize);

/// Threads by (name, module) on both sides, the one whose wake-ups moved most first.
pub(super) fn thread_rows(s: &Value) -> Vec<((String, String), Waker, Waker)> {
    let side = |k: &str| -> BTreeMap<(String, String), Waker> {
        let mut m = BTreeMap::new();
        for t in s["hotspots"][k]["threads"]["threads"].as_array().into_iter().flatten() {
            let key = (t["name"].as_str().unwrap_or("(unnamed)").to_string(), t["module"].as_str().unwrap_or("").to_string());
            let e = m.entry(key).or_insert((t["origin"].as_str().unwrap_or("").to_string(), 0.0, 0.0, 0));
            e.1 += t["wakes_per_s"].as_f64().unwrap_or(0.0);
            e.2 += t["cpu_ms_per_run"].as_f64().unwrap_or(0.0);
            e.3 += 1;
        }
        m
    };
    let (b, h) = (side("base"), side("head"));
    let mut keys: Vec<&(String, String)> = b.keys().chain(h.keys()).collect();
    keys.sort();
    keys.dedup();
    let zero = (String::new(), 0.0, 0.0, 0);
    let mut rows: Vec<((String, String), Waker, Waker)> = keys.into_iter().map(|k| (k.clone(), b.get(k).cloned().unwrap_or(zero.clone()), h.get(k).cloned().unwrap_or(zero.clone()))).collect();
    rows.sort_by(|x, y| (y.2.1 - y.1.1).abs().total_cmp(&(x.2.1 - x.1.1).abs()).then(x.0.cmp(&y.0)));
    rows
}

pub(super) fn layer_name(layer: &str) -> &str {
    match layer {
        "private" => "Private",
        "host" => "Heap and stacks",
        "gpu_mapped" => "GPU memory mapped in",
        "image" => "Loaded code",
        "working_set" => "Working set",
        "private_working_set" => "Private working set",
        "shared" => "Shared",
        "files" => "Mapped files",
        "unmapped" => "Committed, not mapped",
        other => other,
    }
}

/// A warning that changes how to read one series, said right under its line.
pub(super) fn caveat(s: &Value) -> Option<String> {
    let warn = |id: &str| s["warnings"].as_array().into_iter().flatten().any(|w| w["id"] == id && w["level"] == "warn");
    if warn("SessionLocked") {
        Some("**Mixed lock state:** some pairs ran locked and some unlocked, so nothing was decided.".into())
    } else if warn("ScreenChanged") {
        Some("**Different screens:** the builds did not all show the same screen, so part of the difference may be the screen.".into())
    } else if has_warning(s, "BackgroundPriority") {
        Some("**Background priority:** the app was throttled in some blocks.".into())
    } else {
        None
    }
}

/// A change's size for ranking: relative changes as they are, absolute ones against their base.
pub(super) fn size(c: &Value) -> f64 {
    if c["relative"] == true { f(&c["estimate"]).abs() } else { (f(&c["estimate"]) / f(&c["base"]).abs().max(1e-9)).abs() }
}

/// "44% less CPU time", or "Time until ready: 15% faster" for words that need their subject first.
pub(super) fn phrase(amount: &str, word: &str, thing: &str) -> String {
    if matches!(word, "faster" | "slower" | "smoother" | "choppier") { format!("{}: {amount} {word}", capitalize(thing)) } else { format!("{amount} {word} {thing}") }
}

/// `19–23%`, `120–219 MB`: two amounts of one unit as a span.
pub(super) fn span(lo: &str, hi: &str) -> String {
    let split = |s: &str| -> (String, String) {
        let i = s.find(|ch: char| !(ch.is_ascii_digit() || ch == '.')).unwrap_or(s.len());
        (s[..i].to_string(), s[i..].to_string())
    };
    let ((ln, lu), (hn, hu)) = (split(lo), split(hi));
    if lu == hu { format!("{ln}–{hn}{hu}") } else { format!("{lo}–{hi}") }
}

/// What a metric measures, so a hero says each kind of thing once.
pub(super) fn family(m: &str) -> &str {
    match m {
        "cpu.wakes" => "wakes",
        _ if m.starts_with("memory.") || m == "gpu.vram" || m == "gpu.shared" || m.starts_with("js.heap") => "memory",
        _ => m.split('.').next().unwrap_or(m),
    }
}

/// A case whose work grew from run to run: its numbers may not mean what they seem.
pub(super) fn unsteady(s: &Value) -> bool {
    has_warning(s, "NotSteady") || has_warning(s, "StateDrift")
}

/// The display's refresh rate the series ran at.
pub(super) fn refresh(ps: &[Pair], r: &Value) -> f64 {
    let mut v: Vec<f64> = ps.iter().flat_map(|p| [bv(p.base, "display.hz"), bv(p.head, "display.hz")]).filter(|x| x.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().or(r["refresh_hz"].as_f64()).unwrap_or(60.0)
}

/// Late frames (a gap of 1.5 frame times or more) as shares of all gaps, before and this PR, and in
/// how many pairs this PR had a larger share of them, of those with frames on both sides.
pub(super) fn late_frames(s: &Value, r: &Value) -> Option<(f64, f64, usize, usize)> {
    let ps = pairs(s);
    let vs = 1000.0 / refresh(&ps, r);
    let gaps = |b: &Value| b["intervals"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
    let (mut lb, mut tb, mut lh, mut th, mut worse, mut n) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for p in &ps {
        let (gb, gh) = (gaps(p.base), gaps(p.head));
        if gb.len() < 30 || gh.len() < 30 {
            continue;
        }
        let late = |g: &[f64]| g.iter().filter(|x| **x >= 1.5 * vs).count();
        let (xb, xh) = (late(&gb), late(&gh));
        lb += xb;
        tb += gb.len();
        lh += xh;
        th += gh.len();
        n += 1;
        if xh as f64 / gh.len() as f64 > xb as f64 / gb.len() as f64 {
            worse += 1;
        }
    }
    (n > 0).then(|| (lb as f64 / tb as f64, lh as f64 / th as f64, worse, n))
}

/// The change and likely range cells of a metric row; a decided change is bold. When the medians
/// moved the other way, their change stands beside it, so the row never reads as a contradiction.
pub(super) fn change_cells(c: &Value, o: Out) -> (String, String) {
    if o == Out::Refused {
        return ("—".into(), "—".into());
    }
    let d = nb(&delta(c, f(&c["estimate"])));
    let d = if marked(c, o) { format!("**{d}**") } else { d };
    let d = match uneven(c) {
        Some(m) => format!("{d} <sub>medians {}</sub>", nb(&delta(c, m))),
        None => d,
    };
    (d, range(c))
}

/// The medians' own change when it points the other way from the paired change by more than the
/// bound: the paired change is the median of the pairs' own changes, so a few blocks far from the
/// rest (a bimodal side) can put the two on opposite sides of zero.
pub(super) fn uneven(c: &Value) -> Option<f64> {
    let (b, h, est) = (f(&c["base"]), f(&c["head"]), f(&c["estimate"]));
    if c["exact"] == true || !(b.is_finite() && h.is_finite() && est.is_finite()) {
        return None;
    }
    let relative = c["relative"] == true;
    let m = match relative {
        true if b > 0.0 => h / b - 1.0,
        true => return None,
        false => h - b,
    };
    let bound = Some(f(&c["bound"]).abs()).filter(|x| x.is_finite() && *x > 0.0).unwrap_or(if relative { 0.02 } else { 0.02 * b.abs() });
    (m != 0.0 && est != 0.0 && (m > 0.0) != (est > 0.0) && (est - m).abs() >= bound).then_some(m)
}

/// A metric in everyday words, one scheme for each family (`Frame gap, p95`): from this list, else the
/// catalog's description without its parentheses and up to its first comma.
pub(super) fn noun(metric: &str) -> String {
    let known = match metric {
        "cpu.work" => "CPU time per run",
        "cpu.wakes" => "Thread wake-ups",
        "cpu.app.total" | "cpu.process.total" => "Total CPU use",
        "cpu.renderer" => "Renderer CPU use",
        "cpu.gpu" => "GPU process CPU use",
        "cpu.browser" => "Browser process CPU use",
        "memory.app.private" => "App memory",
        "memory.app.host" => "App heap memory",
        "gpu.vram" => "GPU memory",
        "gpu.shared" => "Shared GPU memory",
        "gpu.process.3d" => "GPU use",
        "main.busy" => "Main-thread busy time",
        "main.work" => "Main-thread work per run",
        "main.script" => "JavaScript time per run",
        "main.style" => "Style time per run",
        "main.layout" => "Layout time per run",
        "main.gc" => "Garbage collection per run",
        "main.tasks.long" => "Long tasks per run",
        "layout.count" => "Layouts per run",
        "style.count" => "Style recalculations per run",
        "run.duration" => "Run time",
        "launch.ready" => "Time until ready",
        "io.read" => "File and pipe reads per run",
        "io.write" => "File and pipe writes per run",
        "energy.cpu" => "CPU energy per run",
        "cpu.instructions" => "Instructions per run",
        "cpu.idle_wakes" => "Idle wake-ups",
        "frames.dropped.pct" => "Dropped frames",
        "frames.dropped" => "Dropped frames per run",
        "frames.presented.count" => "Frames shown per run",
        "frames.interval.p50" => "Frame gap, p50",
        "frames.interval.p95" => "Frame gap, p95",
        "frames.interval.p99" => "Frame gap, p99",
        "frames.interval.max" => "Longest frame gap",
        "frames.presented.p50" => "Frame latency, p50",
        "frames.presented.p90" => "Frame latency, p90",
        "frames.presented.p95" => "Frame latency, p95",
        "frames.presented.p99" => "Frame latency, p99",
        "frames.presented.max" => "Longest frame latency",
        "interaction.p95" => "Input response, p95",
        "interaction.p50" => "Input response, p50",
        "scroll.latency.p50" => "Scroll response, p50",
        "scroll.latency.p95" => "Scroll response, p95",
        "ui.cpu" => "UI-thread CPU time per run",
        "ui.wakes" => "UI-thread wake-ups",
        "js.heap.used" => "JavaScript heap",
        "dom.nodes" => "DOM nodes",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    if let Some(rest) = metric.strip_prefix("launch.") {
        return format!("Time until {rest}");
    }
    if let Some(rest) = metric.strip_prefix("phase.") {
        return format!("Time in {rest} per run");
    }
    if let Some(rest) = metric.strip_prefix("span.") {
        return format!("Time in the page's {rest} spans per run");
    }
    if let Some(rest) = metric.strip_prefix("count.") {
        return capitalize(&format!("{rest} count per run"));
    }
    let about = model::metric(metric).map(|m| m.about).unwrap_or(metric);
    // The catalog says which targets a metric is for (`native apps: …`); a name does not.
    let about = regex_lite::Regex::new(r"^[a-z ]+: ").expect("pattern").replace(about, "").to_string();
    let plain = regex_lite::Regex::new(r"\s*\([^)]*\)").expect("pattern").replace_all(&about, "").to_string();
    let first = plain.split(',').next().unwrap_or(&plain).trim().to_string();
    if first.is_empty() { metric.to_string() } else { capitalize(&first) }
}

/// Descriptive rows not already tested, and the names of those that read 0 on both sides.
pub(super) fn context(s: &Value) -> (Vec<&Value>, Vec<String>) {
    let tested: Vec<&str> = tested(s).iter().filter_map(|c| c["metric"].as_str()).collect();
    let mut seen: Vec<&str> = Vec::new();
    let (mut rows, mut zeros) = (Vec::new(), Vec::new());
    for c in s["descriptive"].as_array().into_iter().flatten() {
        let m = c["metric"].as_str().unwrap_or("");
        if m.is_empty() || tested.contains(&m) || seen.contains(&m) {
            continue;
        }
        seen.push(m);
        if f(&c["base"]) == 0.0 && f(&c["head"]) == 0.0 {
            zeros.push(m.to_string());
        } else {
            rows.push(c);
        }
    }
    (rows, zeros)
}

pub(super) fn all_rows(s: &Value) -> Vec<&Value> {
    let mut v = tested(s);
    let have: Vec<String> = v.iter().filter_map(|c| c["metric"].as_str().map(str::to_string)).collect();
    v.extend(s["descriptive"].as_array().into_iter().flatten().filter(|c| !have.iter().any(|h| c["metric"] == h.as_str())));
    v
}

/// A verdict in a table, in one of a few plain words.
pub(super) fn verdict_text(c: &Value, o: Out, s: &Value, aa: bool) -> String {
    let v = verdict(c);
    let sub = |x: &str| if x.is_empty() { String::new() } else { format!(" <sub>{}</sub>", cell(x)) };
    match o {
        Out::Better | Out::Worse if !beyond_margin(c) => format!("**{}**", cell(v.trim_end_matches(" (small)"))),
        Out::AaFalse => "**false positive**".into(),
        Out::Same if aa => "same <sub>A/A passed</sub>".into(),
        Out::Same => "same".into(),
        Out::NoWorse | Out::NoWorseSlower => format!("**no worse**{}", sub(v.trim_start_matches("no worse").trim().trim_matches(['(', ')']).split(',').next().unwrap_or(""))),
        Out::Worse | Out::Better => format!("**worse**{}", sub(v.trim_start_matches("worse").trim().trim_matches(['(', ')']))),
        Out::Refused => {
            let (id, why) = refusal(c);
            format!("skipped{}", sub(&plain_refusal(&id, &why)))
        }
        Out::Saturated => "at the ceiling".into(),
        Out::Stopped => "stopped".into(),
        Out::Gated if s["decided"] == true && (f(&c["lo"]) > 0.0 || f(&c["hi"]) < 0.0) => {
            let need = s["holm_pairs"].as_u64().filter(|n| *n > s["pairs"].as_u64().unwrap_or(0));
            format!("inconclusive{}", sub(&need.map(|n| format!("needs {n} pairs")).unwrap_or_default()))
        }
        Out::Unclear | Out::Gated => {
            // Only a count past the pairs already run: "~10 pairs" after 12 reads as a contradiction.
            let ran = c["n"].as_u64().or(s["pairs"].as_u64()).unwrap_or(0);
            let need = c["needed_pairs"].as_u64().filter(|n| *n <= 1000 && *n > ran && (o == Out::Unclear || s["decided"] == true)).map(|n| format!("~{n} pairs")).unwrap_or_default();
            format!("inconclusive{}", sub(&need))
        }
        Out::Desc => String::new(),
        Out::NotRun => "not run".into(),
    }
}

/// Why a metric was refused, in a few words: the same reason reads the same in every row, and the
/// sentence behind it (which permission, where to grant it) is said once, in How it was measured.
pub(super) fn plain_refusal(id: &str, why: &str) -> String {
    // `scroll.latency.p50: OS present events: …`: the row already names the metric.
    let why = regex_lite::Regex::new(r"^[a-z]+(\.[a-z0-9]+)+: ").expect("pattern").replace(why, "").to_string();
    if let Some(short) = permission(&why) {
        return short.0.into();
    }
    match id {
        "TailTooShort" => match why.split_once(" < ") {
            Some((have, need)) => format!("too few frames, {} of {need}", have.trim_end_matches(" frames")),
            None => "too few frames".into(),
        },
        "SessionLocked" => "lock state mixed".into(),
        "DisplayOff" => "display off".into(),
        "PatternInput" => "acted through accessibility, no input to time".into(),
        _ if why.is_empty() => id.to_string(),
        _ => first_clause(&scrub_paths(&why), 90),
    }
}

/// A missing OS permission behind a refusal or note: (a row's few words, the sentence said once).
pub(super) fn permission(text: &str) -> Option<(&'static str, &'static str)> {
    text.contains("Screen Recording").then_some(("no Screen Recording", "not measured (strobe lacks Screen Recording on this Mac)"))
}

/// Local paths out: a user's home as `~`, other absolute paths by their last part.
pub(super) fn scrub_paths(s: &str) -> String {
    // `C:/Users/x/…` too, or the drive letter stayed behind (`C:~/…`).
    let home = regex_lite::Regex::new(r"([A-Za-z]:)?(/Users|/home)/[^/\s`|)]+").expect("pattern");
    let s = home.replace_all(s, "~");
    let drive = regex_lite::Regex::new(r#"(\\\\\?\\)?\b[A-Za-z]:[\\/](?:[^\s\\/|`)<>"]+[\\/])+"#).expect("pattern");
    drive.replace_all(&s, "…/").to_string()
}

/// A prebuilt executable as a report names it in public: `exe:…/base/app`, the last two parts of
/// its path (the folder tells the builds apart), never the machine's path.
pub(super) fn symbolic(spec: &str) -> String {
    let Some(path) = spec.strip_prefix("exe:") else { return spec.to_string() };
    if !local_path(path) {
        return spec.to_string();
    }
    let parts: Vec<&str> = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).take(2).collect();
    format!("exe:…/{}", parts.into_iter().rev().collect::<Vec<_>>().join("/"))
}

/// A note strobe wrote, in the report's words: each side by its word (`before` and `this PR`, or an
/// implementation's compact name) where the note names base or head (`head: …`, `base attribution
/// block failed`), and metric ids by their names.
pub(super) fn plain_note(text: &str, (base, head): (&str, &str)) -> String {
    let roles = regex_lite::Regex::new(r"\b(base|head)(: | attribution block\b)").expect("pattern");
    let t = roles.replace_all(text, |c: &regex_lite::Captures| {
        let side = if &c[1] == "base" { base } else { head };
        if &c[2] == ": " { format!("{side}: ") } else { format!("{side}'s attribution block") }
    });
    let ids = regex_lite::Regex::new(r"\b[a-z]+(\.[a-z0-9]+)+\b").expect("pattern");
    ids.replace_all(&t, |c: &regex_lite::Captures| if model::metric(&c[0]).is_some() { lc(&noun(&c[0])) } else { c[0].to_string() }).to_string()
}

/// Spaces that may not break: a number stays with its unit.
pub(super) fn nb(s: &str) -> String {
    s.replace(' ', "\u{a0}")
}

/// Three significant figures, no exponent: 229, 50.4, 7.05, 0.0881.
pub(super) fn sig3(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    if a == 0.0 {
        return "0".into();
    }
    let d = if a >= 100.0 { 0 } else { (2 - a.log10().floor() as i32).clamp(0, 4) as usize };
    let s = format!("{v:.d$}");
    if s.trim_start_matches('-').chars().all(|c| c == '0' || c == '.') { "0".into() } else { s }
}

/// A value with three significant figures and its unit; a second or longer in seconds.
pub(super) fn disp(v: f64, unit: &str) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    match unit {
        "ms" if v.abs() >= 1000.0 => format!("{} s", sig3(v / 1000.0)),
        "ms" => format!("{} ms", sig3(v)),
        "%" => format!("{}%", sig3(v)),
        "pp" => format!("{} pp", sig3(v)),
        "MB" => format!("{} MB", sig3(v)),
        "/s" => format!("{}/s", sig3(v)),
        "bytes" => fmt::bytes(v),
        "count" | "" => fmt::value(v, "count"),
        u => format!("{} {u}", sig3(v)),
    }
}

pub(super) fn signed_disp(v: f64, unit: &str) -> String {
    let body = disp(v.abs(), unit);
    // One that shows as zero has no sign.
    if body == disp(0.0, unit) {
        body
    } else if v < 0.0 {
        format!("−{body}")
    } else if v > 0.0 {
        format!("+{body}")
    } else {
        body
    }
}

/// A share as a percentage with two decimals: 10.28%.
pub(super) fn pct2(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

/// `8 s`, `20 s`, `103 ms`: a run's length, rounded for a sentence.
pub(super) fn short_duration(ms: f64) -> String {
    if ms >= 1000.0 {
        let s = ms / 1000.0;
        if s >= 10.0 || (s - s.round()).abs() < 0.05 { format!("{:.0} s", s) } else { format!("{s:.1} s") }
    } else {
        format!("{ms:.0} ms")
    }
}

/// `a`, `a and b`, `a, b and c`.
pub(super) fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `AMD Ryzen 9 9950X3D 16-Core Processor` → `Ryzen 9 9950X3D`.
pub(super) fn short_cpu(cpu: &str) -> String {
    let c = cpu.replace("(R)", "").replace("(TM)", "").replace(" CPU", "");
    let c = c.split(" @ ").next().unwrap_or(&c).to_string();
    c.split_whitespace().filter(|w| !w.ends_with("-Core") && *w != "Processor" && *w != "AMD" && *w != "Intel").collect::<Vec<_>>().join(" ")
}

pub(super) fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|x| x.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `CPU time` → `cPU time` would be wrong: lower the first letter only when the second is lower too.
pub(super) fn lc(s: &str) -> String {
    let mut c = s.chars();
    // `JavaScript heap` keeps its capitals too.
    let camel = s.split(' ').next().unwrap_or("").chars().skip(1).any(char::is_uppercase);
    match (c.next(), s.chars().nth(1)) {
        (Some(a), Some(b)) if b.is_lowercase() && !camel => a.to_lowercase().chain(c).collect(),
        _ => s.to_string(),
    }
}

/// The equivalence bound in the comparison's scale: `±2%`, or `±26.5` in the metric's unit.
pub(super) fn bound_text(c: &Value) -> String {
    let b = f(&c["bound"]);
    if !b.is_finite() || b <= 0.0 {
        return "±2%".into();
    }
    if c["relative"] == true { format!("±{:.0}%", b * 100.0) } else { format!("±{}", fmt::value(b, abs_unit(unit_of(c["metric"].as_str().unwrap_or(""))))) }
}

/// A commit subject without pull request references: `#551` would link to this PR's repository,
/// which is often not the one the commit came from.
pub(super) fn no_refs(subject: &str) -> String {
    let re = regex_lite::Regex::new(r"\s*\(#\d+\)").expect("pattern");
    let s = re.replace_all(subject, "");
    let re = regex_lite::Regex::new(r"#(\d)").expect("pattern");
    re.replace_all(&s, "#\u{2060}$1").to_string()
}

/// A warning's name for people, beside its id.
pub(super) fn warning_name(id: &str) -> &str {
    match id {
        "PowerPlan" => "Power plan",
        "Background" => "Busy machine",
        "OtherLoad" => "Other load",
        "NotSteady" => "Unsteady warmup",
        "StateDrift" => "State drift",
        "HighVariance" => "High variance",
        "Drift" => "Drift",
        "MultimodalDistribution" => "Multimodal values",
        "Unresolved" => "Unmapped scripts",
        "Attribution" => "Attribution failed",
        "PixelsDiffer" => "Pixels differ",
        "CrossAdapterScanout" => "Cross-adapter scanout",
        "FewPairs" => "Few pairs",
        "UnpacedSpin" => "Unpaced frames",
        "BackgroundPriority" => "Background priority",
        "TimerCoalescing" => "Timer coalescing",
        "StaleCalibration" => "Stale calibration",
        "VariantChanged" => "Variant changed",
        "ScreenChanged" => "Different screens",
        "NoPresentSource" => "Frame and latency metrics",
        other => other,
    }
}

pub(super) fn has_warning(s: &Value, id: &str) -> bool {
    s["warnings"].as_array().into_iter().flatten().any(|w| w["id"] == id)
}

/// The primary first, then the secondaries, each metric once.
pub(super) fn tested(s: &Value) -> Vec<&Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut rows = Vec::new();
    for c in std::iter::once(&s["primary"]).chain(s["secondaries"].as_array().into_iter().flatten()) {
        let m = c["metric"].as_str().unwrap_or("");
        if m.is_empty() || seen.contains(&m) {
            continue;
        }
        seen.push(m);
        rows.push(c);
    }
    rows
}

pub(super) fn variants(r: &Value) -> (Value, Value) {
    let find = |role: &str| r["variants"].as_array().into_iter().flatten().find(|v| v["role"] == role).cloned().unwrap_or(Value::Null);
    (find("base"), find("head"))
}

pub(super) fn local_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with('\\') || s.starts_with("~") || s.as_bytes().get(1) == Some(&b':') || s.starts_with("./") || s.starts_with("../")
}

/// The commit in a variant's label (`<name>@<sha>…`).
pub(super) fn label_commit(v: &Value) -> Option<String> {
    let label = v["label"].as_str()?;
    let (_, rest) = label.rsplit_once('@')?;
    let sha: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    (sha.len() >= 7).then_some(sha)
}

/// How a report names a variant in public: `1982f5313f`, `origin/perf-x@7bf4e2d566`, `… (dirty)`;
/// never a local folder's name.
pub(super) fn tag(v: &Value) -> String {
    let label = v["label"].as_str().unwrap_or("");
    let spec = v["spec"].as_str().unwrap_or("");
    let dirty = v["dirty"] == true || label.ends_with("(dirty)");
    let sha = v["commit"].as_str().map(|c| c.chars().take(10).collect::<String>()).or_else(|| label_commit(v));
    let Some(sha) = sha else { return if local_path(spec) || spec == "." { "working tree".into() } else { symbolic(spec) } };
    let is_ref = !spec.is_empty() && spec != "." && !local_path(spec) && !spec.chars().all(|c| c.is_ascii_hexdigit()) && v["kind"] != "dir";
    let mut t = if is_ref { format!("{spec}@{sha}") } else { sha };
    if dirty {
        t += " (dirty)";
    }
    t
}

/// The busiest other processes across blocks, `` `bun.exe` 1230% ``.
pub(super) fn top_procs(blocks: &[&Value], n: usize) -> Vec<String> {
    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for b in blocks {
        for e in b["background"].as_array().into_iter().flatten() {
            let (Some(name), Some(p)) = (e[0].as_str(), e[1].as_f64()) else { continue };
            let x = best.entry(name.to_string()).or_insert(0.0);
            *x = x.max(p);
        }
    }
    let mut v: Vec<(String, f64)> = best.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(n).map(|(name, p)| format!("{} {p:.0}%", code(&name))).collect()
}

/// A series label without the target (`window-closed · tray` → `window-closed`).
pub(super) fn short_label(label: &str) -> &str {
    label.rsplit_once(" · ").map(|(a, _)| a).unwrap_or(label)
}

/// A warning's text up to its first `: ` detail list, at most `max` characters.
pub(super) fn first_clause(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let cut: String = t.chars().take(max).collect();
    let at = cut.rfind([';', ',', ' ']).unwrap_or(cut.len());
    format!("{}…", cut[..at].trim_end_matches([',', ';', ' ']))
}

pub(super) fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A bisect experiment: the culprit (or what is left) and each step's comparison.
pub(super) fn bisect(r: &Value) -> String {
    let id = r["id"].as_str().unwrap_or("");
    let file = repo_relative(r["file"].as_str().unwrap_or(""));
    let span = r["range"].as_str().unwrap_or("").split("..").map(|c| c.chars().take(10).collect::<String>()).collect::<Vec<_>>().join("..");
    let m = r["metric"].as_str().unwrap_or("");
    let verdict_line = r["verdict"].as_str().unwrap_or("");
    let head = match verdict_line.strip_prefix("found: ") {
        Some(rest) => format!("**Bisect:** {} changed at {}.", noun(m), cell(rest)),
        None => format!("**Bisect:** {}.", cell(verdict_line)),
    };
    let mut t = format!("{head}\nSearched {} in {}, one full comparison per step.\n\n", code(&span), code(&file));
    t += "<details>\n<summary><b>Which commit?</b> Each step of the search</summary>\n\n| Step | Commit, against the last good one | Pairs | Change | Likely range | Verdict |\n|---:|:---|---:|---:|---:|:---|\n";
    for s in r["steps"].as_array().into_iter().flatten() {
        let o = outcome(s, false);
        t += &format!(
            "| {} | {} {} | {} | {} | {} | {} |\n",
            s["step"],
            code(s["short"].as_str().unwrap_or("")),
            cell(&no_refs(s["subject"].as_str().unwrap_or(""))),
            s["pairs"],
            if marked(s, o) { format!("**{}**", nb(&delta(s, f(&s["estimate"])))) } else { nb(&delta(s, f(&s["estimate"]))) },
            range(s),
            cell(verdict(s))
        );
    }
    t + &format!("\nEach step is an experiment of its own (`strobe report experiment <step> --format pr`); the whole search: `strobe report experiment {id} --open`.\n\n</details>\n")
}
