//! `strobe bench`: experiments with verdicts (SPEC §9). Blocks of fresh launches, alternating A B, B A.

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::driver::Driver;
use crate::host;
use crate::metrics;
use crate::model::{self, Error, MetricDef, Result};
use crate::probe::{self, Set};
use crate::runner::Runner;
use crate::stats::{self, Comparison};
use crate::store;
use crate::target::chromium::{self, Page, Window};
use crate::target::electron;
use crate::validity::{self, Warning};
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Secondaries that get tested (Holm across these only). Fewer tests keep the correction reachable.
const DEFAULT_SECONDARIES: &[&str] = &["frames.interval.p95", "frames.dropped.pct", "main.busy", "interaction.p95", "cpu.app.total"];

/// Shown with intervals but never tested: context, not claims.
const DESCRIPTIVE: &[&str] = &[
    "frames.interval.p50",
    "frames.presented.p50",
    "frames.presented.p95",
    "frames.presented.p99",
    "main.tasks.long",
    "scroll.latency.p95",
    "run.duration",
    "gpu.process.3d",
    "gpu.vram",
    "memory.app.private",
    "js.heap.used",
    "dom.nodes",
];

#[derive(Clone, Debug, Serialize)]
struct Series {
    case: String,
    target: String,
    params: BTreeMap<String, Value>,
    primary: String,
    metrics: Vec<String>,
    reset: bool,
}

