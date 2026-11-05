//! Paired statistics (SPEC §9.3): Wilcoxon signed-rank, Hodges–Lehmann, Holm, verdicts, mValue.

use crate::model::{Better, Kind, MetricDef, Refusal, ValidatorId, VerdictKind};
use serde::Serialize;

/// Signed-rank null distribution: counts of rank sums for n pairs (exact as f64 up to n = 50).
fn signed_rank_counts(n: usize) -> Vec<f64> {
    let max = n * (n + 1) / 2;
    let mut c = vec![0.0f64; max + 1];
    c[0] = 1.0;
    for k in 1..=n {
        for s in (k..=max).rev() {
            c[s] += c[s - k];
        }
    }
    c
}

/// Two-sided p-value of the signed-rank test for differences `d` (zeros dropped).
pub fn wilcoxon_p(d: &[f64]) -> f64 {
    let nz: Vec<f64> = d.iter().copied().filter(|x| *x != 0.0 && x.is_finite()).collect();
    let n = nz.len();
    if n == 0 {
        return 1.0;
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| nz[a].abs().total_cmp(&nz[b].abs()));
    let mut ranks = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && nz[idx[j + 1]].abs() == nz[idx[i]].abs() {
            j += 1;
        }
        let r = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            ranks[idx[k]] = r;
        }
        i = j + 1;
    }
    let t: f64 = (0..n).filter(|&k| nz[k] > 0.0).map(|k| ranks[k]).sum();
    if n <= 50 {
        let c = signed_rank_counts(n);
        let total = 2f64.powi(n as i32);
        let t_floor = t.floor() as usize;
        let lower: f64 = c[..=t_floor.min(c.len() - 1)].iter().sum::<f64>() / total;
        let t_ceil = t.ceil() as usize;
        let upper: f64 = c[t_ceil.min(c.len() - 1)..].iter().sum::<f64>() / total;
        return (2.0 * lower.min(upper)).min(1.0);
    }
    let nf = n as f64;
    let mean = nf * (nf + 1.0) / 4.0;
    let sd = (nf * (nf + 1.0) * (2.0 * nf + 1.0) / 24.0).sqrt();
    let z = ((t - mean).abs() - 0.5) / sd;
    (2.0 * (1.0 - normal_cdf(z))).min(1.0)
}

fn normal_cdf(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

fn erf(x: f64) -> f64 {
    // Abramowitz–Stegun 7.1.26
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}

pub(crate) fn normal_quantile(p: f64) -> f64 {
    // Acklam's approximation
    let a = [-39.69683028665376, 220.9460984245205, -275.9285104469687, 138.357_751_867_269, -30.66479806614716, 2.506628277459239];
    let b = [-54.47609879822406, 161.5858368580409, -155.6989798598866, 66.80131188771972, -13.28068155288572];
    let c = [-0.007784894002430293, -0.3223964580411365, -2.400758277161838, -2.549732539343734, 4.374664141464968, 2.938163982698783];
    let d = [0.007784695709041462, 0.3224671290700398, 2.445134137142996, 3.754408661907416];
    let pl = 0.02425;
    if p < pl {
        let q = (-2.0 * p.ln()).sqrt();
        (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) / ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0)
    } else if p <= 1.0 - pl {
        let q = p - 0.5;
        let r = q * q;
        (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5]) * q / (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0)
    } else {
        -normal_quantile(1.0 - p)
    }
}

