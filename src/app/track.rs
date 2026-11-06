//! `strobe track`: continuous benchmarking (SPEC §25). Each tracked commit gets one point per series
//! (timed blocks of that commit alone, `bench --blocks`), stored in
//! the device's append-only history; change points on that history are only suspicions, and each one
//! is confirmed by a paired A/B of the commits around it and, when confirmed, bisected to the commit
//! that did it. Only confirmed changes alert. Two targets (`--base <target> --head <target>=git:<ref>`)
//! track a port instead: a short implementation comparison per head commit (`implementations`).

mod implementations;

pub(crate) use implementations::{comparator_updated, mark_line};

use super::{Output, fmt, progress};
use crate::cli::Args;
use crate::config;
use crate::history::{self, detect};
use crate::host::{self, git, git::commit_time, git::short, git::subject};
use crate::model::{self, Error, Kind, Result};
use crate::stats;
use crate::store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Bench flags a track passes on to every bench it runs (points, confirmations, bisects).
const PASS: &[&str] = &["filter", "target", "params", "pin", "power", "reserve-cpus", "runs", "primary", "equivalence", "trace", "engine", "headless"];

/// Blocks per point unless `--blocks`: the fewest whose interval reaches 95% (a signed-rank interval
/// of 6 values covers 96.9%).
const BLOCKS: usize = 6;

/// A point with fewer blocks than this (refusals, a stop) carries no value.
const MIN_BLOCKS: usize = 4;

pub fn run(args: &Args) -> Result<Output> {
    if args.has("schedules") {
        return list_schedules();
    }
    if let Some(id) = args.get("unschedule") {
        return unschedule(id);
    }
    if args.has("confirm") && args.pos.first().is_none_or(|p| p.starts_with("ser_") || p.starts_with("chg_")) {
        return confirm_pending(args, args.pos.first().map(String::as_str));
    }
    let file = args.pos.first().ok_or_else(|| Error::usage("track needs a bench file: strobe track <file.bench.ts> --ref <branch>"))?;
    let file = canon(file)?;
    let root = config::project_root(file.parent().unwrap_or(Path::new(".")));
    let sides = implementations::Sides::of(args, &file, &root)?;
    let pairs = match &sides {
        Some(_) => implementations::pairs(args)?,
        None if args.has("pairs") => return Err(Error::usage("--pairs is for tracking two implementations (--base <target> --head <target>=git:<ref>); one target's point is --blocks N")),
        None => 0,
    };
    if let Some(when) = args.get("schedule") {
        return schedule(args, &file, &root, when, sides.as_ref());
    }
    // One run of a schedule at a time: the daemon starts it, and a slow one outlasts the next tick.
    let _lock = match args.get("from-schedule") {
        Some(id) => match lock(id) {
            Some(l) => Some(l),
            None => return Ok(Output::new(format!("{id} is already running\n"), json!({ "schedule": id, "running": true }))),
        },
        None => None,
    };
    // A port's commits are the head's, in the head's repository.
    let (git_root, reference) = match &sides {
        Some(s) => (s.head_root.clone(), s.head_ref.as_str()),
        None => (root.clone(), args.get("ref").unwrap_or("HEAD")),
    };
    let commits = commits(args, &git_root, reference)?;
    let blocks = args.num("blocks")?.map_or(BLOCKS, |b| b as usize).max(MIN_BLOCKS);
    let statics = static_fingerprint();
    let each = match &sides {
        Some(s) => format!("{} vs {} · {pairs} pairs each", s.head_target, s.base_target),
        None => format!("{blocks} blocks each"),
    };
    let mut text = format!("strobe track {} · {} commit{} · {each} · {}\n", rel(&root, &file), commits.len(), if commits.len() == 1 { "" } else { "s" }, host::device_name());
    progress(text.trim_end());
    let mut touched: Vec<String> = Vec::new();
    let mut points = Vec::new();
    let (mut tried, mut failed) = (0, 0);
    for c in &commits {
        let mut key = run_key(args, &file, c, &statics);
        if let Some(s) = &sides {
            key["sides"] = implementations::key_parts(s, pairs);
        }
        if !args.has("again") && history::runs().iter().any(|r| r["key"] == key && r["outcome"] == "ok") {
            text += &format!("  {}  already tracked here (--again tracks it again)\n", short(c));
            continue;
        }
        tried += 1;
        let tracked = match &sides {
            Some(s) => implementations::point(args, s, &file, &root, c, pairs, &statics, &key),
            None => point(args, &file, &root, c, blocks, &statics, &key),
        };
        match tracked {
            Ok((ids, lines, ps)) => {
                text += &lines;
                points.extend(ps);
                for id in ids {
                    if !touched.contains(&id) {
                        touched.push(id);
                    }
                }
            }
            Err(e) if e.code == 130 => return Err(e),
            Err(e) => {
                failed += 1;
                text += &format!("  {}  {}\n", short(c), e.message.lines().next().unwrap_or(""));
            }
        }
    }
    // Every series of this file is looked at again: a confirmation that failed before runs now.
    let mut found = Vec::new();
    let mut marks = Vec::new();
    for s in history::list().into_iter().filter(|s| touched.contains(&s.id) || Path::new(s.meta["abs_file"].as_str().unwrap_or("")) == file) {
        if s.implementations() {
            for m in implementations::mark(&s)? {
                text += &format!("  {}\n", implementations::mark_line(&m));
                marks.push(m);
            }
        }
        for c in detect_series(&s, args)? {
            text += &format!("  {}  {}\n", c["state"].as_str().unwrap_or("suspected"), change_line(&s, &c));
            found.push(c);
        }
    }
    let mut confirmed = Vec::new();
    if !args.has("no-confirm") {
        let (t, c) = confirm_all(args, Some(&file))?;
        text += &t;
        confirmed = c;
    }
    text += &format!("\nstrobe history {} · strobe report history <ser_…>\n", rel(&root, &file));
    let mut out = Output::new(text, json!({ "file": file, "commits": commits, "points": points, "suspected": found, "confirmed": confirmed, "marks": marks, "failed": failed }));
    // Every commit it tried failed: nothing was tracked, and a script or schedule must hear it.
    if tried > 0 && failed == tried {
        out.code = 1;
    }
    Ok(out)
}

