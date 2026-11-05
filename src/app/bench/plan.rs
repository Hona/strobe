//! `strobe plan <file>` (also `bench --plan`): before a run, what each series will cost (SPEC §9.1
//! "Plan"). Per series the spread of the primary's per-pair differences and the time of a block come
//! from earlier experiments of the same series on this device (else other series measuring that metric,
//! a `--check`, a pilot, or a stated guess); from them the pairs that decide at the expected effect with
//! an 80% chance, the look that happens at, and the minutes, with builds, warm-up blocks and the bench
//! slot's queue. `bench` prints the same plan when it starts and keeps an ETA from it.

use super::super::{Output, fmt, progress};
use super::{FUTILITY_FROM, Kind, LOOKS, POCOCK_ALPHA, Series, Variant, slot};
use crate::cli::Args;
use crate::model::{self, Error, Result};
use crate::runner::Runner;
use crate::{config, host, stats, store};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

/// The chance a planned run decides at the expected effect.
const POWER: f64 = 0.8;

/// A block's wall time when nothing measured one here (October 2026, adais: a 2.7 s scroll case).
const GUESS_BLOCK_MS: f64 = 22_000.0;

pub fn run(args: &Args) -> Result<Output> {
    let file = args.pos.first().ok_or_else(|| Error::usage("plan needs a bench file: strobe plan perf/x.bench.ts [--base <variant>] [--head <variant>]"))?;
    let file = std::fs::canonicalize(file).map_err(|_| Error::usage(format!("no such bench file: {file}")))?;
    if args.has("pilot") {
        pilot(args, &file)?;
    }
    let project = config::project_root(file.parent().unwrap_or(Path::new(".")));
    let config = config::load(&project)?;
    let log = host::paths::run().join(format!("plan-{}.log", std::process::id()));
    let mut runner = Runner::spawn(&file, log.clone(), None)?;
    let describe = runner.describe();
    runner.exit();
    let _ = std::fs::remove_file(&log);
    let describe = describe?;
    let mut notes = Vec::new();
    if let Some(f) = args.get("filter") {
        let (text, err) = super::filter_report(&describe, f);
        if let Some(e) = err {
            return Err(e);
        }
        notes.push(text);
    }
    let series = super::expand(&describe, args)?;
    if series.is_empty() {
        return Err(Error::usage("no series matches --filter / --target / --params"));
    }
    let head = super::resolve_variant("head", args.get("head").unwrap_or("."), &project, &config)?;
    let base = match args.get("base") {
        Some(b) => Some(super::resolve_variant("base", b, &project, &config)?),
        None if args.has("aa") => Some(super::resolve_variant("base", args.get("head").unwrap_or("."), &project, &config)?),
        None => None,
    };
    let variants: Vec<Variant> = base.into_iter().chain(std::iter::once(head)).collect();
    let mut plan = estimate(&series, &describe, &file, args, &config, &project, &variants, None);
    let text = text(&file, &variants, &plan, &notes);
    plan["text"] = json!(text);
    Ok(Output::new(text, plan))
}

/// `--pilot`: a short experiment first (4 pairs per series, no attribution), so the plan has a spread and
/// a block time to start from. An A/A on head when there is no `--base`.
fn pilot(args: &Args, file: &Path) -> Result<()> {
    let mut a = args.clone();
    a.command = "bench".into();
    a.pos = vec![file.display().to_string()];
    for k in ["pilot", "plan", "effect", "json"] {
        a.remove(k);
    }
    a.set("pairs", "4");
    a.set("why", "never");
    if args.get("base").is_none() {
        a.set("aa", "true");
    }
    progress("pilot: 4 pairs per series, to learn each series' spread and block time (stored like any experiment)");
    let out = super::run(&a)?;
    progress(format!("pilot: {}", out.json["id"].as_str().unwrap_or("")));
    Ok(())
}