/// Growth per step of a series taken at equal steps (a census after every run): the Theil–Sen slope,
/// the median of all pairwise slopes, with Sen's distribution-free interval from Kendall's tau at
/// `confidence`. Robust to a few outlying readings (a late GC). `None` below 3 points.
pub fn theil_sen(ys: &[f64], confidence: f64) -> Option<(f64, f64, f64)> {
    let pts: Vec<(f64, f64)> = ys.iter().enumerate().filter(|(_, y)| y.is_finite()).map(|(i, y)| (i as f64, *y)).collect();
    let n = pts.len();
    if n < 3 {
        return None;
    }
    let mut slopes: Vec<f64> = (0..n).flat_map(|i| (i + 1..n).map(move |j| (i, j))).map(|(i, j)| (pts[j].1 - pts[i].1) / (pts[j].0 - pts[i].0)).collect();
    slopes.sort_by(|a, b| a.total_cmp(b));
    let m = slopes.len();
    let median = if m % 2 == 1 { slopes[m / 2] } else { (slopes[m / 2 - 1] + slopes[m / 2]) / 2.0 };
    let z = normal_quantile(1.0 - (1.0 - confidence) / 2.0);
    let c = z * ((n * (n - 1) * (2 * n + 5)) as f64 / 18.0).sqrt();
    let lo = (((m as f64 - c) / 2.0).floor().max(0.0) as usize).min(m - 1);
    let hi = ((((m as f64 + c) / 2.0).ceil()) as usize).min(m - 1);
    Some((median, slopes[lo], slopes[hi]))
}

/// Hodges–Lehmann estimate and its distribution-free interval (Walsh averages) at `confidence`.
pub fn hodges_lehmann(d: &[f64], confidence: f64) -> (f64, f64, f64, f64) {
    let v: Vec<f64> = d.iter().copied().filter(|x| x.is_finite()).collect();
    let n = v.len();
    if n == 0 {
        return (f64::NAN, f64::NAN, f64::NAN, 0.0);
    }
    let mut walsh = Vec::with_capacity(n * (n + 1) / 2);
    for i in 0..n {
        for j in i..n {
            walsh.push((v[i] + v[j]) / 2.0);
        }
    }
    walsh.sort_by(|a, b| a.total_cmp(b));
    let m = walsh.len();
    let est = if m % 2 == 1 { walsh[m / 2] } else { (walsh[m / 2 - 1] + walsh[m / 2]) / 2.0 };
    let alpha = 1.0 - confidence;
    // The interval [W(c), W(m-1-c)] (0-based, sorted Walsh averages) covers 1 - 2·P(T <= c), with c
    // the largest value where P(T <= c) <= alpha/2. When even c = 0 is too likely (fewer than 6
    // pairs at 95%), no interval reaches the level: [min, max] covers only 1 - 2·P(T = 0).
    let (c, achieved) = if n <= 50 {
        let counts = signed_rank_counts(n);
        let total = 2f64.powi(n as i32);
        let mut acc = 0.0;
        let mut c: i64 = -1;
        for (k, cnt) in counts.iter().enumerate() {
            if (acc + cnt) / total > alpha / 2.0 {
                break;
            }
            acc += cnt;
            c = k as i64;
        }
        if c < 0 { (0, 1.0 - 2.0 * counts[0] / total) } else { (c, 1.0 - 2.0 * acc / total) }
    } else {
        let nf = n as f64;
        let mean = nf * (nf + 1.0) / 4.0;
        let sd = (nf * (nf + 1.0) * (2.0 * nf + 1.0) / 24.0).sqrt();
        ((mean - normal_quantile(1.0 - alpha / 2.0) * sd - 0.5).floor().max(0.0) as i64, confidence)
    };
    let lo_i = c.clamp(0, m as i64 - 1) as usize;
    let hi_i = (m as i64 - 1 - c).clamp(0, m as i64 - 1) as usize;
    (est, walsh[lo_i.min(hi_i)], walsh[hi_i.max(lo_i)], achieved)
}

/// Holm step-down: which of `p` are rejected at `alpha`.
pub fn holm(p: &[f64], alpha: f64) -> Vec<bool> {
    let mut idx: Vec<usize> = (0..p.len()).collect();
    idx.sort_by(|&a, &b| p[a].total_cmp(&p[b]));
    let mut out = vec![false; p.len()];
    for (rank, &i) in idx.iter().enumerate() {
        if p[i] <= alpha / (p.len() - rank) as f64 {
            out[i] = true;
        } else {
            break;
        }
    }
    out
}