impl Series {
    fn label(&self) -> String {
        let mut s = format!("{} · {}", self.case, self.target);
        for (k, v) in &self.params {
            s += &format!(" · {k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
        }
        s
    }
}

#[derive(Debug)]
enum Kind {
    Dir,
    Git(String),
    Url(String),
    Exe(PathBuf),
}

struct Variant {
    role: &'static str,
    spec: String,
    kind: Kind,
    label: String,
    root: Option<PathBuf>,
    base_url: Option<String>,
    server: Option<Child>,
}

impl Drop for Variant {
    fn drop(&mut self) {
        if let Some(mut s) = self.server.take() {
            kill_tree(&mut s);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Block {
    variant: String,
    pair: usize,
    values: BTreeMap<String, f64>,
    run_ms: Vec<f64>,
    warmup: usize,
    speed_ratio: Option<f64>,
    nodes: Vec<u64>,
    recording: Option<String>,
    phases: BTreeMap<String, Vec<f64>>,
    frames: Vec<f64>,
    intervals: Vec<f64>,
}

struct Ctx<'a> {
    args: &'a Args,
    exp: String,
    describe: Value,
    runner: Runner,
    window: Window,
    /// The window the last block actually used (a target's viewport overrides the default).
    used: Option<Window>,
    pin: Option<u64>,
    gpu_checked: bool,
    gpu: Value,
    browser_version: String,
    baseline: Option<f64>,
    keep_all: bool,
    runs: usize,
    warnings: Vec<Warning>,
}

pub fn run(args: &Args) -> Result<Output> {
    let file = args.pos.first().ok_or_else(|| Error::usage("bench needs a bench file: strobe bench perf/x.bench.ts --base <variant> --head <variant>"))?;
    let file = std::fs::canonicalize(file).map_err(|_| Error::usage(format!("no such bench file: {file}")))?;
    let head_spec = args.get("head").unwrap_or(".").to_string();
    let aa = args.has("aa");
    let base_spec = match (args.get("base"), aa) {
        (Some(b), false) => b.to_string(),
        (None, true) => head_spec.clone(),
        (Some(_), true) => return Err(Error::usage("--aa runs --head against itself; drop --base")),
        (None, false) => return Err(Error::usage("say what to compare: --base <variant> (or --aa for the same build on both sides)")),
    };
    let exp = args.get("id").map(str::to_string).unwrap_or_else(|| store::new_id("exp"));
    let dir = store::experiment_dir(&exp)?;
    super::progress_to(Some(dir.join("progress.log")));
    let _lock = validity::lock()?;
    let project = project_root(&file);
    let config = read_config(&project);
    let _power = match args.get("power").unwrap_or("keep") {
        "keep" => None,
        "high" => Some(PowerPlan::high()?),
        other => return Err(Error::usage(format!("--power: keep or high, not {other}"))),
    };
    let env = host::env();
    let topo = host::topology();
    let (pin, runner_pin, pin_label) = pinning(args.get("pin").unwrap_or("auto"), &topo)?;
    progress(format!("strobe bench {} · {exp}", file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()));
    let busy = host::system_busy(1000);
    let runner = Runner::spawn(&file, dir.join("runner.log"), runner_pin)?;
    let mut ctx = Ctx {
        args,
        exp: exp.clone(),
        describe: Value::Null,
        runner,
        window: Window::default(),
        used: None,
        pin,
        gpu_checked: false,
        gpu: Value::Null,
        browser_version: String::new(),
        baseline: validity::baseline(),
        keep_all: args.has("keep"),
        runs: args.num("runs")?.unwrap_or(5.0).max(1.0) as usize,
        warnings: Vec::new(),
    };
    ctx.describe = ctx.runner.describe()?;
    let series = expand(&ctx.describe, args)?;
    if series.is_empty() {
        return Err(Error::usage("no series matches --filter / --target / --params"));
    }
    if busy > 0.10 {
        ctx.warnings.push(validity::warn("Background", format!("{:.0}% of all CPUs were busy before the run; close heavy apps for tighter intervals", busy * 100.0)));
    }
    if !env.power_plan.is_empty() && !env.power_plan.contains("performance") {
        ctx.warnings.push(validity::info("PowerPlan", format!("{} (kept; --power high switches it for the run)", env.power_plan)));
    }
    if env.on_battery {
        ctx.warnings.push(validity::warn("Battery", "on battery: clocks may drop mid-run; plug in"));
    }
    if ctx.baseline.is_none() {
        ctx.warnings.push(validity::info("StaleCalibration", "no calibration on this device yet: the Throttled check has no baseline (run strobe up)"));
    }
    let mut variants = vec![resolve_variant("base", &base_spec, &project)?, resolve_variant("head", &head_spec, &project)?];
    let started = Instant::now();
    let max_time = args.get("max-time").map(duration_ms).transpose()?.map(Duration::from_millis);
    let mut series_out = Vec::new();
    let mut all_decided = true;
    for s in &series {
        let out = run_series(&mut ctx, s, &mut variants, &config, aa, started, max_time)?;
        all_decided &= out["decided"].as_bool().unwrap_or(false);
        series_out.push(out);
    }
    ctx.runner.exit();
    let refresh = host::refresh_at(ctx.window.x + 20, ctx.window.y + 20);
    let result_json = json!({
        "id": exp, "kind": "experiment", "file": file, "created": super::record::now_iso(),
        "env": env, "browser": ctx.browser_version, "gpu": ctx.gpu, "window": ctx.used.as_ref().unwrap_or(&ctx.window), "refresh_hz": refresh,
        "plan": { "pin": pin_label, "runs_per_block": ctx.runs, "aa": aa },
        "variants": variants.iter().map(|v| json!({ "role": v.role, "spec": v.spec, "label": v.label })).collect::<Vec<_>>(),
        "series": series_out, "warnings": ctx.warnings, "decided": all_decided,
    });
    let text = render(&result_json);
    let mut result_json = result_json;
    result_json["text"] = json!(text);
    store::write_json(&dir.join("result.json"), &result_json)?;
    store::append_index(&json!({
        "id": exp, "created": result_json["created"], "file": result_json["file"],
        "verdicts": result_json["series"].as_array().map(|a| a.iter().map(|s| json!({ "series": s["label"], "primary": s["primary"]["metric"], "verdict": s["primary"]["verdict"] })).collect::<Vec<_>>()),
    }))?;
    let mut out = Output::new(text, result_json);
    if args.has("require-verdict") && !all_decided {
        out.code = 5;
    }
    Ok(out)
}

fn run_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value, aa: bool, started: Instant, max_time: Option<Duration>) -> Result<Value> {
    let def = model::metric(&s.primary).ok_or_else(|| Error::usage(format!("{}: unknown primary metric {} (known: {})", s.case, s.primary, model::METRICS.iter().map(|m| m.name).collect::<Vec<_>>().join(", "))))?;
    let target = ctx.describe["targets"][&s.target].clone();
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, aa)?;
    }
    let bound = ctx.args.num("equivalence")?.map(|b| b / 100.0).unwrap_or_else(|| config["metrics"]["equivalence"].as_str().and_then(|e| e.trim_end_matches('%').parse::<f64>().ok()).unwrap_or(2.0) / 100.0);
    let (min_pairs, max_pairs) = match ctx.args.get("pairs") {
        Some("auto") | None => (6usize, 40usize),
        Some(n) => {
            let n = n.parse::<usize>().map_err(|_| Error::usage("--pairs: auto or a number"))?.max(2);
            if n < stats::min_pairs(0.95) {
                ctx.warnings.push(validity::info("FewPairs", format!("--pairs {n}: below {} pairs no interval reaches 95%, so nothing gets a verdict (a smoke run)", stats::min_pairs(0.95))));
            }
            (n, n)
        }
    };
    progress(format!("\n{}  primary {}", s.label(), s.primary));
    let mut blocks: Vec<Block> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut stopped = String::from("max pairs");
    let mut last: Option<Comparison> = None;
    for pair in 0..max_pairs {
        let order: [usize; 2] = if pair % 2 == 0 { [0, 1] } else { [1, 0] };
        let mut idx = [0usize; 2];
        for &v in &order {
            let keep = ctx.keep_all || pair == 0;
            let b = run_block(ctx, s, &variants[v], pair, Set::Timing, keep)?;
            idx[v] = blocks.len();
            blocks.push(b);
        }
        pairs.push((idx[0], idx[1]));
        let primary_pairs: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], &s.primary), val(&blocks[b], &s.primary))).collect();
        let cmp = stats::compare(def, &primary_pairs, bound, 0.95);
        progress(format!(
            "  pair {:>2}  base {:>10}  head {:>10}  {}  [{} … {}]  {}",
            pair + 1,
            fmt::value(val(&blocks[idx[0]], &s.primary), def.unit),
            fmt::value(val(&blocks[idx[1]], &s.primary), def.unit),
            change(&cmp),
            bound_text(cmp.lo, &cmp, def),
            bound_text(cmp.hi, &cmp, def),
            if cmp.n >= min_pairs { cmp.verdict.clone() } else { format!("({} more pairs minimum)", min_pairs - cmp.n) }
        ));
        let n = pair + 1;
        let decided = n >= min_pairs && cmp.decided;
        let hopeless = n >= min_pairs && cmp.needed_pairs.is_some_and(|need| need > max_pairs);
        last = Some(cmp);
        if decided {
            stopped = "primary decided".into();
            break;
        }
        if hopeless && ctx.args.get("pairs").is_none() {
            stopped = "more pairs cannot decide it".into();
            break;
        }
        if max_time.is_some_and(|t| started.elapsed() > t) {
            stopped = "max time".into();
            break;
        }
    }
    let primary = last.ok_or_else(|| Error::msg("no pairs completed"))?;
    let primary_decided = primary.decided && pairs.len() >= min_pairs;
    let mut primary = primary;
    if !primary_decided {
        primary.verdict = if stopped == "more pairs cannot decide it" || stopped == "max pairs" || stopped == "max time" {
            match primary.needed_pairs {
                Some(n) if n != usize::MAX && n > pairs.len() => format!("underpowered: ~{n} pairs needed"),
                _ => "unclear".into(),
            }
        } else {
            "unclear".into()
        };
    }
    // Tail percentiles need enough frames in every block.
    let primary = tail_check(def, &blocks, primary);
    // Secondaries: gated on the primary, Holm-corrected.
    let mut names: Vec<String> = s.metrics.clone();
    for m in DEFAULT_SECONDARIES {
        if !names.iter().any(|n| n == m) && *m != s.primary && !(s.primary == "cpu.process.total" && *m == "cpu.app.total") {
            names.push(m.to_string());
        }
    }
    let mut secondaries: Vec<Comparison> = Vec::new();
    for name in &names {
        let Some(d) = model::metric(name) else { continue };
        if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
            continue;
        }
        let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], name), val(&blocks[b], name))).collect();
        secondaries.push(tail_check(d, &blocks, stats::compare(d, &pp, bound, 0.95)));
    }
    let ps: Vec<f64> = secondaries.iter().map(|c| c.p).collect();
    let rejected = stats::holm(&ps, 0.05);
    for (c, rej) in secondaries.iter_mut().zip(rejected) {
        if c.verdict.starts_with("refused") {
            continue;
        }
        let keep = primary_decided && ((rej && (c.lo > 0.0 || c.hi < 0.0)) || c.verdict.starts_with("same"));
        if !keep {
            c.verdict = "—".into();
            c.decided = false;
        }
    }
    // Smallest n whose exact signed-rank test can clear Holm's first step across the tested set.
    let tests = secondaries.len().max(1);
    let holm_pairs = (2..=60).find(|n| 2.0 / 2f64.powi(*n as i32) <= 0.05 / tests as f64).unwrap_or(60);
    let mut descriptive: Vec<Comparison> = Vec::new();
    for name in DESCRIPTIVE {
        if *name == s.primary || names.iter().any(|n| n == name) {
            continue;
        }
        let Some(d) = model::metric(name) else { continue };
        if !blocks.iter().all(|b| b.values.get(*name).is_some_and(|v| v.is_finite())) {
            continue;
        }
        let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], name), val(&blocks[b], name))).collect();
        let mut c = tail_check(d, &blocks, stats::compare(d, &pp, bound, 0.95));
        if !c.verdict.starts_with("refused") {
            c.verdict = "·".into();
        }
        c.decided = false;
        descriptive.push(c);
    }
    let mut warnings: Vec<Warning> = Vec::new();
    analyse(s, def, &blocks, &pairs, &mut warnings);
    // Attribution: one block per variant with profilers, never timed.
    let mut recordings = json!({});
    if ctx.args.get("probes").unwrap_or("") != "timing" {
        progress("  attribution blocks (CPU stacks, invalidations; not timed)");
        for v in variants.iter() {
            match run_block(ctx, s, v, usize::MAX, Set::Attribution, true) {
                Ok(b) => recordings[v.role] = json!(b.recording),
                Err(e) => warnings.push(validity::warn("Attribution", format!("{} attribution block failed: {}", v.role, e.message))),
            }
        }
    }
    let timing_recordings: Vec<Value> = blocks.iter().filter_map(|b| b.recording.clone().map(|r| json!({ "variant": b.variant, "pair": b.pair, "recording": r }))).collect();
    Ok(json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params,
        "primary": primary, "decided": primary_decided, "secondaries": secondaries, "descriptive": descriptive, "holm_pairs": holm_pairs,
        "pairs": pairs.len(), "launches": blocks.len(), "stopped": stopped, "bound": bound,
        "blocks": blocks, "warnings": warnings, "attribution": recordings, "timing_recordings": timing_recordings,
    }))
}

