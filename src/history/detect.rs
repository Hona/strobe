//! Change points in a tracked series (SPEC §25.3): E-divisive with means (Matteson and James 2014)
//! as Hunter/Apache Otava runs it on nightly performance results, with a deterministic significance
//! step instead of permutations.
//!
//! Each point is one commit: the median of its blocks, in log space for ratio metrics. Binary
//! segmentation splits a window at the point that maximises the energy statistic Q̂ (a difference of
//! mean pairwise distances between and within the two sides, so it needs no distribution), and keeps
//! the split only if all three hold:
//! - a Mann–Whitney rank test on the blocks of the two sides gives p < `alpha` (ranks: one odd block
//!   cannot carry it);
//! - the shift of the medians is at least the equivalence bound (a move strobe would call `same` is no
//!   change to chase);
//! - the shift is at least `k_sigma` times the point-to-point spread around each side's median
//!   (1.4826·MAD). Blocks of one commit share that commit's launch and machine state, so the rank test
//!   alone would read one noisy day as a change; this keeps it to moves larger than days differ.
//!
//! Accepted splits are tested again inside their final neighbours until none drops (Otava's pruning).
//! Suspected changes are not verdicts: each one is confirmed by a paired A/B of the commits around it.

pub struct Point {
    /// The point's value in the comparison's scale (log of the median for ratio metrics).
    pub x: f64,
    /// Its blocks, in the same scale.
    pub blocks: Vec<f64>,
}

pub struct Params {
    pub min_segment: usize,
    pub alpha: f64,
    /// The equivalence bound in the comparison's scale.
    pub bound: f64,
    pub k_sigma: f64,
}

#[derive(Clone, Debug)]
pub struct Change {
    /// The first point after the change.
    pub at: usize,
    /// After minus before (medians of the points), in the comparison's scale.
    pub shift: f64,
    pub p: f64,
    pub before: f64,
    pub after: f64,
    pub spread: f64,
}

pub fn changes(points: &[Point], p: &Params) -> Vec<Change> {
    let mut cps = Vec::new();
    split(points, 0, points.len(), p, &mut cps);
    cps.sort_unstable();
    // Prune: each split must still hold between its neighbours.
    loop {
        let mut drop = None;
        for i in 0..cps.len() {
            let a = if i == 0 { 0 } else { cps[i - 1] };
            let b = if i + 1 == cps.len() { points.len() } else { cps[i + 1] };
            if test(points, a, cps[i], b, p).is_none() {
                drop = Some(i);
                break;
            }
        }
        match drop {
            Some(i) => {
                cps.remove(i);
            }
            None => break,
        }
    }
    (0..cps.len())
        .filter_map(|i| {
            let a = if i == 0 { 0 } else { cps[i - 1] };
            let b = if i + 1 == cps.len() { points.len() } else { cps[i + 1] };
            test(points, a, cps[i], b, p)
        })
        .collect()
}

fn split(points: &[Point], a: usize, b: usize, p: &Params, out: &mut Vec<usize>) {
    if b - a < 2 * p.min_segment {
        return;
    }
    let Some(tau) = best_split(&points[a..b], p.min_segment).map(|t| a + t) else { return };
    if test(points, a, tau, b, p).is_some() {
        out.push(tau);
        split(points, a, tau, p, out);
        split(points, tau, b, p, out);
    }
}

