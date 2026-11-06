//! A port's tracked history (SPEC §25.7): the series of one bench file's two targets (`track --base
//! <target> --head <target>=git:<ref>`) as journeys × the head's commits. Each cell is a point,
//! `ln(head / base)` of a few alternating pairs, read in words only when its whole interval lies past
//! the equivalence bound; the latest cells make the scoreboard the card and badge show.

use crate::app::fmt;
use crate::cli::Args;
use crate::history::Series;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    /// The whole interval past the bound, the way the metric is better.
    Better,
    Worse,
    /// The whole interval inside the bound.
    Same,
    /// The interval crosses the bound: no word.
    Unclear,
    /// The journey was a parity gap at this commit.
    Gap,
    /// A point without a value (refused, too few pairs): its reason.
    NoValue,
}

#[derive(Clone, Debug)]
pub(crate) struct Cell {
    pub state: State,
    /// `ln(head / base)`: the median of the pairs and its interval.
    pub est: f64,
    pub lo: f64,
    pub hi: f64,
    pub why: String,
}

pub(crate) struct Journey {
    pub series: String,
    pub label: String,
    pub metric: String,
    /// The metric's unit, for a confirmation that compared differences.
    pub unit: String,
    /// The metric's words for its two directions (`less`/`more`, `faster`/`slower`).
    pub words: (String, String),
    /// Higher is better for its metric.
    pub higher: bool,
    /// One per commit of the port, None where the journey was not tracked.
    pub cells: Vec<Option<Cell>>,
    pub changes: Vec<Value>,
}

pub(crate) struct Port {
    pub file: String,
    pub base: String,
    pub head: String,
    pub device: String,
    /// The equivalence bound (a fraction).
    pub bound: f64,
    /// Pairs per point (the most common).
    pub pairs: usize,
    /// The interval's confidence those pairs reach (3 pairs: 75%): the median over the points.
    pub confidence: f64,
    /// Commits in commit order: `{commit, short, subject, date}`.
    pub commits: Vec<Value>,
    pub journeys: Vec<Journey>,
    /// Comparator updates, one per commit where the base's identity changed.
    pub epochs: Vec<Value>,
    /// Journeys that went from a gap to compared, or back.
    pub parity: Vec<Value>,
    /// Series of these journeys on an earlier fingerprint (another OS build, power plan, …): not drawn.
    pub hidden: usize,
    pub fingerprint: Value,
}

/// The ports among `series`: one per bench file and pair of targets.
pub(crate) fn ports(series: &[Series], args: &Args) -> Vec<Port> {
    let mut groups: BTreeMap<(String, String, String), Vec<&Series>> = BTreeMap::new();
    for s in series.iter().filter(|s| s.implementations()) {
        let key = |k: &str| s.meta[k].as_str().unwrap_or("").to_string();
        groups.entry((key("abs_file"), key("base_target"), key("head_target"))).or_default().push(s);
    }
    groups.into_values().map(|g| Port::of(&g, args)).collect()
}

fn last_at(s: &Series) -> Option<&str> {
    s.points.iter().filter_map(|p| p["at"].as_str()).max()
}