fn tail_check(def: &MetricDef, blocks: &[Block], mut c: Comparison) -> Comparison {
    if def.min_n > 1 {
        let least = blocks.iter().map(|b| b.values.get("frames.n").copied().unwrap_or(0.0) as usize).min().unwrap_or(0);
        if least < def.min_n {
            c.verdict = format!("refused: {least} frames < {}", def.min_n);
            c.decided = false;
        }
    }
    c
}

fn val(b: &Block, name: &str) -> f64 {
    b.values.get(name).copied().unwrap_or(f64::NAN)
}

fn change(c: &Comparison) -> String {
    if c.relative { fmt::pct(c.estimate) } else { fmt::signed(c.estimate, "count") }
}

fn bound_text(x: f64, c: &Comparison, def: &MetricDef) -> String {
    if c.relative { fmt::pct(x) } else { fmt::signed(x, def.unit) }
}

fn analyse(s: &Series, def: &MetricDef, blocks: &[Block], pairs: &[(usize, usize)], w: &mut Vec<Warning>) {
    for role in ["base", "head"] {
        let v: Vec<f64> = blocks.iter().filter(|b| b.variant == role).map(|b| val(b, &s.primary)).filter(|x| x.is_finite()).collect();
        if v.len() >= 3 {
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt();
            // A CV means nothing near zero: 0.1% vs 0% dropped frames is not variance worth a warning.
            let tiny = matches!(def.unit, "%" | "count") && mean < 1.0;
            if mean > 0.0 && !tiny && sd / mean > 0.10 {
                w.push(validity::warn("HighVariance", format!("{role}: {} varies {:.0}% between launches (CV)", def.name, 100.0 * sd / mean)));
            }
        }
        if let Some(m) = stats::mvalue(&v)
            && m >= 3.2
        {
            w.push(validity::warn("MultimodalDistribution", format!("{role}: {} looks multimodal (mValue {m:.1}); check pinning and background load", def.name)));
        }
    }
    let d: Vec<f64> = pairs.iter().map(|&(a, b)| (val(&blocks[b], &s.primary) / val(&blocks[a], &s.primary)).ln()).filter(|x| x.is_finite()).collect();
    if d.len() >= 8 {
        let rho = validity::spearman(&d);
        if rho.abs() > 0.6 {
            w.push(validity::warn("Drift", format!("the difference trends across pairs (Spearman {rho:.2}): the machine changed during the run")));
        }
    }
    for b in blocks {
        if b.nodes.len() >= 3 {
            let first = b.nodes[0] as f64;
            let last = *b.nodes.last().unwrap_or(&0) as f64;
            if first > 0.0 && (last - first) / first > 0.05 && b.nodes.windows(2).all(|x| x[1] >= x[0]) {
                w.push(validity::warn("StateDrift", format!("DOM nodes grew {:.0} → {:.0} across runs in a {} block: add a reset to the case", first, last, b.variant)));
                break;
            }
        }
    }
}

