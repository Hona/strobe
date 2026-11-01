//! `strobe bench`: experiments with verdicts (SPEC §9). Blocks of fresh launches, alternating A B, B A.

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::config;
use crate::driver::Driver;
use crate::host;
use crate::metrics;
use crate::model::{self, Error, MetricDef, Result};
use crate::png;
use crate::probe::{self, Set};
use crate::runner::Runner;
use crate::stats::{self, Comparison};
use crate::store;
use crate::target::chromium::{self, Page, Window};
use crate::target::electron;
use crate::validity::{self, Warning};
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod launch;
mod script;

/// Secondaries that get tested (Holm across these only). Fewer tests keep the correction reachable.
const DEFAULT_SECONDARIES: &[&str] = &["frames.interval.p95", "frames.dropped.pct", "main.busy", "interaction.p95", "cpu.app.total"];

/// Where main-thread work went, from the Performance domain's cumulative counters (metric, counter,
/// scale): seconds to ms, counts as they are. Cheap enough for every block, so they carry intervals.
const WORK_PARTS: [(&str, &str, f64); 5] = [
    ("main.script", "ScriptDuration", 1e3),
    ("main.layout", "LayoutDuration", 1e3),
    ("main.style", "RecalcStyleDuration", 1e3),
    ("layout.count", "LayoutCount", 1.0),
    ("style.count", "RecalcStyleCount", 1.0),
];

/// The page's side of `count.<name>` metrics (SPEC §8.4): `globalThis.__strobeCount?.(name, n)` in app
/// code costs one optional call when strobe is not attached, and adds to this run's totals when it is.
const COUNT_SCRIPT: &str = "globalThis.__strobeCount ??= (name, n = 1) => { const c = (globalThis.__strobeCounts ??= {}); c[name] = (c[name] ?? 0) + n }";

/// `canvas.*` metrics (SPEC §8.4): when the page takes a WebGPU canvas's texture for a new frame. Calls in one
/// task are one frame, however many canvases it draws.
const CANVAS_SCRIPT: &str = r#"(() => {
  if (globalThis.__strobeCanvasHooked || typeof GPUCanvasContext === "undefined") return
  globalThis.__strobeCanvasHooked = true
  globalThis.__strobeCanvas ??= []
  const take = GPUCanvasContext.prototype.getCurrentTexture
  let open = false
  GPUCanvasContext.prototype.getCurrentTexture = function () {
    if (!open) {
      open = true
      globalThis.__strobeCanvas.push(performance.now())
      queueMicrotask(() => (open = false))
    }
    return take.call(this)
  }
})()"#;

/// A run with this much other work on the target's CPUs (% of one core) flags its block `Background`.
const BACKGROUND_PCT: f64 = 25.0;

/// Shown with intervals but never tested: context, not claims.
const DESCRIPTIVE: &[&str] = &[
    "frames.interval.p50",
    "frames.presented.p50",
    "frames.presented.p95",
    "frames.presented.p99",
    "main.tasks.long",
    "main.work",
    "main.script",
    "main.layout",
    "main.style",
    "layout.count",
    "style.count",
    "scroll.latency.p95",
    "run.duration",
    "cpu.renderer",
    "cpu.gpu",
    "cpu.browser",
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
    /// A launch case's `launch` spec (`until`, `milestones`, `before`, …); null for a run case.
    launch: Value,
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
    App(String),
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

/// Warmup runs at most, before strobe measures anyway and flags the block (`NotSteady`).
const MAX_WARMUP: usize = 10;

/// Pairs before a series may stop as underpowered.
const FUTILITY_FROM: usize = 12;

/// Planned looks (pairs) for `--pairs auto`. Verdicts happen only here.
const LOOKS: [usize; 5] = [8, 12, 18, 26, 40];

/// Pocock's constant nominal level for 5 looks at 5% overall, two-sided: each look's interval is
/// 98.4%. Looking after every pair at 95% called A/A runs different 10% of the time (simulated, and
/// once on adais); five Pocock looks keep it at 1.5–3.7% and still catch a 3% effect by pair 8–9.
const POCOCK_ALPHA: f64 = 0.0158;

/// One run as the block saw it: wall time, the target's CPU (% of a core), renderer main-thread
/// task time (ms), and its span on the host clock.
#[derive(Clone, Debug)]
struct Run {
    ms: f64,
    cpu: f64,
    task: f64,
    /// Per run, the Performance domain's other counters in `WORK_PARTS` order.
    parts: [f64; 5],
    /// CPU (% of a core) of the browser's renderer, GPU and browser processes, in `PROCESS_ROLES` order.
    roles: [f64; 3],
    t0: u64,
    t1: u64,
    /// The run with its reset (ms): what it cost the block.
    total: f64,
}

/// Chromium process types (`SystemInfo.getProcessInfo`) and the metric each one's CPU goes to.
const PROCESS_ROLES: [(&str, &str); 3] = [("renderer", "cpu.renderer"), ("GPU", "cpu.gpu"), ("browser", "cpu.browser")];

/// Each Chromium process's index in `PROCESS_ROLES`, by pid; empty where the browser does not say.
fn process_roles(browser: &chromium::Browser) -> BTreeMap<u32, usize> {
    let info = browser.cdp.send("SystemInfo.getProcessInfo", json!({}), None).unwrap_or(Value::Null);
    info["processInfo"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["id"].as_u64()? as u32, PROCESS_ROLES.iter().position(|(t, _)| p["type"].as_str() == Some(*t))?)))
        .collect()
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
    /// Wall time of each step of the block (ms), in order: where a bench's time goes.
    steps: Vec<(String, f64)>,
    /// The busiest processes outside strobe's own tree during the block: name and % of one core.
    background: Vec<(String, f64)>,
    /// A leak run's growth verdicts and suspects (memory blocks only).
    #[serde(skip_serializing_if = "Option::is_none")]
    leak: Option<Value>,
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
    config: Value,
    awake: host::Awake,
    /// Time spent waiting for the screen to unlock (ms): not part of any series' run time.
    waited_ms: f64,
    /// Series and variants that had their discarded first launch (launch cases).
    launched: std::collections::BTreeSet<String>,
    /// Warm launch homes per series and variant, removed when the experiment ends.
    homes: BTreeMap<String, PathBuf>,
    /// `--check`: one block per series on head, one run per case, no statistics.
    check: bool,
    /// A pixels block's snapshot folder (`--pixels`); `None` in every timed block.
    pixels_dir: Option<PathBuf>,
}

pub fn run(args: &Args) -> Result<Output> {
    if let Some(range) = args.get("bisect") {
        return super::bisect::run(args, range);
    }
    let file = args.pos.first().ok_or_else(|| Error::usage("bench needs a bench file: strobe bench perf/x.bench.ts --base <variant> --head <variant>"))?;
    let file = std::fs::canonicalize(file).map_err(|_| Error::usage(format!("no such bench file: {file}")))?;
    let head_spec = args.get("head").unwrap_or(".").to_string();
    let aa = args.has("aa");
    // `--probes memory`: a leak run per variant (SPEC §8.6); `--base` is optional there.
    let leak_run = args.get("probes") == Some("memory");
    let check = args.has("check");
    let base_spec = match (args.get("base"), aa) {
        _ if check && (aa || args.get("base").is_some()) => return Err(Error::usage("--check runs each case once on --head, with nothing to compare; drop --base and --aa")),
        _ if check => None,
        (Some(b), false) => Some(b.to_string()),
        (None, true) => Some(head_spec.clone()),
        (Some(_), true) => return Err(Error::usage("--aa runs --head against itself; drop --base")),
        (None, false) if leak_run => None,
        (None, false) if args.get("head").is_some() => {
            return Err(Error::usage("--head alone compares nothing: add --base <variant>, or --check to run each case once and see that it passes"));
        }
        (None, false) => return Err(Error::usage("say what to compare: --base <variant> (or --aa for the same build on both sides, or --check to validate the file)")),
    };
    let exp = args.get("id").map(str::to_string).unwrap_or_else(|| store::new_id("exp"));
    let dir = store::experiment_dir(&exp)?;
    super::progress_to(Some(dir.join("progress.log")));
    let _lock = validity::lock()?;
    chromium::reap_orphans();
    let project = config::project_root(file.parent().unwrap_or(Path::new(".")));
    let config = config::load(&project);
    let _power = match args.get("power").unwrap_or("keep") {
        "keep" => None,
        "high" => Some(PowerPlan::high()?),
        other => return Err(Error::usage(format!("--power: keep or high, not {other}"))),
    };
    let env = host::env();
    let topo = host::topology();
    let (pin, runner_pin, pin_label) = pinning(args.get("pin").unwrap_or("auto"), &topo)?;
    let _own = OwnAffinity(runner_pin.and_then(host::set_affinity));
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
        // A leak run's N: S0, N runs, S1, N runs, S2.
        runs: if check {
            1
        } else if leak_run {
            args.num("repeat")?.unwrap_or(10.0).max(3.0) as usize
        } else {
            args.num("runs")?.unwrap_or(5.0).max(1.0) as usize
        },
        warnings: Vec::new(),
        config: config.clone(),
        awake: host::Awake::start(),
        waited_ms: 0.0,
        launched: Default::default(),
        homes: BTreeMap::new(),
        check,
        pixels_dir: None,
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
    if let Some(vm) = &env.virtual_machine {
        ctx.warnings.push(validity::warn("VirtualMachine", format!("running in a virtual machine ({vm}): its timings move with the host's load; prefer bare metal")));
    }
    match validity::calibration() {
        None => ctx.warnings.push(validity::info("StaleCalibration", "no calibration on this device yet: the Throttled check has no baseline (run strobe up)")),
        Some(c) if c["ok"] == false => {
            ctx.warnings.push(validity::warn("StaleCalibration", "the last calibration failed: strobe could not recover its known costs here (run strobe status --calibrate)"))
        }
        Some(c) if c["at"].as_str().is_some_and(|at| at < super::record::iso(super::record::now_secs().saturating_sub(7 * 86_400)).as_str()) => {
            ctx.warnings.push(validity::info("StaleCalibration", format!("calibrated {} (more than 7 days ago; strobe up recalibrates)", c["at"].as_str().unwrap_or(""))))
        }
        Some(_) => {}
    }
    let mut variants = match &base_spec {
        Some(b) => vec![resolve_variant("base", b, &project, &config)?, resolve_variant("head", &head_spec, &project, &config)?],
        None => vec![resolve_variant("head", &head_spec, &project, &config)?],
    };
    let _ = HARNESS.set(harness(&file, &project, &config));
    let started = Instant::now();
    let max_time = args.get("max-time").map(duration_ms).transpose()?.map(Duration::from_millis);
    let mut series_out = Vec::new();
    let mut all_decided = true;
    let pixels_only = args.has("pixels-only");
    if pixels_only && variants.len() < 2 {
        return Err(Error::usage("--pixels-only compares base with head: add --base <variant>"));
    }
    for s in &series {
        let out = if check {
            check_series(&mut ctx, s, &mut variants, &config)?
        } else if pixels_only {
            let target = ctx.describe["targets"][&s.target].clone();
            for v in variants.iter_mut() {
                prepare_variant(v, &target, &config, false)?;
            }
            let pixels = pixels_series(&mut ctx, s, &variants)?;
            json!({ "label": s.label(), "case": s.case, "kind": "pixels", "pixels": pixels, "decided": pixels.as_array().is_some_and(|a| a.iter().all(|p| p["state"] == "same")) })
        } else if leak_run {
            leak_series(&mut ctx, s, &mut variants, &config)?
        } else {
            run_series(&mut ctx, s, &mut variants, &config, aa, started, max_time)?
        };
        all_decided &= out["decided"].as_bool().unwrap_or(false);
        series_out.push(out);
    }
    ctx.runner.exit();
    for home in std::mem::take(&mut ctx.homes).into_values() {
        let _ = std::fs::remove_dir_all(home);
    }
    if check {
        return check_output(&exp, &dir, &file, series_out, ctx.warnings);
    }
    if pixels_only {
        let mut text = String::new();
        for s in &series_out {
            text += &format!("{}\n", s["label"].as_str().unwrap_or(""));
            for p in s["pixels"].as_array().into_iter().flatten() {
                text += &pixel_line(p);
            }
        }
        text += &format!("\n{exp}\n");
        let result = json!({ "id": exp, "kind": "pixels", "file": file, "created": super::record::now_iso(), "series": series_out, "warnings": ctx.warnings, "text": text });
        store::write_json(&dir.join("result.json"), &result)?;
        let mut out = Output::new(text, result);
        if !all_decided {
            out.code = 1;
        }
        return Ok(out);
    }
    let refresh = host::refresh_at(ctx.window.x + 20, ctx.window.y + 20);
    let result_json = json!({
        "id": exp, "kind": "experiment", "file": file, "created": super::record::now_iso(),
        "env": env, "browser": ctx.browser_version, "gpu": ctx.gpu, "window": ctx.used.as_ref().unwrap_or(&ctx.window), "refresh_hz": refresh,
        "plan": {
            "pin": pin_label, "runs_per_block": ctx.runs, "aa": aa,
            "looks": if args.get("pairs").is_none_or(|p| p == "auto") { LOOKS.to_vec() } else { vec![args.num("pairs")?.unwrap_or(0.0) as usize] },
            "alpha": if args.get("pairs").is_none_or(|p| p == "auto") { POCOCK_ALPHA } else { 0.05 },
        },
        "variants": variants.iter().map(|v| json!({ "role": v.role, "spec": v.spec, "label": v.label, "subject": subject(v) })).collect::<Vec<_>>(),
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
    // Handles, not payloads (SPEC §17): per-frame arrays stay in result.json for reports and views.
    let mut brief = result_json;
    for b in brief["series"].as_array_mut().into_iter().flatten().flat_map(|s| s["blocks"].as_array_mut().into_iter().flatten()) {
        if let Some(o) = b.as_object_mut() {
            for k in ["frames", "intervals", "phases"] {
                o.remove(k);
            }
        }
    }
    brief["result"] = json!(dir.join("result.json"));
    let mut out = Output::new(text, brief);
    if args.has("require-verdict") && !all_decided {
        out.code = 5;
    }
    Ok(out)
}

/// `--check`: one block on head with one run, no warmup and no statistics. A failing case is reported
/// with what timed out and its screenshot, and the next case runs.
fn check_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value) -> Result<Value> {
    let target = ctx.describe["targets"][&s.target].clone();
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, false)?;
    }
    let started = Instant::now();
    let r = block_on_screen(ctx, s, &variants[0], 0, Set::Lean, false);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    let (passed, error, values) = match r {
        Ok(b) => (true, Value::Null, json!(b.values)),
        Err(e) => (false, json!(e.message), Value::Null),
    };
    progress(format!("  {} {}{}", if passed { "pass" } else { "FAIL" }, s.label(), error.as_str().map(|e| format!("  {e}")).unwrap_or_default()));
    // `--why always` profiles the case on head too: one attribution block, warmed like a bench's, for finding
    // where a single build spends its time before there is anything to compare.
    let mut attribution = Value::Null;
    if passed && ctx.args.get("why") == Some("always") {
        progress("  attribution block (CPU and allocation stacks, forced layouts, GC, a heap snapshot; not timed)");
        let v = &variants[0];
        // Warmed and run as in a bench: `--check`'s single cold run would start no profiler.
        ctx.check = false;
        let attributed = if s.launch.is_object() { launch::attribution(ctx, s, v) } else { block_on_screen(ctx, s, v, usize::MAX, Set::Attribution, true) };
        ctx.check = true;
        match attributed {
            Ok(b) => {
                progress(format!("  {} attribution  {}", b.variant, steps_text(&b.steps)));
                attribution = json!(b.recording);
            }
            Err(e) => ctx.warnings.push(validity::warn("Attribution", format!("{} attribution block failed: {}", v.role, e.message))),
        }
    }
    let declared: Vec<&String> = std::iter::once(&s.primary).chain(&s.metrics).collect();
    Ok(json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "check", "primary": { "metric": s.primary }, "declared": declared,
        "passed": passed, "decided": passed, "error": error, "values": values, "elapsed_ms": elapsed_ms, "attribution": attribution,
    }))
}