impl Port {
    fn of(group: &[&Series], args: &Args) -> Port {
        // One row per journey: the series with the newest point (a journey on an older fingerprint is
        // another series, kept and listed, never mixed into this one).
        let mut by_label: BTreeMap<String, &Series> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut sorted: Vec<&&Series> = group.iter().collect();
        sorted.sort_by(|a, b| a.meta["created"].as_str().cmp(&b.meta["created"].as_str()));
        for s in sorted {
            let label = s.meta["label"].as_str().unwrap_or("").to_string();
            if !order.contains(&label) {
                order.push(label.clone());
            }
            let newer = by_label.get(&label).is_none_or(|k| last_at(s) > last_at(k));
            if newer {
                by_label.insert(label, s);
            }
        }
        let rows: Vec<&Series> = order.iter().filter_map(|l| by_label.get(l).copied()).collect();
        let hidden = group.len() - rows.len();
        // The commits, in commit order (depth, committer time).
        let mut commits: Vec<(u64, u64, String, Value)> = Vec::new();
        for s in &rows {
            for p in &s.points {
                let c = p["commit"].as_str().unwrap_or("").to_string();
                if commits.iter().any(|x| x.2 == c) {
                    continue;
                }
                let date = p["commit_time"].as_u64().filter(|t| *t > 0).map(|t| crate::app::record::iso(t)[..10].to_string()).unwrap_or_default();
                commits.push((p["depth"].as_u64().unwrap_or(0), p["commit_time"].as_u64().unwrap_or(0), c.clone(), json!({ "commit": c, "short": p["short"], "subject": p["subject"], "date": date })));
            }
        }
        commits.sort_by_key(|a| (a.0, a.1));
        let first = rows.first();
        let bound = first.map(|s| crate::app::track::bound(args, s)).unwrap_or(0.02);
        let (b_lo, b_hi) = ((1.0 - bound).max(1e-9).ln(), (1.0 + bound).ln());
        let mut ns: BTreeMap<u64, usize> = BTreeMap::new();
        let mut confs: Vec<f64> = Vec::new();
        let journeys: Vec<Journey> = rows
            .iter()
            .map(|s| {
                let higher = s.meta["better"] == "higher";
                let words =
                    s.meta["words"].as_array().map(|w| (w[0].as_str().unwrap_or("better").to_string(), w[1].as_str().unwrap_or("worse").to_string())).unwrap_or(("better".into(), "worse".into()));
                let cells = commits
                    .iter()
                    .map(|(_, _, c, _)| {
                        // The latest point of this commit (tracked again: the last run counts).
                        let p = s.points.iter().filter(|p| p["commit"] == c.as_str()).max_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()))?;
                        let f = |k: &str| p[k].as_f64().unwrap_or(f64::NAN);
                        let (est, lo, hi) = (f("median"), f("lo"), f("hi"));
                        if p["valid"] == true {
                            *ns.entry(p["n"].as_u64().unwrap_or(0)).or_default() += 1;
                            confs.push(p["confidence"].as_f64().unwrap_or(f64::NAN));
                        }
                        let state = if p["parity"] == "gap" {
                            State::Gap
                        } else if p["valid"] != true || !est.is_finite() {
                            State::NoValue
                        } else if lo >= b_lo && hi <= b_hi {
                            State::Same
                        } else if hi < b_lo {
                            if higher { State::Worse } else { State::Better }
                        } else if lo > b_hi {
                            if higher { State::Better } else { State::Worse }
                        } else {
                            State::Unclear
                        };
                        Some(Cell { state, est, lo, hi, why: p["why"].as_str().unwrap_or("").to_string() })
                    })
                    .collect();
                Journey {
                    series: s.id.clone(),
                    label: s.meta["label"].as_str().unwrap_or("").to_string(),
                    metric: s.meta["metric"].as_str().unwrap_or("").to_string(),
                    unit: s.meta["metric_unit"].as_str().unwrap_or("").to_string(),
                    words,
                    higher,
                    cells,
                    // Changes only: a comparator update is a mark, never a change (older files held some).
                    changes: s.changes.iter().filter(|c| c["state"] != "epoch").cloned().collect(),
                }
            })
            .collect();
        let pairs = ns.iter().max_by_key(|(_, k)| **k).map(|(n, _)| *n as usize).unwrap_or(0);
        let confidence = crate::history::detect::median(&confs);
        let mut epochs: Vec<Value> = Vec::new();
        let mut parity: Vec<Value> = Vec::new();
        for s in &rows {
            for m in &s.marks {
                if m["t"] == "epoch" && !epochs.iter().any(|e| e["commit"] == m["commit"] && e["to_identity"] == m["to_identity"]) {
                    epochs.push(m.clone());
                } else if m["t"] == "parity" {
                    parity.push(m.clone());
                }
            }
        }
        let meta = first.map(|s| s.meta.clone()).unwrap_or_default();
        Port {
            file: meta["file"].as_str().unwrap_or("").to_string(),
            base: meta["base_name"].as_str().unwrap_or("base").to_string(),
            head: meta["head_name"].as_str().unwrap_or("head").to_string(),
            device: meta["device"].as_str().unwrap_or("").to_string(),
            bound,
            pairs,
            confidence,
            commits: commits.into_iter().map(|c| c.3).collect(),
            journeys,
            epochs,
            parity,
            hidden,
            fingerprint: meta["fingerprint"].clone(),
        }
    }

    /// The commit index each epoch took effect at.
    pub fn epoch_at(&self) -> Vec<(usize, &Value)> {
        self.epochs.iter().filter_map(|e| self.commits.iter().position(|c| c["commit"] == e["commit"]).map(|i| (i, e))).collect()
    }
}