fn run_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    let target = ctx.describe["targets"][&s.target].clone();
    let kind = target["kind"].as_str().unwrap_or("");
    let app = kind == "app" || kind == "exe";
    let url = if app { String::new() } else { variant_url(v, &target)? };
    let mut window = ctx.window.clone();
    if let Some(vp) = target.get("viewport").filter(|v| v.is_object()) {
        window.width = vp["width"].as_u64().unwrap_or(1400) as u32;
        window.height = vp["height"].as_u64().unwrap_or(900) as u32;
    }
    window.scale = target["scale"].as_f64();
    ctx.used = Some(window.clone());
    let mut service = None;
    let browser = if app {
        let exe = variant_exe(v, &target)?;
        let seed = target["home"].as_str().map(|h| {
            let mut h = h.to_string();
            for (k, val) in &s.params {
                h = h.replace(&format!("{{{k}}}"), &value_text(val));
            }
            let base = Path::new(ctx.describe["file"].as_str().unwrap_or(".")).parent().unwrap_or(Path::new(".")).to_path_buf();
            base.join(h)
        });
        let (b, before) = electron::launch(&exe, seed.as_deref(), &window, ctx.pin)?;
        service = Some(before);
        b
    } else {
        chromium::launch(&window, false, ctx.pin, &[])?
    };
    let result = (|| -> Result<Block> {
        if !ctx.gpu_checked {
            let gpu = validity::software_gpu(&browser)?;
            ctx.gpu = serde_json::to_value(&gpu)?;
            ctx.browser_version = browser.version.clone();
            ctx.gpu_checked = true;
        }
        let page = if app { electron::main_page(&browser)? } else { browser.page()? };
        if let Some(before) = &service {
            electron::guard(before, "after the app opened its window")?;
        }
        if !app && target.get("viewport").is_some_and(Value::is_object) {
            browser.fit_viewport(&page, window.width, window.height)?;
        }
        let id = store::new_id("rec");
        let token = id.trim_start_matches("rec_").to_string();
        let driver = Driver::new(page.clone(), token.clone());
        let current = RefCell::new(String::new());
        let serve = |op: &str, a: &Value| -> Result<Value> {
            match op {
                "driver" => {
                    let call = a["call"].as_str().unwrap_or("");
                    let args = a["args"].as_array().cloned().unwrap_or_default();
                    driver.dispatch(call, &args)
                }
                "phase" => {
                    let name: String = a["name"].as_str().unwrap_or("phase").chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
                    let edge = a["edge"].as_str().unwrap_or("start");
                    let key = current.borrow().clone();
                    if !key.is_empty() {
                        driver.mark(&format!("{key}:phase:{name}:{edge}"))?;
                    }
                    Ok(Value::Bool(true))
                }
                other => Err(Error::msg(format!("unknown runner request {other}"))),
            }
        };
        let t = Duration::from_secs(120);
        ctx.runner.call("block", json!({ "params": s.params, "target": s.target, "variant": v.role, "url": url }), t, &serve)?;
        if target["prepare"].as_bool() == Some(true) {
            ctx.runner.call("prepare", json!({ "endpoint": browser.ws, "target": page.target }), t, &serve).map_err(|e| Error::refused("RunIncomplete", format!("prepare: {}", e.message), "fix the target's prepare in the bench file"))?;
        }
        if !app {
            page.navigate(&url, Duration::from_secs(60))?;
        }
        validity::dev_build(&page)?;
        let speed = validity::speed_check(&page)?;
        let ratio = validity::throttled(speed, ctx.baseline)?;
        validity::window_visible(&page)?;
        ctx.runner.call("setup", json!({}), t, &serve).map_err(|e| Error::refused("RunIncomplete", format!("setup: {}{}", e.message, snapshot(&page, &ctx.exp, "setup")), "fix the bench file's setup"))?;
        let tracing = probe::trace::start(&browser.cdp, set)?;
        let run_timeout = Duration::from_secs(60);
        let mut run_once = |key: &str| -> Result<(f64, f64)> {
            *current.borrow_mut() = key.to_string();
            let cpu0: u64 = browser.processes().iter().map(|p| p.cpu_ns).sum();
            driver.mark(&format!("{key}:start"))?;
            let t0 = host::now_ns();
            ctx.runner
                .call("run", json!({ "case": s.case }), run_timeout, &serve)
                .map_err(|e| Error::refused("RunIncomplete", format!("{} run: {}{}", s.case, e.message, snapshot(&page, &ctx.exp, "run")), "the run threw or timed out; fix the bench file or the app"))?;
            driver.dispatch("frame", &[])?;
            let t1 = host::now_ns();
            driver.mark(&format!("{key}:end"))?;
            let cpu1: u64 = browser.processes().iter().map(|p| p.cpu_ns).sum();
            current.borrow_mut().clear();
            if s.reset {
                ctx.runner
                    .call("reset", json!({ "case": s.case }), run_timeout, &serve)
                    .map_err(|e| Error::refused("RunIncomplete", format!("{} reset: {}{}", s.case, e.message, snapshot(&page, &ctx.exp, "reset")), "fix the case's reset"))?;
            }
            let wall = (t1 - t0) as f64;
            Ok((wall / 1e6, if wall > 0.0 { 100.0 * cpu1.saturating_sub(cpu0) as f64 / wall } else { f64::NAN }))
        };
        // Warmup until the last three runs agree within 5%, at most 10.
        let mut warm: Vec<f64> = Vec::new();
        for i in 0..10 {
            let (ms, _) = run_once(&format!("warm:{i}"))?;
            warm.push(ms);
            if warm.len() >= 3 {
                let last3 = &warm[warm.len() - 3..];
                let med = metrics::median(last3);
                let spread = last3.iter().copied().fold(f64::NEG_INFINITY, f64::max) - last3.iter().copied().fold(f64::INFINITY, f64::min);
                if med > 0.0 && spread / med < 0.05 {
                    break;
                }
            }
        }
        if set == Set::Attribution {
            probe::cpu::start(&page)?;
        }
        let mut run_ms = Vec::new();
        let mut cpu: BTreeMap<String, f64> = BTreeMap::new();
        let mut walls: BTreeMap<String, f64> = BTreeMap::new();
        let mut nodes = Vec::new();
        for i in 0..ctx.runs {
            nodes.push(driver.nodes().unwrap_or(0));
            let (ms, c) = run_once(&format!("run:{i}"))?;
            run_ms.push(ms);
            walls.insert(format!("run:{i}"), ms);
            cpu.insert(format!("run:{i}"), c);
        }
        // The window must have stayed visible for the whole block.
        validity::window_visible(&page)?;
        let private_mb = browser.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
        let heap_mb = page.send("Runtime.getHeapUsage", json!({})).ok().and_then(|h| h["usedSize"].as_f64()).map(|b| b / (1024.0 * 1024.0));
        let dom_nodes = driver.nodes().ok();
        let dir = if keep { store::recording_dir(&id)? } else { host::paths::run().join(&id) };
        std::fs::create_dir_all(&dir)?;
        if set == Set::Attribution {
            let profile = probe::cpu::stop(&page)?;
            store::write_json(&dir.join("cpu-0.cpuprofile"), &profile)?;
        }
        let trace = dir.join("trace.pftrace");
        tracing.stop(&trace)?;
        let _ = ctx.runner.call("close", json!({}), t, &serve);
        let data = metrics::read(&trace, &token)?;
        let mut extra = BTreeMap::new();
        extra.insert("run.duration".to_string(), walls);
        extra.insert("cpu.app.total".to_string(), cpu);
        let mut values = metrics::block_values(&data, "run", &extra);
        if private_mb > 0.0 {
            values.insert("memory.app.private".into(), private_mb);
        }
        if let Some(h) = heap_mb {
            values.insert("js.heap.used".into(), h);
        }
        if let Some(n) = dom_nodes {
            values.insert("dom.nodes".into(), n as f64);
        }
        let phases = metrics::phase_frames(&data);
        let frames = metrics::run_frames(&data);
        let intervals = metrics::run_intervals(&data);
        let recording = if keep {
            store::write_json(
                &dir.join("meta.json"),
                &json!({
                    "id": id, "kind": "block", "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label,
                    "target": url, "probes": set, "token": token, "values": values, "duration_ms": run_ms.iter().sum::<f64>() as u64,
                    "created": super::record::now_iso(), "valid": true, "env": host::env(), "browser": browser.version,
                }),
            )?;
            Some(id)
        } else {
            let _ = std::fs::remove_dir_all(&dir);
            None
        };
        Ok(Block { variant: v.role.to_string(), pair, values, run_ms, warmup: warm.len(), speed_ratio: ratio, nodes, recording, phases, frames, intervals })
    })();
    browser.close();
    if let Some(before) = &service {
        electron::guard(before, "after a block")?;
    }
    result
}

