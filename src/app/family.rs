//! An implementation comparison's journeys as one family (SPEC §9.3, §9.10): each journey keeps its
//! own verdict; the scoreboard ("better in 4 of 5 compared journeys") counts a win or a regression
//! only when its repeated p clears Holm across the compared journeys, and makes the no-worse claim
//! over all of them by intersection–union. Read from a result as stored, so the bench that writes it
//! and every report that draws it say the same.

use crate::model::VerdictKind;
use crate::stats::{self, Family, Journey};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Whether a result compares two implementations (two targets) rather than two builds of one.
pub fn implementations(r: &Value) -> bool {
    r["comparison"] == "implementations"
}

/// A series one implementation could not run: its parity gap (`kind: "gap"` with `series[].gap`, or a
/// primary refused `ParityGap`).
pub fn gap(s: &Value) -> bool {
    s["kind"] == "gap" || s["gap"].is_object() || s["primary"]["refusal"]["validator"] == "ParityGap"
}

/// Why a gap series was not compared, in a few words: `no @find in <name>` when the side lacked a
/// named step, else `<name> cannot run it: <reason>`.
pub fn gap_reason(r: &Value, s: &Value) -> String {
    let g = &s["gap"];
    let (b, h) = crate::app::pr::side_names(r);
    let side = g["name"].as_str().map(str::to_string).unwrap_or_else(|| if g["side"] == "base" { b } else { h });
    match (g["missing"].as_str(), g["reason"].as_str().filter(|x| !x.is_empty())) {
        (Some(missing), _) => format!("no {missing} in {side}"),
        (None, Some(why)) => format!("{side} cannot run it: {why}"),
        (None, None) => format!("{side} cannot run it"),
    }
}

/// The planned looks of a result's primaries: `plan.looks`, else the default five.
fn looks(r: &Value) -> Vec<usize> {
    let l: Vec<usize> = r["plan"]["looks"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|n| n as usize)).collect();
    if l.is_empty() { vec![8, 12, 18, 26, 40] } else { l }
}

/// A series' primary differences per pair in its comparison's scale (log ratios when it compared
/// ratios, else differences), from its timed blocks.
pub fn primary_diffs(s: &Value) -> Vec<f64> {
    let m = s["primary"]["metric"].as_str().unwrap_or("");
    let relative = s["primary"]["relative"] == true;
    let mut by: BTreeMap<u64, (f64, f64)> = BTreeMap::new();
    for b in s["blocks"].as_array().into_iter().flatten() {
        let Some(p) = b["pair"].as_u64().filter(|p| *p < 1_000_000) else { continue };
        let v = b["values"][m].as_f64().unwrap_or(f64::NAN);
        let e = by.entry(p).or_insert((f64::NAN, f64::NAN));
        if b["variant"] == "base" {
            e.0 = v;
        } else if b["variant"] == "head" {
            e.1 = v;
        }
    }
    by.into_values().filter(|(a, b)| a.is_finite() && b.is_finite() && (!relative || (*a > 0.0 && *b > 0.0))).map(|(a, b)| if relative { (b / a).ln() } else { b - a }).collect()
}

/// A series' repeated p: as the bench stored it, else from its blocks and the plan's looks. A stable
/// counter compared exactly has no noise: decided, its p is 0.
pub fn p_repeated(r: &Value, s: &Value) -> f64 {
    let c = &s["primary"];
    if let Some(p) = c["p_repeated"].as_f64() {
        return p;
    }
    if c["exact"] == true {
        return if stats::kind_of(c).decided() { 0.0 } else { 1.0 };
    }
    stats::repeated_p(&primary_diffs(s), &looks(r))
}

/// How far a comparison moved, as a log ratio's size: `|ln(head / base)|` (a 5× change and an 80%
/// cut weigh by what they are, not by their percentages).
pub fn magnitude(c: &Value) -> f64 {
    let (est, b, h) = (c["estimate"].as_f64().unwrap_or(f64::NAN), c["base"].as_f64().unwrap_or(f64::NAN), c["head"].as_f64().unwrap_or(f64::NAN));
    let m = if c["relative"] == true {
        (1.0 + est).max(1e-9).ln().abs()
    } else if b > 0.0 && h > 0.0 {
        (h / b).ln().abs()
    } else {
        (est / b.abs().max(1e-9)).abs().ln_1p()
    };
    if m.is_finite() { m } else { 0.0 }
}

/// A result's journeys read once as a family: the counts, each journey's repeated p and verdict, and
/// the headline.
pub struct Board {
    pub family: Family,
    /// Per series, in order: its repeated p (NaN for a gap or a series that did not run).
    pub p: Vec<f64>,
    kinds: Vec<VerdictKind>,
    /// The headline journey: the case the bench file marks `hero: true` when its win is confirmed
    /// across the family, else the biggest confirmed win (largest `magnitude` of its primary; the
    /// first in the file on a tie). None without one. Every other journey stays on the forest plot
    /// and the proof lines, and every regression in the caution, so the headline never hides one.
    pub hero: Option<usize>,
    /// Without a headline: the biggest confirmed regression of a main metric, which leads instead.
    pub worst: Option<usize>,
}