fn check_output(exp: &str, dir: &Path, file: &Path, series: Vec<Value>, warnings: Vec<Warning>) -> Result<Output> {
    let failed = series.iter().filter(|s| s["passed"] != true).count();
    let mut text = String::new();
    for s in &series {
        // The declared metrics' values: one run's worth, enough to see that a counter counts.
        let shown = |name: &str| s["values"][name].as_f64().map(|v| format!("{name} {}", fmt::value(v, model::metric(name).map_or("", |m| m.unit))));
        let value = s["declared"].as_array().into_iter().flatten().filter_map(|m| m.as_str().and_then(shown)).collect::<Vec<_>>().join(" · ");
        let secs = s["elapsed_ms"].as_f64().unwrap_or(0.0) / 1e3;
        if s["passed"] == true {
            text += &format!("pass  {}  {value}  {secs:.1} s\n", s["label"].as_str().unwrap_or(""));
            if let Some(rec) = s["attribution"].as_str() {
                text += &format!("      why  {rec} · strobe query {rec} hotspots --by package\n");
            }
        } else {
            text += &format!("FAIL  {}  {secs:.1} s\n      {}\n", s["label"].as_str().unwrap_or(""), s["error"].as_str().unwrap_or("").replace('\n', "\n      "));
        }
    }
    text += &format!("\n{} of {} cases passed (one cold run each on head, no warmup: values include first-run work such as shader compiles; nothing compared)\n", series.len() - failed, series.len());
    for w in &warnings {
        text += &format!("{} {:<24}{}\n", if w.level == "warn" { "!" } else { "i" }, w.id, w.text);
    }
    text += &format!("\n{exp}\n");
    let result = json!({ "id": exp, "kind": "check", "file": file, "created": super::record::now_iso(), "series": series, "warnings": warnings, "passed": failed == 0, "text": text });
    store::write_json(&dir.join("result.json"), &result)?;
    let mut out = Output::new(text, result);
    if failed > 0 {
        out.code = 1;
    }
    Ok(out)
}

/// A leak run (`--probes memory`, SPEC §8.6): one memory block per variant, nothing paired. Each gets
/// growth verdicts for its post-GC JS heap and DOM nodes, and its suspects.
fn leak_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value) -> Result<Value> {
    let target = ctx.describe["targets"][&s.target].clone();
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, false)?;
    }
    let n = ctx.runs;
    progress(format!("\n{}  leak run · warmup, then S0 · {n} runs · S1 · {n} runs · S2, a post-GC census after every run", s.label()));
    let series_started = Instant::now();
    let mut leak = json!({});
    let mut decided = true;
    for v in variants.iter() {
        let b = block_on_screen(ctx, s, v, 0, Set::Memory, true)?;
        progress(format!("  {} block  {}", b.variant, steps_text(&b.steps)));
        let mut entry = b.leak.clone().unwrap_or(Value::Null);
        progress(format!("  {:<5} js heap  {}", v.role, entry["js_heap"]["verdict"].as_str().unwrap_or("")));
        progress(format!("  {:<5} dom      {}", v.role, entry["dom_nodes"]["verdict"].as_str().unwrap_or("")));
        decided &= entry["js_heap"]["decided"].as_bool() == Some(true);
        entry["recording"] = json!(b.recording);
        entry["values"] = json!(b.values);
        leak[v.role] = entry;
    }
    let head = &leak["head"]["js_heap"];
    Ok(json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "leak",
        "primary": { "metric": "js.heap.growth", "verdict": head["verdict"], "decided": head["decided"] },
        "decided": decided, "leak": leak, "runs_per_half": n, "warnings": [], "blocks": [], "attribution": {},
        "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3,
    }))
}

fn run_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value, aa: bool, started: Instant, max_time: Option<Duration>) -> Result<Value> {
    let def = model::metric(&s.primary)
        .ok_or_else(|| Error::usage(format!("{}: unknown primary metric {} (known: {})", s.case, s.primary, model::METRICS.iter().map(|m| m.name).collect::<Vec<_>>().join(", "))))?;
    let target = ctx.describe["targets"][&s.target].clone();
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, aa)?;
    }
    let bound =
        ctx.args.num("equivalence")?.map(|b| b / 100.0).unwrap_or_else(|| config["metrics"]["equivalence"].as_str().and_then(|e| e.trim_end_matches('%').parse::<f64>().ok()).unwrap_or(2.0) / 100.0);
    // Verdicts only at planned looks, each at a Pocock level, so looking more than once keeps the
    // overall false-positive rate at 5% (SPEC §9.1). `--pairs N` is one look at N at 95%.
    let (looks, confidence): (Vec<usize>, f64) = match ctx.args.get("pairs") {
        Some("auto") | None => (LOOKS.to_vec(), 1.0 - POCOCK_ALPHA),
        Some(n) => {
            let n = n.parse::<usize>().map_err(|_| Error::usage("--pairs: auto or a number"))?.max(2);
            if n < stats::min_pairs(0.95) {
                ctx.warnings.push(validity::info("FewPairs", format!("--pairs {n}: below {} pairs no interval reaches 95%, so nothing gets a verdict (a smoke run)", stats::min_pairs(0.95))));
            }
            (vec![n], 0.95)
        }
    };
    let max_pairs = *looks.last().unwrap_or(&40);
    let min_pairs = looks[0];
    progress(format!("\n{}  primary {}", s.label(), s.primary));
    let series_started = Instant::now();
    let waited_before = ctx.waited_ms;
    let mut blocks: Vec<Block> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut stopped = String::from("max pairs");
    let mut last: Option<Comparison> = None;
    for pair in 0..max_pairs {
        let order: [usize; 2] = if pair % 2 == 0 { [0, 1] } else { [1, 0] };
        let mut idx = [0usize; 2];
        for &v in &order {
            let keep = ctx.keep_all || pair == 0;
            // Timing blocks trace only what the metrics read; `--trace full` keeps every timeline category.
            let set = if ctx.args.get("trace") == Some("full") { Set::Timing } else { Set::Lean };
            let b = block_on_screen(ctx, s, &variants[v], pair, set, keep)?;
            if pair == 0 {
                progress(format!("  {} block  {}", b.variant, steps_text(&b.steps)));
            }
            idx[v] = blocks.len();
            blocks.push(b);
        }
        pairs.push((idx[0], idx[1]));
        let primary_pairs: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], &s.primary), val(&blocks[b], &s.primary))).collect();
        let n = pair + 1;
        let look = looks.contains(&n);
        let cmp = stats::compare(&s.primary, def, &primary_pairs, bound, confidence);
        let next = looks.iter().find(|&&l| l > n).copied();
        progress(format!(
            "  pair {:>2}  base {:>10}  head {:>10}  {}  [{} … {}]  {}",
            n,
            fmt::value(val(&blocks[idx[0]], &s.primary), def.unit),
            fmt::value(val(&blocks[idx[1]], &s.primary), def.unit),
            change(&cmp),
            bound_text(cmp.lo, &cmp, def),
            bound_text(cmp.hi, &cmp, def),
            match (look, next) {
                (true, _) => format!("look: {}", cmp.verdict),
                (false, Some(l)) => format!("(next look at pair {l})"),
                (false, None) => String::new(),
            }
        ));
        let decided = look && cmp.decided;
        // Give up only at a look from 12 pairs, on a robust projection: early intervals are [min, max]
        // of few Walsh averages, so one odd pair would stop a series that more pairs decide.
        let d: Vec<f64> = primary_pairs.iter().map(|&(a, b)| if cmp.relative { (b / a).ln() } else { b - a }).collect();
        let mut cmp = cmp;
        if !cmp.decided {
            cmp.needed_pairs = Some(stats::pairs_needed(&d, if cmp.relative { (1.0 + bound).ln() } else { cmp.bound }, confidence));
        }
        let hopeless = look && n >= FUTILITY_FROM && cmp.needed_pairs.is_some_and(|need| need > max_pairs);
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
    let primary_decided = stopped == "primary decided" && pairs.len() >= min_pairs;
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
    // Secondaries: gated on the primary, Holm-corrected. A script's CPU per run is its main.work, so it
    // is tested like main.busy is for a page.
    let script = target["kind"] == "script";
    // `cpu.app.total` is CPU over run time: once runs get shorter or longer, its move says nothing about
    // the CPU they spent (a run 84% shorter for 80% less CPU reads +28%), so it is then context.
    let rate_context = !same(&blocks, &pairs, "run.duration", bound);
    let mut names: Vec<String> = s.metrics.clone();
    for m in DEFAULT_SECONDARIES.iter().copied().chain(script.then_some("cpu.work")) {
        let skip = names.iter().any(|n| n == m) || m == s.primary || (s.primary == "cpu.process.total" && m == "cpu.app.total") || (m == "cpu.app.total" && rate_context);
        if !skip {
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
        secondaries.push(tail_check(d, &blocks, stats::compare(name, d, &pp, bound, 0.95)));
    }
    // An exact comparison (a stable counter) is a fact, not a test: it skips Holm and the primary's gate.
    let tested: Vec<usize> = (0..secondaries.len()).filter(|&i| !secondaries[i].exact).collect();
    let ps: Vec<f64> = tested.iter().map(|&i| secondaries[i].p).collect();
    let rejected = stats::holm(&ps, 0.05);
    for (&i, rej) in tested.iter().zip(rejected) {
        let c = &mut secondaries[i];
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
    let tests = tested.len().max(1);
    let holm_pairs = (2..=60).find(|n| 2.0 / 2f64.powi(*n) <= 0.05 / tests as f64).unwrap_or(60);
    let mut descriptive: Vec<Comparison> = Vec::new();
    // A launch's milestones and CPU, and a process's CPU, are context like the fixed list.
    let found: Vec<String> =
        blocks.first().map(|b| b.values.keys().filter(|k| k.starts_with("launch.") || *k == "cpu.work" || (*k == "cpu.app.total" && rate_context)).cloned().collect()).unwrap_or_default();
    for name in DESCRIPTIVE.iter().map(|n| n.to_string()).chain(found) {
        let name = name.as_str();
        if name == s.primary || names.iter().any(|n| n == name) {
            continue;
        }
        let Some(d) = model::metric(name) else { continue };
        if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
            continue;
        }
        let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], name), val(&blocks[b], name))).collect();
        let mut c = tail_check(d, &blocks, stats::compare(name, d, &pp, bound, 0.95));
        if !c.verdict.starts_with("refused") {
            c.verdict = "·".into();
        }
        c.decided = false;
        descriptive.push(c);
    }
    let mut warnings: Vec<Warning> = Vec::new();
    analyse(s, def, &blocks, &pairs, &mut warnings);
    // Attribution: one block per variant with profilers, never timed. By default only when there is a
    // difference to explain; `--why always` keeps them for every series, `--why never` (or
    // `--probes timing`) skips them.
    let mut recordings = json!({});
    let differs = primary_decided && (primary.lo > 0.0 || primary.hi < 0.0);
    let why = match ctx.args.get("why").unwrap_or(if ctx.args.get("probes") == Some("timing") { "never" } else { "auto" }) {
        "always" => true,
        "never" => false,
        "auto" => differs,
        other => return Err(Error::usage(format!("--why: auto, always or never, not {other}"))),
    };
    if !why && !differs {
        progress("  no attribution blocks: nothing to explain (--why always runs them anyway)");
    }
    if why {
        progress("  attribution blocks (CPU and allocation stacks, forced layouts, GC, a heap snapshot; not timed)");
        for v in variants.iter() {
            // A launch's attribution is a launch of its own, profiled from the first statement.
            let attributed = if s.launch.is_object() { launch::attribution(ctx, s, v) } else { block_on_screen(ctx, s, v, usize::MAX, Set::Attribution, true) };
            match attributed {
                Ok(b) => {
                    progress(format!("  {} attribution  {}", b.variant, steps_text(&b.steps)));
                    if let Some(line) =
                        b.recording.as_deref().and_then(|r| store::resolve(r).ok()).and_then(|d| store::read_json(&d.join("shifts.json")).ok()).and_then(|v| probe::shifts::headline(&v))
                    {
                        progress(format!("  {} {line}", b.variant));
                    }
                    // `Unresolved` (SPEC §10.2): script samples the stored source maps could not name.
                    // A script target's profile names its own source files: nothing to map.
                    let script = ctx.describe["targets"][&s.target]["kind"] == "script";
                    let hot = b.recording.as_deref().filter(|_| !script).and_then(|r| store::resolve(r).ok()).and_then(|d| super::query::hotspots(&d, "function", 1).ok());
                    if let Some(h) = hot.map(|o| o.json) {
                        let (scripts, mapped) = (h["script_samples"].as_f64().unwrap_or(0.0), h["mapped_samples"].as_f64().unwrap_or(0.0));
                        if scripts > 0.0 && mapped / scripts < 0.95 {
                            warnings.push(validity::warn(
                                "Unresolved",
                                format!(
                                    "{}: {:.0}% of script samples have no source map, so their names and files stay minified; strobe builds with STROBE_SOURCEMAP=hidden: let the bundler read it (Vite: build: {{ sourcemap: process.env.STROBE_SOURCEMAP }}), which ships the same scripts",
                                    v.role,
                                    100.0 * (1.0 - mapped / scripts)
                                ),
                            ));
                        }
                    }
                    recordings[v.role] = json!(b.recording);
                }
                Err(e) => warnings.push(validity::warn("Attribution", format!("{} attribution block failed: {}", v.role, e.message))),
            }
        }
    }
    let timing_recordings: Vec<Value> = blocks.iter().filter_map(|b| b.recording.clone().map(|r| json!({ "variant": b.variant, "pair": b.pair, "recording": r }))).collect();
    let pixels = if ctx.args.has("pixels") && variants.len() > 1 { pixels_series(ctx, s, variants)? } else { Value::Null };
    if pixels.as_array().is_some_and(|a| a.iter().any(|p| p["state"] != "same")) {
        warnings.push(validity::warn("PixelsDiffer", "base and head drew different pixels at a snapshot: see pixels"));
    }
    Ok(json!({
        "pixels": pixels,
        "label": s.label(), "case": s.case, "target": s.target, "target_kind": target["kind"], "params": s.params, "kind": if s.launch.is_object() { "launch" } else { "runs" },
        "primary": primary, "decided": primary_decided, "secondaries": secondaries, "descriptive": descriptive, "holm_pairs": holm_pairs,
        "pairs": pairs.len(), "launches": blocks.len(), "stopped": stopped, "bound": bound,
        "steps": mean_steps(&blocks), "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3 - (ctx.waited_ms - waited_before), "waited_ms": ctx.waited_ms - waited_before,
        "blocks": blocks, "warnings": warnings, "attribution": recordings, "timing_recordings": timing_recordings,
    }))
}