/// mValue (Brendan Gregg), as BenchmarkDotNet uses it: above 3.2 the values likely have several modes.
pub fn mvalue(values: &[f64]) -> Option<f64> {
    let v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    let n = v.len();
    if n < 10 {
        return None;
    }
    let mean = v.iter().sum::<f64>() / n as f64;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt();
    if sd == 0.0 {
        return Some(2.0);
    }
    let width = 3.49 * sd * (n as f64).powf(-1.0 / 3.0);
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let bins = (((max - min) / width).ceil() as usize).max(1);
    let mut counts = vec![0usize; bins + 2];
    for x in &v {
        let b = (((x - min) / width).floor() as usize).min(bins - 1);
        counts[b + 1] += 1;
    }
    let peak = *counts.iter().max().unwrap_or(&1) as f64;
    let sum: f64 = counts.windows(2).map(|w| (w[1] as f64 - w[0] as f64).abs()).sum();
    Some(sum / peak)
}

/// The comparison of one metric between base and head over pairs.
#[derive(Clone, Debug, Serialize)]
pub struct Comparison {
    pub metric: String,
    pub n: usize,
    pub base: f64,
    pub head: f64,
    /// Relative change for ratio metrics, absolute difference for counts.
    pub estimate: f64,
    pub lo: f64,
    pub hi: f64,
    pub relative: bool,
    pub confidence: f64,
    pub p: f64,
    pub bound: f64,
    /// What it decided, in words for people (`faster (small)`, `refused (TailTooShort): …`).
    /// `set_verdict` and `refuse` change it together with `kind` and `refusal`.
    pub verdict: String,
    /// What it decided, for code and result.json readers.
    #[serde(rename = "verdict_kind")]
    pub kind: VerdictKind,
    pub decided: bool,
    pub needed_pairs: Option<usize>,
    /// A counter that held one value on each side in every block: compared exactly (SPEC §10.1).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub exact: bool,
    /// Non-inferiority (SPEC §9.3): the margin by which head may be worse (a fraction for ratio metrics,
    /// the metric's unit otherwise); `p` then tests the difference against the margin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noworse: Option<f64>,
    /// The validator that refused this metric (`kind` is `Refused`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

impl Comparison {
    /// Says what it decided, in words and as a kind at once (a gate, a stop, a descriptive metric).
    pub fn set_verdict(&mut self, kind: VerdictKind, verdict: impl Into<String>) {
        self.kind = kind;
        self.verdict = verdict.into();
        self.refusal = None;
    }