/// `--power high`: the High performance plan for the run, then the previous plan again (also on error).
/// Not the default: AMD X3D core parking relies on Balanced to keep work on the V-Cache CCD.
struct PowerPlan {
    previous: String,
}

impl PowerPlan {
    fn high() -> Result<PowerPlan> {
        if !cfg!(windows) {
            return Err(Error::usage("--power high only switches Windows power plans"));
        }
        let active = host::output(Command::new("powercfg").arg("/getactivescheme")).unwrap_or_default();
        let previous = active.split_whitespace().find(|w| w.len() == 36 && w.matches('-').count() == 4).map(str::to_string).ok_or_else(|| Error::msg("powercfg: could not read the active power plan"))?;
        if !Command::new("powercfg").args(["/setactive", "SCHEME_MIN"]).stdout(Stdio::null()).status()?.success() {
            return Err(Error::msg("powercfg could not switch to High performance"));
        }
        progress("  power plan: High performance for this run");
        Ok(PowerPlan { previous })
    }
}

impl Drop for PowerPlan {
    fn drop(&mut self) {
        let _ = Command::new("powercfg").args(["/setactive", &self.previous]).stdout(Stdio::null()).status();
    }
}

/// What the page showed when a step failed: its URL and a screenshot saved with the experiment.
fn snapshot(page: &Page, exp: &str, step: &str) -> String {
    use base64::Engine;
    let quick = Duration::from_secs(5);
    let url = page.eval_timeout("location.href", quick).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "unresponsive".into());
    let shot = page
        .cdp
        .send_timeout("Page.captureScreenshot", json!({ "format": "png" }), Some(&page.session), quick)
        .ok()
        .and_then(|r| r["data"].as_str().and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok()));
    let path = store::experiment_dir(exp).ok().map(|d| d.join(format!("failed-{step}-{}.png", store::new_id("shot").trim_start_matches("shot_"))));
    match (shot, path) {
        (Some(png), Some(path)) if std::fs::write(&path, &png).is_ok() => format!("\n  page: {url}\n  screenshot: {}", path.display()),
        _ => format!("\n  page: {url}"),
    }
}

