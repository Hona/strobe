//! Paired statistics (SPEC §9.3): Wilcoxon signed-rank, Hodges–Lehmann, Holm, verdicts, mValue.

use crate::model::{Better, MetricDef};
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

fn normal_quantile(p: f64) -> f64 {
    // Acklam's approximation
    let a = [-39.69683028665376, 220.9460984245205, -275.9285104469687, 138.3577518672690, -30.66479806614716, 2.506628277459239];
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
    pub verdict: String,
    pub decided: bool,
    pub needed_pairs: Option<usize>,
}

/// Compares paired block values. `pairs` holds (base, head) per pair.
pub fn compare(def: &MetricDef, pairs: &[(f64, f64)], bound: f64, confidence: f64) -> Comparison {
    let finite: Vec<(f64, f64)> = pairs.iter().copied().filter(|(a, b)| a.is_finite() && b.is_finite()).collect();
    let relative = def.unit != "count" && finite.iter().all(|(a, b)| *a > 0.0 && *b > 0.0);
    let d: Vec<f64> = finite.iter().map(|(a, b)| if relative { (b / a).ln() } else { b - a }).collect();
    let base = crate::metrics::median(&finite.iter().map(|p| p.0).collect::<Vec<_>>());
    let head = crate::metrics::median(&finite.iter().map(|p| p.1).collect::<Vec<_>>());
    let (est, lo, hi, achieved) = hodges_lehmann(&d, confidence);
    let p = wilcoxon_p(&d);
    let (est, lo, hi) = if relative { (est.exp() - 1.0, lo.exp() - 1.0, hi.exp() - 1.0) } else { (est, lo, hi) };
    let bound_abs = if relative { bound } else { (bound * base.abs()).max(0.5) };
    let good_low = def.better == Better::Lower;
    let n = d.len();
    // Below 6 pairs no signed-rank interval reaches 95%: its [min, max] covers only 1 − 2^(1−n).
    let covered = achieved + 1e-9 >= confidence;
    let (verdict, decided) = if n < 2 || !est.is_finite() || !covered {
        ("unclear".to_string(), false)
    } else if hi < 0.0 || lo > 0.0 {
        let improved = (hi < 0.0) == good_low;
        let word = if improved { def.words.0 } else { def.words.1 };
        let small = if hi < 0.0 { hi > -bound_abs } else { lo < bound_abs };
        (if small { format!("{word} (small)") } else { word.to_string() }, true)
    } else if lo > -bound_abs && hi < bound_abs {
        (format!("same (±{})", fmt_bound(bound, relative, bound_abs)), true)
    } else {
        ("unclear".to_string(), false)
    };
    let needed_pairs = (!decided && n >= 2).then(|| {
        let hw = (hi - lo) / 2.0;
        let dist_diff = est.abs();
        let dist_same = bound_abs - est.abs();
        let dist = dist_diff.max(dist_same);
        let estimate = if dist <= 0.0 || hw <= 0.0 { usize::MAX } else { ((n as f64) * (hw / dist).powi(2)).ceil() as usize };
        estimate.max(min_pairs(confidence))
    });
    Comparison { metric: def.name.to_string(), n, base, head, estimate: est, lo, hi, relative, confidence: achieved, p, bound: bound_abs, verdict, decided, needed_pairs }
}

/// The fewest pairs whose signed-rank interval can reach `confidence` (6 at 95%).
pub fn min_pairs(confidence: f64) -> usize {
    (1..=50).find(|&n| 1.0 - 2f64.powi(1 - n as i32) >= confidence).unwrap_or(50)
}

fn fmt_bound(bound: f64, relative: bool, abs: f64) -> String {
    if relative { format!("{}%", (bound * 100.0).round()) } else { format!("{abs:.1}") }
}