/// `-62%`, from a log ratio.
pub(crate) fn pct(ln: f64) -> String {
    fmt::pct(ln.exp() - 1.0)
}

impl Cell {
    /// The cell as the matrix shows it: `less -62%`, `more +18%`, `same`, `unclear -3%`, `gap`.
    pub fn text(&self, j: &Journey) -> String {
        match self.state {
            State::Better => format!("{} {}", j.words.0, pct(self.est)),
            State::Worse => format!("{} {}", j.words.1, pct(self.est)),
            State::Same => "same".into(),
            State::Unclear => format!("unclear {}", pct(self.est)),
            State::Gap => "gap".into(),
            State::NoValue => "no value".into(),
        }
    }
}

/// The journeys at the newest tracked commit: each one's point at that commit, never an older one (a
/// journey without a point there, or without a value, is counted as such).
pub(crate) struct Board {
    pub journeys: usize,
    pub compared: usize,
    pub better: usize,
    pub worse: usize,
    pub same: usize,
    pub unclear: usize,
    pub gaps: usize,
    /// Journeys with no point at that commit, or a point without a value: a partial run.
    pub missing: usize,
    /// The newest commit any journey has a point at.
    pub at: Option<Value>,
}

impl Board {
    pub fn latest(p: &Port) -> Board {
        let mut b = Board { journeys: p.journeys.len(), compared: 0, better: 0, worse: 0, same: 0, unclear: 0, gaps: 0, missing: 0, at: None };
        let Some(i) = (0..p.commits.len()).rev().find(|&i| p.journeys.iter().any(|j| j.cells[i].is_some())) else { return b };
        b.at = Some(p.commits[i].clone());
        for j in &p.journeys {
            let Some(c) = &j.cells[i] else {
                b.missing += 1;
                continue;
            };
            match c.state {
                State::Gap => b.gaps += 1,
                State::NoValue => b.missing += 1,
                s => {
                    b.compared += 1;
                    match s {
                        State::Better => b.better += 1,
                        State::Worse => b.worse += 1,
                        State::Same => b.same += 1,
                        _ => b.unclear += 1,
                    }
                }
            }
        }
        b
    }

    /// The counts after the headline: `worse in 1`, `the same in 1`, `unclear in 2`, `1 gap`, `1 not
    /// tracked there`; every one that is not zero, so they add up to the journeys.
    pub fn rest(&self) -> Vec<String> {
        let mut parts = Vec::new();
        for (n, w) in [(self.worse, "worse in"), (self.same, "the same in"), (self.unclear, "unclear in")] {
            if n > 0 {
                parts.push(format!("{w} {n}"));
            }
        }
        if self.gaps > 0 {
            parts.push(format!("{} gap{}", self.gaps, if self.gaps == 1 { "" } else { "s" }));
        }
        if self.missing > 0 {
            parts.push(format!("{} without a value there", self.missing));
        }
        parts
    }

    /// `better in 4 of 5 · worse in 1 · 1 gap`.
    pub fn words(&self) -> String {
        let mut parts = vec![format!("better in {} of {}", self.better, self.compared)];
        parts.extend(self.rest());
        fmt::fields(parts)
    }

    pub fn json(&self) -> Value {
        json!({ "journeys": self.journeys, "compared": self.compared, "better": self.better, "worse": self.worse, "same": self.same, "unclear": self.unclear, "gaps": self.gaps, "missing": self.missing, "at": self.at })
    }
}

/// `3 pairs, 75%`: what one point is, said wherever a point's interval is.
pub(crate) fn tracked(p: &Port) -> String {
    format!("{} pairs, {}", p.pairs, confidence_text(p.confidence))
}

pub(crate) fn confidence_text(c: f64) -> String {
    if c.is_finite() { format!("{:.0}%", c * 100.0) } else { "?".into() }
}

/// How the cells read, under the matrix and on the card.
pub(crate) fn method(p: &Port) -> String {
    format!(
        "each point: ln({} / {}) of {} alternating pairs at one {} commit, median and its {} interval; a word only when that whole interval lies past ±{}; tracked points are evidence, a change is confirmed by an A/B of {} alone",
        p.head,
        p.base,
        p.pairs,
        p.head,
        confidence_text(p.confidence),
        bound_text(p.bound),
        p.head
    )
}

fn bound_text(b: f64) -> String {
    let p = b * 100.0;
    if (p - p.round()).abs() < 1e-9 { format!("{p:.0}%") } else { format!("{p:.1}%") }
}