/// What one earlier series told: its primary's per-pair differences and what its blocks cost.
struct Past {
    id: String,
    d: Vec<f64>,
    relative: bool,
    base: f64,
    estimate: f64,
    block_ms: f64,
    warmup_blocks: f64,
    warmup_ms: f64,
    attribution_ms: Option<f64>,
    holm_pairs: Option<usize>,
}

fn past(id: &str, s: &Value) -> Option<Past> {
    let primary = s["primary"]["metric"].as_str()?;
    let relative = s["primary"]["relative"].as_bool().unwrap_or(true);
    let blocks = s["blocks"].as_array()?;
    let mut by_pair: std::collections::BTreeMap<u64, (f64, f64)> = Default::default();
    for b in blocks {
        let v = b["values"][primary].as_f64().unwrap_or(f64::NAN);
        let e = by_pair.entry(b["pair"].as_u64().unwrap_or(0)).or_insert((f64::NAN, f64::NAN));
        if b["variant"] == "base" { e.0 = v } else { e.1 = v }
    }
    let pairs: Vec<(f64, f64)> = by_pair.into_values().filter(|(a, b)| a.is_finite() && b.is_finite() && (!relative || (*a > 0.0 && *b > 0.0))).collect();
    let d: Vec<f64> = pairs.iter().map(|(a, b)| if relative { (b / a).ln() } else { b - a }).collect();
    let block = |b: &Value| b["steps"].as_array().map(|st| st.iter().filter_map(|x| x[1].as_f64()).sum::<f64>());
    let times: Vec<f64> = blocks.iter().filter_map(block).filter(|t| *t > 0.0).collect();
    if times.is_empty() {
        return None;
    }
    let block_ms = times.iter().sum::<f64>() / times.len() as f64;
    let warmup_blocks = s["warmup_blocks"].as_f64().unwrap_or(0.0);
    let warmup_ms = s["warmup_ms"].as_f64().unwrap_or(warmup_blocks * block_ms);
    // What the series spent beyond its timed and warm-up blocks: attribution blocks and analysis.
    let attributed = ["base", "head"].iter().filter(|r| s["attribution"][**r].is_string()).count();
    let extra = s["elapsed_ms"].as_f64().unwrap_or(0.0) - times.iter().sum::<f64>() - warmup_ms;
    let attribution_ms = (attributed > 0 && extra > 0.0).then(|| extra / attributed as f64);
    Some(Past {
        id: id.to_string(),
        d,
        relative,
        base: crate::metrics::median(&pairs.iter().map(|p| p.0).collect::<Vec<_>>()),
        estimate: s["primary"]["estimate"].as_f64().unwrap_or(f64::NAN),
        block_ms,
        warmup_blocks,
        warmup_ms,
        attribution_ms,
        holm_pairs: s["holm_pairs"].as_u64().map(|n| n as usize),
    })
}

/// Earlier experiments' results, read once per plan.
struct History {
    index: Vec<Value>,
    results: HashMap<String, Option<Value>>,
}

impl History {
    fn result(&mut self, id: &str) -> Option<Value> {
        self.results.entry(id.to_string()).or_insert_with(|| store::read_json(&host::paths::experiments().join(id).join("result.json")).ok()).clone()
    }

    /// Up to 5 newest earlier runs of this series (same bench file name, series label and primary).
    fn same_series(&mut self, file_name: &str, s: &Series) -> Vec<Past> {
        let label = s.label();
        let ids: Vec<String> = self
            .index
            .iter()
            .rev()
            .filter(|l| Path::new(l["file"].as_str().unwrap_or("")).file_name().is_some_and(|f| f.to_string_lossy() == file_name))
            .filter(|l| l["verdicts"].as_array().is_some_and(|v| v.iter().any(|v| v["series"] == label.as_str() && v["primary"] == s.primary.as_str())))
            .filter_map(|l| l["id"].as_str().map(str::to_string))
            .collect();
        let mut out = Vec::new();
        for id in ids {
            let Some(r) = self.result(&id) else { continue };
            if let Some(p) = r["series"].as_array().into_iter().flatten().find(|x| x["label"] == label.as_str()).and_then(|x| past(&id, x)) {
                out.push(p);
            }
            if out.len() == 5 {
                break;
            }
        }
        out
    }