/// The executable an app target runs for a variant: the variant's own (`app:`/`exe:`), else the target's.
fn variant_exe(v: &Variant, target: &Value) -> Result<PathBuf> {
    if let Kind::Exe(p) = &v.kind {
        return Ok(p.clone());
    }
    match target["kind"].as_str() {
        Some("app") => {
            let id = target["id"].as_str().unwrap_or("");
            electron::find_app(id).ok_or_else(|| Error::usage(format!("{id} is not installed")))
        }
        Some("exe") => Ok(PathBuf::from(target["path"].as_str().unwrap_or(""))),
        _ => Err(Error::usage("not an app target")),
    }
}

fn variant_url(v: &Variant, target: &Value) -> Result<String> {
    let kind = target["kind"].as_str().unwrap_or("");
    if let Kind::Url(href) = &v.kind {
        if kind == "url" {
            return Ok(href.clone());
        }
        return Ok(join_url(href, target["url"].as_str().unwrap_or("/")));
    }
    match kind {
        "url" => Ok(target["href"].as_str().unwrap_or("about:blank").to_string()),
        "web" => {
            let base = v.base_url.clone().ok_or_else(|| Error::msg(format!("{}: no server for this variant (set serve in strobe.json)", v.role)))?;
            Ok(join_url(&base, target["url"].as_str().unwrap_or("/")))
        }
        other => Err(Error::msg(format!("target kind {other} is not supported yet"))),
    }
}

fn join_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

fn expand(describe: &Value, args: &Args) -> Result<Vec<Series>> {
    let filter = args.get("filter");
    let only_target = args.get("target");
    let fixed: BTreeMap<String, String> = args
        .get("params")
        .map(|p| p.split(',').filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect())
        .unwrap_or_default();
    let params = describe["params"].as_object().cloned().unwrap_or_default();
    let mut combos: Vec<BTreeMap<String, Value>> = vec![BTreeMap::new()];
    for (k, values) in &params {
        let values: Vec<Value> = values.as_array().cloned().unwrap_or_default().into_iter().filter(|v| fixed.get(k).is_none_or(|f| value_text(v) == *f)).collect();
        combos = combos.into_iter().flat_map(|c| values.iter().map(move |v| { let mut c = c.clone(); c.insert(k.clone(), v.clone()); c })).collect();
    }
    let mut out = Vec::new();
    let targets = describe["targets"].as_object().cloned().unwrap_or_default();
    for (case, c) in describe["cases"].as_object().cloned().unwrap_or_default() {
        if filter.is_some_and(|f| !glob(f, &case)) {
            continue;
        }
        for target in targets.keys() {
            if only_target.is_some_and(|t| t != target) {
                continue;
            }
            for p in &combos {
                out.push(Series {
                    case: case.clone(),
                    target: target.clone(),
                    params: p.clone(),
                    primary: args.get("primary").map(str::to_string).unwrap_or_else(|| c["primary"].as_str().unwrap_or("frames.presented.p95").to_string()),
                    metrics: c["metrics"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                    reset: c["reset"].as_bool().unwrap_or(false),
                });
            }
        }
    }
    Ok(out)
}

fn value_text(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(pos) if i > 0 || pos == 0 => rest = &rest[pos + part.len()..],
            _ => return false,
        }
    }
    parts.last().is_none_or(|l| l.is_empty() || text.ends_with(l))
}

fn project_root(file: &Path) -> PathBuf {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d.join("strobe.json").is_file() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    host::output(Command::new("git").arg("-C").arg(file.parent().unwrap_or(Path::new("."))).args(["rev-parse", "--show-toplevel"]))
        .map(PathBuf::from)
        .unwrap_or_else(|| file.parent().unwrap_or(Path::new(".")).to_path_buf())
}

fn read_config(root: &Path) -> Value {
    std::fs::read_to_string(root.join("strobe.json")).ok().and_then(|t| serde_json::from_str(&strip_jsonc(&t)).ok()).unwrap_or(Value::Null)
}

fn strip_jsonc(t: &str) -> String {
    t.lines().map(|l| if l.trim_start().starts_with("//") { "" } else { l }).collect::<Vec<_>>().join("\n")
}

fn resolve_variant(role: &'static str, spec: &str, project: &Path) -> Result<Variant> {
    let (kind, label, root) = if let Some(href) = spec.strip_prefix("url:") {
        (Kind::Url(href.to_string()), href.to_string(), None)
    } else if let Some(id) = spec.strip_prefix("app:") {
        let exe = electron::find_app(id).ok_or_else(|| Error::usage(format!("{id} is not installed (opencode, opencode-beta, opencode-dev)")))?;
        (Kind::Exe(exe), spec.to_string(), None)
    } else if let Some(p) = spec.strip_prefix("exe:") {
        (Kind::Exe(PathBuf::from(p)), p.to_string(), None)
    } else if let Some(r) = spec.strip_prefix("git:") {
        (Kind::Git(r.to_string()), r.to_string(), None)
    } else {
        let p = if Path::new(spec).is_absolute() { PathBuf::from(spec) } else { std::env::current_dir()?.join(spec) };
        if p.is_dir() {
            let root = std::fs::canonicalize(&p)?;
            let sha = host::output(Command::new("git").arg("-C").arg(&root).args(["rev-parse", "--short=10", "HEAD"])).unwrap_or_default();
            let dirty = host::output(Command::new("git").arg("-C").arg(&root).args(["status", "--porcelain", "--untracked-files=no"])).is_some_and(|s| !s.is_empty());
            let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            (Kind::Dir, format!("{name}@{sha}{}", if dirty { " (dirty)" } else { "" }), Some(root))
        } else {
            (Kind::Git(spec.to_string()), spec.to_string(), None)
        }
    };
    let mut v = Variant { role, spec: spec.to_string(), kind, label, root, base_url: None, server: None };
    if let Kind::Git(r) = &v.kind {
        let sha = host::output(Command::new("git").arg("-C").arg(project).args(["rev-parse", "--short=10", r])).ok_or_else(|| Error::usage(format!("{spec} is neither a directory nor a git ref in {}", project.display())))?;
        v.label = format!("{r}@{sha}");
    }
    Ok(v)
}

