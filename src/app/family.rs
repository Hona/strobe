//! An implementation comparison's journeys as one family (SPEC §9.3, §9.10): each journey keeps its
//! own verdict; the scoreboard ("better in 4 of 5 compared journeys") counts a win only when its
//! repeated p clears Holm across the compared journeys, counts every regression, and makes the
//! no-worse claim over all of them by intersection–union. Read from a result as stored, so the bench
//! that writes it and every report that draws it say the same.

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

/// A series' repeated p: as the bench stored it, else from its blocks and the plan's looks.
pub fn p_repeated(r: &Value, s: &Value) -> f64 {
    s["primary"]["p_repeated"].as_f64().unwrap_or_else(|| stats::repeated_p(&primary_diffs(s), &looks(r)))
}

/// The family of a result's series, in their order.
pub fn of(r: &Value) -> Family {
    let series: Vec<&Value> = r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let js: Vec<Journey> = series
        .iter()
        .map(|s| Journey {
            kind: if s["not_run"] == true { VerdictKind::Stopped } else { stats::kind_of(&s["primary"]) },
            p: if gap(s) || s["not_run"] == true { 1.0 } else { p_repeated(r, s) },
            gap: gap(s),
        })
        .collect();
    // Non-inferiority: every compared journey's primary was tested against a margin.
    let noworse = series.iter().any(|s| !gap(s)) && series.iter().filter(|s| !gap(s)).all(|s| s["primary"]["noworse"].is_number());
    stats::family(&js, noworse)
}

/// How far a comparison moved, as a log ratio's size: `|ln(head / base)|` (a 5× change and a 80%
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

/// The headline journey: the case the bench file marks `hero: true` when its win counts on the
/// scoreboard, else the biggest win that counts (largest `magnitude` of its primary; the first in the
/// file on a tie). None without a counted win. Every other journey stays on the forest plot and the
/// proof lines, and every regression in the caution, so the headline never hides one.
pub fn hero(r: &Value, f: &Family) -> Option<usize> {
    let series = r["series"].as_array()?;
    let counted: Vec<usize> = (0..series.len().min(f.holm.len())).filter(|&i| f.holm[i]).collect();
    if let Some(&i) = counted.iter().find(|&&i| series[i]["hero"] == true) {
        return Some(i);
    }
    counted.into_iter().fold(None, |best: Option<usize>, i| match best {
        Some(b) if magnitude(&series[b]["primary"]) >= magnitude(&series[i]["primary"]) => Some(b),
        _ => Some(i),
    })
}

/// The scoreboard for result.json (`scoreboard`), and per series, in order, whether its win counts
/// after Holm (`holm_decided`) with its repeated p (`p_repeated`).
pub fn scoreboard(r: &Value) -> (Value, Vec<(bool, f64)>) {
    let f = of(r);
    let series: Vec<&Value> = r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let per: Vec<(bool, f64)> = series.iter().zip(&f.holm).map(|(s, h)| (*h, if gap(s) { f64::NAN } else { p_repeated(r, s) })).collect();
    (json!(f), per)
}