    /// Up to 5 newest series of any bench file whose primary is this metric on this kind of target.
    fn same_metric(&mut self, metric: &str, kind: &str) -> Vec<Past> {
        let ids: Vec<String> = self
            .index
            .iter()
            .rev()
            .filter(|l| l["verdicts"].as_array().is_some_and(|v| v.iter().any(|v| v["primary"] == metric)))
            .filter_map(|l| l["id"].as_str().map(str::to_string))
            .take(10)
            .collect();
        let mut out = Vec::new();
        for id in ids {
            let Some(r) = self.result(&id) else { continue };
            for x in r["series"].as_array().into_iter().flatten().filter(|x| x["primary"]["metric"] == metric && x["target_kind"] == kind) {
                if let Some(p) = past(&id, x) {
                    out.push(p);
                }
            }
            if out.len() >= 5 {
                break;
            }
        }
        out
    }
}

/// A `--check` of this series: one cold block with one run, plus the warmup and runs a timed block adds.
fn check_block(file_name: &str, label: &str, runs: f64) -> Option<(f64, String)> {
    for (id, _) in store::list("exp", 40) {
        let path = host::paths::experiments().join(&id).join("result.json");
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX) > 2 << 20 {
            continue;
        }
        let Ok(r) = store::read_json(&path) else { continue };
        if r["kind"] != "check" || Path::new(r["file"].as_str().unwrap_or("")).file_name().is_none_or(|f| f.to_string_lossy() != file_name) {
            continue;
        }
        let Some(s) = r["series"].as_array().into_iter().flatten().find(|s| s["label"] == label && s["passed"] == true) else { continue };
        let one = s["values"]["run.duration"].as_f64().unwrap_or(0.0);
        return Some((s["elapsed_ms"].as_f64()? + (runs + 1.0) * one, id));
    }
    None
}