/// `--pixels`: untimed blocks run the case once each with `page.snapshot(name)` capturing the page: base twice,
/// then head. The same build does not draw bit for bit the same twice (a GPU's blending and rasterization vary
/// between launches: an A/A once differed in 4 to 109 pixels by 1 or 2 of 255), so base's two runs measure that
/// noise per snapshot, and head differs only where it moves a pixel further than base moved against itself.
fn pixels_series(ctx: &mut Ctx, s: &Series, variants: &[Variant]) -> Result<Value> {
    let slug: String = s.label().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let root = store::experiment_dir(&ctx.exp)?.join("pixels").join(slug);
    progress("  pixels blocks (one run each, untimed: base twice for the noise between launches, then head)");
    let (check, runs) = (ctx.check, ctx.runs);
    ctx.check = true;
    ctx.runs = 1;
    let mut failed = None;
    for (v, folder) in [(&variants[0], "base"), (&variants[0], "base-again"), (&variants[1], "head")] {
        ctx.pixels_dir = Some(root.join(folder));
        if let Err(e) = block_on_screen(ctx, s, v, usize::MAX, Set::Lean, false) {
            failed = Some(e);
            break;
        }
    }
    ctx.pixels_dir = None;
    ctx.check = check;
    ctx.runs = runs;
    if let Some(e) = failed {
        return Err(e);
    }
    let shots = |role: &str| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root.join(role)).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".png").map(str::to_string)).collect();
        names.sort();
        names
    };
    let (base_shots, head_shots) = (shots("base"), shots("head"));
    let mut names: Vec<String> = base_shots.iter().chain(&head_shots).cloned().collect();
    names.sort();
    names.dedup();
    let read = |role: &str, name: &str| std::fs::read(root.join(role).join(format!("{name}.png"))).ok().and_then(|b| png::read(&b).ok());
    // The launch noise: the largest channel change base made against itself at any snapshot. One pair per
    // snapshot underestimates it (a snapshot that drew the same twice once read as noise 0, then head moved 3
    // pixels by 1 of 255), so it is the series', not each snapshot's.
    let noise = names.iter().filter_map(|n| Some(png::diff(&read("base", n)?, &read("base-again", n)?, 0).max_delta)).max().unwrap_or(0);
    let mut out = Vec::new();
    for name in names {
        let entry = match (read("base", &name), read("head", &name)) {
            (Some(b), Some(h)) => {
                let d = png::diff(&b, &h, noise);
                let diff_path = root.join(format!("{name}.diff.png"));
                if let Some(img) = &d.image {
                    std::fs::write(&diff_path, png::write(img)?)?;
                }
                json!({
                    "name": name, "state": if d.differing == 0 { "same" } else { "differ" }, "differing": d.differing, "total": d.total, "max_delta": d.max_delta, "noise": noise,
                    "diff": d.image.as_ref().map(|_| diff_path.display().to_string()), "base": root.join("base").join(format!("{name}.png")), "head": root.join("head").join(format!("{name}.png")),
                })
            }
            (b, _) => json!({ "name": name, "state": if b.is_some() { "only base" } else { "only head" } }),
        };
        progress(format!("  pixels {name}: {}", entry["state"].as_str().unwrap_or("")));
        out.push(entry);
    }
    Ok(Value::Array(out))
}

/// A comparison's cells as the tables show them: base, head, change, interval.
fn cells(c: &Value) -> (String, String, String, String) {
    let name = c["metric"].as_str().unwrap_or("");
    let unit = model::metric(name).map(|m| m.unit).unwrap_or("");
    let rel = c["relative"].as_bool().unwrap_or(false);
    let est = c["estimate"].as_f64().unwrap_or(f64::NAN);
    let (lo, hi) = (c["lo"].as_f64().unwrap_or(f64::NAN), c["hi"].as_f64().unwrap_or(f64::NAN));
    let abs_unit = if unit == "%" { "pp" } else { unit };
    let delta = if rel { fmt::pct(est) } else { fmt::signed(est, abs_unit) };
    let ci = if c["exact"] == true {
        "exact".to_string()
    } else if rel {
        format!("{} … {}", fmt::pct(lo), fmt::pct(hi))
    } else {
        format!("{} … {}", fmt::signed(lo, abs_unit), fmt::signed(hi, abs_unit))
    };
    (fmt::value(c["base"].as_f64().unwrap_or(f64::NAN), unit), fmt::value(c["head"].as_f64().unwrap_or(f64::NAN), unit), delta, ci)
}