/// Builds and serves a variant for `web` targets (once per experiment).
fn prepare_variant(v: &mut Variant, target: &Value, config: &Value, aa: bool) -> Result<()> {
    if target["kind"].as_str() != Some("web") || v.base_url.is_some() || matches!(v.kind, Kind::Url(_)) {
        return Ok(());
    }
    if let Kind::Git(r) = &v.kind {
        v.root = Some(worktree(r)?);
    }
    let root = v.root.clone().ok_or_else(|| Error::msg("variant has no directory"))?;
    let _ = aa;
    if let Some(build) = config.get("build").filter(|b| b.is_object()) {
        build_variant(v.role, &root, build)?;
    }
    let serve = config.get("serve").filter(|s| s.is_object()).ok_or_else(|| Error::usage("web targets need \"serve\" in strobe.json (a command with {port})"))?;
    let port = free_port()?;
    let cmd = serve["command"].as_str().unwrap_or("").replace("{port}", &port.to_string());
    let cwd = root.join(serve["cwd"].as_str().unwrap_or("."));
    progress(format!("  serve {} on :{port}", v.role));
    let child = shell(&cmd).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| Error::msg(format!("serve {}: {e}", v.role)))?;
    v.server = Some(child);
    let ready = serve["ready"].as_str().unwrap_or("http://127.0.0.1:{port}/").replace("{port}", &port.to_string());
    let deadline = Instant::now() + Duration::from_secs(90);
    while !http_ok(&ready) {
        if Instant::now() > deadline {
            return Err(Error::msg(format!("{}: the server did not answer {ready} in 90 s", v.role)));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    v.base_url = Some(format!("http://127.0.0.1:{port}"));
    Ok(())
}

fn build_variant(role: &str, root: &Path, build: &Value) -> Result<()> {
    let cmd = build["command"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return Ok(());
    }
    let cwd = root.join(build["cwd"].as_str().unwrap_or("."));
    let key = build_key(root, build);
    let marker = host::paths::sub("builds").join(format!("{}.key", hash_str(&root.to_string_lossy())));
    let output = build["output"].as_str().map(|o| root.join(o));
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(key.as_str()) && output.as_ref().is_none_or(|o| o.exists()) {
        progress(format!("  build {role}: unchanged, reusing"));
        return Ok(());
    }
    progress(format!("  build {role}: {cmd}"));
    let mut c = shell(cmd);
    c.current_dir(&cwd);
    if let Some(env) = build["env"].as_object() {
        for (k, v) in env {
            c.env(k, v.as_str().unwrap_or(""));
        }
    }
    let out = c.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr).lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(Error::msg(format!("build {role} failed:\n{tail}")));
    }
    std::fs::write(marker, key)?;
    Ok(())
}

fn build_key(root: &Path, build: &Value) -> String {
    let git = |a: &[&str]| host::output(Command::new("git").arg("-C").arg(root).args(a)).unwrap_or_default();
    hash_str(&format!("{}\n{}\n{}\n{}", git(&["rev-parse", "HEAD"]), git(&["status", "--porcelain"]), git(&["diff"]), build))
}

fn hash_str(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn worktree(r: &str) -> Result<PathBuf> {
    let sha = host::output(Command::new("git").args(["rev-parse", r])).ok_or_else(|| Error::usage(format!("unknown git ref {r}")))?;
    let dir = host::paths::sub("builds").join(&sha[..12.min(sha.len())]);
    if !dir.join(".git").exists() {
        progress(format!("  worktree {r} → {}", dir.display()));
        let ok = Command::new("git").args(["worktree", "add", "--detach"]).arg(&dir).arg(&sha).stdout(Stdio::null()).status()?.success();
        if !ok {
            return Err(Error::msg(format!("git worktree add for {r} failed")));
        }
        if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
            progress("  bun install (worktree)");
            Command::new(crate::runner::find_bun().unwrap_or_else(|| "bun".into())).arg("install").current_dir(&dir).stdout(Stdio::null()).status()?;
        }
    }
    Ok(dir)
}

fn shell(cmd: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/d", "/s", "/c", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    }
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn http_ok(url: &str) -> bool {
    use std::io::{Read, Write};
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let Ok(mut s) = std::net::TcpStream::connect(host) else { return false };
    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
    if write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 16];
    let n = s.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.1 3") || head.starts_with("HTTP/1.0 2")
}

fn kill_tree(child: &mut Child) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn pinning(spec: &str, topo: &host::Topology) -> Result<(Option<u64>, Option<u64>, String)> {
    let all: u64 = if topo.logical >= 64 { u64::MAX } else { (1u64 << topo.logical) - 1 };
    let pick = |g: &host::CacheGroup, i: usize| (Some(g.mask), Some(all & !g.mask), format!("CCD{i} ({} MB L3, {} CPUs) · runner on the other CPUs", g.l3_mb, g.cpus));
    match spec {
        "none" => Ok((None, None, "unpinned".into())),
        "auto" => match topo.vcache() {
            Some(g) => {
                let i = topo.groups.iter().position(|x| x.mask == g.mask).unwrap_or(0);
                let (a, b, l) = pick(g, i);
                Ok((a, b, format!("{l} · V-Cache")))
            }
            None => Ok((None, None, "unpinned (no V-Cache CCD)".into())),
        },
        s if s.starts_with("ccd") => {
            let i: usize = s[3..].parse().map_err(|_| Error::usage("--pin ccd0|ccd1|…"))?;
            let g = topo.groups.get(i).ok_or_else(|| Error::usage(format!("no {s}: this CPU has {} L3 groups", topo.groups.len())))?;
            Ok(pick(g, i))
        }
        s => {
            let mut mask = 0u64;
            for part in s.split(',') {
                let (a, b) = part.split_once('-').unwrap_or((part, part));
                let (a, b): (u32, u32) = (a.trim().parse().map_err(|_| Error::usage("--pin: auto, none, ccdN or a CPU list like 0-7"))?, b.trim().parse().map_err(|_| Error::usage("--pin list"))?);
                for c in a..=b.min(63) {
                    mask |= 1 << c;
                }
            }
            Ok((Some(mask), Some(all & !mask), format!("CPUs {s}")))
        }
    }
}