/// The plan of `series` (see the module comment). `me` is the bench that asks (its own registration is
/// not queue).
#[allow(clippy::too_many_arguments)]
pub(super) fn estimate(series: &[Series], describe: &Value, file: &Path, args: &Args, config: &Value, project: &Path, variants: &[Variant], me: Option<&str>) -> Value {
    let file_name = file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let mut history = History { index: store::index(), results: HashMap::new() };
    let fixed = args.get("pairs").filter(|p| *p != "auto").and_then(|p| p.parse::<usize>().ok());
    let confidence = if fixed.is_some() { 0.95 } else { 1.0 - POCOCK_ALPHA };
    let runs = args.num("runs").ok().flatten().unwrap_or(5.0);
    let bound = args
        .num("equivalence")
        .ok()
        .flatten()
        .map(|b| b / 100.0)
        .unwrap_or_else(|| config["metrics"]["equivalence"].as_str().and_then(|e| e.trim_end_matches('%').parse::<f64>().ok()).unwrap_or(2.0) / 100.0);
    let why = args.get("why").unwrap_or(if args.get("probes") == Some("timing") { "never" } else { "auto" });
    let mut out = Vec::new();
    let mut total = 0.0;
    for s in series {
        let target = &describe["targets"][&s.target];
        let kind = target["kind"].as_str().unwrap_or("");
        let def = model::metric(&s.primary);
        let same = history.same_series(&file_name, s);
        // The spread from this series when it ran 4 pairs or more here, else from other series of its metric.
        let from_series = same.iter().map(|p| p.d.len()).sum::<usize>() >= 4;
        let spread_from = if from_series { "series" } else { "metric" };
        let metric_pool = if from_series { Vec::new() } else { history.same_metric(&s.primary, kind) };
        let pool: Vec<&Past> = if from_series { same.iter().collect() } else { metric_pool.iter().collect() };
        // The spread of one pair's difference, from each experiment's differences around its own median.
        let centred: Vec<f64> = pool
            .iter()
            .flat_map(|p| {
                let m = crate::metrics::median(&p.d);
                p.d.iter().map(move |x| x - m)
            })
            .collect();
        let sigma = (centred.len() >= 4).then(|| stats::robust_sd(&centred)).filter(|s| s.is_finite() && *s > 0.0);
        let relative = pool.first().map(|p| p.relative).unwrap_or(true);
        let base_value = pool.first().map(|p| p.base).unwrap_or(f64::NAN);
        let spread_source = match (sigma, spread_from) {
            (None, _) => "unknown: no earlier experiment measured this metric here (--pilot learns it)".to_string(),
            (Some(_), "series") => {
                format!("{} earlier experiment{} of this series ({})", pool.len(), if pool.len() == 1 { "" } else { "s" }, pool.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(", "))
            }
            (Some(_), _) => {
                let mut ids: Vec<&str> = pool.iter().map(|p| p.id.as_str()).collect();
                ids.dedup();
                format!("other series measuring {} on {kind} targets here ({}); this case may differ", s.primary, ids.join(", "))
            }
        };
        // A block's time comes only from this series: another case's blocks say nothing about this one's.
        let (block_ms, block_source) = match same.first() {
            Some(_) => (same.iter().map(|p| p.block_ms).sum::<f64>() / same.len() as f64, format!("mean of {} earlier", same.len())),
            None => match check_block(&file_name, &s.label(), runs) {
                Some((ms, id)) => (ms, format!("from the --check {id} plus warmup and {runs} runs; rough: a check's block is a cold launch")),
                None => (GUESS_BLOCK_MS, "a guess: no earlier run of this case here".to_string()),
            },
        };
        let gpu_warm = matches!(kind, "native" | "app" | "exe") && s.launch.is_null() && target["gpuCache"] != "cold" && !cfg!(target_os = "macos");
        let (warmup_blocks, warmup_ms) = match same.first() {
            Some(p) if p.warmup_blocks > 0.0 => (p.warmup_blocks, p.warmup_ms),
            _ if gpu_warm || s.launch.is_object() => (2.0, 2.0 * block_ms),
            _ => (0.0, 0.0),
        };
        // The expected effect: --effect, else what the latest run of this series measured, else none.
        let (effect, effect_source) = match args.num("effect").ok().flatten() {
            Some(e) => (e / 100.0, "--effect".to_string()),
            None => match same.first().filter(|p| p.estimate.is_finite()) {
                Some(p) => (if p.relative { p.estimate } else { p.estimate / p.base.abs().max(1e-9) }, format!("as {} measured", p.id)),
                None => (0.0, "none expected".to_string()),
            },
        };
        let margin = s.noworse.get(&s.primary).copied();
        // In the differences' scale: log ratios, or the unit for counts.
        let theta = if relative { (1.0 + effect).max(1e-9).ln() } else { effect * base_value };
        let (expect, need) = match (sigma, def) {
            (Some(sig), Some(def)) => match margin {
                Some(m) => {
                    let m_abs = if relative { m } else { (m * base_value.abs()).max(0.5) };
                    let shift = stats::margin_shift(def, relative, m_abs);
                    let worse = if def.better == model::Better::Lower { theta } else { -theta };
                    let word = if worse < shift.abs() { format!("no worse (≤{}%)", (m * 1000.0).round() / 10.0) } else { format!("worse (>{}%)", (m * 1000.0).round() / 10.0) };
                    (word, stats::pairs_for(sig, (shift - theta).abs(), confidence, POWER))
                }
                None => {
                    let b = if relative { (1.0 + bound).ln() } else { (bound * base_value.abs()).max(0.5) };
                    let differs = stats::pairs_for(sig, theta.abs(), confidence, POWER);
                    let same = if b > theta.abs() { stats::pairs_for(sig, b - theta.abs(), confidence, POWER) } else { f64::INFINITY };
                    let improved = (theta < 0.0) == (def.better == model::Better::Lower);
                    if differs < same { ((if improved { def.words.0 } else { def.words.1 }).to_string(), differs) } else { (format!("same (±{}%)", (bound * 1000.0).round() / 10.0), same) }
                }
            },
            _ => (String::new(), f64::NAN),
        };
        let max_pairs = fixed.unwrap_or(*LOOKS.last().unwrap_or(&40));
        let look = if need.is_finite() {
            match fixed {
                Some(n) => (need <= n as f64).then_some(n),
                None => LOOKS.iter().copied().find(|&l| l as f64 >= need.ceil()),
            }
        } else {
            None
        };
        let holm = same.first().and_then(|p| p.holm_pairs).unwrap_or_else(|| super::holm_pairs(s.metrics.len() + 2));
        // Pairs the series runs: to its deciding look and on to Holm's; an undecidable one stops at the
        // first look its projection gives up (12 pairs), or runs to the end with --pairs N.
        let pairs = match (look, fixed) {
            (_, Some(n)) => n,
            (Some(l), None) => l.max(holm).min(max_pairs),
            (None, None) if sigma.is_some() => FUTILITY_FROM,
            (None, None) => LOOKS[0],
        };
        let differs = !expect.starts_with("same") && !expect.starts_with("no worse") && !expect.is_empty();
        let attribution = why == "always" || (why == "auto" && differs);
        let attribution_ms = if attribution { 2.0 * same.first().and_then(|p| p.attribution_ms).unwrap_or(1.5 * block_ms) } else { 0.0 };
        let ms = 2.0 * pairs as f64 * block_ms + warmup_ms + attribution_ms;
        total += ms;
        let detectable = sigma.map(|sig| {
            let x = stats::detectable(sig, max_pairs, confidence, POWER);
            if relative { x.exp() - 1.0 } else { x }
        });
        out.push(json!({
            "label": s.label(), "case": s.case, "target": s.target, "primary": s.primary, "relative": relative,
            "mode": if margin.is_some() { "noworse" } else { "two-sided" }, "margin": margin,
            "sigma": sigma, "spread_source": spread_source, "from": pool.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            "block_ms": block_ms, "block_source": block_source, "warmup_blocks": warmup_blocks, "warmup_ms": warmup_ms,
            "effect": effect, "effect_source": effect_source, "expect": expect, "pairs_needed": need.is_finite().then_some(need.ceil()),
            "look": look, "pairs": pairs, "holm_pairs": holm, "underpowered": sigma.is_some() && look.is_none(), "detectable": detectable, "detectable_at": max_pairs,
            "attribution_ms": attribution_ms, "ms": ms, "power": POWER, "confidence": confidence,
        }));
    }
    let builds = builds(series, describe, config, project, variants);
    let build_ms: f64 = builds.iter().filter_map(|b| b["ms"].as_f64()).sum();
    let others: Vec<Value> = slot::list().into_iter().filter(|b| me.is_none_or(|m| b["exp"] != m)).collect();
    let queue_ms: f64 = others.iter().filter_map(|b| b["eta_ms"].as_f64()).sum();
    json!({
        "series": out, "builds": builds, "build_ms": build_ms,
        "slot": { "others": others.iter().map(|b| json!({ "exp": b["exp"], "state": b["state"], "eta_ms": b["eta_ms"] })).collect::<Vec<_>>(), "wait_ms_max": queue_ms },
        "blocks_ms": total, "total_ms": total + build_ms,
    })
}