/// `report experiment <id> --format pr`: the experiment as Markdown for a pull request body. Per series a table
/// of the primary and the tested metrics with their intervals and verdicts, the context metrics folded away,
/// pixels and warnings, then the setup and the command that reruns it.
pub fn pr_markdown(r: &Value) -> String {
    let variant = |role: &str| r["variants"].as_array().into_iter().flatten().find(|v| v["role"] == role).cloned().unwrap_or(Value::Null);
    let (base, head) = (variant("base"), variant("head"));
    let mut out = String::new();
    for s in r["series"].as_array().into_iter().flatten() {
        out += &format!("**{}**, {} pairs of fresh launches", s["label"].as_str().unwrap_or(""), s["pairs"]);
        if s["kind"] == "launch" {
            out += " (one timed launch each)";
        }
        out += ":\n\n| Metric | base | head | Change [interval] | Verdict |\n|---|---|---|---|---|\n";
        let mut rows = vec![(true, s["primary"].clone())];
        rows.extend(s["secondaries"].as_array().cloned().unwrap_or_default().into_iter().map(|c| (false, c)));
        for (primary, c) in &rows {
            let (b, h, d, ci) = cells(c);
            let name = c["metric"].as_str().unwrap_or("");
            let verdict = c["verdict"].as_str().unwrap_or("");
            out += &format!("| `{name}`{} | {b} | {h} | {d} [{ci}] | {} |\n", if *primary { " (primary)" } else { "" }, if verdict == "·" { "" } else { verdict });
        }
        let descriptive = s["descriptive"].as_array().cloned().unwrap_or_default();
        if !descriptive.is_empty() {
            out += "\n<details><summary>Context metrics (not tested)</summary>\n\n| Metric | base | head | Change [interval] |\n|---|---|---|---|\n";
            for c in &descriptive {
                let (b, h, d, ci) = cells(c);
                out += &format!("| `{}` | {b} | {h} | {d} [{ci}] |\n", c["metric"].as_str().unwrap_or(""));
            }
            out += "\n</details>\n";
        }
        if let Some(shots) = s["pixels"].as_array().filter(|a| !a.is_empty()) {
            let same = shots.iter().filter(|p| p["state"] == "same").count();
            out += &format!("\n- **Pixels:** {same} of {} snapshots the same within launch noise.\n", shots.len());
            for p in shots.iter().filter(|p| p["state"] != "same") {
                out += &format!("  - `{}`: {}", p["name"].as_str().unwrap_or(""), pixel_line(p).trim().trim_start_matches("differ").trim());
                out.push('\n');
            }
        }
        let warns: Vec<&Value> = s["warnings"].as_array().into_iter().flatten().filter(|w| w["level"] == "warn").collect();
        for w in warns {
            out += &format!("\n- **{}:** {}\n", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or(""));
        }
        out += "\n";
    }
    let gpu = r["gpu"]["devices"].as_array().and_then(|d| d.first()).and_then(Value::as_str).unwrap_or("");
    out += &format!(
        "`{}` against `{}`, alternating fresh launches: {} · {} · {gpu} · {} · {}×{} · {} Hz display. Intervals over pairs (Wilcoxon signed-rank, Hodges–Lehmann); secondaries gated on the primary and Holm-corrected.\n\n",
        base["label"].as_str().unwrap_or("base"),
        head["label"].as_str().unwrap_or("head"),
        r["env"]["os"].as_str().unwrap_or(""),
        r["env"]["cpu"].as_str().unwrap_or(""),
        r["browser"].as_str().unwrap_or(""),
        r["window"]["width"],
        r["window"]["height"],
        r["refresh_hz"].as_f64().map(|h| format!("{h:.0}")).unwrap_or_else(|| "?".into())
    );
    let file = r["file"].as_str().unwrap_or("").trim_start_matches(r"\\?\").replace('\\', "/");
    out += &format!("```sh\nstrobe bench {file} --base {} --head {}\n```\n", base["spec"].as_str().unwrap_or("<base>"), head["spec"].as_str().unwrap_or("."));
    out
}

/// One snapshot's line: `same`, `differ` with how many pixels moved beyond the launch noise, or only one side.
fn pixel_line(p: &Value) -> String {
    let name = p["name"].as_str().unwrap_or("");
    let n = |k: &str| p[k].as_u64().unwrap_or(0);
    match p["state"].as_str().unwrap_or("") {
        "same" if n("noise") > 0 => format!("  same     {name} (within launch noise, ±{} of 255)\n", n("noise")),
        "same" => format!("  same     {name}\n"),
        "differ" => format!(
            "  differ   {name}: {} of {} pixels moved beyond launch noise (±{}), by up to {} of 255 → {}\n",
            n("differing"),
            n("total"),
            n("noise"),
            n("max_delta"),
            p["diff"].as_str().unwrap_or("")
        ),
        other => format!("  {other:<8} {name}\n"),
    }
}

/// `TailTooShort` (SPEC §10.1): a percentile needs its minimum n of the events it is a percentile of.
fn tail_check(def: &MetricDef, blocks: &[Block], mut c: Comparison) -> Comparison {
    let (count, what) = match def.name.split('.').next() {
        Some("interaction") => ("interaction.n", "inputs"),
        Some("scroll") => ("scroll.n", "scroll updates"),
        Some("canvas") => ("canvas.n", "canvas frame gaps"),
        _ => ("frames.n", "frames"),
    };
    if def.min_n > 1 {
        let least = blocks.iter().map(|b| b.values.get(count).copied().unwrap_or(0.0) as usize).min().unwrap_or(0);
        if least < def.min_n {
            c.verdict = format!("refused (TailTooShort): {least} {what} < {}", def.min_n);
            c.decided = false;
        }
    }
    c
}

/// `launch 1.2s · attach 0.3s · …  = 31.0s`: where one block's wall time went.
fn steps_text(steps: &[(String, f64)]) -> String {
    let total: f64 = steps.iter().map(|s| s.1).sum();
    format!("{}  = {:.1}s", steps.iter().map(|(n, ms)| format!("{n} {:.1}s", ms / 1e3)).collect::<Vec<_>>().join(" · "), total / 1e3)
}

/// Mean wall time per step over blocks, in step order.
fn mean_steps(blocks: &[Block]) -> Vec<(String, f64)> {
    let names: Vec<String> = blocks.first().map(|b| b.steps.iter().map(|s| s.0.clone()).collect()).unwrap_or_default();
    names
        .into_iter()
        .map(|n| {
            let v: Vec<f64> = blocks.iter().filter_map(|b| b.steps.iter().find(|s| s.0 == n).map(|s| s.1)).collect();
            let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
            (n, mean)
        })
        .collect()
}

fn val(b: &Block, name: &str) -> f64 {
    b.values.get(name).copied().unwrap_or(f64::NAN)
}

/// Whether `name` held on both sides: within the equivalence bound, or absent from the blocks.
fn same(blocks: &[Block], pairs: &[(usize, usize)], name: &str, bound: f64) -> bool {
    let Some(d) = model::metric(name) else { return true };
    if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
        return true;
    }
    let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], name), val(&blocks[b], name))).collect();
    stats::compare(name, d, &pp, bound, 0.95).verdict.starts_with("same")
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
    let unsteady = blocks.iter().filter(|b| b.values.contains_key("warmup.unsteady")).count();
    if unsteady > 0 {
        w.push(validity::warn("NotSteady", format!("{unsteady} block(s) never did the same work twice in {MAX_WARMUP} warmup runs: the case's state grows across runs; add a reset")));
    }
    let d: Vec<f64> = pairs.iter().map(|&(a, b)| (val(&blocks[b], &s.primary) / val(&blocks[a], &s.primary)).ln()).filter(|x| x.is_finite()).collect();
    if d.len() >= 8 {
        let rho = validity::spearman(&d);
        if rho.abs() > 0.6 {
            w.push(validity::warn("Drift", format!("the difference trends across pairs (Spearman {rho:.2}): the machine changed during the run")));
        }
    }
    // Other work on the measured CPUs during a run: kept, flagged (SPEC §10.2).
    let busy: Vec<String> = blocks
        .iter()
        .filter(|b| b.values.get("cpu.foreign.max").is_some_and(|f| *f >= BACKGROUND_PCT))
        .map(|b| {
            let who = b.background.iter().map(|(n, p)| format!("{n} {p:.0}%")).collect::<Vec<_>>().join(", ");
            format!("pair {} {}: up to {:.0}% of a core{}", b.pair + 1, b.variant, b.values["cpu.foreign.max"], if who.is_empty() { String::new() } else { format!(" (busiest elsewhere: {who})") })
        })
        .collect();
    if !busy.is_empty() {
        w.push(validity::warn("Background", format!("other work ran on the measured CPUs during {} block(s): {}; blocks kept, flagged", busy.len(), busy.join("; "))));
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

/// A leak run's verdicts (SPEC §8.6): growth per run of the post-GC JS heap and of DOM nodes, as the
/// Theil–Sen slope over the 2 × N + 1 censuses with Sen's 95% interval (each half too, since a cache
/// that fills up grows in the first half only), and the suspects: objects allocated between S0 and
/// S1 that are still alive in S2. Writes `census.json` and `leaks.json` into the recording.
fn leak_analysis(census: &[Value], dir: &Path, n: usize) -> Result<Value> {
    let series = |key: &str| -> Vec<f64> { census.iter().map(|c| key.split('.').fold(c, |v, k| &v[k]).as_f64().unwrap_or(f64::NAN)).collect() };
    let js = series("js_heap_bytes");
    let dom = series("dom.nodes");
    let bytes = |v: f64| format!("{}{}", if v < 0.0 { "−" } else { "+" }, fmt::bytes(v.abs()));
    let nodes = |v: f64| format!("{v:+.1} nodes");
    let bound = (0.001 * metrics::median(&js)).max(16_384.0);
    let mut js_v = growth(&js, n, bound, &bytes);
    let dom_v = growth(&dom, n, 1.0, &nodes);
    let s = |name: &str| dir.join(format!("{name}.heapsnapshot.gz"));
    let suspects = crate::heapsnap::leaks(&crate::heapsnap::ids(&s("heap-s0"))?, &crate::heapsnap::ids(&s("heap-s1"))?, &crate::heapsnap::read(&s("heap-0"))?, 15, false);
    // A growing heap is a leak only when the app's own objects explain it: V8's compiled code and
    // internals keep growing while the JIT compiles and optimizes, and that stops on its own.
    let app_per_run = suspects["app"]["retained_size"].as_f64().unwrap_or(0.0) / n as f64;
    js_v["app_per_run"] = json!(app_per_run);
    let growing = js_v["lo"].as_f64().is_some_and(|lo| lo > 0.0);
    let undecided_or_leak = js_v["verdict"].as_str().is_some_and(|v| v.starts_with("leaks") || v.starts_with("unclear"));
    if growing && undecided_or_leak && app_per_run <= bound {
        js_v["verdict"] = json!(format!(
            "no app leak: the heap grows {}/run in V8's compiled code and internals (the JIT warming up); app objects from the first {n} runs hold {}/run",
            bytes(js_v["slope"].as_f64().unwrap_or(0.0)),
            bytes(app_per_run)
        ));
        js_v["decided"] = json!(true);
    }
    store::write_json(&dir.join("census.json"), &json!(census))?;
    store::write_json(&dir.join("leaks.json"), &suspects)?;
    Ok(json!({
        "runs": 2 * n, "censuses": census.len(), "js_heap": js_v, "dom_nodes": dom_v,
        "js_heap_bytes": js, "dom_node_counts": dom,
        "suspects": { "count": suspects["count"], "self_size": suspects["self_size"], "retained_size": suspects["retained_size"], "app": suspects["app"], "classes_total": suspects["classes_total"], "classes": suspects["classes"] },
    }))
}

/// Growth per run of one census series against `bound` (per run): "leaks" when the whole interval
/// is above it and the last N runs still grow; "grows, then levels off" when only the first N do.
fn growth(ys: &[f64], n: usize, bound: f64, show: &dyn Fn(f64) -> String) -> Value {
    let Some((slope, lo, hi)) = stats::theil_sen(ys, 0.95) else { return json!({ "verdict": "unclear: too few censuses", "decided": false }) };
    let mid = n.min(ys.len().saturating_sub(1));
    let half = |h: &[f64]| stats::theil_sen(h, 0.95).map_or(f64::NAN, |x| x.0);
    let (first, second) = (half(&ys[..=mid]), half(&ys[mid..]));
    let (verdict, decided) = if lo > bound && second > bound {
        (format!("leaks {}/run", show(slope)), true)
    } else if lo > bound {
        (format!("grows, then levels off ({}/run in the first {n} runs, {}/run in the last {n}): a cache filling up, not a leak so far", show(first), show(second)), true)
    } else if lo >= -bound && hi <= bound {
        (format!("no growth (within ±{}/run)", show(bound).trim_start_matches('+')), true)
    } else {
        (format!("unclear: {} … {}/run; more runs (--repeat) narrow it", show(lo), show(hi)), false)
    };
    json!({ "slope": slope, "lo": lo, "hi": hi, "bound": bound, "first_half": first, "second_half": second, "verdict": verdict, "decided": decided })
}

/// The three busiest processes outside strobe's own tree (the target, runner and servers) between two
/// process snapshots, as % of one core, from 5% up.
fn busiest_others(a: &(u64, Vec<host::ProcInfo>), b: &(u64, Vec<host::ProcInfo>)) -> Vec<(String, f64)> {
    let ours: std::collections::BTreeSet<u32> = host::tree_pids(std::process::id()).into_iter().collect();
    let before: BTreeMap<u32, u64> = a.1.iter().map(|p| (p.pid, p.cpu_ns)).collect();
    let span = b.0.saturating_sub(a.0).max(1) as f64;
    let mut out: Vec<(String, f64)> =
        b.1.iter()
            .filter(|p| p.pid != 0 && !ours.contains(&p.pid))
            .filter_map(|p| Some((p.name.clone(), 100.0 * p.cpu_ns.saturating_sub(*before.get(&p.pid)?) as f64 / span)))
            .filter(|(_, pct)| *pct >= 5.0)
            .collect();
    out.sort_by(|x, y| y.1.total_cmp(&x.1));
    out.truncate(3);
    out
}

/// The commit subject of a git variant, so a report still says what was compared days later.
fn subject(v: &Variant) -> Option<String> {
    let root = v.root.as_ref()?;
    let out = Command::new("git").arg("-C").arg(root).args(["log", "-1", "--format=%s"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Runs a block while the screen is unlocked and lit: waits out a locked session, redoes a block the
/// lock screen touched (Chromium hides its windows and nothing reaches the display, so its numbers
/// describe nothing), and wakes a display that went dark on idle, once, before a block refuses
/// `WindowHidden`. The block's own checks still refuse anything else.
fn block_on_screen(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    let mut woken = false;
    loop {
        host::wake_display();
        if woken {
            // The display and Chromium's occlusion tracking settle after the display comes back.
            std::thread::sleep(Duration::from_secs(3));
        }
        if host::session_locked() {
            progress("  waiting: the session is locked; strobe resumes when it is unlocked");
            let waiting = Instant::now();
            while host::session_locked() {
                std::thread::sleep(Duration::from_secs(2));
            }
            // The desktop and the GPU settle after an unlock.
            std::thread::sleep(Duration::from_secs(5));
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
        }
        let before = ctx.awake.locks();
        let r = if s.launch.is_object() {
            launch::block(ctx, s, v, pair)
        } else if ctx.describe["targets"][&s.target]["kind"] == "script" {
            script::block(ctx, s, v, pair, set, keep)
        } else {
            run_block(ctx, s, v, pair, set, keep)
        };
        if !woken && r.as_ref().err().is_some_and(|e| e.message.contains("WindowHidden")) {
            progress(format!("  the {} window was hidden (a display off on idle?): waking the display and running the block again", v.role));
            woken = true;
            continue;
        }
        if ctx.awake.locks() == before && !host::session_locked() {
            return r;
        }
        if let Some(dir) = r.as_ref().ok().and_then(|b| b.recording.as_deref()).and_then(|id| store::resolve(id).ok()) {
            let _ = std::fs::remove_dir_all(dir);
        }
        progress(format!("  the session locked during a {} block: discarded, it runs again once unlocked", v.role));
    }
}

fn run_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    let target = ctx.describe["targets"][&s.target].clone();
    let kind = target["kind"].as_str().unwrap_or("");
    let app = kind == "app" || kind == "exe" || kind == "command";
    // What the series declared beyond the default probes: counters, network counts, layout shift.
    let declared: Vec<&String> = std::iter::once(&s.primary).chain(&s.metrics).collect();
    let counted: Vec<String> = declared.iter().filter_map(|m| m.strip_prefix("count.")).map(str::to_string).collect();
    let nets: Vec<String> = declared.iter().filter(|m| m.starts_with("net.")).map(|m| m.to_string()).collect();
    let shift = declared.iter().any(|m| *m == "layout.shift");
    let gc = declared.iter().any(|m| *m == "main.gc");
    let gpu_compile = declared.iter().any(|m| *m == "gpu.compile");
    let canvas = declared.iter().any(|m| m.starts_with("canvas."));
    let url = if app { String::new() } else { variant_url(v, &target)? };
    let mut window = ctx.window.clone();
    if let Some(vp) = target.get("viewport").filter(|v| v.is_object()) {
        window.width = vp["width"].as_u64().unwrap_or(1400) as u32;
        window.height = vp["height"].as_u64().unwrap_or(900) as u32;
    }
    window.scale = target["scale"].as_f64();
    ctx.used = Some(window.clone());
    let launched = Instant::now();
    let mut guard = None;
    let browser = if kind == "command" {
        let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a command target runs in a variant's folder (a directory or a git ref)", v.role)))?;
        crate::target::command::launch(&crate::target::command::Spec::from_target(&target, &root), &electron::new_home(), &window, ctx.pin)?.0
    } else if app {
        let launch = variant_app(v, &target, &ctx.config)?;
        let seed = target["home"].as_str().map(|h| {
            let mut h = h.to_string();
            for (k, val) in &s.params {
                h = h.replace(&format!("{{{k}}}"), &value_text(val));
            }
            let base = Path::new(ctx.describe["file"].as_str().unwrap_or(".")).parent().unwrap_or(Path::new(".")).to_path_buf();
            base.join(h)
        });
        let (b, g) = electron::launch(&launch, seed.as_deref(), &window, ctx.pin)?;
        guard = Some(g);
        b
    } else {
        chromium::launch(&window, false, ctx.pin, &[])?
    };
    let steps = RefCell::new(vec![("launch".to_string(), launched.elapsed().as_secs_f64() * 1e3)]);
    let lapped = std::cell::Cell::new(Instant::now());
    let lap = |name: &str| {
        steps.borrow_mut().push((name.to_string(), lapped.get().elapsed().as_secs_f64() * 1e3));
        lapped.set(Instant::now());
    };
    let result = (|| -> Result<Block> {
        if !ctx.gpu_checked {
            let gpu = validity::software_gpu(&browser)?;
            ctx.gpu = serde_json::to_value(&gpu)?;
            ctx.browser_version = browser.version.clone();
            ctx.gpu_checked = true;
        }
        let page = if app { electron::main_page(&browser)? } else { browser.page()? };
        if let Some(g) = &guard {
            g.check("after the app opened its window")?;
        }
        if !app && target.get("viewport").is_some_and(Value::is_object) {
            browser.fit_viewport(&page, window.width, window.height)?;
        }
        // The driver's frame waits use the browser's own rAF, kept before the page's scripts can replace it.
        page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": crate::driver::NATIVE_RAF }))?;
        // `count.<name>` metrics: the page counts through a global installed in every document it loads.
        if !counted.is_empty() {
            page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": COUNT_SCRIPT }))?;
            page.eval(COUNT_SCRIPT)?;
        }
        if canvas {
            page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": CANVAS_SCRIPT }))?;
            page.eval(CANVAS_SCRIPT)?;
        }
        lap("attach");
        let id = store::new_id("rec");
        let token = id.trim_start_matches("rec_").to_string();
        let mut driver = Driver::new(page.clone(), token.clone());
        driver.pixels = ctx.pixels_dir.clone();
        let current = RefCell::new(String::new());
        // TaskDuration (ms), then WORK_PARTS: cumulative per document (`Performance.enable` comes before the runs).
        let counters = || -> [f64; 6] {
            let m = page.send("Performance.getMetrics", json!({})).unwrap_or(Value::Null);
            let get = |name: &str, scale: f64| m["metrics"].as_array().and_then(|a| a.iter().find(|x| x["name"] == name)).and_then(|x| x["value"].as_f64()).map_or(f64::NAN, |v| v * scale);
            let mut out = [get("TaskDuration", 1e3), 0.0, 0.0, 0.0, 0.0, 0.0];
            for (i, (_, name, scale)) in WORK_PARTS.iter().enumerate() {
                out[i + 1] = get(name, *scale);
            }
            out
        };
        // A run that loads a page (`goto`) starts the counters again in the new document: what the
        // departing document did is read just before it goes and carried into the run's totals.
        let carried = RefCell::new([0.0f64; 6]);
        // This run's `count.<name>` values: the bench file's `ctx.count` and the page's counter global.
        let counts: RefCell<BTreeMap<String, f64>> = RefCell::new(BTreeMap::new());
        let page_counts =
            || -> BTreeMap<String, f64> { page.eval("JSON.stringify(globalThis.__strobeCounts ?? {})").ok().and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())).unwrap_or_default() };
        let add_counts = |from: BTreeMap<String, f64>| {
            let mut c = counts.borrow_mut();
            for (k, n) in from {
                *c.entry(k).or_default() += n;
            }
        };
        // This run's canvas frame times, one list per document it showed (a page load starts a new clock).
        let canvas_docs: RefCell<Vec<Vec<f64>>> = RefCell::new(Vec::new());
        let take_canvas = || {
            if canvas {
                let times: Vec<f64> = page.eval("JSON.stringify(globalThis.__strobeCanvas?.splice(0) ?? [])").ok().and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())).unwrap_or_default();
                canvas_docs.borrow_mut().push(times);
            }
        };
        // An attribution block's profiles, one pair per document: a page load ends the V8 profilers with
        // their document, so each `goto` (in a run or a reset) closes a segment and starts the next.
        let profiling = Cell::new(false);
        let segments: RefCell<Vec<(Value, Value)>> = RefCell::new(Vec::new());
        let serve = |op: &str, a: &Value| -> Result<Value> {
            match op {
                "driver" => {
                    let call = a["call"].as_str().unwrap_or("");
                    let args = a["args"].as_array().cloned().unwrap_or_default();
                    if call == "goto" && !current.borrow().is_empty() {
                        let before = counters();
                        let mut c = carried.borrow_mut();
                        for (sum, v) in c.iter_mut().zip(before) {
                            *sum += v;
                        }
                        if !counted.is_empty() {
                            add_counts(page_counts());
                        }
                        take_canvas();
                    }
                    let segment = call == "goto" && profiling.get();
                    if segment {
                        segments.borrow_mut().push((probe::cpu::stop(&page)?, probe::alloc::stop(&page)?));
                    }
                    let r = driver.dispatch(call, &args);
                    if segment {
                        probe::cpu::start(&page)?;
                        probe::alloc::start(&page)?;
                    }
                    r
                }
                "count" => {
                    if !current.borrow().is_empty() {
                        add_counts(BTreeMap::from([(a["name"].as_str().unwrap_or("").to_string(), a["n"].as_f64().unwrap_or(1.0))]));
                    }
                    Ok(Value::Bool(true))
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
        ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": url, "root": v.root }), t, &serve)?;
        // `prepare` (and a target `url` function) may name the first page, so a block loads one page, not two.
        let mut first = url.clone();
        if target["prepare"].as_bool() == Some(true) {
            let r = ctx
                .runner
                .call("prepare", json!({ "endpoint": browser.ws, "target": page.target }), t, &serve)
                .map_err(|e| Error::refused("RunIncomplete", format!("prepare: {}", e.message), "fix the target's prepare in the bench file"))?;
            if let Some(u) = r.get("url").and_then(Value::as_str) {
                first = join_url(origin(&url), u);
            }
        }
        lap("prepare");
        if !app {
            page.navigate(&first, Duration::from_secs(60))?;
        }
        lap("load");
        validity::dev_build(&page)?;
        let speed = validity::speed_check(&page)?;
        let ratio = validity::throttled(speed, ctx.baseline)?;
        validity::window_visible(&page)?;
        lap("checks");
        ctx.runner
            .call("setup", json!({}), t, &serve)
            .map_err(|e| Error::refused("RunIncomplete", format!("setup: {}{}", e.message, snapshot(&page, &ctx.exp, "setup")), "fix the bench file's setup"))?;
        lap("setup");
        let procs0 = (host::now_ns(), host::all_processes());
        // A memory block (a leak run, SPEC §8.6) takes no trace: a live trace subscription would copy
        // every heap snapshot chunk, and nothing in it is timed. Its recording holds the snapshots.
        let extra_categories: Vec<&str> = [(shift, "loading"), (gc, "disabled-by-default-v8.gc"), (gpu_compile, "gpu.dawn"), (gpu_compile, "disabled-by-default-gpu.dawn")]
            .into_iter()
            .filter(|c| c.0)
            .map(|c| c.1)
            .collect();
        let tracing = if set == Set::Memory { None } else { Some(probe::trace::start_with(&browser.cdp, set, &extra_categories)?) };
        let mem_dir = if set == Set::Memory { Some(store::recording_dir(&id)?) } else { None };
        let mut census: Vec<Value> = Vec::new();
        // A leak run measures 2 × N runs: N to S1 and N more to S2.
        let measured_runs = if set == Set::Memory { 2 * ctx.runs } else { ctx.runs };
        // GPU engines and VRAM of the target's process tree, read from the OS (SPEC §13.6).
        let sampler = probe::os::Sampler::start(browser.pid);
        // Renderer main-thread task time, a cumulative counter: the live work signal for warmup, and
        // main.busy when the trace carries no RunTask slices.
        page.send("Performance.enable", json!({}))?;
        // `net.*` metrics: requests and bytes per run from the Network domain, on only for such series.
        let net = if nets.is_empty() {
            None
        } else {
            let rx = browser.cdp.events();
            page.send("Network.enable", json!({}))?;
            Some(RefCell::new(Net::new(rx, page.session.clone())))
        };
        let run_counts: RefCell<Vec<BTreeMap<String, f64>>> = RefCell::new(Vec::new());
        let run_canvas: RefCell<Vec<Vec<Vec<f64>>>> = RefCell::new(Vec::new());

        let run_timeout = Duration::from_secs(60);
        let mut run_once = |key: &str| -> Result<Run> {
            let began = Instant::now();
            *current.borrow_mut() = key.to_string();
            // The target's CPU in TSC cycles over TSC time: process times move in 15.6 ms ticks, which
            // made an idle 5 s run read 0, 0.31% or 0.62% and nothing between.
            let cycles = || -> (u64, Option<u64>) { (browser.processes().iter().map(|p| p.cycles).sum(), host::tsc()) };
            let by_role = |procs: &[host::ProcTimes], roles: &BTreeMap<u32, usize>| -> [u64; 3] {
                let mut out = [0u64; 3];
                for p in procs {
                    if let Some(i) = roles.get(&p.pid) {
                        out[*i] += p.cycles;
                    }
                }
                out
            };
            let mut roles = process_roles(&browser);
            let procs_start = browser.processes();
            let (cyc0, tsc0) = cycles();
            let cpu0: u64 = browser.processes().iter().map(|p| p.cpu_ns).sum();
            *carried.borrow_mut() = [0.0; 6];
            counts.borrow_mut().clear();
            if !counted.is_empty() {
                page.eval("globalThis.__strobeCounts = {}, true")?;
            }
            if canvas {
                page.eval("globalThis.__strobeCanvas = [], true")?;
                canvas_docs.borrow_mut().clear();
            }
            if let Some(n) = &net {
                n.borrow_mut().drain(None);
            }
            let c0 = counters();
            driver.mark(&format!("{key}:start"))?;
            let t0 = host::now_ns();
            ctx.runner.call("run", json!({ "case": s.case }), run_timeout, &serve).map_err(|e| {
                Error::refused("RunIncomplete", format!("{} run: {}{}", s.case, e.message, snapshot(&page, &ctx.exp, "run")), "the run threw or timed out; fix the bench file or the app")
            })?;
            driver.dispatch("frame", &[])?;
            let t1 = host::now_ns();
            let c1 = counters();
            driver.mark(&format!("{key}:end"))?;
            let (cyc1, tsc1) = cycles();
            let procs_end = browser.processes();
            roles.extend(process_roles(&browser));
            let (role0, role1) = (by_role(&procs_start, &roles), by_role(&procs_end, &roles));
            if !counted.is_empty() {
                add_counts(page_counts());
            }
            take_canvas();
            run_canvas.borrow_mut().push(canvas_docs.take());
            let index = run_counts.borrow().len();
            run_counts.borrow_mut().push(counts.borrow().clone());
            if let Some(n) = &net {
                n.borrow_mut().drain(Some(index));
            }
            let cpu1: u64 = browser.processes().iter().map(|p| p.cpu_ns).sum();
            current.borrow_mut().clear();
            if s.reset {
                ctx.runner
                    .call("reset", json!({ "case": s.case }), run_timeout, &serve)
                    .map_err(|e| Error::refused("RunIncomplete", format!("{} reset: {}{}", s.case, e.message, snapshot(&page, &ctx.exp, "reset")), "fix the case's reset"))?;
            }
            let wall = (t1 - t0) as f64;
            let cpu = match (tsc0, tsc1) {
                // No process readings (an OS without a process sampler yet): unknown, never 0%.
                _ if cyc1 == 0 && cpu1 == 0 => f64::NAN,
                (Some(a), Some(b)) if b > a && cyc0 > 0 => 100.0 * cyc1.saturating_sub(cyc0) as f64 / (b - a) as f64,
                _ if wall > 0.0 => 100.0 * cpu1.saturating_sub(cpu0) as f64 / wall,
                _ => f64::NAN,
            };
            let carry = *carried.borrow();
            let parts = std::array::from_fn(|i| c1[i + 1] + carry[i + 1] - c0[i + 1]);
            // Cycles only: a process's share needs the exact counter, and a process born during the run
            // (a renderer after a cross-site navigation) counts from zero.
            let roles = std::array::from_fn(|i| match (tsc0, tsc1) {
                (Some(a), Some(b)) if b > a && !roles.is_empty() => 100.0 * role1[i].saturating_sub(role0[i]) as f64 / (b - a) as f64,
                _ => f64::NAN,
            });
            Ok(Run { ms: wall / 1e6, cpu, task: c1[0] + carry[0] - c0[0], parts, roles, t0, t1, total: began.elapsed().as_secs_f64() * 1e3 })
        };
        // Warm until two consecutive runs do the same main-thread work (within 5%); both of them count
        // as measured (SPEC §9.2). Wall time says nothing here: a fixed-speed scroll or paced typing
        // takes the same time cold or warm, while the work it costs settles over 1–4 runs. Attribution
        // blocks start their profiler at the steady point, so they measure the runs after it.
        let mut runs: Vec<Run> = Vec::new();
        let mut nodes_all = Vec::new();
        let mut first: Option<usize> = None;
        let mut unsteady = false;
        while first.is_none_or(|f| runs.len() < f + measured_runs) {
            let i = runs.len();
            nodes_all.push(driver.nodes().unwrap_or(0));
            runs.push(run_once(&format!("run:{i}"))?);
            if let Some(f) = first {
                // Leak run: a post-GC census after every run; S1 after N runs, S2 after 2 × N.
                if let Some(dir) = &mem_dir {
                    let mut c = probe::census::take(&browser, &page);
                    c["os_after_snapshot"] = json!(true);
                    let done = runs.len() - f;
                    if done == 2 * ctx.runs {
                        c["allocators"] = probe::census::allocators(&browser, &dir.join("memdump-s2.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
                    }
                    census.push(c);
                    if done == ctx.runs {
                        probe::heap::snapshot(&page, &dir.join("heap-s1.heapsnapshot.gz"))?;
                    }
                    if done == 2 * ctx.runs {
                        probe::heap::snapshot(&page, &dir.join("heap-0.heapsnapshot.gz"))?;
                    }
                }
                continue;
            }
            // Within 5%, or within 1 ms of main-thread time: an idle run does about 3 ms, where 5% is noise.
            let steady = i >= 1 && {
                let (a, b) = (runs[i - 1].task, runs[i].task);
                a.is_finite() && b.is_finite() && ((a - b).abs() <= 1.0 || (a.max(b) > 0.0 && (a - b).abs() / a.max(b) <= 0.05))
            };
            // `--check` measures the first run: it asks whether a case works, not what it costs.
            if ctx.check {
                first = Some(0);
            } else if steady || i >= MAX_WARMUP {
                unsteady = !steady;
                first = Some(if set == Set::Attribution || set == Set::Memory || !steady { i + 1 } else { i - 1 });
                if set == Set::Attribution {
                    probe::cpu::start(&page)?;
                    probe::alloc::start(&page)?;
                    profiling.set(true);
                }
                if let Some(dir) = &mem_dir {
                    let mut c = probe::census::take(&browser, &page);
                    c["allocators"] = probe::census::allocators(&browser, &dir.join("memdump-s0.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
                    census.push(c);
                    probe::heap::snapshot(&page, &dir.join("heap-s0.heapsnapshot.gz"))?;
                }
            }
        }
        let first = first.unwrap_or(0);
        // Warmup and measured runs interleave with the steadiness check: split the loop by run.
        // `+ 0.0`: an empty f64 sum is -0.0, which would print as "-0.0s".
        let warm_ms: f64 = runs[..first].iter().map(|r| r.total).sum::<f64>() + 0.0;
        steps.borrow_mut().push(("warmup".into(), warm_ms));
        steps.borrow_mut().push(("runs".into(), (lapped.get().elapsed().as_secs_f64() * 1e3 - warm_ms).max(0.0)));
        lapped.set(Instant::now());
        let measured = &runs[first..];
        let key = |j: usize| format!("run:{}", first + j);
        let run_ms: Vec<f64> = measured.iter().map(|r| r.ms).collect();
        let walls: BTreeMap<String, f64> = measured.iter().enumerate().map(|(j, r)| (key(j), r.ms)).collect();
        let cpu: BTreeMap<String, f64> = measured.iter().enumerate().map(|(j, r)| (key(j), r.cpu)).collect();
        let busy: BTreeMap<String, f64> = measured.iter().enumerate().filter(|(_, r)| r.task.is_finite() && r.ms > 0.0).map(|(j, r)| (key(j), 100.0 * r.task / r.ms)).collect();
        let work: BTreeMap<String, f64> = measured.iter().enumerate().filter(|(_, r)| r.task.is_finite()).map(|(j, r)| (key(j), r.task)).collect();
        let spans: Vec<(String, u64, u64)> = measured.iter().enumerate().map(|(j, r)| (key(j), r.t0, r.t1)).collect();
        let nodes = nodes_all[first.min(nodes_all.len())..].to_vec();
        let warm = runs[..first].to_vec();
        let samples = sampler.stop();
        let procs1 = (host::now_ns(), host::all_processes());
        let gpu_runs: Vec<_> = spans.iter().map(|(k, t0, t1)| (k.clone(), samples.window(*t0, *t1))).collect();
        // Other work on the CPUs the target is pinned to, per run (% of one core): their busy time beyond
        // the target's own cycles (SPEC §10.2 Background). It shares the target's cores and its L3.
        let pinned: Vec<u32> = ctx.pin.map(|m| (0..64).filter(|c| m >> c & 1 == 1).collect()).unwrap_or_default();
        let foreign: Vec<f64> = gpu_runs
            .iter()
            .zip(measured)
            .filter(|_| !pinned.is_empty())
            .filter_map(|((_, w), r)| {
                let busy: f64 = w.cores.iter().filter(|c| pinned.contains(&c.cpu)).map(|c| c.busy_pct).collect::<Option<Vec<f64>>>()?.iter().sum();
                r.cpu.is_finite().then(|| (busy - r.cpu).max(0.0))
            })
            .collect();
        let background = busiest_others(&procs0, &procs1);
        let gpu_3d: BTreeMap<String, f64> = gpu_runs.iter().filter_map(|(k, s)| Some((k.clone(), s.gpu_3d_pct?))).collect();
        let vram = gpu_runs.last().and_then(|(_, s)| s.vram_dedicated_mb);
        // The window must have stayed visible for the whole block.
        validity::window_visible(&page)?;
        let private_mb = browser.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
        let heap_mb = page.send("Runtime.getHeapUsage", json!({})).ok().and_then(|h| h["usedSize"].as_f64()).map(|b| b / (1024.0 * 1024.0));
        let dom_nodes = driver.nodes().ok();
        let dir = if keep || set == Set::Memory { store::recording_dir(&id)? } else { host::paths::run().join(&id) };
        std::fs::create_dir_all(&dir)?;
        let leak = if set == Set::Memory { Some(leak_analysis(&census, &dir, ctx.runs)?) } else { None };
        if set == Set::Attribution {
            profiling.set(false);
            let mut segments = segments.take();
            segments.push((probe::cpu::stop(&page)?, probe::alloc::stop(&page)?));
            for (k, (profile, allocs)) in segments.iter().enumerate() {
                store::write_json(&dir.join(format!("cpu-{k}.cpuprofile")), profile)?;
                store::write_json(&dir.join(format!("alloc-{k}.heapprofile")), allocs)?;
            }
            // The page's source maps, kept with the recording while its server still runs, so any view
            // can resolve any frame later. The served folder (strobe.json `serve.dir`, else
            // `build.output`) turns original sources into files on disk.
            let served = v.root.as_ref().zip(ctx.config["serve"]["dir"].as_str().or(ctx.config["build"]["output"].as_str())).map(|(r, d)| r.join(d));
            let mut scripts = std::collections::BTreeSet::new();
            for (profile, allocs) in &segments {
                probe::symbols::urls(profile, &mut scripts);
                probe::symbols::urls(allocs, &mut scripts);
            }
            probe::symbols::store_maps(&page, &scripts, &dir, served.as_deref())?;
            lap("maps");
        }
        let trace = dir.join("trace.pftrace");
        if let Some(tracing) = tracing {
            tracing.stop(&trace)?;
            lap("trace");
        }
        // `layout.shift`'s explanation: the largest shifts and what moved, named while the page is open.
        if set == Set::Attribution && shift {
            match probe::shifts::largest(&trace, &token, &page, first, 5) {
                Ok(v) => store::write_json(&dir.join("shifts.json"), &v)?,
                Err(e) => ctx.warnings.push(validity::warn("Shifts", format!("{} layout shifts: {}", v.role, e.message))),
            }
        }
        if set == Set::Attribution {
            // Memory by process and layer after the runs (`query memory`), before the snapshot raises it.
            let mut c = probe::census::take(&browser, &page);
            c["allocators"] = probe::census::allocators(&browser, &dir.join("memdump-0.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
            census.push(c);
            // The live objects after the runs (post-GC), once no trace subscription copies every
            // snapshot chunk (SPEC §8.6).
            if let Err(e) = probe::heap::snapshot(&page, &dir.join("heap-0.heapsnapshot.gz")) {
                ctx.warnings.push(validity::warn("HeapSnapshot", format!("{} heap snapshot failed: {}", v.role, e.message)));
            }
            lap("heap");
        }
        if let Err(e) = ctx.runner.call("close", json!({}), t, &serve)
            && !ctx.warnings.iter().any(|w| w.id == "Teardown")
        {
            ctx.warnings.push(validity::warn("Teardown", format!("{} block: {}", v.role, e.message)));
        }
        let mut data = if set == Set::Memory { metrics::BlockData::default() } else { metrics::read(&trace, &token)? };
        data.warm_below(first);
        lap("analyse");
        let mut extra = BTreeMap::new();
        let run_counts = run_counts.into_inner();
        for name in &counted {
            extra.insert(format!("count.{name}"), (0..measured.len()).map(|j| (key(j), run_counts.get(first + j).and_then(|c| c.get(name)).copied().unwrap_or(0.0))).collect());
        }
        if let Some(n) = net {
            let mut n = n.into_inner();
            n.settle();
            for m in &nets {
                extra.insert(m.clone(), (0..measured.len()).map(|j| (key(j), n.value(m, first + j))).collect());
            }
        }
        if shift && set != Set::Memory {
            extra.insert("layout.shift".to_string(), data.shift_per_run("run"));
        }
        if gc && set != Set::Memory {
            extra.insert("main.gc".to_string(), data.gc_per_run("run"));
        }
        if gpu_compile && set != Set::Memory {
            extra.insert("gpu.compile".to_string(), data.gpu_compile_per_run("run"));
        }
        extra.insert("run.duration".to_string(), walls);
        extra.insert("cpu.app.total".to_string(), cpu);
        for (i, (_, metric)) in PROCESS_ROLES.iter().enumerate() {
            let per_run: BTreeMap<String, f64> = measured.iter().enumerate().filter(|(_, r)| r.roles[i].is_finite()).map(|(j, r)| (key(j), r.roles[i])).collect();
            if !per_run.is_empty() {
                extra.insert(metric.to_string(), per_run);
            }
        }
        if !gpu_3d.is_empty() {
            extra.insert("gpu.process.3d".to_string(), gpu_3d);
        }
        let busy_cdp = metrics::median(&busy.values().copied().collect::<Vec<_>>());
        if !work.is_empty() {
            extra.insert("main.work".to_string(), work);
        }
        for (i, (metric, _, _)) in WORK_PARTS.iter().enumerate() {
            let per_run: BTreeMap<String, f64> = measured.iter().enumerate().filter(|(_, r)| r.parts[i].is_finite()).map(|(j, r)| (key(j), r.parts[i])).collect();
            if !per_run.is_empty() {
                extra.insert(metric.to_string(), per_run);
            }
        }
        if set == Set::Lean {
            extra.insert("main.busy".to_string(), busy);
        }
        // canvas.*: frames per run, and gaps between consecutive frames pooled over the measured runs, as
        // frames.interval pools presented frames. A gap never spans two documents.
        let run_canvas = run_canvas.into_inner();
        let mut canvas_gaps: Vec<f64> = Vec::new();
        if canvas {
            let measured_canvas = &run_canvas[first.min(run_canvas.len())..];
            extra.insert("canvas.frames".to_string(), measured_canvas.iter().enumerate().map(|(j, docs)| (key(j), docs.iter().map(Vec::len).sum::<usize>() as f64)).collect());
            for doc in measured_canvas.iter().flatten() {
                canvas_gaps.extend(doc.windows(2).map(|w| w[1] - w[0]));
            }
            canvas_gaps.sort_by(f64::total_cmp);
        }
        let mut values = metrics::block_values(&data, "run", &extra);
        if canvas {
            values.insert("canvas.n".into(), canvas_gaps.len() as f64);
            if !canvas_gaps.is_empty() {
                for (name, p) in [("canvas.interval.p50", 0.50), ("canvas.interval.p95", 0.95), ("canvas.interval.p99", 0.99)] {
                    values.insert(name.into(), metrics::percentile(&canvas_gaps, p));
                }
                values.insert("canvas.interval.max".into(), canvas_gaps.last().copied().unwrap_or(0.0));
            }
        }
        if set != Set::Lean && busy_cdp.is_finite() {
            // The same quantity from the Performance domain, next to the trace's: they must agree.
            values.insert("main.busy.cdp".into(), busy_cdp);
        }
        if unsteady {
            values.insert("warmup.unsteady".into(), 1.0);
        }
        if private_mb > 0.0 {
            values.insert("memory.app.private".into(), private_mb);
        }
        if let Some(v) = vram {
            values.insert("gpu.vram".into(), v);
        }
        if let Some(h) = heap_mb {
            values.insert("js.heap.used".into(), h);
        }
        if let Some(n) = dom_nodes {
            values.insert("dom.nodes".into(), n as f64);
        }
        if !foreign.is_empty() {
            values.insert("cpu.foreign".into(), metrics::median(&foreign));
            values.insert("cpu.foreign.max".into(), foreign.iter().copied().fold(0.0, f64::max));
        }
        let phases = metrics::phase_frames(&data);
        let frames = metrics::run_frames(&data);
        let intervals = metrics::run_intervals(&data);
        let recording = if keep || set == Set::Memory {
            let mut meta = json!({
                "id": id, "kind": "block", "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label,
                "target": url, "probes": set, "token": token, "runs": measured.len(), "values": values, "duration_ms": run_ms.iter().sum::<f64>() as u64,
                "created": super::record::now_iso(), "valid": true, "env": host::env(), "browser": browser.version, "background": background,
            });
            if !census.is_empty() {
                meta["census"] = json!(census);
            }
            if let Some(l) = &leak {
                // What `query leaks` reads, as for a memory recording.
                meta["leaks"] = l["suspects"].clone();
                meta["leak"] = json!({ "js_heap": l["js_heap"], "dom_nodes": l["dom_nodes"], "runs": l["runs"] });
            }
            store::write_json(&dir.join("meta.json"), &meta)?;
            Some(id)
        } else {
            let _ = std::fs::remove_dir_all(&dir);
            None
        };
        Ok(Block {
            variant: v.role.to_string(),
            pair,
            values,
            run_ms,
            warmup: warm.len(),
            speed_ratio: ratio,
            nodes,
            recording,
            phases,
            frames,
            intervals,
            steps: steps.borrow().clone(),
            background,
            leak,
        })
    })();
    lapped.set(Instant::now());
    browser.close();
    let result = result.map(|mut b| {
        b.steps.push(("close".into(), lapped.get().elapsed().as_secs_f64() * 1e3));
        b
    });
    if let Some(g) = &guard {
        g.check("after a block")?;
    }
    result
}

/// Keeps strobe itself (the driver relay, samplers, CDP) off the measured CPUs for the bench (SPEC rule
/// 2), on the runner's; the variant servers it spawns inherit that. The previous mask comes back on
/// drop, since the daemon runs benches in its own process.
struct OwnAffinity(Option<u64>);

impl Drop for OwnAffinity {
    fn drop(&mut self) {
        if let Some(mask) = self.0 {
            host::set_affinity(mask);
        }
    }
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
        let previous =
            active.split_whitespace().find(|w| w.len() == 36 && w.matches('-').count() == 4).map(str::to_string).ok_or_else(|| Error::msg("powercfg: could not read the active power plan"))?;
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

/// What an app target launches for a variant. `app:<id>` is that app's own profile; `exe:<path>` is
/// the target's profile (`app(id)`, or `exe(path, { app })`) with another build.
fn variant_app(v: &Variant, target: &Value, config: &Value) -> Result<electron::App> {
    if let Kind::App(id) = &v.kind {
        return electron::app(id, config);
    }
    let profile = match target["kind"].as_str() {
        Some("app") => target["id"].as_str(),
        Some("exe") => target["app"].as_str(),
        _ => return Err(Error::usage("not an app target")),
    };
    let base = match profile {
        Some(id) => electron::app(id, config)?,
        None => electron::App::bare(Path::new(target["path"].as_str().unwrap_or(""))),
    };
    match &v.kind {
        Kind::Exe(p) => Ok(base.with_exe(p)),
        _ if target["kind"] == "exe" => Ok(base.with_exe(Path::new(target["path"].as_str().unwrap_or("")))),
        _ => Ok(base),
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

/// `http://host:port` of a URL.
fn origin(url: &str) -> &str {
    let start = url.find("://").map(|i| i + 3).unwrap_or(0);
    url[start..].find('/').map(|i| &url[..start + i]).unwrap_or(url)
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
    let fixed: BTreeMap<String, String> =
        args.get("params").map(|p| p.split(',').filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect()).unwrap_or_default();
    // `--params all`: every value, the `more` ones too.
    let all = args.get("params").is_some_and(|p| p.split(',').any(|x| x.trim() == "all"));
    let params = describe["params"].as_object().cloned().unwrap_or_default();
    let mut combos: Vec<BTreeMap<String, Value>> = vec![BTreeMap::new()];
    for (k, values) in &params {
        // `{ run, more }`: `run` by default; `more` with `--params all`, or when `--params k=v` names one.
        let values: Vec<Value> = match values {
            Value::Object(o) => {
                let list = |key: &str| o.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
                let mut v = list("run");
                if all || fixed.contains_key(k) {
                    v.extend(list("more"));
                }
                v
            }
            v => v.as_array().cloned().unwrap_or_default(),
        };
        let values: Vec<Value> = values.into_iter().filter(|v| fixed.get(k).is_none_or(|f| value_text(v) == *f)).collect();
        if values.is_empty()
            && let Some(f) = fixed.get(k)
        {
            return Err(Error::usage(format!("--params {k}={f}: not one of {k}'s values")));
        }
        combos = combos
            .into_iter()
            .flat_map(|c| {
                values.iter().map(move |v| {
                    let mut c = c.clone();
                    c.insert(k.clone(), v.clone());
                    c
                })
            })
            .collect();
    }
    let mut out = Vec::new();
    let targets = describe["targets"].as_object().cloned().unwrap_or_default();
    for (case, c) in describe["cases"].as_object().cloned().unwrap_or_default() {
        if filter.is_some_and(|f| !glob(f, &case)) {
            continue;
        }
        // A case may name the targets it runs on; by default it runs on every target.
        let on: Option<Vec<&str>> = c["targets"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect());
        for target in targets.keys() {
            if only_target.is_some_and(|t| t != target) || on.as_ref().is_some_and(|on| !on.contains(&target.as_str())) {
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
                    launch: c["launch"].clone(),
                });
            }
        }
    }
    Ok(out)
}

/// Requests a page started per run and the bytes they received, from Network domain events read at run
/// boundaries. A request belongs to the run it started in; its bytes may arrive after that run ends.
struct Net {
    rx: std::sync::mpsc::Receiver<Value>,
    session: String,
    /// Per run: (request id, URL) of every request it started, redirects included.
    runs: Vec<Vec<(String, String)>>,
    owner: BTreeMap<String, usize>,
    /// Bytes on the wire per request: its finished total, else the chunks so far.
    finished: BTreeMap<String, f64>,
    chunks: BTreeMap<String, f64>,
}

impl Net {
    fn new(rx: std::sync::mpsc::Receiver<Value>, session: String) -> Net {
        Net { rx, session, runs: Vec::new(), owner: BTreeMap::new(), finished: BTreeMap::new(), chunks: BTreeMap::new() }
    }

    /// Reads the events so far; requests started now belong to run `current` (none between runs).
    fn drain(&mut self, current: Option<usize>) {
        while let Ok(ev) = self.rx.try_recv() {
            self.take(&ev, current);
        }
    }

    /// The last bytes of requests that were still loading when the runs ended: up to 300 ms of quiet.
    fn settle(&mut self) {
        while let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(300)) {
            self.take(&ev, None);
        }
    }

    fn take(&mut self, ev: &Value, current: Option<usize>) {
        if ev["sessionId"].as_str() != Some(self.session.as_str()) {
            return;
        }
        let p = &ev["params"];
        let id = p["requestId"].as_str().unwrap_or("").to_string();
        match ev["method"].as_str().unwrap_or("") {
            "Network.requestWillBeSent" => {
                let url = p["request"]["url"].as_str().unwrap_or("");
                if let Some(i) = current
                    && (url.starts_with("http") || url.starts_with("ws"))
                {
                    if self.runs.len() <= i {
                        self.runs.resize(i + 1, Vec::new());
                    }
                    self.runs[i].push((id.clone(), url.to_string()));
                    self.owner.insert(id, i);
                }
            }
            "Network.dataReceived" if self.owner.contains_key(&id) => *self.chunks.entry(id).or_default() += p["encodedDataLength"].as_f64().unwrap_or(0.0),
            "Network.loadingFinished" if self.owner.contains_key(&id) => {
                self.finished.insert(id, p["encodedDataLength"].as_f64().unwrap_or(0.0));
            }
            _ => {}
        }
    }

    /// `net.requests` or `net.bytes` of run `i`, optionally `{<url glob>}`.
    fn value(&self, metric: &str, i: usize) -> f64 {
        let (base, pattern) = metric.split_once('{').map_or((metric, None), |(b, p)| (b, Some(p.trim_end_matches('}'))));
        let requests = self.runs.get(i).map(Vec::as_slice).unwrap_or_default().iter().filter(|(_, url)| pattern.is_none_or(|p| url_glob(p, url)));
        match base {
            "net.bytes" => {
                // A redirected request repeats its id: its bytes count once.
                let ids: std::collections::BTreeSet<&String> = requests.map(|(id, _)| id).collect();
                ids.into_iter().map(|id| self.finished.get(id).or(self.chunks.get(id)).copied().unwrap_or(0.0)).sum()
            }
            _ => requests.count() as f64,
        }
    }
}

/// A URL against a pattern: `*` within one path segment, `**` across them. A pattern without `://`
/// matches the path; the query string is never part of the match.
fn url_glob(pattern: &str, url: &str) -> bool {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let subject = if pattern.contains("://") { url } else { url.find("://").and_then(|i| url[i + 3..].find('/').map(|j| &url[i + 3 + j..])).unwrap_or("/") };
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) if rest.first() == Some(&b'*') => (0..=s.len()).any(|k| go(&rest[1..], &s[k..])),
            Some((b'*', rest)) => (0..=s.len()).take_while(|k| *k == 0 || s[k - 1] != b'/').any(|k| go(rest, &s[k..])),
            Some((c, rest)) => s.first() == Some(c) && go(rest, &s[1..]),
        }
    }
    go(pattern.as_bytes(), subject.as_bytes())
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

fn resolve_variant(role: &'static str, spec: &str, project: &Path, config: &Value) -> Result<Variant> {
    let (kind, label, root) = if let Some(href) = spec.strip_prefix("url:") {
        (Kind::Url(href.to_string()), href.to_string(), None)
    } else if let Some(id) = spec.strip_prefix("app:") {
        // Fails early when the profile is missing or the app is not installed.
        electron::app(id, config)?;
        (Kind::App(id.to_string()), spec.to_string(), None)
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
        let sha = host::output(Command::new("git").arg("-C").arg(project).args(["rev-parse", "--short=10", r]))
            .ok_or_else(|| Error::usage(format!("{spec} is neither a directory nor a git ref in {}", project.display())))?;
        v.label = format!("{r}@{sha}");
    }
    Ok(v)
}

/// The bench's harness: the bench file's folder, the strobe.json in use and strobe.json's `harness` paths, in
/// the checkout that holds the bench file. Every variant builds and serves with it (`overlay_harness`).
struct Harness {
    from: PathBuf,
    paths: Vec<PathBuf>,
}

static HARNESS: std::sync::OnceLock<Harness> = std::sync::OnceLock::new();

fn harness(file: &Path, project: &Path, config: &Value) -> Harness {
    let dir = file.parent().unwrap_or(Path::new("."));
    let from = host::output(Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]))
        .map(PathBuf::from)
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or_else(|| project.to_path_buf());
    let rel = |p: &Path| std::fs::canonicalize(p).ok().and_then(|c| c.strip_prefix(&from).ok().map(Path::to_path_buf));
    let mut paths: Vec<PathBuf> = [rel(dir), rel(&project.join("strobe.json"))].into_iter().flatten().filter(|p| !p.as_os_str().is_empty()).collect();
    paths.extend(config["harness"].as_array().into_iter().flatten().filter_map(Value::as_str).map(PathBuf::from));
    Harness { from, paths }
}

/// Copies the harness into a variant's folder, so a base from before the bench existed (its serve script, its
/// strobe.json) builds and serves the way head does. A worktree strobe made is strobe's own: head's harness
/// replaces whatever the ref had. A folder of the user's only gets the files it lacks.
fn overlay_harness(v: &Variant, root: &Path) -> Result<()> {
    let Some(h) = HARNESS.get() else { return Ok(()) };
    let root = std::fs::canonicalize(root)?;
    if root == h.from {
        return Ok(());
    }
    let owned = std::fs::canonicalize(host::paths::sub("builds")).is_ok_and(|b| root.starts_with(b));
    let mut copied = 0;
    let mut kept = Vec::new();
    let mut copy = |src: &Path, dst: &Path| -> Result<()> {
        let same = std::fs::read(dst).ok().is_some_and(|d| std::fs::read(src).is_ok_and(|s| s == d));
        if same {
            return Ok(());
        }
        if dst.exists() && !owned {
            kept.push(dst.strip_prefix(&root).unwrap_or(dst).display().to_string());
            return Ok(());
        }
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst)?;
        copied += 1;
        Ok(())
    };
    for p in &h.paths {
        let src = h.from.join(p);
        if src.is_dir() {
            for f in walk(&src) {
                copy(&f, &root.join(p).join(f.strip_prefix(&src).unwrap_or(&f)))?;
            }
        } else if src.is_file() {
            copy(&src, &root.join(p))?;
        }
    }
    if copied > 0 {
        progress(format!("  harness {}: {copied} file(s) from {}", v.role, h.from.display()));
    }
    if !kept.is_empty() {
        progress(format!("  harness {}: kept its own {}", v.role, kept.join(", ")));
    }
    Ok(())
}

/// Every file under `dir`, skipping dependency and output folders a harness never needs.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !matches!(name.as_str(), "node_modules" | ".git" | "dist" | "target") {
                out.extend(walk(&p));
            }
        } else {
            out.push(p);
        }
    }
    out
}