    /// Refuses the metric: `refused (<validator>): <summary>`, undecided.
    pub fn refuse(&mut self, r: Refusal) {
        self.verdict = format!("refused ({}): {}", r.validator, r.summary);
        self.kind = VerdictKind::Refused;
        self.decided = false;
        self.refusal = Some(r);
    }
}

/// What a verdict decided, from its words, for result.json files from before `verdict_kind`: new
/// comparisons carry their kind. `def` is the metric's (or its family's) definition, whose words
/// say which direction is better.
pub fn verdict_kind(verdict: &str, def: Option<&MetricDef>) -> VerdictKind {
    let v = verdict.trim();
    let first = v.split([' ', ':', ',']).next().unwrap_or("");
    match first {
        "—" => VerdictKind::Untested,
        "·" => VerdictKind::Descriptive,
        "refused" => VerdictKind::Refused,
        "no" if v.starts_with("no worse") => VerdictKind::NoWorse,
        "worse" => VerdictKind::Worse,
        "same" => VerdictKind::Same,
        "underpowered" => VerdictKind::Underpowered,
        "saturated" => VerdictKind::Saturated,
        "stopped" => VerdictKind::Stopped,
        w if def.is_some_and(|d| w == d.words.0) => VerdictKind::Better,
        w if def.is_some_and(|d| w == d.words.1) => VerdictKind::Worse,
        _ => VerdictKind::Unclear,
    }
}

/// The refusal a `refused (<validator>): <why>` verdict names, for result.json files from before
/// `refusal`.
pub fn refusal_of(verdict: &str) -> Option<Refusal> {
    let rest = verdict.trim().strip_prefix("refused (")?;
    let (id, why) = rest.split_once(')')?;
    Some(Refusal::new(ValidatorId::from_name(id)?, why.trim_start_matches(':').trim()))
}

/// Per-pair differences of a metric: `ln(head / base)` when every value is positive and the metric is a
/// ratio metric, else `head − base`. Returns the finite pairs, the differences and whether they are ratios.
fn diffs(def: &MetricDef, pairs: &[(f64, f64)]) -> (Vec<(f64, f64)>, Vec<f64>, bool) {
    let finite: Vec<(f64, f64)> = pairs.iter().copied().filter(|(a, b)| a.is_finite() && b.is_finite()).collect();
    // Counts compare by difference: a zero on either side (no fetches, no shift) has no ratio.
    let relative = def.kind != Kind::Exact && def.unit != "count" && def.unit != "score" && finite.iter().all(|(a, b)| *a > 0.0 && *b > 0.0);
    let d = finite.iter().map(|(a, b)| if relative { (b / a).ln() } else { b - a }).collect();
    (finite, d, relative)
}

/// The margin as a difference in `diffs`' scale: where "worse by exactly the margin" sits. `margin_abs`
/// is the margin in the comparison's scale (a fraction for ratios, the unit otherwise).
pub fn margin_shift(def: &MetricDef, relative: bool, margin_abs: f64) -> f64 {
    let worse = if def.better == Better::Lower { 1.0 } else { -1.0 };
    if relative { (1.0 + worse * margin_abs).max(1e-9).ln() } else { worse * margin_abs }
}

/// Non-inferiority (SPEC §9.3): is head at most `margin` worse than base? The same interval as `compare`
/// (at `confidence`, a planned look's level for a primary), judged against the margin instead of 0:
/// `no worse (≤X)` when its worse end lies inside the margin, `worse (>X)` when its better end lies
/// beyond it, else `unclear`. Each claim is a one-sided test at (1 − confidence)/2; `p` is the two-sided
/// signed-rank test of the differences against the margin, so Holm treats it like any other secondary.
/// A `no worse` whose interval also excludes 0 says which way (`, faster` or `, but slower`).
pub fn compare_noworse(name: &str, def: &MetricDef, pairs: &[(f64, f64)], margin: f64, confidence: f64) -> Comparison {
    let mut c = compare(name, def, pairs, margin, confidence);
    let (_, d, relative) = diffs(def, pairs);
    let margin_abs = if relative { margin } else { (margin * c.base.abs()).max(0.5) };
    let label = fmt_bound(margin, relative, margin_abs);
    let good_low = def.better == Better::Lower;
    c.noworse = Some(margin_abs);
    c.bound = margin_abs;
    // Oriented so that larger is worse.
    let (w_est, w_lo, w_hi) = if good_low { (c.estimate, c.lo, c.hi) } else { (-c.estimate, -c.hi, -c.lo) };
    if c.exact {
        if w_est <= margin_abs {
            c.set_verdict(VerdictKind::NoWorse, format!("no worse (≤{label}) (exact)"));
        } else {
            c.set_verdict(VerdictKind::Worse, format!("worse (>{label}) (exact)"));
        }
        c.p = 0.0;
        c.decided = true;
        return c;
    }
    let shift = margin_shift(def, relative, margin_abs);
    c.p = wilcoxon_p(&d.iter().map(|x| x - shift).collect::<Vec<_>>());
    let covered = c.confidence + 1e-9 >= confidence;
    let n = d.len();
    let (kind, verdict) = if n < 2 || !w_est.is_finite() || !covered {
        (VerdictKind::Unclear, "unclear".to_string())
    } else if w_hi < margin_abs {
        let tag = if w_hi < 0.0 {
            format!(", {}", def.words.0)
        } else if w_lo > 0.0 {
            format!(", but {}", def.words.1)
        } else {
            String::new()
        };
        (VerdictKind::NoWorse, format!("no worse (≤{label}{tag})"))
    } else if w_lo > margin_abs {
        (VerdictKind::Worse, format!("worse (>{label})"))
    } else {
        (VerdictKind::Unclear, "unclear".to_string())
    };
    let decided = kind.decided();
    c.set_verdict(kind, verdict);
    c.decided = decided;
    c.needed_pairs = (!decided && n >= 2).then(|| {
        let hw = (c.hi - c.lo) / 2.0;
        let dist = (margin_abs - w_est).abs();
        let estimate = if dist <= 0.0 || hw <= 0.0 { usize::MAX } else { ((n as f64) * (hw / dist).powi(2)).ceil() as usize };
        estimate.max(min_pairs(confidence))
    });
    c
}

/// `pairs_needed` for a non-inferiority comparison: the distance that decides is the one from the
/// differences' median to the margin (`shift`, in the differences' scale), either way.
pub fn pairs_needed_noworse(d: &[f64], shift: f64, confidence: f64) -> usize {
    let v: Vec<f64> = d.iter().copied().filter(|x| x.is_finite()).collect();
    if v.len() < 2 {
        return usize::MAX;
    }
    let dist = (crate::metrics::median(&v) - shift).abs();
    let sigma = robust_sd(&v);
    if dist <= 0.0 {
        return usize::MAX;
    }
    let z = normal_quantile(1.0 - (1.0 - confidence) / 2.0);
    (((z * sigma / dist).powi(2) / 0.955).ceil() as usize).max(min_pairs(confidence))
}

/// A robust standard deviation: 1.4826 × the median absolute deviation from the median.
pub fn robust_sd(v: &[f64]) -> f64 {
    let med = crate::metrics::median(v);
    1.4826 * crate::metrics::median(&v.iter().map(|x| (x - med).abs()).collect::<Vec<_>>())
}

/// Planning (SPEC §9.1 `plan`): pairs for a `power` chance that an interval at `confidence` clears a
/// boundary `dist` away from the true difference, for per-pair differences with spread `sigma` (same
/// scale). Normal approximation with the Hodges–Lehmann efficiency 0.955; at least 6 pairs.
pub fn pairs_for(sigma: f64, dist: f64, confidence: f64, power: f64) -> f64 {
    if dist <= 0.0 || !sigma.is_finite() {
        return f64::INFINITY;
    }
    let z = normal_quantile(1.0 - (1.0 - confidence) / 2.0) + normal_quantile(power);
    ((z * sigma / dist).powi(2) / 0.955).max(min_pairs(0.95) as f64)
}

/// The smallest distance from a boundary that `n` pairs clear with `power` (the minimum detectable effect).
pub fn detectable(sigma: f64, n: usize, confidence: f64, power: f64) -> f64 {
    let z = normal_quantile(1.0 - (1.0 - confidence) / 2.0) + normal_quantile(power);
    z * sigma / (0.955 * n as f64).sqrt()
}

/// Compares paired block values of metric `name` (`def` is its definition, or its family's).
/// `pairs` holds (base, head) per pair.
pub fn compare(name: &str, def: &MetricDef, pairs: &[(f64, f64)], bound: f64, confidence: f64) -> Comparison {
    let (finite, d, relative) = diffs(def, pairs);
    let base = crate::metrics::median(&finite.iter().map(|p| p.0).collect::<Vec<_>>());
    let head = crate::metrics::median(&finite.iter().map(|p| p.1).collect::<Vec<_>>());
    // An exact counter that is stable on both sides (every block the same, from 2 pairs) has no noise
    // to test: its difference is the answer. One that varies is compared like a timed metric.
    if def.kind == Kind::Exact && finite.len() >= 2 && finite.iter().all(|p| p.0 == finite[0].0 && p.1 == finite[0].1) {
        let diff = head - base;
        let improved = (diff < 0.0) == (def.better == Better::Lower);
        let (kind, verdict) = match (diff == 0.0, improved) {
            (true, _) => (VerdictKind::Same, "same (exact)".to_string()),
            (false, true) => (VerdictKind::Better, format!("{} (exact)", def.words.0)),
            (false, false) => (VerdictKind::Worse, format!("{} (exact)", def.words.1)),
        };
        let p = if diff == 0.0 { 1.0 } else { 0.0 };
        return Comparison {
            metric: name.to_string(),
            n: finite.len(),
            base,
            head,
            estimate: diff,
            lo: diff,
            hi: diff,
            relative: false,
            confidence: 1.0,
            p,
            bound: 0.0,
            verdict,
            kind,
            decided: true,
            needed_pairs: None,
            exact: true,
            noworse: None,
            refusal: None,
        };
    }
    let (est, lo, hi, achieved) = hodges_lehmann(&d, confidence);
    let p = wilcoxon_p(&d);
    let (est, lo, hi) = if relative { (est.exp() - 1.0, lo.exp() - 1.0, hi.exp() - 1.0) } else { (est, lo, hi) };
    let bound_abs = if relative { bound } else { (bound * base.abs()).max(0.5) };
    let good_low = def.better == Better::Lower;
    let n = d.len();
    // Below 6 pairs no signed-rank interval reaches 95%: its [min, max] covers only 1 − 2^(1−n).
    let covered = achieved + 1e-9 >= confidence;
    let (kind, verdict) = if n < 2 || !est.is_finite() || !covered {
        (VerdictKind::Unclear, "unclear".to_string())
    } else if hi < 0.0 || lo > 0.0 {
        let improved = (hi < 0.0) == good_low;
        let (kind, word) = if improved { (VerdictKind::Better, def.words.0) } else { (VerdictKind::Worse, def.words.1) };
        let small = if hi < 0.0 { hi > -bound_abs } else { lo < bound_abs };
        (kind, if small { format!("{word} (small)") } else { word.to_string() })
    } else if lo > -bound_abs && hi < bound_abs {
        (VerdictKind::Same, format!("same (±{})", fmt_bound(bound, relative, bound_abs)))
    } else {
        (VerdictKind::Unclear, "unclear".to_string())
    };
    let decided = kind.decided();
    let needed_pairs = (!decided && n >= 2).then(|| {
        let hw = (hi - lo) / 2.0;
        let dist_diff = est.abs();
        let dist_same = bound_abs - est.abs();
        let dist = dist_diff.max(dist_same);
        let estimate = if dist <= 0.0 || hw <= 0.0 { usize::MAX } else { ((n as f64) * (hw / dist).powi(2)).ceil() as usize };
        estimate.max(min_pairs(confidence))
    });
    Comparison { metric: name.to_string(), n, base, head, estimate: est, lo, hi, relative, confidence: achieved, p, bound: bound_abs, verdict, kind, decided, needed_pairs, exact: false, noworse: None, refusal: None }
}

/// Pairs a verdict needs, projected from a robust spread of the differences so far (1.4826·MAD,
/// HL efficiency 0.955): a small-n interval is [min, max] of Walsh averages, so one odd pair would
/// make a width-based projection give up early. `d` and `bound` are in the comparison's scale
/// (log ratios for ratio metrics).
pub fn pairs_needed(d: &[f64], bound: f64, confidence: f64) -> usize {
    let v: Vec<f64> = d.iter().copied().filter(|x| x.is_finite()).collect();
    if v.len() < 2 {
        return usize::MAX;
    }
    let med = crate::metrics::median(&v);
    let sigma = 1.4826 * crate::metrics::median(&v.iter().map(|x| (x - med).abs()).collect::<Vec<_>>());
    let dist = med.abs().max(bound - med.abs());
    if dist <= 0.0 {
        return usize::MAX;
    }
    let z = normal_quantile(1.0 - (1.0 - confidence) / 2.0);
    (((z * sigma / dist).powi(2) / 0.955).ceil() as usize).max(min_pairs(confidence))
}

/// The fewest pairs whose signed-rank interval can reach `confidence` (6 at 95%).
pub fn min_pairs(confidence: f64) -> usize {
    (1..=50).find(|&n| 1.0 - 2f64.powi(1 - n as i32) >= confidence).unwrap_or(50)
}

fn fmt_bound(bound: f64, relative: bool, abs: f64) -> String {
    if relative { format!("{}%", (bound * 100.0).round()) } else { format!("{abs:.1}") }
}