/// Whether each variant builds before its blocks, and about how long: unchanged builds are reused, and a
/// build's last time on this device (`builds/times.json`) stands for the next one.
fn builds(series: &[Series], describe: &Value, config: &Value, project: &Path, variants: &[Variant]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut kinds: Vec<&Value> = Vec::new();
    for s in series {
        let t = &describe["targets"][&s.target];
        if !kinds.iter().any(|k| k["kind"] == t["kind"] && k["build"] == t["build"]) {
            kinds.push(t);
        }
    }
    let times = store::read_json(&host::paths::sub("builds").join("times.json")).unwrap_or(Value::Null);
    for (i, v) in variants.iter().enumerate() {
        // An A/A builds its one variant once.
        if !matches!(v.kind, Kind::Dir | Kind::Git(_)) || variants[..i].iter().any(|w| w.spec == v.spec) {
            continue;
        }
        for t in &kinds {
            let build = match t["kind"].as_str() {
                Some("script") | Some("command") => t["build"].as_str().map(|c| json!({ "command": c })),
                Some("native") if !t["path"].is_string() => config.get("build").map(crate::target::isolate::build_for_os).filter(|b| b["command"].is_string()),
                Some("web") => config.get("build").filter(|b| b.is_object()).map(crate::target::isolate::build_for_os),
                _ => None,
            };
            let Some(build) = build else { continue };
            let root = match &v.kind {
                Kind::Git(_) => v
                    .commit
                    .as_deref()
                    .and_then(|c| host::output(std::process::Command::new("git").arg("-C").arg(project).args(["rev-parse", c])))
                    .map(|sha| host::paths::sub("builds").join(&sha[..12.min(sha.len())])),
                _ => v.root.clone(),
            };
            let marker = root.as_ref().map(|r| host::paths::sub("builds").join(format!("{}.key", super::hash_str(&r.to_string_lossy()))));
            let last = times[super::build_time_key(&build)]["ms"].as_f64();
            let (state, ms) = match (&root, &marker) {
                (Some(r), Some(m)) if r.exists() && matches!(v.kind, Kind::Dir) && std::fs::read_to_string(m).ok().as_deref() == Some(super::build_key(r, &build).as_str()) => {
                    ("unchanged: reused".to_string(), 0.0)
                }
                (Some(r), Some(m)) if r.exists() && m.exists() && matches!(v.kind, Kind::Git(_)) => ("built before: reused unless its harness or build changed".to_string(), 0.0),
                _ => match last {
                    Some(ms) => (format!("builds: ~{} (its last build here)", fmt::ms_duration(ms as u64)), ms),
                    None => ("builds: time unknown (no build of this command here yet)".to_string(), 0.0),
                },
            };
            out.push(json!({ "role": v.role, "spec": v.spec, "command": build["command"], "state": state, "ms": ms }));
        }
    }
    out
}