/// Builds and serves a variant for `web` targets (once per experiment).
fn prepare_variant(v: &mut Variant, target: &Value, config: &Value, aa: bool) -> Result<()> {
    // Command and script targets run in the variant's folder: a git ref gets its worktree, and the
    // target's own `build`, if any, runs there. strobe.json's web build and server are not theirs.
    if matches!(target["kind"].as_str(), Some("command") | Some("script")) {
        if let Kind::Git(r) = &v.kind
            && v.root.is_none()
        {
            v.root = Some(worktree(r)?);
        }
        if let Some(root) = v.root.clone() {
            overlay_harness(v, &root)?;
        }
        if let (Some(root), Some(cmd)) = (v.root.clone(), target["build"].as_str()) {
            build_variant(v.role, &root, &json!({ "command": cmd }))?;
        }
        return Ok(());
    }
    if target["kind"].as_str() != Some("web") || v.base_url.is_some() || matches!(v.kind, Kind::Url(_)) {
        return Ok(());
    }
    if let Kind::Git(r) = &v.kind {
        v.root = Some(worktree(r)?);
    }
    let root = v.root.clone().ok_or_else(|| Error::msg("variant has no directory"))?;
    overlay_harness(v, &root)?;
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
    // A package with its own lockfile below the repository root (a site in a Rust repo) gets its own install:
    // a fresh worktree has none.
    if (cwd.join("bun.lock").exists() || cwd.join("bun.lockb").exists()) && !cwd.join("node_modules").exists() {
        progress(format!("  bun install ({role}, {})", build["cwd"].as_str().unwrap_or(".")));
        Command::new(crate::runner::find_bun().unwrap_or_else(|| "bun".into())).args(["install", "--frozen-lockfile"]).current_dir(&cwd).stdout(Stdio::null()).status()?;
    }
    progress(format!("  build {role}: {cmd}"));
    let mut c = shell(cmd);
    c.current_dir(&cwd);
    // Source maps for attribution without changing what ships: a bundler config that reads this (Vite:
    // `build: { sourcemap: process.env.STROBE_SOURCEMAP }`) writes each map beside its script without the
    // `sourceMappingURL` comment, so the served scripts stay byte for byte the production ones.
    c.env("STROBE_SOURCEMAP", "hidden");
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
    // In a worktree strobe made for a git ref, untracked files are strobe's own (a script target's
    // harness): they do not change what the build sees.
    let owned = std::fs::canonicalize(root).ok().zip(std::fs::canonicalize(host::paths::sub("builds")).ok()).is_some_and(|(r, b)| r.starts_with(b));
    let status = if owned { git(&["status", "--porcelain", "--untracked-files=no"]) } else { git(&["status", "--porcelain"]) };
    hash_str(&format!("{}\n{}\n{}\n{}", git(&["rev-parse", "HEAD"]), status, git(&["diff"]), build))
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
    crate::target::command::shell(cmd)
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

/// A leak run's text: per variant, growth per run of the JS heap and DOM with their intervals, and the
/// top suspects.
fn leak_text(r: &Value, s: &Value, file: &str) -> String {
    let n = s["runs_per_half"].as_u64().unwrap_or(0);
    let mut out = format!("strobe bench {file} · {} · leak run\n", s["label"].as_str().unwrap_or(""));
    out += &format!("plan   warmup, then S0 · {n} runs · S1 · {n} runs · S2 · a post-GC census after every run · {}\n", r["plan"]["pin"].as_str().unwrap_or(""));
    out += &format!("time   {}\n", fmt::ms_duration(s["elapsed_ms"].as_f64().unwrap_or(0.0) as u64));
    for v in r["variants"].as_array().into_iter().flatten() {
        let role = v["role"].as_str().unwrap_or("");
        let l = &s["leak"][role];
        if l.is_null() {
            continue;
        }
        let range = |g: &Value, show: &dyn Fn(f64) -> String| {
            format!("{} [{} … {}] per run", show(g["slope"].as_f64().unwrap_or(f64::NAN)), show(g["lo"].as_f64().unwrap_or(f64::NAN)), show(g["hi"].as_f64().unwrap_or(f64::NAN)))
        };
        let bytes = |x: f64| format!("{}{}", if x < 0.0 { "−" } else { "+" }, fmt::bytes(x.abs()));
        let nodes = |x: f64| format!("{x:+.1}");
        out += &format!("\n{role} {}\n", v["label"].as_str().unwrap_or(""));
        out += &format!("  js heap   {}  · {}\n", l["js_heap"]["verdict"].as_str().unwrap_or(""), range(&l["js_heap"], &bytes));
        out += &format!("  dom       {}  · {}\n", l["dom_nodes"]["verdict"].as_str().unwrap_or(""), range(&l["dom_nodes"], &nodes));
        let sus = &l["suspects"];
        out += &format!("  suspects  {} objects from runs 1–{n} still alive after run {} · {} retained\n", sus["count"], 2 * n, fmt::bytes(sus["retained_size"].as_f64().unwrap_or(0.0)));
        for c in sus["classes"].as_array().into_iter().flatten().take(5) {
            out += &format!("            {:>9}  {:>6}  {}\n", fmt::bytes(c["retained_size"].as_f64().unwrap_or(0.0)), c["count"], c["class"].as_str().unwrap_or(""));
        }
        out += &format!("  next      strobe query {} leaks · strobe query {} memory\n", l["recording"].as_str().unwrap_or(""), l["recording"].as_str().unwrap_or(""));
    }
    out += "\n  slopes: Theil–Sen over the censuses, Sen's 95% interval; bound max(16 KB, 0.1% of the heap) and 1 DOM node per run\n\n";
    out
}

fn render(r: &Value) -> String {
    let mut out = String::new();
    let file = r["file"].as_str().map(|f| Path::new(f).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).unwrap_or_default();
    let base = &r["variants"][0];
    let head = &r["variants"][1];
    let env = &r["env"];
    for s in r["series"].as_array().cloned().unwrap_or_default() {
        if s["kind"] == "leak" {
            out += &leak_text(r, &s, &file);
            continue;
        }
        out += &format!("strobe bench {file} · {}   base {}   head {}\n", s["label"].as_str().unwrap_or(""), base["label"].as_str().unwrap_or(""), head["label"].as_str().unwrap_or(""));
        out += &format!("env    {} · {} · {} logical CPUs\n", env["os"].as_str().unwrap_or(""), env["cpu"].as_str().unwrap_or(""), env["logical_cpus"]);
        let gpu = r["gpu"]["devices"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
        let refresh = r["refresh_hz"].as_u64().map(|h| format!(" · display {h} Hz")).unwrap_or_default();
        out += &format!("       {} · {} · window {}×{}{}\n", r["browser"].as_str().unwrap_or(""), gpu, r["window"]["width"], r["window"]["height"], refresh);
        out += &format!("plan   {} · power plan {} · visible, unthrottled\n", r["plan"]["pin"].as_str().unwrap_or(""), env["power_plan"].as_str().unwrap_or("?"));
        let per_block = if s["kind"] == "launch" {
            "one timed launch per block · first launch of each variant discarded".to_string()
        } else {
            format!("warmup until steady · {} runs per block", r["plan"]["runs_per_block"])
        };
        out += &format!("blocks {} pairs ({} fresh launches) · {per_block} · stopped: {}\n", s["pairs"], s["launches"], s["stopped"].as_str().unwrap_or(""));
        let steps: Vec<(String, f64)> = serde_json::from_value(s["steps"].clone()).unwrap_or_default();
        out += &format!("time   {} · per block {}\n\n", fmt::ms_duration(s["elapsed_ms"].as_f64().unwrap_or(0.0) as u64), steps_text(&steps));
        out += &format!("{}{}{}{}{}{}\n", fmt::pad("metric", 28), fmt::pad("base", 11), fmt::pad("head", 11), fmt::pad("Δ", 9), fmt::pad("CI", 18), "verdict");
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
            // A percentage compared as a difference is in percentage points (`cells`).
            let (b, h, delta, ci) = cells(&c);
            out += &format!(
                "{}{}{}{}{}{}\n",
                fmt::pad(&format!("{} {name}", if primary { "★" } else { " " }), 28),
                fmt::pad(&b, 11),
                fmt::pad(&h, 11),
                fmt::pad(&delta, 9),
                fmt::pad(&ci, 18),
                c["verdict"].as_str().unwrap_or("")
            );
        }
        out += "\n★ primary, declared before the run · secondaries gated on the primary, Holm-corrected\n";
        out += "  intervals over pairs of fresh launches (Wilcoxon signed-rank, Hodges–Lehmann)\n";
        let alpha = r["plan"]["alpha"].as_f64().unwrap_or(0.05);
        let looks: Vec<String> = r["plan"]["looks"].as_array().map(|a| a.iter().map(|l| l.to_string()).collect()).unwrap_or_default();
        if looks.len() > 1 {
            out += &format!("  primary: {:.1}% intervals at planned looks ({} pairs), 95% overall (Pocock); others 95%\n", 100.0 * (1.0 - alpha), looks.join(", "));
        }
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
        if let Some(shots) = s["pixels"].as_array().filter(|a| !a.is_empty()) {
            out += "\npixels\n";
            for p in shots {
                out += &pixel_line(p);
            }
        }
        out += "\n";
    }
    out += &format!("{} · strobe report experiment {} --open\n", r["id"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or(""));
    out
}