fn render(r: &Value) -> String {
    let mut out = String::new();
    let file = r["file"].as_str().map(|f| Path::new(f).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).unwrap_or_default();
    let base = &r["variants"][0];
    let head = &r["variants"][1];
    let env = &r["env"];
    for s in r["series"].as_array().cloned().unwrap_or_default() {
        out += &format!("strobe bench {file} · {}   base {}   head {}\n", s["label"].as_str().unwrap_or(""), base["label"].as_str().unwrap_or(""), head["label"].as_str().unwrap_or(""));
        out += &format!("env    {} · {} · {} logical CPUs\n", env["os"].as_str().unwrap_or(""), env["cpu"].as_str().unwrap_or(""), env["logical_cpus"]);
        let gpu = r["gpu"]["devices"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
        let refresh = r["refresh_hz"].as_u64().map(|h| format!(" · display {h} Hz")).unwrap_or_default();
        out += &format!("       {} · {} · window {}×{}{}\n", r["browser"].as_str().unwrap_or(""), gpu, r["window"]["width"], r["window"]["height"], refresh);
        out += &format!("plan   {} · power plan {} · visible, unthrottled\n", r["plan"]["pin"].as_str().unwrap_or(""), env["power_plan"].as_str().unwrap_or("?"));
        out += &format!(
            "blocks {} pairs ({} fresh launches) · warmup until steady · {} runs per block · stopped: {}\n\n",
            s["pairs"], s["launches"], r["plan"]["runs_per_block"], s["stopped"].as_str().unwrap_or("")
        );
        out += &format!("{}{}{}{}{}{}\n", fmt::pad("metric", 28), fmt::pad("base", 11), fmt::pad("head", 11), fmt::pad("Δ", 9), fmt::pad("95% CI", 18), "verdict");
        let mut rows = vec![(true, s["primary"].clone())];
        rows.extend(s["secondaries"].as_array().cloned().unwrap_or_default().into_iter().map(|c| (false, c)));
        let descriptive = s["descriptive"].as_array().cloned().unwrap_or_default();
        let tested = rows.len();
        rows.extend(descriptive.into_iter().map(|c| (false, c)));
        for (i, (primary, c)) in rows.into_iter().enumerate() {
            if i == tested {
                out += &fmt::pad("  · descriptive (not tested)", 28);
                out.push('\n');
            }
            let name = c["metric"].as_str().unwrap_or("");
            let unit = model::metric(name).map(|m| m.unit).unwrap_or("");
            let rel = c["relative"].as_bool().unwrap_or(false);
            let est = c["estimate"].as_f64().unwrap_or(f64::NAN);
            let (lo, hi) = (c["lo"].as_f64().unwrap_or(f64::NAN), c["hi"].as_f64().unwrap_or(f64::NAN));
            // A percentage compared as a difference is in percentage points.
            let abs_unit = if unit == "%" { "pp" } else { unit };
            let delta = if rel { fmt::pct(est) } else { fmt::signed(est, abs_unit) };
            let ci = if rel { format!("{} … {}", fmt::pct(lo), fmt::pct(hi)) } else { format!("{} … {}", fmt::signed(lo, abs_unit), fmt::signed(hi, abs_unit)) };
            out += &format!(
                "{}{}{}{}{}{}\n",
                fmt::pad(&format!("{} {name}", if primary { "★" } else { " " }), 28),
                fmt::pad(&fmt::value(c["base"].as_f64().unwrap_or(f64::NAN), unit), 11),
                fmt::pad(&fmt::value(c["head"].as_f64().unwrap_or(f64::NAN), unit), 11),
                fmt::pad(&delta, 9),
                fmt::pad(&ci, 18),
                c["verdict"].as_str().unwrap_or("")
            );
        }
        out += "\n★ primary, declared before the run · secondaries gated on the primary, Holm-corrected\n";
        out += "  intervals over pairs of fresh launches (Wilcoxon signed-rank, Hodges–Lehmann)\n";
        let holm = s["holm_pairs"].as_u64().unwrap_or(0);
        if s["pairs"].as_u64().unwrap_or(0) < holm && s["decided"].as_bool() == Some(true) {
            out += &format!("  secondaries can only clear Holm from {holm} pairs: add --pairs {holm} for their verdicts\n");
        }
        let warns: Vec<Value> = r["warnings"].as_array().cloned().unwrap_or_default().into_iter().chain(s["warnings"].as_array().cloned().unwrap_or_default()).collect();
        if !warns.is_empty() {
            out += "\nwarnings\n";
            for w in warns {
                out += &format!("{} {:<24}{}\n", if w["level"] == "warn" { "!" } else { "i" }, w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or(""));
            }
        }
        let attr = &s["attribution"];
        if attr.get("base").is_some() || attr.get("head").is_some() {
            out += &format!(
                "\nwhy    attribution blocks: base {} · head {}\n       strobe query {} hotspots --by package\n",
                attr["base"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("rec_…")
            );
        }
        out += "\n";
    }
    out += &format!("{} · strobe report experiment {} --open\n", r["id"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or(""));
    out
}