/// The matrix as text: a row per commit, a column per journey, comparator updates between rows, then
/// the scoreboard, the changes and the marks.
pub(crate) fn text(p: &Port, top: Option<usize>, change_text: impl Fn(&str, &Value) -> String) -> String {
    let metrics: Vec<&str> = p.journeys.iter().map(|j| j.metric.as_str()).collect();
    let metric = if metrics.windows(2).all(|w| w[0] == w[1]) { metrics.first().copied().unwrap_or("") } else { "primaries per journey" };
    let mut t = format!("{} vs {} · {} · {} · {metric}\n{}\n", p.head, p.base, p.file, p.device, method(p));
    if p.hidden > 0 {
        t += &format!("({} series of these journeys on an earlier fingerprint are not shown: strobe history lists them)\n", p.hidden);
    }
    t += "\n";
    let shown = top.unwrap_or(40);
    let skip = p.commits.len().saturating_sub(shown);
    if skip > 0 {
        t += &format!("  ({skip} earlier commits; --top N shows more)\n");
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut head = vec!["date".to_string(), "commit".into(), "subject".into()];
    head.extend(p.journeys.iter().map(|j| j.label.clone()));
    rows.push(head);
    let epochs = p.epoch_at();
    for (i, c) in p.commits.iter().enumerate().skip(skip) {
        for (_, e) in epochs.iter().filter(|(k, _)| *k == i) {
            rows.push(vec![format!("  ── {}", crate::app::track::comparator_updated(e))]);
        }
        let commit = c["commit"].as_str().unwrap_or("");
        let mut row = vec![c["date"].as_str().unwrap_or("").to_string(), c["short"].as_str().unwrap_or("").to_string(), c["subject"].as_str().unwrap_or("").chars().take(32).collect()];
        for j in &p.journeys {
            let mut cell = j.cells[i].as_ref().map(|x| x.text(j)).unwrap_or_else(|| "·".into());
            if j.changes.iter().any(|k| k["bad"] == commit && k["state"] != "rejected") {
                cell += " ◆";
            }
            row.push(cell);
        }
        rows.push(row);
    }
    t += &fmt::table(&rows, &[], "  ", 3);
    let board = Board::latest(p);
    if let Some(at) = &board.at {
        t += &format!("\nat {} {} (tracked, {}): {}\n", at["short"].as_str().unwrap_or(""), at["subject"].as_str().unwrap_or(""), tracked(p), board.words());
    }
    let changes: Vec<String> = p.journeys.iter().flat_map(|j| j.changes.iter().map(|c| format!("  {} · {}", j.label, change_text(&j.series, c)))).collect();
    if !changes.is_empty() {
        t += &format!("\nchanges (◆)\n{}\n", changes.join("\n"));
    }
    let mut marks: Vec<String> = p.parity.iter().map(|m| format!("  {}", crate::app::track::mark_line(m))).collect();
    marks.extend(p.epochs.iter().map(|m| format!("  {}", crate::app::track::mark_line(m))));
    if !marks.is_empty() {
        t += &format!("\nmarks\n{}\n", marks.join("\n"));
    }
    t
}

pub(crate) fn json(p: &Port) -> Value {
    let finite = |v: f64| if v.is_finite() { json!(v) } else { Value::Null };
    let cell = |c: &Option<Cell>, j: &Journey| match c {
        None => Value::Null,
        Some(c) => json!({
            "state": format!("{:?}", c.state).to_lowercase(), "text": c.text(j), "ln": finite(c.est), "lo": finite(c.lo), "hi": finite(c.hi),
            "change": finite(c.est.exp() - 1.0), "why": if c.why.is_empty() { Value::Null } else { json!(c.why) },
        }),
    };
    json!({
        "file": p.file, "base": p.base, "head": p.head, "device": p.device, "bound": p.bound, "pairs": p.pairs, "confidence": finite(p.confidence), "method": method(p),
        "commits": p.commits,
        "journeys": p.journeys.iter().map(|j| json!({ "series": j.series, "label": j.label, "metric": j.metric, "cells": j.cells.iter().map(|c| cell(c, j)).collect::<Vec<_>>(), "changes": j.changes })).collect::<Vec<_>>(),
        "epochs": p.epochs, "parity": p.parity, "hidden": p.hidden, "scoreboard": Board::latest(p).json(),
    })
}