/// The split of `xs` (index of the first point of the right side) with the largest Q̂, α = 1.
fn best_split(xs: &[Point], min: usize) -> Option<usize> {
    let n = xs.len();
    // Prefix sums of the pairwise distance matrix: any block of it sums in O(1).
    let mut s = vec![0.0f64; (n + 1) * (n + 1)];
    for i in 0..n {
        for j in 0..n {
            let d = (xs[i].x - xs[j].x).abs();
            s[(i + 1) * (n + 1) + j + 1] = d + s[i * (n + 1) + j + 1] + s[(i + 1) * (n + 1) + j] - s[i * (n + 1) + j];
        }
    }
    let rect = |r0: usize, r1: usize, c0: usize, c1: usize| s[r1 * (n + 1) + c1] - s[r0 * (n + 1) + c1] - s[r1 * (n + 1) + c0] + s[r0 * (n + 1) + c0];
    let mut best: Option<(usize, f64)> = None;
    for t in min..=n - min {
        let (l, r) = (t as f64, (n - t) as f64);
        let between = 2.0 * rect(0, t, t, n) / (l * r);
        // Ordered pairs within a side sum each distance twice: / (l(l-1)) is the mean over unordered pairs.
        let within_l = if t > 1 { rect(0, t, 0, t) / (l * (l - 1.0)) } else { 0.0 };
        let within_r = if n - t > 1 { rect(t, n, t, n) / (r * (r - 1.0)) } else { 0.0 };
        let q = l * r / (l + r) * (between - within_l - within_r);
        if best.is_none_or(|(_, bq)| q > bq) {
            best = Some((t, q));
        }
    }
    best.filter(|(_, q)| *q > 0.0).map(|(t, _)| t)
}

fn test(points: &[Point], a: usize, tau: usize, b: usize, p: &Params) -> Option<Change> {
    if tau - a < p.min_segment || b - tau < p.min_segment {
        return None;
    }
    let left: Vec<f64> = points[a..tau].iter().map(|x| x.x).collect();
    let right: Vec<f64> = points[tau..b].iter().map(|x| x.x).collect();
    let (before, after) = (median(&left), median(&right));
    let shift = after - before;
    let resid: Vec<f64> = left.iter().map(|x| x - before).chain(right.iter().map(|x| x - after)).collect();
    let spread = 1.4826 * median(&resid.iter().map(|r| r.abs()).collect::<Vec<_>>());
    let lb: Vec<f64> = points[a..tau].iter().flat_map(|x| x.blocks.iter().copied()).collect();
    let rb: Vec<f64> = points[tau..b].iter().flat_map(|x| x.blocks.iter().copied()).collect();
    let pv = mann_whitney(&lb, &rb);
    (pv < p.alpha && shift.abs() >= p.bound && shift.abs() >= p.k_sigma * spread).then_some(Change { at: tau, shift, p: pv, before, after, spread })
}

pub fn median(v: &[f64]) -> f64 {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return f64::NAN;
    }
    s.sort_by(|a, b| a.total_cmp(b));
    let m = s.len();
    if m % 2 == 1 { s[m / 2] } else { (s[m / 2 - 1] + s[m / 2]) / 2.0 }
}

/// Two-sided Mann–Whitney U test, normal approximation with tie correction.
pub fn mann_whitney(a: &[f64], b: &[f64]) -> f64 {
    let (n1, n2) = (a.len(), b.len());
    if n1 == 0 || n2 == 0 {
        return 1.0;
    }
    let mut all: Vec<(f64, bool)> = a.iter().map(|x| (*x, true)).chain(b.iter().map(|x| (*x, false))).collect();
    all.sort_by(|x, y| x.0.total_cmp(&y.0));
    let n = all.len();
    let (mut r1, mut ties) = (0.0, 0.0);
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && all[j + 1].0 == all[i].0 {
            j += 1;
        }
        let rank = (i + j) as f64 / 2.0 + 1.0;
        let t = (j - i + 1) as f64;
        ties += t * t * t - t;
        r1 += all[i..=j].iter().filter(|x| x.1).count() as f64 * rank;
        i = j + 1;
    }
    let (f1, f2, nf) = (n1 as f64, n2 as f64, n as f64);
    let u = r1 - f1 * (f1 + 1.0) / 2.0;
    let var = f1 * f2 / 12.0 * ((nf + 1.0) - ties / (nf * (nf - 1.0)));
    if var <= 0.0 {
        return 1.0;
    }
    let z = ((u - f1 * f2 / 2.0).abs() - 0.5).max(0.0) / var.sqrt();
    (2.0 * (1.0 - normal_cdf(z))).min(1.0)
}

fn normal_cdf(z: f64) -> f64 {
    // Abramowitz–Stegun 7.1.26
    let x = z / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    0.5 * (1.0 + if x >= 0.0 { y } else { -y })
}