impl Board {
    pub fn of(r: &Value) -> Board {
        let series: Vec<&Value> = r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        let kinds: Vec<VerdictKind> = series.iter().map(|s| if s["not_run"] == true { VerdictKind::Stopped } else { stats::kind_of(&s["primary"]) }).collect();
        let p: Vec<f64> = series.iter().map(|s| if gap(s) || s["not_run"] == true { f64::NAN } else { p_repeated(r, s) }).collect();
        let js: Vec<Journey> = series.iter().enumerate().map(|(i, s)| Journey { kind: kinds[i], p: if p[i].is_finite() { p[i] } else { 1.0 }, gap: gap(s) }).collect();
        // Non-inferiority: every compared journey's primary was tested against a margin.
        let noworse = series.iter().any(|s| !gap(s)) && series.iter().filter(|s| !gap(s)).all(|s| s["primary"]["noworse"].is_number());
        let family = stats::family(&js, noworse);
        let biggest = |of: VerdictKind| {
            (0..series.len()).filter(|&i| family.holm[i] && kinds[i] == of).fold(None, |best: Option<usize>, i| match best {
                Some(b) if magnitude(&series[b]["primary"]) >= magnitude(&series[i]["primary"]) => Some(b),
                _ => Some(i),
            })
        };
        let hero = (0..series.len()).find(|&i| family.holm[i] && kinds[i] == VerdictKind::Better && series[i]["hero"] == true).or_else(|| biggest(VerdictKind::Better));
        let worst = if hero.is_none() { biggest(VerdictKind::Worse) } else { None };
        Board { family, p, kinds, hero, worst }
    }

    /// `Better in 4 of 5` (or the no-worse claim), then every other count that is not zero, so the
    /// counts always add up to the compared journeys: `worse in 1`, `the same in 1`, `unclear in 2`.
    pub fn counts(&self) -> (String, Vec<String>) {
        let f = &self.family;
        let head = match f.all_no_worse {
            Some(true) => format!("No worse in all {}", f.compared),
            Some(false) => format!("Not no worse in all {}", f.compared),
            None => format!("Better in {} of {}", f.better, f.compared),
        };
        let mut rest = Vec::new();
        if f.all_no_worse.is_some() && f.better > 0 {
            rest.push(format!("better in {}", f.better));
        }
        if f.worse > 0 {
            rest.push(format!("worse in {}", f.worse));
        }
        if f.same > 0 && f.all_no_worse != Some(true) {
            rest.push(format!("{} in {}", if f.all_no_worse.is_some() { "no worse" } else { "the same" }, f.same));
        }
        if f.unclear > 0 {
            rest.push(format!("unclear in {}", f.unclear));
        }
        (head, rest)
    }

    /// The family's correction in words, for two or more compared journeys: `Holm-adjusted across 5
    /// journeys`, and the verdicts it did not keep.
    pub fn correction(&self) -> String {
        let f = &self.family;
        if f.compared < 2 {
            return String::new();
        }
        if f.all_no_worse.is_some() {
            return "each journey at its own 2.5%: all of them no worse needs no correction (intersection–union)".into();
        }
        let mut t = format!("Holm-adjusted across {} journeys", f.compared);
        let alone: Vec<String> = [(f.better_alone, "better"), (f.worse_alone, "worse")].iter().filter(|(n, _)| *n > 0).map(|(n, w)| format!("{n} {w}")).collect();
        if !alone.is_empty() {
            t += &format!("; {} on {} own did not clear it", alone.join(" and "), if f.better_alone + f.worse_alone == 1 { "its" } else { "their" });
        }
        t
    }

    /// Series `i` decided better, confirmed across the family.
    pub fn win(&self, i: usize) -> bool {
        self.family.holm.get(i).copied().unwrap_or(false) && self.kinds.get(i) == Some(&VerdictKind::Better)
    }

    /// Series `i` decided worse, confirmed across the family.
    pub fn regression(&self, i: usize) -> bool {
        self.family.holm.get(i).copied().unwrap_or(false) && self.kinds.get(i) == Some(&VerdictKind::Worse)
    }

    /// Series `i` decided better or worse on its own but not across the family.
    pub fn alone(&self, i: usize) -> bool {
        !self.family.holm.get(i).copied().unwrap_or(false) && matches!(self.kinds.get(i), Some(VerdictKind::Better | VerdictKind::Worse))
    }

    /// Writes the family into a result as result.json keeps it: `scoreboard`, and on each series'
    /// primary `p_repeated` and `holm_decided` (its better or worse verdict confirmed across the
    /// family).
    pub fn write(&self, r: &mut Value) {
        r["scoreboard"] = json!(self.family);
        for (i, s) in r["series"].as_array_mut().into_iter().flatten().enumerate() {
            if !s["primary"].is_object() {
                continue;
            }
            s["primary"]["holm_decided"] = json!(self.family.holm.get(i).copied().unwrap_or(false));
            if let Some(p) = self.p.get(i).filter(|p| p.is_finite()) {
                s["primary"]["p_repeated"] = json!(p);
            }
        }
    }
}