fn minutes(ms: f64) -> String {
    if ms < 60_000.0 { fmt::ms_duration(ms as u64) } else { format!("{:.0} min", ms / 60_000.0) }
}

/// One line per series for `bench`'s start: pairs, minutes, and what they rest on.
pub(super) fn summary(plan: &Value) -> Vec<String> {
    let mut lines: Vec<String> = plan["series"].as_array().into_iter().flatten().map(series_line).collect();
    lines.push(total_line(plan));
    lines
}

fn series_line(s: &Value) -> String {
    let what = match (s["sigma"].as_f64(), s["look"].as_u64()) {
        (None, _) => format!("{} pairs at least; spread unknown, so no estimate of when it decides", s["pairs"]),
        (Some(_), Some(l)) => format!("{} by pair {l} (~{} pairs needed)", s["expect"].as_str().unwrap_or(""), s["pairs_needed"].as_f64().unwrap_or(0.0)),
        (Some(_), None) => {
            format!("underpowered at the expected effect (~{} pairs needed); stops at about {} pairs saying so", s["pairs_needed"].as_f64().map_or("many".to_string(), |n| n.to_string()), s["pairs"])
        }
    };
    format!("{}: {what}, ~{}", s["label"].as_str().unwrap_or(""), minutes(s["ms"].as_f64().unwrap_or(0.0)))
}