fn canon(file: &str) -> Result<PathBuf> {
    let p = std::fs::canonicalize(file).map_err(|_| Error::usage(format!("no such bench file: {file}")))?;
    Ok(PathBuf::from(p.display().to_string().trim_start_matches(r"\\?\")))
}

/// What to track: `--range a..b` (every first-parent commit from a to b, a included, thinned with
/// `--every N`), else the commit `reference` names now (`--ref`, default HEAD; a port's `--head` ref).
fn commits(args: &Args, root: &Path, reference: &str) -> Result<Vec<String>> {
    if let Some(range) = args.get("range") {
        let (a, b) = range.split_once("..").ok_or_else(|| Error::usage("--range <from>..<to>"))?;
        let (a, b) = (rev_parse(root, a)?, rev_parse(root, b)?);
        let mut list = vec![a.clone()];
        list.extend(git::run(root, &["rev-list", "--first-parent", "--reverse", &format!("{a}..{b}")])?.lines().map(str::to_string));
        let every = args.num("every")?.unwrap_or(1.0).max(1.0) as usize;
        let last = list.len() - 1;
        return Ok(list.into_iter().enumerate().filter(|(i, _)| i % every == 0 || *i == last).map(|(_, c)| c).collect());
    }
    Ok(vec![rev_parse(root, reference)?])
}

/// The machine's part of the fingerprint, known before anything runs.
fn static_fingerprint() -> Value {
    let env = host::env();
    json!({ "device": host::device_name(), "os": env.os, "cpu": env.cpu, "gpu": gpu_fingerprint(), "vm": env.virtual_machine, "strobe": env.strobe })
}

/// The graphics adapters and their driver versions: a driver update splits a series.
fn gpu_fingerprint() -> String {
    let mut names: Vec<String> = crate::probe::gpu::adapters().into_iter().map(|a| a.name).collect();
    names.sort();
    names.dedup();
    let mut drivers = driver_versions();
    drivers.sort();
    drivers.dedup();
    fmt::fields([names.join(", "), drivers.join(", ")])
}

/// Display driver versions: Windows' display adapter class in the registry; Linux's NVIDIA module and
/// kernel (Mesa and amdgpu ship with it); macOS's come with the OS build.
fn driver_versions() -> Vec<String> {
    #[cfg(windows)]
    {
        let out = Command::new("reg").args(["query", r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}", "/s", "/v", "DriverVersion"]).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        text.lines().filter(|l| l.trim_start().starts_with("DriverVersion")).filter_map(|l| l.split_whitespace().last().map(str::to_string)).collect()
    }
    #[cfg(target_os = "linux")]
    {
        ["/sys/module/nvidia/version", "/proc/sys/kernel/osrelease"].iter().filter_map(|f| std::fs::read_to_string(f).ok()).map(|v| v.trim().to_string()).collect()
    }
    #[cfg(target_os = "macos")]
    {
        Vec::new()
    }
}

fn run_key(args: &Args, file: &Path, commit: &str, statics: &Value) -> Value {
    let flags: Vec<Value> = ["filter", "target", "params", "primary", "blocks", "engine", "headless"].iter().map(|k| json!(args.get(k))).collect();
    json!({ "file": file, "commit": commit, "flags": flags, "env": statics })
}

/// The bench a point runs: `blocks` timed blocks of the commit alone (`--blocks`), no attribution.
fn point_args(args: &Args, file: &Path, commit: &str, blocks: usize, exp: &str) -> Result<Args> {
    let mut a = Args::parse(vec!["bench".into(), file.display().to_string()])?;
    for k in PASS {
        if let Some(v) = args.get(k) {
            a.set(k, v);
        }
    }
    a.set("head", &format!("git:{commit}"));
    a.set("blocks", &blocks.to_string());
    a.set("why", "never");
    a.set("id", exp);
    Ok(a)
}

/// Tracks one commit: one bench, then one point per series it ran.
fn point(args: &Args, file: &Path, root: &Path, commit: &str, blocks: usize, statics: &Value, key: &Value) -> Result<(Vec<String>, String, Vec<Value>)> {
    let exp = store::new_id("exp");
    let (subject, commit_time) = (subject(root, commit), commit_time(root, commit));
    let depth: u64 = git::run(root, &["rev-list", "--count", commit]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    progress(format!("\n{} {subject} · {exp}", short(commit)));
    let a = point_args(args, file, commit, blocks, &exp)?;
    let result = super::bench::run(&a).and_then(super::unless_halted);
    let out = match result {
        Ok(o) => o,
        Err(e) => {
            history::log_run(&json!({ "at": super::record::now_iso(), "key": key, "commit": commit, "exp": exp, "outcome": "failed", "code": e.code, "error": e.message }))?;
            return Err(e);
        }
    };
    let r = &out.json;
    let repo = repo_id(root);
    let file_rel = rel(root, file);
    let mut ids = Vec::new();
    let mut text = String::new();
    let mut points = Vec::new();
    let warns: Vec<Value> = r["warnings"].as_array().into_iter().flatten().map(|w| json!(format!("{}: {}", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or("")))).collect();
    // A series refused before it ran (an engine without its primary) is no point of the history.
    for s in r["series"].as_array().into_iter().flatten().filter(|s| s["kind"] != "refused") {
        let metric = s["primary"]["metric"].as_str().unwrap_or("").to_string();
        let label = s["label"].as_str().unwrap_or("").to_string();
        let blocks_v: Vec<&Value> = s["blocks"].as_array().into_iter().flatten().collect();
        let values: Vec<f64> = blocks_v.iter().filter_map(|b| b["values"][&metric].as_f64()).filter(|v| v.is_finite()).collect();
        let displays: Vec<&str> = blocks_v.iter().map(|b| match b["display"].as_u64() { Some(0) => "off", Some(_) => "on", None => "unknown" }).collect();
        let display = majority(&displays);
        let session = match s["session"].as_str() {
            Some("mixed") | None => majority(&blocks_v.iter().map(|b| if b["locked"] == true { "locked" } else { "unlocked" }).collect::<Vec<_>>()),
            Some(x) => x.to_string(),
        };
        let mut why: Option<String> = None;
        if s["not_run"] == true {
            why = Some(format!("not run: {}", s["halt"]["message"].as_str().unwrap_or("")));
        } else if stats::kind_of(&s["primary"]) == model::VerdictKind::Refused {
            // A refusal across the blocks (TailTooShort, NoPhase, a block's): no value, whatever the blocks hold.
            why = Some(s["primary"]["verdict"].as_str().unwrap_or("refused").to_string());
        } else if values.len() < MIN_BLOCKS {
            why = Some(blocks_v.iter().find_map(|b| b["refused"][&metric].as_str()).map(|w| format!("refused ({w})")).unwrap_or_else(|| format!("{} of {} blocks gave {metric}", values.len(), blocks_v.len())));
        } else if s["session"] == "mixed" {
            why = Some("refused (SessionLocked): blocks ran both locked and unlocked".into());
        } else if displays.iter().any(|d| *d != displays[0]) {
            why = Some("the display turned off or on between the point's blocks".into());
        }
        let def = model::metric(&metric);
        let mut fingerprint = json!({
            "machine": statics, "power": r["env"]["power_plan"], "battery": r["env"]["on_battery"], "pin": r["plan"]["pin"],
            "runs": r["plan"]["runs_per_block"], "session": session, "display": display, "target_kind": s["target_kind"],
        });
        // The OS updates its webview runtime under the app: another version is another series.
        if s["target_kind"] == "webview" {
            fingerprint["browser"] = r["browser"].clone();
        }
        // A page's engine, browser version and headless run split a series: never one history across them.
        if s["engine"].is_string() {
            fingerprint["engine"] = s["engine"].clone();
            fingerprint["browser"] = s["browser"].clone();
            fingerprint["headless"] = s["headless"].clone();
        }
        let family = json!({ "device": statics["device"], "repo": repo, "file": file_rel, "label": label, "metric": metric });
        let id = history::series_id(&family, &fingerprint);
        if !history::exists(&id) {
            history::append(
                &id,
                &json!({
                    "t": "series", "id": id, "family": history::family_id(&family), "device": statics["device"], "repo": repo, "file": file_rel, "abs_file": file, "root": root,
                    "label": label, "case": s["case"], "target": s["target"], "params": s["params"], "metric": metric,
                    "unit": def.map(|d| d.unit), "better": def.map(|d| d.better), "kind": def.map(|d| d.kind), "fingerprint": fingerprint, "created": super::record::now_iso(),
                }),
            )?;
        }
        let ratio = def.is_none_or(|d| d.kind != Kind::Exact && d.unit != "count" && d.unit != "score") && values.iter().all(|v| *v > 0.0);
        let (median, lo, hi, conf) = if values.is_empty() {
            (f64::NAN, f64::NAN, f64::NAN, 0.0)
        } else if ratio {
            let (_, lo, hi, c) = stats::hodges_lehmann(&values.iter().map(|v| v.ln()).collect::<Vec<_>>(), 0.95);
            (detect::median(&values), lo.exp(), hi.exp(), c)
        } else {
            let (_, lo, hi, c) = stats::hodges_lehmann(&values, 0.95);
            (detect::median(&values), lo, hi, c)
        };
        let mut notes = warns.clone();
        notes.extend(s["warnings"].as_array().into_iter().flatten().map(|w| json!(format!("{}: {}", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or("")))));
        let p = json!({
            "t": "point", "at": super::record::now_iso(), "commit": commit, "short": short(commit), "subject": subject, "commit_time": commit_time, "depth": depth, "ref": args.get("ref"),
            "exp": exp, "valid": why.is_none(), "why": why, "values": values, "median": finite(median), "lo": finite(lo), "hi": finite(hi), "confidence": conf,
            "n": values.len(), "ratio": ratio, "notes": notes,
        });
        history::append(&id, &p)?;
        let unit = def.map_or("", |d| d.unit);
        text += &match &why {
            None => format!("  {}  {id}  {label} · {metric} {} [{} … {}] ({} blocks)\n", short(commit), fmt::value(median, unit), fmt::value(lo, unit), fmt::value(hi, unit), values.len()),
            Some(w) => format!("  {}  {id}  {label} · no value: {w}\n", short(commit)),
        };
        points.push(json!({ "series": id, "commit": commit, "median": finite(median), "lo": finite(lo), "hi": finite(hi), "valid": why.is_none(), "why": why }));
        ids.push(id);
    }
    history::log_run(&json!({ "at": super::record::now_iso(), "key": key, "commit": commit, "exp": exp, "outcome": "ok", "series": ids }))?;
    Ok((ids, text, points))
}

fn finite(v: f64) -> Value {
    if v.is_finite() { json!(v) } else { Value::Null }
}

fn majority(v: &[&str]) -> String {
    let mut best = ("unknown", 0);
    for x in v {
        let n = v.iter().filter(|y| *y == x).count();
        if n > best.1 {
            best = (x, n);
        }
    }
    best.0.to_string()
}

/// The equivalence bound for a series: `--equivalence`, else the project's, else 2%.
pub(crate) fn bound(args: &Args, s: &history::Series) -> f64 {
    let root = PathBuf::from(s.meta["root"].as_str().unwrap_or("."));
    args.num("equivalence").ok().flatten().map(|b| b / 100.0).unwrap_or_else(|| {
        config::load(&root).unwrap_or_default()["metrics"]["equivalence"].as_str().and_then(|e| e.trim_end_matches('%').parse::<f64>().ok()).unwrap_or(2.0) / 100.0
    })
}

/// Runs the detector on a series and appends the changes it has not seen yet as `suspected`. A port's
/// series is looked at in runs of one comparator (`implementations::runs`).
fn detect_series(s: &history::Series, args: &Args) -> Result<Vec<Value>> {
    let logs = s.implementations();
    let runs = if logs { implementations::runs(s) } else { vec![s.valid()] };
    let mut out = Vec::new();
    for valid in runs {
        out.extend(detect_run(s, args, &valid, logs)?);
    }
    Ok(out)
}

fn detect_run(s: &history::Series, args: &Args, valid: &[&Value], logs: bool) -> Result<Vec<Value>> {
    if valid.len() < 6 {
        return Ok(Vec::new());
    }
    // A port's points are already log ratios (head / base); a ratio metric's points go to log space.
    let ratio = !logs && valid.iter().all(|p| p["ratio"] == true);
    let tf = |v: f64| if ratio { v.ln() } else { v };
    let back = |v: f64| if ratio || logs { v.exp() } else { v };
    let pts: Vec<detect::Point> =
        valid.iter().map(|p| detect::Point { x: tf(p["median"].as_f64().unwrap_or(f64::NAN)), blocks: p["values"].as_array().into_iter().flatten().filter_map(Value::as_f64).map(tf).collect() }).collect();
    let b = bound(args, s);
    let level = detect::median(&valid.iter().filter_map(|p| p["median"].as_f64()).collect::<Vec<_>>()).abs();
    let params = detect::Params { min_segment: 3, alpha: 0.001, bound: if ratio || logs { (1.0 + b).ln() } else { (b * level).max(0.5) }, k_sigma: 3.0 };
    let better_low = s.meta["better"] != "higher";
    let mut out = Vec::new();
    for c in detect::changes(&pts, &params) {
        let (good, bad) = (valid[c.at - 1]["commit"].as_str().unwrap_or(""), valid[c.at]["commit"].as_str().unwrap_or(""));
        let idx = |commit: &str| valid.iter().position(|p| p["commit"] == commit);
        // A change already known around here (confirmed, rejected or waiting) is not new.
        let overlaps = s.changes.iter().any(|k| match (k["good"].as_str().and_then(idx), k["bad"].as_str().and_then(idx)) {
            (Some(g), Some(bd)) => c.at > g && c.at - 1 < bd,
            _ => false,
        });
        if overlaps {
            continue;
        }
        let worse = (c.shift > 0.0) == better_low;
        let estimate = if ratio || logs { c.shift.exp() - 1.0 } else { c.shift };
        let mut ev = json!({
            "t": "change", "id": history::change_id(&s.id, good, bad), "series": s.id, "state": "suspected", "detected": super::record::now_iso(),
            "good": good, "bad": bad, "good_short": short(good), "bad_short": short(bad), "direction": if worse { "regression" } else { "improvement" },
            "relative": ratio || logs, "estimate": estimate, "p": c.p, "before": back(c.before), "after": back(c.after),
            "method": "e-divisive means, Mann-Whitney p < 0.001 on blocks, shift >= bound and >= 3x point spread",
        });
        if logs {
            ev["method"] = json!("e-divisive means on ln(head / base) within one comparator, Mann-Whitney p < 0.001 on pairs, shift >= bound and >= 3x point spread");
        }
        history::append(&s.id, &ev)?;
        out.push(ev);
    }
    Ok(out)
}

fn change_line(s: &history::Series, c: &Value) -> String {
    let unit = s.meta["unit"].as_str().unwrap_or("");
    let est = if c["relative"] == true { fmt::pct(c["estimate"].as_f64().unwrap_or(f64::NAN)) } else { fmt::signed(c["estimate"].as_f64().unwrap_or(f64::NAN), unit) };
    format!(
        "{} {}  {} · {}  {est} between {}..{}  ({})",
        c["id"].as_str().unwrap_or(""),
        c["direction"].as_str().unwrap_or(""),
        s.id,
        s.meta["label"].as_str().unwrap_or(""),
        c["good_short"].as_str().unwrap_or(""),
        c["bad_short"].as_str().unwrap_or(""),
        c["state"].as_str().unwrap_or("")
    )
}

/// `strobe track --confirm [<ser_…|chg_…>]`: confirms what is still `suspected` (or that change again).
fn confirm_pending(args: &Args, which: Option<&str>) -> Result<Output> {
    if let Some(chg) = which.filter(|w| w.starts_with("chg_")) {
        let s = history::list().into_iter().find(|s| s.changes.iter().any(|c| c["id"] == chg)).ok_or_else(|| Error::usage(format!("no change {chg}")))?;
        let c = s.changes.iter().find(|c| c["id"] == chg).cloned().unwrap_or_default();
        let (text, res) = confirm(args, &s, &c)?;
        return Ok(Output::new(text, json!({ "confirmed": [res] })));
    }
    let only = which.map(history::resolve).transpose()?;
    let (text, confirmed) = confirm_all_in(args, |s| only.as_ref().is_none_or(|o| *o == s.id))?;
    Ok(Output::new(if text.is_empty() { "nothing waits for a confirmation\n".into() } else { text }, json!({ "confirmed": confirmed })))
}

fn confirm_all(args: &Args, file: Option<&Path>) -> Result<(String, Vec<Value>)> {
    confirm_all_in(args, |s| file.is_none_or(|f| Path::new(s.meta["abs_file"].as_str().unwrap_or("")) == f))
}

fn confirm_all_in(args: &Args, keep: impl Fn(&history::Series) -> bool) -> Result<(String, Vec<Value>)> {
    let mut text = String::new();
    let mut out = Vec::new();
    for s in history::list().into_iter().filter(|s| keep(s)) {
        for c in s.changes.iter().filter(|c| c["state"] == "suspected") {
            let (t, r) = confirm(args, &s, c)?;
            text += &t;
            out.push(r);
        }
    }
    Ok((text, out))
}

/// The flags that narrow a bench to this series alone.
fn series_args(args: &Args, s: &history::Series, id: &str) -> Result<Args> {
    let file = s.meta["abs_file"].as_str().unwrap_or("");
    let mut a = Args::parse(vec!["bench".into(), file.to_string()])?;
    for k in ["pin", "power", "reserve-cpus", "runs", "equivalence", "trace"] {
        if let Some(v) = args.get(k) {
            a.set(k, v);
        }
    }
    a.set("filter", s.meta["case"].as_str().unwrap_or(""));
    // A port's series names its head on both sides (`variant`), which narrows the bench to that target.
    if !s.implementations() {
        a.set("target", s.meta["target"].as_str().unwrap_or(""));
    }
    let params: Vec<String> = s.meta["params"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
    if !params.is_empty() {
        a.set("params", &params.join(","));
    }
    a.set("primary", s.meta["metric"].as_str().unwrap_or(""));
    // The engine and headless mode the series ran in (its fingerprint), so a later `--confirm` without
    // the flags benches what the points measured.
    let fp = &s.meta["fingerprint"];
    if let Some(e) = args.get("engine").or(fp["engine"].as_str()) {
        a.set("engine", e);
    }
    if args.has("headless") || fp["headless"] == true {
        a.set("headless", "true");
    }
    a.set("why", "never");
    a.set("id", id);
    Ok(a)
}

/// A series' build at a commit: `git:<commit>`, or for a port's series its head target at the head's
/// commit (`<head>=git:<commit>`, built in the head's own repository).
fn variant(s: &history::Series, commit: &str) -> String {
    match s.meta["head_target"].as_str().filter(|_| s.implementations()) {
        Some(t) => format!("{t}=git:{commit}"),
        None => format!("git:{commit}"),
    }
}

/// The repository a series' commits are in: a port's head's, else the bench file's project.
fn commits_root(s: &history::Series) -> PathBuf {
    PathBuf::from(s.meta[if s.implementations() { "head_root" } else { "root" }].as_str().unwrap_or("."))
}

/// One suspected change: a paired A/B of the commits around it (`--pairs auto`), and when it moved
/// the way the history did, a bisect of the commits between them. Then the alert, if any. A port's
/// change (its ratio to the comparator moved, the comparator the same) is confirmed by an ordinary
/// A/B of the head alone between the two commits, and bisected in the head's repository.
fn confirm(args: &Args, s: &history::Series, c: &Value) -> Result<(String, Value)> {
    let (good, bad) = (c["good"].as_str().unwrap_or(""), c["bad"].as_str().unwrap_or(""));
    let cid = c["id"].as_str().unwrap_or("");
    let file = s.meta["abs_file"].as_str().unwrap_or("");
    if !Path::new(file).is_file() {
        return Ok((format!("  {cid}  not confirmed: the bench file {file} is gone\n"), json!({ "id": cid, "state": "suspected" })));
    }
    let exp = store::new_id("exp");
    let alone = s.meta["head_name"].as_str().filter(|_| s.implementations()).map(|h| format!(", {h} alone")).unwrap_or_default();
    progress(format!("\nconfirming {cid}: {} against {} ({} · {}{alone})", short(bad), short(good), s.meta["label"].as_str().unwrap_or(""), s.meta["metric"].as_str().unwrap_or("")));
    history::append(&s.id, &json!({ "t": "change", "id": cid, "state": "confirming", "confirm": { "exp": exp } }))?;
    let mut a = series_args(args, s, &exp)?;
    a.set("base", &variant(s, good));
    a.set("head", &variant(s, bad));
    let res = super::bench::run(&a).and_then(super::unless_halted);
    let out = match res {
        Ok(o) => o,
        Err(e) => {
            // A stop or a failure leaves it suspected: the next track run tries again.
            history::append(&s.id, &json!({ "t": "change", "id": cid, "state": "suspected", "confirm": { "exp": exp, "error": e.message } }))?;
            if e.code == 130 {
                return Err(e);
            }
            return Ok((format!("  {cid}  confirmation failed: {}\n", e.message.lines().next().unwrap_or("")), json!({ "id": cid, "state": "suspected", "error": e.message })));
        }
    };
    let p = &out.json["series"][0]["primary"];
    let decided = p["decided"] == true && out.json["series"][0]["decided"] == true;
    let (lo, hi) = (p["lo"].as_f64().unwrap_or(f64::NAN), p["hi"].as_f64().unwrap_or(f64::NAN));
    let moved = if lo > 0.0 {
        1.0
    } else if hi < 0.0 {
        -1.0
    } else {
        0.0
    };
    let detected = c["estimate"].as_f64().unwrap_or(0.0).signum();
    let verdict = p["verdict"].as_str().unwrap_or("").to_string();
    let (state, why) = match (decided, moved) {
        (true, m) if m == detected => ("confirmed", format!("the paired A/B moved the same way: {verdict}")),
        (true, 0.0) => ("rejected", format!("the paired A/B says {verdict}")),
        (true, _) => ("rejected", format!("the paired A/B moved the other way: {verdict}")),
        (false, _) => ("unclear", format!("the paired A/B gave no verdict ({verdict}); nothing alerts (strobe track --confirm {cid} runs it again)")),
    };
    let confirm = json!({ "exp": exp, "verdict": verdict, "estimate": p["estimate"], "lo": p["lo"], "hi": p["hi"], "relative": p["relative"], "pairs": out.json["series"][0]["pairs"] });
    history::append(&s.id, &json!({ "t": "change", "id": cid, "state": state, "why": why, "confirm": confirm }))?;
    let mut text = format!("  {cid}  {state}: {why} · {exp}\n");
    let mut result = json!({ "id": cid, "series": s.id, "state": state, "confirm": confirm });
    if state != "confirmed" {
        return Ok((text, result));
    }
    // The culprit: the one commit in the range, or a bisect that starts from this A/B.
    let root = commits_root(s);
    let range: Vec<String> = git::run(&root, &["rev-list", "--first-parent", &format!("{good}..{bad}")]).unwrap_or_default().lines().map(str::to_string).collect();
    let mut culprit = Value::Null;
    let mut bisect = Value::Null;
    if range.len() <= 1 {
        culprit = json!({ "commit": bad, "short": short(bad), "subject": subject(&root, bad) });
    } else {
        let bid = store::new_id("exp");
        let mut b = series_args(args, s, &bid)?;
        b.set("bisect", &format!("{good}..{bad}"));
        let side = s.meta["head_target"].as_str().filter(|_| s.implementations()).map(|t| (t, root.as_path()));
        match super::bisect::run_from(&b, &format!("{good}..{bad}"), Some(&out), side) {
            Ok(r) => {
                let v = r.json["verdict"].as_str().unwrap_or("").to_string();
                if super::bisect::Outcome::of(&r.json) == Some(super::bisect::Outcome::Found) {
                    culprit = r.json["culprit"].clone();
                }
                bisect = json!({ "exp": bid, "verdict": v, "remaining": r.json["remaining"] });
            }
            Err(e) if e.code == 130 => return Err(e),
            Err(e) => bisect = json!({ "exp": bid, "error": e.message }),
        }
    }
    history::append(&s.id, &json!({ "t": "change", "id": cid, "culprit": culprit, "bisect": bisect }))?;
    text += &match (&culprit, &bisect) {
        (Value::Object(_), _) => format!("  {cid}  culprit {} {}{}\n", culprit["short"].as_str().unwrap_or(""), culprit["subject"].as_str().unwrap_or(""), bisect["exp"].as_str().map(|e| format!(" · bisect {e}")).unwrap_or_default()),
        (_, b) => format!("  {cid}  bisect: {}\n", b["verdict"].as_str().or(b["error"].as_str()).unwrap_or("")),
    };
    result["culprit"] = culprit.clone();
    result["bisect"] = bisect.clone();
    let wanted = args.get("alert").unwrap_or("regressions");
    if wanted == "all" || c["direction"] == "regression" {
        let mut full = c.clone();
        full["state"] = json!("confirmed");
        full["confirm"] = confirm;
        full["culprit"] = culprit;
        full["bisect"] = bisect;
        let sent = alert(args, s, &full)?;
        if !sent.is_null() {
            text += &format!("  {cid}  alerted: {}\n", sent["text"].as_str().unwrap_or(""));
            history::append(&s.id, &json!({ "t": "change", "id": cid, "alerted": sent }))?;
            result["alerted"] = sent;
        }
    }
    Ok((text, result))
}

/// Runs the alert hooks for a confirmed change: `--alert-command` (through the shell, the payload on
/// stdin and in `STROBE_ALERT_FILE`) and `--alert-webhook` (a JSON POST through curl). The payload holds
/// the series, the change and the paired verdict: no trace, recording or string from the app.
fn alert(args: &Args, s: &history::Series, c: &Value) -> Result<Value> {
    let (cmd, hook) = (args.get("alert-command"), args.get("alert-webhook"));
    if cmd.is_none() && hook.is_none() {
        return Ok(Value::Null);
    }
    let unit = s.meta["unit"].as_str().unwrap_or("");
    let cf = &c["confirm"];
    let change = if cf["relative"] == true {
        format!("{} [{} … {}]", fmt::pct(cf["estimate"].as_f64().unwrap_or(f64::NAN)), fmt::pct(cf["lo"].as_f64().unwrap_or(f64::NAN)), fmt::pct(cf["hi"].as_f64().unwrap_or(f64::NAN)))
    } else {
        format!("{} [{} … {}]", fmt::signed(cf["estimate"].as_f64().unwrap_or(f64::NAN), unit), fmt::signed(cf["lo"].as_f64().unwrap_or(f64::NAN), unit), fmt::signed(cf["hi"].as_f64().unwrap_or(f64::NAN), unit))
    };
    let culprit = c["culprit"]["short"].as_str().map(|sh| format!("{sh} {}", c["culprit"]["subject"].as_str().unwrap_or(""))).unwrap_or_else(|| format!("in {}..{}", c["good_short"].as_str().unwrap_or(""), c["bad_short"].as_str().unwrap_or("")));
    // A port's change was confirmed on its head alone: say whose.
    let label = match (s.implementations(), s.meta["head_name"].as_str(), s.meta["base_name"].as_str()) {
        (true, Some(h), Some(b)) => format!("{} ({h} vs {b}; paired A/B of {h})", s.meta["label"].as_str().unwrap_or("")),
        _ => s.meta["label"].as_str().unwrap_or("").to_string(),
    };
    let summary = format!(
        "strobe: confirmed {} on {}: {} · {} {change} ({}) · culprit {culprit}",
        c["direction"].as_str().unwrap_or("change"),
        host::device_name(),
        label,
        s.meta["metric"].as_str().unwrap_or(""),
        cf["verdict"].as_str().unwrap_or("")
    );
    let payload = json!({ "text": summary, "device": host::device_name(), "series": s.id, "label": s.meta["label"], "metric": s.meta["metric"], "file": s.meta["file"], "change": c });
    let path = history::alert_path(c["id"].as_str().unwrap_or("chg"));
    store::write_json(&path, &payload)?;
    let mut done = Vec::new();
    if let Some(cmd) = cmd {
        let code = shell(cmd, &path, &payload.to_string());
        done.push(format!("command exit {}", code.map_or("none (timed out after 60 s)".into(), |c| c.to_string())));
    }
    if let Some(url) = hook {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let out = Command::new("curl")
            .args(["-sS", "-m", "20", "-o", null, "-w", "%{http_code}", "-X", "POST", "-H", "Content-Type: application/json", "--data-binary"])
            .arg(format!("@{}", path.display()))
            .arg(url)
            .output();
        done.push(match out {
            Ok(o) => format!("webhook HTTP {}", String::from_utf8_lossy(&o.stdout).trim()),
            Err(e) => format!("webhook failed: {e}"),
        });
    }
    Ok(json!({ "at": super::record::now_iso(), "text": done.join(" · "), "file": path, "summary": summary }))
}

fn shell(cmd: &str, file: &Path, stdin: &str) -> Option<i32> {
    #[cfg(windows)]
    let mut c = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.raw_arg(format!("/d /s /c \"{cmd}\""));
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let mut child = c.env("STROBE_ALERT_FILE", file).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()?;
    if let Some(mut i) = child.stdin.take() {
        use std::io::Write;
        let _ = i.write_all(stdin.as_bytes());
    }
    for _ in 0..600 {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st.code().unwrap_or(1));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    None
}

// Schedules: the daemon tracks a branch's new commits, or its head once per period.

fn schedule(args: &Args, file: &Path, root: &Path, when: &str, sides: Option<&implementations::Sides>) -> Result<Output> {
    let every_ms = match when {
        "commits" => 0,
        "nightly" | "daily" => 86_400_000,
        d => period_ms(d)?,
    };
    // A port follows its head's ref, in the head's repository; each run names the commit on `--head`.
    let (reference, git_root) = match sides {
        Some(s) => (s.head_ref.clone(), s.head_root.clone()),
        None => (args.get("ref").unwrap_or("HEAD").to_string(), root.to_path_buf()),
    };
    rev_parse(&git_root, &reference)?;
    let mut flags = args.to_json()["flags"].clone();
    if let Some(o) = flags.as_object_mut() {
        for k in ["schedule", "ref", "json", "quiet", "progress", "range", "every"] {
            o.remove(k);
        }
        if sides.is_some() {
            o.remove("head");
        }
    }
    let id = format!("sch_{}", &store::new_id("x")[2..]);
    let mut entry = json!({ "id": id, "file": file, "ref": reference, "when": when, "every_ms": every_ms, "flags": flags, "created": super::record::now_iso(), "last_commit": null, "last_run": 0 });
    if let Some(s) = sides {
        entry["head_target"] = json!(s.head_target);
        entry["git_root"] = json!(s.head_root);
    }
    let mut all = history::schedules();
    all.push(entry.clone());
    history::save_schedules(&all)?;
    let daemon = if super::mcp::running() { "the daemon checks it every minute" } else { "no daemon runs: strobe up starts it" };
    let text = format!(
        "{id}: tracks {reference} of {} {} ({daemon}); each run waits for the bench slot like any bench\nstrobe track --schedules lists schedules · strobe track --unschedule {id}\n",
        file.display(),
        if every_ms == 0 { "at every new commit".to_string() } else { format!("once every {} when it moved", fmt::ms_duration(every_ms)) }
    );
    Ok(Output::new(text, entry))
}

fn period_ms(d: &str) -> Result<u64> {
    crate::cli::duration_ms(d).map_err(|_| Error::usage(format!("--schedule commits, nightly or a period (6h, 1d), not {d}")))
}

fn list_schedules() -> Result<Output> {
    let all = history::schedules();
    let mut text = String::new();
    for s in &all {
        let running = lock(s["id"].as_str().unwrap_or("")).is_none();
        text += &format!(
            "{}  {} {}  {}  last {}{}\n",
            s["id"].as_str().unwrap_or(""),
            s["file"].as_str().unwrap_or(""),
            s["ref"].as_str().unwrap_or(""),
            s["when"].as_str().unwrap_or(""),
            s["last_commit"].as_str().map(short).unwrap_or_else(|| "—".into()),
            if running { " · running" } else { "" }
        );
    }
    if text.is_empty() {
        text = "no schedules (strobe track <file> --ref <branch> --schedule commits|nightly|6h)\n".into();
    }
    Ok(Output::new(text, json!({ "schedules": all })))
}

fn unschedule(id: &str) -> Result<Output> {
    let mut all = history::schedules();
    let n = all.len();
    all.retain(|s| s["id"] != id);
    if all.len() == n {
        return Err(Error::usage(format!("no schedule {id} (strobe track --schedules)")));
    }
    history::save_schedules(&all)?;
    Ok(Output::new(format!("removed {id}; its history stays\n"), json!({ "removed": id })))
}

fn lock(id: &str) -> Option<std::fs::File> {
    let f = std::fs::File::options().create(true).truncate(false).write(true).open(history::root().join(format!("{id}.lock"))).ok()?;
    f.try_lock().ok()?;
    Some(f)
}

/// How long a schedule waits before it tries a commit whose run did not track it.
const RETRY_SECS: u64 = 3600;

/// The daemon's schedule loop: once a minute, each schedule whose ref points at a commit not tracked
/// yet (at most once per period) starts a detached `strobe track` that holds the schedule's lock.
/// `last_commit` is the commit it last started a run for.
pub fn daemon_loop() {
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let mut all = history::schedules();
        if all.is_empty() {
            continue;
        }
        let mut changed = false;
        let now = super::record::now_secs();
        for s in all.iter_mut() {
            let id = s["id"].as_str().unwrap_or("").to_string();
            let file = PathBuf::from(s["file"].as_str().unwrap_or(""));
            let root = s["git_root"].as_str().map(PathBuf::from).unwrap_or_else(|| config::project_root(file.parent().unwrap_or(Path::new("."))));
            let Ok(head) = rev_parse(&root, s["ref"].as_str().unwrap_or("HEAD")) else { continue };
            let every = s["every_ms"].as_u64().unwrap_or(0) / 1000;
            let last_run = s["last_run"].as_u64().unwrap_or(0);
            // Done when a run tracked this commit (runs.jsonl says ok), not when one started: a run that
            // failed (a build, a lock that never came) is tried again, at most once an hour.
            let done = history::runs().iter().any(|r| r["commit"] == head.as_str() && r["outcome"] == "ok" && r["key"]["file"] == s["file"]);
            let wait = if s["last_commit"] == head.as_str() { every.max(RETRY_SECS) } else { every };
            if done || now < last_run + wait {
                continue;
            }
            // Free only while no run of it holds the lock.
            if lock(&id).is_none() {
                continue;
            }
            let mut argv: Vec<String> = match s["head_target"].as_str() {
                Some(t) => vec!["track".into(), file.display().to_string(), "--head".into(), format!("{t}=git:{head}"), "--from-schedule".into(), id.clone()],
                None => vec!["track".into(), file.display().to_string(), "--ref".into(), head.clone(), "--from-schedule".into(), id.clone()],
            };
            for (k, v) in s["flags"].as_object().into_iter().flatten() {
                argv.push(format!("--{k}"));
                if v != "true" {
                    argv.push(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
                }
            }
            if spawn(&argv).is_ok() {
                s["last_commit"] = json!(head);
                s["last_run"] = json!(now);
                changed = true;
            }
        }
        if changed {
            let _ = history::save_schedules(&all);
        }
    }
}

fn spawn(argv: &[String]) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(windows)]
    {
        let quoted: Vec<String> = argv.iter().map(|a| format!("\"{}\"", a.replace('"', "\\\""))).collect();
        let refs: Vec<&str> = quoted.iter().map(String::as_str).collect();
        host::win::spawn_detached(&exe, &refs).map(|_| ())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        Command::new(exe).args(argv).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn().map(|_| ())
    }
}

// git

fn rev_parse(root: &Path, rev: &str) -> Result<String> {
    git::commit(root, rev).ok_or_else(|| Error::usage(format!("{} is not a commit in {}", rev.strip_prefix("git:").unwrap_or(rev), root.display())))
}

/// The repository's identity across clones and worktrees: its first root commit.
fn repo_id(root: &Path) -> String {
    let mut roots: Vec<String> = git::run(root, &["rev-list", "--max-parents=0", "HEAD"]).unwrap_or_default().lines().map(str::to_string).collect();
    roots.sort();
    roots.first().cloned().unwrap_or_else(|| root.display().to_string())
}

fn rel(root: &Path, file: &Path) -> String {
    let top = git::toplevel(root).unwrap_or_else(|| root.to_path_buf());
    let top = std::fs::canonicalize(&top).unwrap_or(top);
    let f = std::fs::canonicalize(file).unwrap_or(file.to_path_buf());
    f.strip_prefix(&top).map(|p| p.display().to_string().replace('\\', "/")).unwrap_or_else(|_| file.display().to_string())
}