fn total_line(plan: &Value) -> String {
    let mut t = format!("total ~{} of blocks", minutes(plan["blocks_ms"].as_f64().unwrap_or(0.0)));
    let build = plan["build_ms"].as_f64().unwrap_or(0.0);
    if build > 0.0 {
        t += &format!(" + ~{} of builds", minutes(build));
    }
    let wait = plan["slot"]["wait_ms_max"].as_f64().unwrap_or(0.0);
    let others = plan["slot"]["others"].as_array().map_or(0, Vec::len);
    if others > 0 {
        t += &format!(
            "; {others} other bench{} on this device{}",
            if others == 1 { "" } else { "es" },
            if wait > 0.0 {
                format!(" with ~{} of blocks left: blocks take turns, so up to that much longer", minutes(wait))
            } else {
                " (time left unknown): blocks take turns, so up to that bench's time longer".to_string()
            }
        );
    }
    t
}

fn text(file: &Path, variants: &[Variant], plan: &Value, notes: &[String]) -> String {
    let name = file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let mut t = format!("strobe plan {name} · {}\n", variants.iter().map(|v| format!("{} {}", v.role, v.label)).collect::<Vec<_>>().join(" · "));
    for n in notes {
        t += &format!("{}\n", n.trim_end());
    }
    for b in plan["builds"].as_array().into_iter().flatten() {
        t += &format!("build  {} {}\n", b["role"].as_str().unwrap_or(""), b["state"].as_str().unwrap_or(""));
    }
    for s in plan["series"].as_array().into_iter().flatten() {
        let unit = if s["relative"] == true { "" } else { " (in the metric's unit)" };
        let pct = |x: f64| if s["relative"] == true { format!("{:.1}%", 100.0 * (x.exp() - 1.0)) } else { format!("{x:.2}") };
        t += &format!(
            "\n{}  {}{}\n",
            s["label"].as_str().unwrap_or(""),
            s["primary"].as_str().unwrap_or(""),
            s["margin"].as_f64().map(|m| format!(" · no worse than {}%?", (m * 1000.0).round() / 10.0)).unwrap_or_default()
        );
        t += &format!(
            "  spread   {}\n",
            match s["sigma"].as_f64() {
                Some(sig) => format!("{} per pair{unit}, from {}", pct(sig), s["spread_source"].as_str().unwrap_or("")),
                None => s["spread_source"].as_str().unwrap_or("").to_string(),
            }
        );
        let warm = s["warmup_blocks"].as_f64().unwrap_or(0.0);
        t += &format!(
            "  block    {} ({}){}\n",
            fmt::ms_duration(s["block_ms"].as_f64().unwrap_or(0.0) as u64),
            s["block_source"].as_str().unwrap_or(""),
            if warm > 0.0 { format!(" · {warm:.0} discarded warm-up blocks first") } else { String::new() }
        );
        if s["sigma"].is_number() {
            t += &format!(
                "  expect   at an effect of {} ({}): {}\n",
                fmt::pct(s["effect"].as_f64().unwrap_or(0.0)).replace("0.0%", "0%"),
                s["effect_source"].as_str().unwrap_or(""),
                series_line(s).split_once(": ").map(|x| x.1).unwrap_or("")
            );
            if let Some(x) = s["detectable"].as_f64() {
                t += &format!(
                    "  smallest {} pairs decide an effect of about {} (80% of the time)\n",
                    s["detectable_at"],
                    if s["relative"] == true { format!("{:.1}%", 100.0 * x) } else { format!("{x:.2}") }
                );
            }
        } else {
            t += &format!("  expect   {}\n", series_line(s).split_once(": ").map(|x| x.1).unwrap_or(""));
        }
    }
    t += &format!("\n{}\n", total_line(plan));
    t += "pairs for an 80% chance to decide at the expected effect (normal approximation of the planned looks); --effect <pct> asks about another\n";
    t
}
