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
use crate::target::{electron, isolate};
use crate::validity::{self, Warning};
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod launch;
mod native;
mod script;
pub mod slot;

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
    "input.rate",
    "input.gap.p95",
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
    "gpu.shared",
    "memory.app.private",
    "memory.app.host",
    "cpu.wakes",
    "io.read",
    "io.write",
    "ui.cpu",
    "ui.wakes",
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
    /// A native target's executable built for this variant (strobe.json `build`), copied out of the build.
    exe: Option<PathBuf>,
    /// The commit a directory or git variant is at (short), and whether a directory had uncommitted changes.
    commit: Option<String>,
    dirty: bool,
    /// A directory variant's git state when the bench started (`tree_state`): its builds must see the same.
    state: Option<String>,
    /// When a build found the directory changed since then (`VariantChanged`).
    changed: Vec<String>,
}

/// A checkout's commit and a hash of its uncommitted changes to tracked files: what a build of it reads.
fn tree_state(root: &Path) -> Option<String> {
    use sha2::Digest;
    let sha = host::output(Command::new("git").arg("-C").arg(root).args(["rev-parse", "HEAD"]))?;
    let diff = Command::new("git").arg("-C").arg(root).args(["diff", "HEAD", "--no-ext-diff", "--binary"]).output().ok()?;
    Some(format!("{sha} {:x}", sha2::Sha256::digest(&diff.stdout)))
}

impl Drop for Variant {
    fn drop(&mut self) {
        if let Some(mut s) = self.server.take() {
            kill_tree(&mut s);
        }
    }
}

thread_local! {
    /// The saved state `--resume` hands to the bench it starts.
    static RESUMED: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// Where `strobe bench --stop <exp>` asks the running bench to stop (its experiment's `stop` file).
static STOP_FILE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Whether the bench should stop once the block that runs ends: Ctrl-C, SIGINT, SIGTERM, or
/// `strobe bench --stop <exp>`. Waits (a locked session, the slot, busy CPUs) end at once.
pub(crate) fn stop_requested() -> bool {
    host::ctrl_c().load(std::sync::atomic::Ordering::SeqCst) || STOP_FILE.lock().unwrap().as_ref().is_some_and(|p| p.exists())
}

pub(crate) fn stop_error() -> Error {
    Error { code: 130, message: "stopped (Ctrl-C, SIGTERM or strobe bench --stop)".into(), hint: Some("strobe bench --resume <exp> runs what is left".into()) }
}

/// Sleeps for `d`, or until a stop is asked for.
fn nap(d: Duration) -> Result<()> {
    let end = Instant::now() + d;
    while let Some(left) = end.checked_duration_since(Instant::now()).filter(|l| !l.is_zero()) {
        if stop_requested() {
            return Err(stop_error());
        }
        std::thread::sleep(left.min(Duration::from_millis(500)));
    }
    Ok(())
}

/// Times a block is run again after a launch that failed for a reason that passes (a browser that did
/// not open its debugging port under load, a DevTools call with no reply), before the error stands.
const LAUNCH_RETRIES: u32 = 3;

/// A launch or attach failure that another try can get past: the target never came up to talk to,
/// as opposed to a run that failed or a validator that refused.
fn transient(e: &Error) -> bool {
    let m = e.message.as_str();
    !m.contains("RunIncomplete")
        && (e.code == 255 || m.contains("did not open its debugging port") || (e.code == 124 && m.starts_with("cdp ")) || (m.starts_with("cdp ") && m.contains("no reply in")))
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
    /// Drags in the run: moves strobe sent per second and the 95th percentile gap (ms).
    input: Option<(f64, f64)>,
}

/// Chromium process types (`SystemInfo.getProcessInfo`) and the metric each one's CPU goes to.
const PROCESS_ROLES: [(&str, &str); 3] = [("renderer", "cpu.renderer"), ("GPU", "cpu.gpu"), ("browser", "cpu.browser")];

/// Each Chromium process's index in `PROCESS_ROLES`, by pid; empty where the browser does not say.
fn process_roles(browser: &chromium::Browser) -> BTreeMap<u32, usize> {
    let info = browser.cdp.send("SystemInfo.getProcessInfo", json!({}), None).unwrap_or(Value::Null);
    info["processInfo"].as_array().into_iter().flatten().filter_map(|p| Some((p["id"].as_u64()? as u32, PROCESS_ROLES.iter().position(|(t, _)| p["type"].as_str() == Some(*t))?))).collect()
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
#[serde(default)]
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
    /// Metrics this block cannot defend, with the validator and why ("WindowHidden: …"): they get no
    /// value, and a series that declares one shows it refused.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    refused: BTreeMap<String, String>,
    /// The block ran on a locked screen (a series that needs no presenting window, `runs_locked`).
    locked: bool,
    /// The console display's state through the block (0 off, 1 on, 2 dimmed), when known: a display
    /// that sleeps under a locked series changes what the machine does (the compositor parks, the GPU
    /// idles deeper), so both blocks of a pair share it.
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<u8>,
    /// What `--reserve-cpus` did in this block: processes moved, started during it, not movable, and
    /// the sweep's own CPU (% of one core).
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve: Option<Value>,
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
    /// A native target's GPU adapter and window scale per variant (`VariantMismatch`).
    native_setup: BTreeMap<String, String>,
    /// The bench's harness, overlaid on every variant's folder (`overlay_harness`).
    harness: Harness,
    /// The repository the bench file is in: git refs resolve and worktrees are made there.
    project: PathBuf,
    /// This bench in the device's registry: what it does now, for `strobe status` and the slot queue.
    reg: slot::Registration,
    /// `--reserve-cpus`: auto (timed blocks), on (every block) or off.
    reserve: &'static str,
    /// What the reservations of all blocks did, merged (`plan.reserve`).
    reserved: Option<ReserveTotal>,
    /// The machine state blocks run in, held from a block until the bench waits, builds or hands the
    /// slot to another bench (`hold_machine`, `release_machine`): the slot, the power plan (and
    /// keep-awake, `awake.hold`).
    slot: Option<slot::Slot>,
    power: Option<host::power::Power>,
    /// The plan blocks ran on, for the env line.
    power_used: Option<String>,
    /// Why strobe runs at background priority, which every target inherits (`BackgroundPriority`).
    qos: Option<String>,
    /// Why the last block's target process ran at background priority, read after its launch.
    target_qos: Option<String>,
    /// `state.json`: the command line and each series' finished pairs or result (`checkpoint`).
    saved: Value,
}

/// Every block's reservation, merged for the experiment's context.
#[derive(Clone, Debug, Default, Serialize)]
struct ReserveTotal {
    cpus: u64,
    blocks: u32,
    /// Per name, the most of that executable one block moved.
    moved: BTreeMap<String, u32>,
    moved_per_block: f64,
    started: u32,
    failed: BTreeMap<String, String>,
    restored: u32,
    /// Processes moved in all blocks together (each block counts its own).
    moved_total: u32,
    overhead_pct_max: f64,
    overhead_pct_mean: f64,
    setup_ms_mean: f64,
}

impl ReserveTotal {
    fn add(&mut self, r: &host::reserve::Report) {
        let n = self.blocks as f64;
        self.cpus = r.cpus;
        self.moved_per_block = (self.moved_per_block * n + r.moved_total() as f64) / (n + 1.0);
        self.overhead_pct_mean = (self.overhead_pct_mean * n + r.overhead_pct) / (n + 1.0);
        self.overhead_pct_max = self.overhead_pct_max.max(r.overhead_pct);
        self.setup_ms_mean = (self.setup_ms_mean * n + r.setup_ms) / (n + 1.0);
        self.moved_total += r.moved_total();
        self.blocks += 1;
        for (k, v) in &r.moved {
            let e = self.moved.entry(k.clone()).or_default();
            *e = (*e).max(*v);
        }
        self.started += r.started;
        self.failed.extend(r.failed.clone());
        self.restored += r.restored;
    }

    /// One line for the plan, the context note and the PR footer.
    fn text(&self) -> String {
        let cpus = cpu_list(self.cpus);
        let mut top: Vec<(&String, &u32)> = self.moved.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let names = top.iter().take(4).map(|(n, c)| if **c > 1 { format!("{n} ×{c}") } else { n.to_string() }).collect::<Vec<_>>().join(", ");
        let mut t = format!("other processes of this user kept off CPUs {cpus} during {} timed block{}: {:.0} moved per block ({names}{})", self.blocks, if self.blocks == 1 { "" } else { "s" }, self.moved_per_block, if top.len() > 4 { ", …" } else { "" });
        if self.started > 0 {
            t += &format!(", {} more started during blocks and moved within 200 ms", self.started);
        }
        if !self.failed.is_empty() {
            let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (n, why) in &self.failed {
                by.entry(why.as_str()).or_default().push(n.as_str());
            }
            let failed = by.iter().map(|(why, names)| format!("{} ({why})", names.iter().take(4).copied().collect::<Vec<_>>().join(", ") + if names.len() > 4 { ", …" } else { "" })).collect::<Vec<_>>().join("; ");
            t += &format!("; {} could not be moved: {failed}", self.failed.len());
        }
        let back = self.restored as f64 / self.moved_total.max(1) as f64;
        t += &format!(
            "; {:.0}% put back after (the rest exited or changed their own set); moving took {:.0} ms before each block, and the sweep then cost {:.2}% of one core on strobe's CPUs (max {:.2}%)",
            100.0 * back,
            self.setup_ms_mean,
            self.overhead_pct_mean,
            self.overhead_pct_max
        );
        t
    }
}

/// `0-15`, `0-7,16-23`: a CPU mask as ranges.
fn cpu_list(mask: u64) -> String {
    let mut parts = Vec::new();
    let mut c = 0;
    while c < 64 {
        if mask >> c & 1 == 1 {
            let start = c;
            while c < 64 && mask >> c & 1 == 1 {
                c += 1;
            }
            parts.push(if c - 1 == start { start.to_string() } else { format!("{start}-{}", c - 1) });
        }
        c += 1;
    }
    parts.join(",")
}

pub fn run(args: &Args) -> Result<Output> {
    // `--stop <exp>`: ask a running bench to stop once its block ends; it keeps its finished pairs.
    if let Some(exp) = args.get("stop") {
        let dir = host::paths::experiments().join(exp);
        if !dir.is_dir() {
            return Err(Error::usage(format!("no experiment {exp}")));
        }
        std::fs::write(dir.join("stop"), b"")?;
        let text = format!("asked {exp} to stop once its current block ends; it writes what it finished (strobe report experiment {exp}), and strobe bench --resume {exp} goes on\n");
        return Ok(Output::new(text, json!({ "id": exp, "stop": true })));
    }
    // `--resume <exp>`: the same command line, from the pairs and series it had finished.
    if let Some(exp) = args.get("resume") {
        let dir = host::paths::experiments().join(exp);
        let saved = store::read_json(&dir.join("state.json")).map_err(|_| Error::usage(format!("{exp} has no saved state to resume (state.json): only experiments started by this strobe or later can resume")))?;
        let mut resumed = Args::from_json(&saved["args"]).ok_or_else(|| Error::usage(format!("{exp}: state.json holds no command line")))?;
        resumed.set("id", exp);
        resumed.remove("resume");
        let _ = std::fs::remove_file(dir.join("stop"));
        RESUMED.with(|r| *r.borrow_mut() = Some(saved));
        return run(&resumed);
    }
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
    let reserve = match args.get("reserve-cpus").unwrap_or("auto") {
        "auto" => "auto",
        "on" => "on",
        "off" => "off",
        other => return Err(Error::usage(format!("--reserve-cpus: auto, on or off, not {other}"))),
    };
    // Benches share the device by taking turns per block (the slot); this one registers what it does.
    let reg = slot::Registration::new(&exp, &file.display().to_string());
    chromium::reap_orphans();
    let project = config::project_root(file.parent().unwrap_or(Path::new(".")));
    let config = config::load(&project);
    // The plan switches before the first block, not now: builds and waits run on the user's own.
    power_keep(args)?;
    let mut env = host::env();
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
        native_setup: BTreeMap::new(),
        // Per bench: a long-lived daemon runs benches of several repositories.
        harness: harness(&file, &project, &config),
        project: project.clone(),
        reg,
        reserve,
        reserved: None,
        slot: None,
        power: None,
        power_used: None,
        qos: host::background_qos(None),
        target_qos: None,
        saved: RESUMED.with(|r| r.borrow_mut().take()).unwrap_or_else(|| json!({ "args": args.to_json(), "series": {} })),
    };
    // A stop asked of an earlier run of this experiment (or a Ctrl-C a daemon saw for another) is not this one's.
    let _ = std::fs::remove_file(dir.join("stop"));
    *STOP_FILE.lock().unwrap() = Some(dir.join("stop"));
    host::ctrl_c().store(false, std::sync::atomic::Ordering::SeqCst);
    // What `--resume` needs from another folder: the bench file and directory variants as full paths.
    ctx.saved["args"]["pos"] = json!([file]);
    for k in ["head", "base"] {
        if let Some(p) = args.get(k).filter(|p| Path::new(p).is_dir()).and_then(|p| std::fs::canonicalize(p).ok()) {
            ctx.saved["args"]["flags"][k] = json!(p.display().to_string().trim_start_matches(r"\\?\"));
        }
    }
    let _ = store::write_json(&dir.join("state.json"), &ctx.saved);
    ctx.awake.hold(false);
    ctx.describe = ctx.runner.describe()?;
    if let Some(f) = args.get("filter") {
        let (text, err) = filter_report(&ctx.describe, f);
        if let Some(e) = err {
            return Err(e);
        }
        progress(text);
    }
    let series = expand(&ctx.describe, args)?;
    if series.is_empty() {
        return Err(Error::usage("no series matches --filter / --target / --params"));
    }
    if busy > 0.10 {
        ctx.warnings.push(validity::warn("Background", format!("{:.0}% of all CPUs were busy before the run; close heavy apps for tighter intervals", busy * 100.0)));
    }
    if env.on_battery {
        ctx.warnings.push(validity::warn("Battery", "on battery: clocks may drop mid-run; plug in"));
    }
    if let Some(why) = ctx.qos.clone() {
        // Frames and input latencies at background priority describe no person's machine: refused
        // (before anything builds when a primary is one). CPU metrics stay, with this warning.
        if let Some(s) = series.iter().find(|s| foreground_metric(&s.primary)).filter(|_| !check) {
            return Err(background_refusal(&why, &format!("{}'s primary {}", s.case, s.primary)));
        }
        ctx.warnings.push(validity::warn(
            "BackgroundPriority",
            format!("strobe runs at {why}, and the target inherits it: the OS keeps it on slow cores or behind other work, so frame and input-latency metrics are refused; CPU metrics are kept, but compare them only with runs at the same priority (run strobe at normal priority)"),
        ));
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
    let started = Instant::now();
    let max_time = args.get("max-time").map(duration_ms).transpose()?.map(Duration::from_millis);
    let mut series_out = Vec::new();
    let mut all_decided = true;
    let pixels_only = args.has("pixels-only");
    if pixels_only && variants.len() < 2 {
        return Err(Error::usage("--pixels-only compares base with head: add --base <variant>"));
    }
    // A stop (Ctrl-C, SIGTERM, `--stop`) or a failure ends the experiment with what it finished: the
    // series done, the pairs of the one running, and the series it never reached.
    let mut halted: Option<Error> = None;
    let mut not_run: Vec<String> = Vec::new();
    for s in &series {
        if halted.is_none() && stop_requested() {
            halted = Some(stop_error());
        }
        if halted.is_some() {
            not_run.push(s.label());
            continue;
        }
        // A resumed experiment keeps the series it had finished.
        if let Some(done) = ctx.saved["series"][s.label()].get("done").filter(|d| d.is_object()).cloned() {
            progress(format!("\n{}  finished before the resume: kept", s.label()));
            all_decided &= done["decided"].as_bool().unwrap_or(false);
            series_out.push(done);
            continue;
        }
        let r = if check {
            check_series(&mut ctx, s, &mut variants, &config)
        } else if pixels_only {
            (|| -> Result<Value> {
                let target = ctx.describe["targets"][&s.target].clone();
                release_machine(&mut ctx);
                ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
                for v in variants.iter_mut() {
                    prepare_variant(v, &target, &config, &ctx.harness, &ctx.project)?;
                }
                let pixels = pixels_series(&mut ctx, s, &variants)?;
                Ok(json!({ "label": s.label(), "case": s.case, "kind": "pixels", "pixels": pixels, "decided": pixels.as_array().is_some_and(|a| a.iter().all(|p| p["state"] == "same")) }))
            })()
        } else if leak_run {
            leak_series(&mut ctx, s, &mut variants, &config)
        } else {
            run_series(&mut ctx, s, &mut variants, &config, started, max_time)
        };
        let out = match r {
            Ok(out) => out,
            Err(e) => {
                halted = Some(if e.code == 130 || stop_requested() { stop_error() } else { e });
                not_run.push(s.label());
                continue;
            }
        };
        if let Some(h) = out.get("halt").filter(|h| h.is_object()) {
            halted = Some(Error { code: h["code"].as_i64().unwrap_or(1) as i32, message: h["message"].as_str().unwrap_or("").to_string(), hint: None });
        }
        if out["not_run"] == true {
            not_run.push(s.label());
            continue;
        }
        if halted.is_none() {
            checkpoint(&mut ctx, &s.label(), json!({ "done": out }));
        }
        all_decided &= out["decided"].as_bool().unwrap_or(false);
        series_out.push(out);
    }
    if halted.as_ref().is_some_and(|h| h.code == 130) {
        progress(format!("\nstopped: kept {} series; strobe bench --resume {exp} runs the rest", series_out.len()));
    }
    // Nothing finished and nothing to resume: the failure is the answer.
    if let Some(e) = halted.take_if(|e| e.code != 130 && series_out.is_empty() && ctx.saved["series"].as_object().is_none_or(|o| o.is_empty())) {
        release_machine(&mut ctx);
        ctx.runner.exit();
        return Err(e);
    }
    release_machine(&mut ctx);
    if let Some(p) = &ctx.power_used {
        env.power_plan = p.clone();
    }
    ctx.runner.exit();
    for home in std::mem::take(&mut ctx.homes).into_values() {
        let _ = std::fs::remove_dir_all(home);
    }
    if let Some(t) = &ctx.reserved {
        ctx.warnings.push(validity::info("ReservedCpus", t.text()));
    }
    for text in variants.iter().flat_map(|v| v.changed.iter()) {
        ctx.warnings.push(validity::warn("VariantChanged", text.clone()));
    }
    if let Some(h) = halted.as_ref().filter(|_| check || pixels_only) {
        ctx.warnings.push(validity::warn("Stopped", format!("{}; not run: {}", h.message, not_run.join(", "))));
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
            "pin": pin_label, "runs_per_block": ctx.runs, "aa": aa, "reserve": ctx.reserved, "reserve_mode": ctx.reserve,
            "looks": if args.get("pairs").is_none_or(|p| p == "auto") { LOOKS.to_vec() } else { vec![args.num("pairs")?.unwrap_or(0.0) as usize] },
            "alpha": if args.get("pairs").is_none_or(|p| p == "auto") { POCOCK_ALPHA } else { 0.05 },
        },
        "variants": variants.iter().map(|v| json!({
            "role": v.role, "spec": v.spec, "label": v.label, "subject": subject(v), "commit": v.commit, "dirty": v.dirty,
            "kind": match v.kind { Kind::Dir => "dir", Kind::Git(_) => "git", Kind::Url(_) => "url", Kind::App(_) => "app", Kind::Exe(_) => "exe" },
        })).collect::<Vec<_>>(),
        // The command line, for the rerun line under a PR table.
        "args": ctx.saved["args"],
        "series": series_out, "warnings": ctx.warnings, "decided": all_decided && halted.is_none(),
        "stopped": halted.as_ref().map(|h| json!({ "code": h.code, "why": h.message, "not_run": not_run })),
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
    if let Some(h) = &halted {
        out.code = h.code;
    }
    Ok(out)
}

/// `--check`: one block on head with one run, no warmup and no statistics. A failing case is reported
/// with what timed out and its screenshot, and the next case runs.
fn check_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value) -> Result<Value> {
    let target = ctx.describe["targets"][&s.target].clone();
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, &ctx.harness, &ctx.project)?;
    }
    let started = Instant::now();
    let log_from = std::fs::metadata(&ctx.runner.log).map(|m| m.len()).unwrap_or(0);
    // Kept, so its views (`query <rec> threads`, …) can follow up on what the check showed.
    let r = block_on_screen(ctx, s, &variants[0], 0, Set::Lean, true);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    // What the bench file's hooks printed during this case (console.log/error go to runner.log).
    let log: Vec<String> = std::fs::read(&ctx.runner.log)
        .ok()
        .and_then(|b| b.get(log_from as usize..).map(|b| String::from_utf8_lossy(b).lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).map(str::to_string).collect()))
        .unwrap_or_default();
    let (passed, error, hint, values, refused, locked, recording) = match r {
        Ok(b) => (true, Value::Null, Value::Null, json!(b.values), json!(b.refused), b.locked, json!(b.recording)),
        Err(e) => (false, json!(e.message), json!(e.hint), Value::Null, Value::Null, false, Value::Null),
    };
    progress(format!("  {} {}{}", if passed { "pass" } else { "FAIL" }, s.label(), error.as_str().map(|e| format!("  {e}")).unwrap_or_default()));
    // `--why always` profiles the case on head too: one attribution block, warmed like a bench's, for finding
    // where a single build spends its time before there is anything to compare.
    let mut attribution = Value::Null;
    if passed && ctx.args.get("why") == Some("always") {
        progress(format!("  attribution block ({}; not timed)", why_text(&target)));
        let v = &variants[0];
        // Warmed and run as in a bench (`--runs`, default 5): `--check`'s single cold run would start no
        // profiler, and one measured run is too few to read per-run numbers from.
        ctx.check = false;
        let check_runs = ctx.runs;
        ctx.runs = ctx.args.num("runs")?.unwrap_or(5.0).max(1.0) as usize;
        let attributed = if s.launch.is_object() { in_slot(ctx, &format!("{} {} attribution", s.label(), v.role), |ctx| launch::attribution(ctx, s, v)) } else { block_on_screen(ctx, s, v, usize::MAX, Set::Attribution, true) };
        ctx.runs = check_runs;
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
        "passed": passed, "decided": passed, "error": error, "hint": hint, "values": values, "refused": refused, "session_locked": locked, "elapsed_ms": elapsed_ms, "attribution": attribution, "target_kind": target["kind"],
        "recording": recording,
        "log": log, "log_path": ctx.runner.log,
    }))
}

fn check_output(exp: &str, dir: &Path, file: &Path, series: Vec<Value>, warnings: Vec<Warning>) -> Result<Output> {
    let failed = series.iter().filter(|s| s["passed"] != true).count();
    let mut text = String::new();
    for s in &series {
        // The declared metrics' values: one run's worth, enough to see that a counter counts.
        let shown = |name: &str| {
            s["values"][name]
                .as_f64()
                .map(|v| format!("{name} {}", fmt::value(v, model::metric(name).map_or("", |m| m.unit))))
                .or_else(|| s["refused"][name].as_str().map(|why| format!("{name} refused ({why})")))
        };
        // A launch's milestones too, declared or not: when each line or pixel change came.
        let declared: Vec<&str> = s["declared"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let milestones = s["values"].as_object().into_iter().flatten().map(|(k, _)| k.as_str()).filter(|k| k.starts_with("launch.") && !declared.contains(k));
        let value = declared.iter().copied().chain(milestones).filter_map(shown).collect::<Vec<_>>().join(" · ");
        let secs = s["elapsed_ms"].as_f64().unwrap_or(0.0) / 1e3;
        if s["passed"] == true {
            text += &format!("pass  {}  {value}  {secs:.1} s{}\n", s["label"].as_str().unwrap_or(""), if s["session_locked"] == true { " · session locked" } else { "" });
            if let Some(rec) = s["recording"].as_str() {
                text += &format!("      rec  {rec} · strobe query {rec} {}\n", if s["target_kind"] == "native" { "threads" } else { "summary" });
            }
            if let Some(rec) = s["attribution"].as_str() {
                text += &format!("      why  {rec} · strobe query {rec} {}\n", why_view(&s["target_kind"]));
            }
        } else {
            text += &format!("FAIL  {}  {secs:.1} s\n      {}\n", s["label"].as_str().unwrap_or(""), s["error"].as_str().unwrap_or("").replace('\n', "\n      "));
            if let Some(h) = s["hint"].as_str() {
                text += &format!("      fix: {h}\n");
            }
        }
        // The case's own console output: the last 10 lines, and where the rest is.
        let log: Vec<&str> = s["log"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !log.is_empty() {
            let skipped = log.len().saturating_sub(10);
            for l in &log[skipped..] {
                text += &format!("      log  {l}\n");
            }
            if skipped > 0 {
                text += &format!("      log  ({skipped} earlier lines in {})\n", s["log_path"].as_str().unwrap_or("runner.log"));
            }
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
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, &ctx.harness, &ctx.project)?;
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
        // A native target has no JS heap: its host memory (memory.app.host) is what grows.
        if entry["native_host"].is_object() {
            progress(format!("  {:<5} host     {}", v.role, entry["native_host"]["verdict"].as_str().unwrap_or("")));
            decided &= entry["native_host"]["decided"].as_bool() == Some(true);
        } else {
            progress(format!("  {:<5} js heap  {}", v.role, entry["js_heap"]["verdict"].as_str().unwrap_or("")));
            progress(format!("  {:<5} dom      {}", v.role, entry["dom_nodes"]["verdict"].as_str().unwrap_or("")));
            decided &= entry["js_heap"]["decided"].as_bool() == Some(true);
        }
        entry["recording"] = json!(b.recording);
        entry["values"] = json!(b.values);
        leak[v.role] = entry;
    }
    let (metric, part) = if leak["head"]["native_host"].is_object() { ("memory.app.host", "native_host") } else { ("js.heap.growth", "js_heap") };
    let head = &leak["head"][part];
    Ok(json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "leak",
        "primary": { "metric": metric, "verdict": head["verdict"], "decided": head["decided"] },
        "decided": decided, "leak": leak, "runs_per_half": n, "warnings": [], "blocks": [], "attribution": {},
        "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3,
    }))
}

/// The secondaries a series tests: its declared metrics and the defaults. A script's or native app's CPU
/// per run is its main.work, so it is tested as main.busy is for a page. `cpu.app.total` is CPU over run
/// time: once runs get shorter or longer (`rate_context`), its move says nothing about the CPU they spent
/// (a run 84% shorter for 80% less CPU reads +28%), so it is then context.
fn secondary_names(s: &Series, script: bool, rate_context: bool) -> Vec<String> {
    let mut names: Vec<String> = s.metrics.clone();
    for m in DEFAULT_SECONDARIES.iter().copied().chain(script.then_some("cpu.work")) {
        let skip = names.iter().any(|n| n == m) || m == s.primary || (s.primary == "cpu.process.total" && m == "cpu.app.total") || (m == "cpu.app.total" && rate_context);
        if !skip {
            names.push(m.to_string());
        }
    }
    names
}

/// The secondaries that will be tested after these pairs: those every block has a value of.
fn tested_names(s: &Series, script: bool, blocks: &[Block], pairs: &[(usize, usize)], bound: f64) -> Vec<String> {
    let rate_context = !same(blocks, pairs, "run.duration", bound);
    secondary_names(s, script, rate_context).into_iter().filter(|n| model::metric(n).is_some() && blocks.iter().all(|b| b.values.get(n).is_some_and(|v| v.is_finite()))).collect()
}

/// The fewest pairs whose exact signed-rank test can clear Holm's first step across `tests` secondaries.
fn holm_pairs(tests: usize) -> usize {
    (2..=60).find(|n| 2.0 / 2f64.powi(*n) <= 0.05 / tests.max(1) as f64).unwrap_or(60) as usize
}

/// Saves a series' finished blocks and pairs (or its result, once done) in `state.json`, so a bench
/// that stops or fails keeps them and `--resume` goes on from there.
fn checkpoint(ctx: &mut Ctx, label: &str, entry: Value) {
    ctx.saved["series"][label] = entry;
    if let Ok(dir) = store::experiment_dir(&ctx.exp) {
        let _ = store::write_json(&dir.join("state.json"), &ctx.saved);
    }
}

fn run_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value, started: Instant, max_time: Option<Duration>) -> Result<Value> {
    let def = model::metric(&s.primary)
        .ok_or_else(|| Error::usage(format!("{}: unknown primary metric {} (known: {})", s.case, s.primary, model::METRICS.iter().map(|m| m.name).collect::<Vec<_>>().join(", "))))?;
    let target = ctx.describe["targets"][&s.target].clone();
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, &ctx.harness, &ctx.project)?;
    }
    let bound =
        ctx.args.num("equivalence")?.map(|b| b / 100.0).unwrap_or_else(|| config["metrics"]["equivalence"].as_str().and_then(|e| e.trim_end_matches('%').parse::<f64>().ok()).unwrap_or(2.0) / 100.0);
    // Verdicts only at planned looks, each at a Pocock level, so looking more than once keeps the
    // overall false-positive rate at 5% (SPEC §9.1). `--pairs N` is one look at N at 95%.
    let (looks, confidence): (Vec<usize>, f64) = match ctx.args.get("pairs") {
        Some("auto") | None => (LOOKS.to_vec(), 1.0 - POCOCK_ALPHA),
        Some(n) => {
            let n = n.parse::<usize>().map_err(|_| Error::usage("--pairs: auto or a number"))?.max(2);
            if n < stats::min_pairs(0.95) && !ctx.warnings.iter().any(|w| w.id == "FewPairs") {
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
    // A resumed experiment (`--resume`) starts from the pairs it had finished.
    let saved = &ctx.saved["series"][s.label()];
    let mut blocks: Vec<Block> = serde_json::from_value(saved["blocks"].clone()).unwrap_or_default();
    let mut pairs: Vec<(usize, usize)> = serde_json::from_value(saved["pairs"].clone()).unwrap_or_default();
    pairs.retain(|&(a, b)| a < blocks.len() && b < blocks.len());
    if !pairs.is_empty() {
        progress(format!("  resumed with the {} pairs it had finished", pairs.len()));
    }
    let script = target["kind"] == "script" || is_native(&target);
    let mut stopped = String::from("max pairs");
    let mut last: Option<Comparison> = None;
    // Why the series ended early (a stop, or an error after some pairs), as the error to report.
    let mut halted: Option<Error> = None;
    // The primary's verdict at the look that decided it, kept while pairs continue for the secondaries.
    let mut decided_at: Option<Comparison> = None;
    'pairs: for pair in pairs.len()..max_pairs {
        if stop_requested() {
            halted = Some(stop_error());
            break;
        }
        let order: [usize; 2] = if pair % 2 == 0 { [0, 1] } else { [1, 0] };
        let mut idx = [0usize; 2];
        let mut redone = 0;
        loop {
            let start = blocks.len();
            for &v in &order {
                let keep = ctx.keep_all || pair == 0;
                // Timing blocks trace only what the metrics read; `--trace full` keeps every timeline category.
                let set = if ctx.args.get("trace") == Some("full") { Set::Timing } else { Set::Lean };
                let b = match block_on_screen(ctx, s, &variants[v], pair, set, keep) {
                    Ok(b) => b,
                    // A stop, or an error once pairs are done: the half pair goes, the finished ones stay.
                    Err(e) if e.code == 130 || stop_requested() || !pairs.is_empty() => {
                        blocks.truncate(start);
                        halted = Some(if e.code == 130 || stop_requested() { stop_error() } else { e });
                        break 'pairs;
                    }
                    Err(e) => return Err(e),
                };
                if pair == 0 {
                    progress(format!("  {} block  {}{}", b.variant, steps_text(&b.steps), if b.locked { " · session locked" } else { "" }));
                }
                idx[v] = blocks.len();
                blocks.push(b);
            }
            // Base and head of a pair share one lock state and one display state, or the pair runs again.
            if blocks[start].locked == blocks[start + 1].locked && blocks[start].display == blocks[start + 1].display {
                break;
            }
            for b in blocks.drain(start..) {
                if let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            redone += 1;
            if redone >= 5 {
                return Err(Error::refused(
                    "SessionLocked",
                    "the session kept locking and unlocking, or the display kept turning off and on, between a pair's blocks",
                    "bench while the session stays locked or stays unlocked",
                ));
            }
            progress(format!("  pair {}: the session locked or unlocked, or the display turned off or on, between its blocks; the pair runs again", pair + 1));
        }
        pairs.push((idx[0], idx[1]));
        checkpoint(ctx, &s.label(), json!({ "blocks": blocks, "pairs": pairs }));
        let primary_pairs: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], &s.primary), val(&blocks[b], &s.primary))).collect();
        let n = pair + 1;
        // Decided at a look, then run on to the pairs the secondaries need to clear Holm.
        if let Some(c) = &decided_at {
            progress(format!("  pair {n:>2}  for the secondaries (the primary was decided: {})", c.verdict));
            if n >= holm_pairs(tested_names(s, script, &blocks, &pairs, bound).len()) {
                stopped = "primary decided".into();
                break;
            }
            continue;
        }
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
        // A share at its ceiling on both sides (91% vs 92% dropped frames at background priority) moves
        // with nothing the builds do: more pairs only narrow an interval around "same at the ceiling".
        let medians = |side: usize| metrics::median(&primary_pairs.iter().map(|p| if side == 0 { p.0 } else { p.1 }).collect::<Vec<_>>());
        let saturated = look && !decided && def.unit == "%" && !s.primary.starts_with("cpu.") && medians(0) >= 80.0 && medians(1) >= 80.0;
        last = Some(cmp);
        if decided {
            // Secondaries clear Holm only from enough pairs: run on to them rather than leave them blank.
            let need = holm_pairs(tested_names(s, script, &blocks, &pairs, bound).len());
            if n < need && n < max_pairs && ctx.args.get("pairs").is_none() {
                progress(format!("  the primary is decided; {} more pair{} so the secondaries can clear Holm ({need} pairs)", need - n, if need - n == 1 { "" } else { "s" }));
                decided_at = last.clone();
                continue;
            }
            stopped = "primary decided".into();
            break;
        }
        if saturated {
            stopped = "saturated".into();
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
    if decided_at.is_some() {
        stopped = "primary decided".into();
    }
    if let Some(h) = &halted {
        if pairs.is_empty() {
            return Ok(json!({ "label": s.label(), "case": s.case, "not_run": true, "halt": { "code": h.code, "message": h.message } }));
        }
        if decided_at.is_none() {
            stopped = if h.code == 130 { "stopped".into() } else { format!("failed: {}", h.message.lines().next().unwrap_or("")) };
        }
    }
    // Pairs a resumed series had finished: the comparison at the last of them.
    if last.is_none() && !pairs.is_empty() {
        let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], &s.primary), val(&blocks[b], &s.primary))).collect();
        last = Some(stats::compare(&s.primary, def, &pp, bound, confidence));
    }
    let primary = last.ok_or_else(|| Error::msg("no pairs completed"))?;
    let primary_decided = stopped == "primary decided" && pairs.len() >= min_pairs;
    let mut primary = primary;
    if !primary_decided {
        primary.verdict = match stopped.as_str() {
            "saturated" => format!(
                "saturated: base {} and head {} sit near the ceiling, where the builds cannot differ; choose another primary (main.work, cpu.work)",
                fmt::value(primary.base, def.unit),
                fmt::value(primary.head, def.unit)
            ),
            "stopped" => format!("stopped after {} pairs: no verdict (resume to finish)", pairs.len()),
            s if s.starts_with("failed") => format!("unclear: {} pairs, then the bench failed", pairs.len()),
            "more pairs cannot decide it" | "max pairs" | "max time" => match primary.needed_pairs {
                Some(n) if n != usize::MAX && n > pairs.len() => format!("underpowered: ~{n} pairs needed"),
                _ if stopped == "max pairs" && ctx.args.get("pairs").is_none() => format!("unclear after {} pairs, the last planned look: the difference sits on the equivalence bound", pairs.len()),
                _ => "unclear".into(),
            },
            _ => "unclear".into(),
        };
    }
    // Tail percentiles need enough frames in every block.
    let primary = tail_check(def, &blocks, primary);
    // Secondaries: gated on the primary, Holm-corrected.
    let rate_context = !same(&blocks, &pairs, "run.duration", bound);
    let names = secondary_names(s, script, rate_context);
    let mut secondaries: Vec<Comparison> = Vec::new();
    for name in &names {
        let Some(d) = model::metric(name) else { continue };
        if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
            // A metric the series declared that a block refused shows as refused, not as missing.
            if let Some(why) = blocks.iter().find_map(|b| b.refused.get(name)) {
                secondaries.push(refused(name, d, why));
            }
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
    let holm = holm_pairs(tested.len());
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
    // A number from a locked screen is not comparable with one from an unlocked screen: pairs that
    // ran in both states decide nothing.
    let locked_pairs = pairs.iter().filter(|&&(a, _)| blocks[a].locked).count();
    let session = match locked_pairs {
        0 => "unlocked",
        n if n == pairs.len() => "locked",
        _ => "mixed",
    };
    let mut primary = primary;
    let mut primary_decided = primary_decided;
    if session == "mixed" {
        let why = format!("refused (SessionLocked): {locked_pairs} of {} pairs ran on a locked screen, the rest unlocked", pairs.len());
        primary.verdict = why.clone();
        primary.decided = false;
        primary_decided = false;
        for c in secondaries.iter_mut().chain(descriptive.iter_mut()) {
            c.verdict = why.clone();
            c.decided = false;
        }
        warnings.push(validity::warn("SessionLocked", format!("{locked_pairs} of {} pairs ran on a locked screen and the rest unlocked: nothing is decided; rerun in one state", pairs.len())));
    } else if session == "locked" {
        warnings.push(validity::info("SessionLocked", "every block ran on a locked screen (the series needs no presenting window): compare it only with runs that were locked too"));
    }
    // Each pair ran in one display state; pairs in different ones still compare within themselves.
    let dark = pairs.iter().filter(|&&(a, _)| blocks[a].display == Some(0)).count();
    if dark > 0 {
        let text = if dark == pairs.len() {
            "every block ran with the display off (a locked session's display sleeps): compare it only with runs whose display was off too".to_string()
        } else {
            format!("{dark} of {} pairs ran with the display off and the rest with it on; each pair ran in one state, so the comparison holds, but absolute values mix both", pairs.len())
        };
        warnings.push(validity::info("DisplayOff", text));
    }
    // Attribution: one block per variant with profilers, never timed. By default only when there is a
    // difference to explain; `--why always` keeps them for every series, `--why never` (or
    // `--probes timing`) skips them.
    let mut recordings = json!({});
    let differs = primary_decided && (primary.lo > 0.0 || primary.hi < 0.0);
    let why = match ctx.args.get("why").unwrap_or(if ctx.args.get("probes") == Some("timing") { "never" } else { "auto" }) {
        _ if halted.is_some() => false,
        "always" => true,
        "never" => false,
        "auto" => differs,
        other => return Err(Error::usage(format!("--why: auto, always or never, not {other}"))),
    };
    if !why && !differs && halted.is_none() {
        progress("  no attribution blocks: nothing to explain (--why always runs them anyway)");
    }
    if why {
        progress(format!("  attribution blocks ({}; not timed)", why_text(&target)));
        for v in variants.iter() {
            // A launch's attribution is a launch of its own, profiled from the first statement.
            let attributed = if s.launch.is_object() { in_slot(ctx, &format!("{} {} attribution", s.label(), v.role), |ctx| launch::attribution(ctx, s, v)) } else { block_on_screen(ctx, s, v, usize::MAX, Set::Attribution, true) };
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
    let pixels = if ctx.args.has("pixels") && variants.len() > 1 && halted.is_none() { pixels_series(ctx, s, variants)? } else { Value::Null };
    if pixels.as_array().is_some_and(|a| a.iter().any(|p| p["state"] != "same")) {
        warnings.push(validity::warn("PixelsDiffer", "base and head drew different pixels at a snapshot: see pixels"));
    }
    Ok(json!({
        "pixels": pixels,
        "label": s.label(), "case": s.case, "target": s.target, "target_kind": target["kind"], "params": s.params, "kind": if s.launch.is_object() { "launch" } else { "runs" },
        "primary": primary, "decided": primary_decided, "session": session, "secondaries": secondaries, "descriptive": descriptive, "holm_pairs": holm,
        "pairs": pairs.len(), "launches": blocks.len(), "stopped": stopped, "bound": bound,
        "steps": mean_steps(&blocks), "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3 - (ctx.waited_ms - waited_before), "waited_ms": ctx.waited_ms - waited_before,
        "blocks": blocks, "warnings": warnings, "attribution": recordings, "timing_recordings": timing_recordings,
        "halt": halted.map(|h| json!({ "code": h.code, "message": h.message })),
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
        let mut names: Vec<String> =
            std::fs::read_dir(root.join(role)).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".png").map(str::to_string)).collect();
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
/// Whether a note belongs in a PR table: every warning, and the context notes a reader needs to
/// know what the numbers compare with (a locked session, a sleeping display, the power plan, a
/// GPU that does not drive the display, a smoke run's few pairs, other processes kept off the target's CPUs).
fn in_pr(w: &Value) -> bool {
    w["level"] == "warn" || ["SessionLocked", "DisplayOff", "PowerPlan", "CrossAdapterScanout", "FewPairs", "StaleCalibration", "ReservedCpus"].contains(&w["id"].as_str().unwrap_or(""))
}

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
        for w in s["warnings"].as_array().into_iter().flatten().filter(|w| in_pr(w)) {
            out += &format!("\n- **{}:** {}\n", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or(""));
        }
        out += "\n";
    }
    if let Some(st) = r["stopped"].as_object() {
        let not_run: Vec<&str> = st.get("not_run").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let what = if st.get("code").and_then(Value::as_i64) == Some(130) { "stopped before it finished".to_string() } else { format!("failed before it finished ({})", st.get("why").and_then(Value::as_str).unwrap_or("")) };
        out += &format!("- **Incomplete:** the bench {what}; the tables hold the pairs it finished{}.\n\n", if not_run.is_empty() { String::new() } else { format!(", and these series did not run: {}", not_run.join(", ")) });
    }
    // The bench's own notes (power plan, lost present events, …), once.
    let general: Vec<&Value> = r["warnings"].as_array().into_iter().flatten().filter(|w| in_pr(w)).collect();
    for w in &general {
        out += &format!("- **{}:** {}\n", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or(""));
    }
    if !general.is_empty() {
        out += "\n";
    }
    let gpu = r["gpu"]["devices"].as_array().and_then(|d| d.first()).and_then(Value::as_str).unwrap_or("");
    let context = fmt::fields([
        r["env"]["os"].as_str().unwrap_or("").to_string(),
        r["env"]["cpu"].as_str().unwrap_or("").to_string(),
        r["env"]["power_plan"].as_str().filter(|p| !p.is_empty()).map(|p| format!("power plan {p}")).unwrap_or_default(),
        r["plan"]["reserve"]["cpus"].as_u64().map(|c| format!("other processes kept off CPUs {}", cpu_list(c))).unwrap_or_default(),
        gpu.to_string(),
        r["browser"].as_str().unwrap_or("").to_string(),
        fmt::size(&r["window"]),
        r["refresh_hz"].as_f64().map(|h| format!("{h:.0} Hz display")).unwrap_or_default(),
    ]);
    out += &format!(
        "`{}` against `{}`, alternating fresh launches: {context}. Intervals over pairs (Wilcoxon signed-rank, Hodges–Lehmann); secondaries gated on the primary and Holm-corrected.\n\n",
        variant_name(&base),
        variant_name(&head),
    );
    out += &format!("```sh\n{}\n```\n", rerun_command(r, &base, &head));
    out
}

/// The `strobe bench` line that reruns an experiment: its bench file, its variants by commit where it
/// has one (a folder is a commit plus whatever is in it now), and every flag that chose what ran
/// and how (filter, primary, pairs, runs, probes, …), not the handles.
fn rerun_command(r: &Value, base: &Value, head: &Value) -> String {
    let file = r["file"].as_str().unwrap_or("").trim_start_matches(r"\\?\").replace('\\', "/");
    let quote = |s: &str| if s.is_empty() || s.contains([' ', '"', '\'', '*', '&', '|', '<', '>', ';']) { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() };
    let spec = |v: &Value, fallback: &str| match (v["kind"].as_str(), v["commit"].as_str()) {
        (Some("dir"), Some(c)) => c.to_string(),
        _ => v["spec"].as_str().unwrap_or(fallback).to_string(),
    };
    let flags = r["args"]["flags"].as_object();
    let mut line = format!("strobe bench {}", quote(&file));
    if flags.is_some_and(|f| f.contains_key("aa")) {
        line += &format!(" --aa --head {}", quote(&spec(head, ".")));
    } else {
        line += &format!(" --base {} --head {}", quote(&spec(base, "<base>")), quote(&spec(head, ".")));
    }
    const SKIP: &[&str] = &["base", "head", "aa", "id", "json", "quiet", "verbose", "resume", "stop", "keep", "open"];
    let mut rest: Vec<(&String, &Value)> = flags.into_iter().flatten().filter(|(k, _)| !SKIP.contains(&k.as_str())).collect();
    rest.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in rest {
        match v.as_str() {
            Some("true") => line += &format!(" --{k}"),
            Some(x) => line += &format!(" --{k} {}", quote(x)),
            None => {}
        }
    }
    line
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

/// What an attribution block records for this target.
fn why_text(target: &Value) -> &'static str {
    if is_native(target) { "OS samples and native stacks (through the deep helper)" } else { "CPU and allocation stacks, forced layouts, GC, a heap snapshot" }
}

/// The view that explains an attribution recording of a target of this kind.
fn why_view(kind: &Value) -> &'static str {
    if kind == "native" { "hotspots --by package · threads · frames" } else { "hotspots --by package" }
}

/// A metric a block refused: `why` is "<validator>: <reason>".
fn refused(name: &str, def: &MetricDef, why: &str) -> Comparison {
    let mut c = stats::compare(name, def, &[], 0.0, 0.95);
    let (id, text) = why.split_once(": ").unwrap_or((why, ""));
    c.verdict = format!("refused ({id}): {text}");
    c.decided = false;
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
        // A trend smaller than 1% end to end (rounding of a near-constant difference) changes no verdict.
        let range = d.iter().copied().fold(f64::NEG_INFINITY, f64::max) - d.iter().copied().fold(f64::INFINITY, f64::min);
        if rho.abs() > 0.6 && range >= 0.01 {
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

/// How many times a block waits for other work to end before `Background` refuses it: each wait
/// and speed check takes about 4 minutes.
const BUSY_WAITS: u32 = 15;

/// Runs a block while the screen is unlocked and lit: waits out a locked session, redoes a block the
/// lock screen touched (Chromium hides its windows and nothing reaches the display, so its numbers
/// describe nothing), and wakes a display that went dark on idle, once, before a block refuses
/// `WindowHidden`. It also waits out someone using a Mac whose window it would cover, and other work
/// that keeps the CPUs busy (`Background`) for up to about an hour. The block's own checks still refuse
/// anything else.
fn block_on_screen(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    // A series that needs no presenting window runs through a lock, in one lock state per block.
    let mut retries = 0;
    if runs_locked(ctx, s, set) {
        loop {
            let (start, locks, unlocks) = (host::session_locked(), ctx.awake.locks(), ctx.awake.unlocks());
            let display = host::display_state();
            let r = slotted_block(ctx, s, v, pair, set, keep);
            if retry_launch(ctx, s, v, &r, &mut retries)? {
                continue;
            }
            let end = host::session_locked();
            let steady = start == end && if start { ctx.awake.unlocks() == unlocks } else { ctx.awake.locks() == locks };
            // The display turned off (or on) during the block: it ran in two machine states.
            let lit = display.is_none_or(|(_, n)| host::display_state().is_some_and(|(_, m)| m == n));
            if steady && lit {
                return r.map(|mut b| {
                    b.display = display.map(|d| d.0);
                    session_state(s, b, start)
                });
            }
            if let Some(dir) = r.as_ref().ok().and_then(|b| b.recording.as_deref()).and_then(|id| store::resolve(id).ok()) {
                let _ = std::fs::remove_dir_all(dir);
            }
            let what = if steady { "the display turned off or on" } else { "the session locked or unlocked" };
            progress(format!("  {what} during a {} block: discarded, it runs again", v.role));
        }
    }
    let mut woken = false;
    let mut busy_waits = 0;
    loop {
        host::wake_display();
        if woken {
            // The display and Chromium's occlusion tracking settle after the display comes back.
            std::thread::sleep(Duration::from_secs(3));
        }
        if host::session_locked() {
            progress("  waiting: the session is locked; strobe resumes when it is unlocked");
            // Nothing held while waiting: the user's power plan, sleep as they set it, and the slot, so
            // other benches run what needs no presenting window meanwhile.
            release_machine(ctx);
            ctx.reg.set("waiting", format!("{} {} block: the session is locked and the series needs a presenting window", s.label(), v.role));
            let waiting = Instant::now();
            while host::session_locked() {
                nap(Duration::from_secs(2))?;
            }
            // The desktop and the GPU settle after an unlock.
            nap(Duration::from_secs(5))?;
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
        }
        let before = ctx.awake.locks();
        let r = slotted_block(ctx, s, v, pair, set, keep);
        if retry_launch(ctx, s, v, &r, &mut retries)? {
            continue;
        }
        if !woken && r.as_ref().err().is_some_and(|e| e.message.contains("WindowHidden")) {
            progress(format!("  the {} window was hidden (a display off on idle?): waking the display and running the block again", v.role));
            woken = true;
            continue;
        }
        // macOS: someone is using the Mac, so strobe did not take their focus to uncover the window.
        // Wait for them to leave it, as for a locked session, holding nothing meanwhile.
        if r.as_ref().err().is_some_and(|e| e.message.contains(crate::target::native::COVERED)) && host::person_idle_s().is_some_and(|i| i < 60.0) {
            progress(format!("  waiting: someone is using this Mac and the {} window is covered; strobe runs the block again once nobody has touched it for a minute", v.role));
            release_machine(ctx);
            ctx.reg.set("waiting", format!("{} {} block: someone is using this Mac, and its window would be covered", s.label(), v.role));
            let waiting = Instant::now();
            while host::person_idle_s().is_some_and(|i| i < 60.0) {
                nap(Duration::from_secs(5))?;
            }
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
            continue;
        }
        // Other work (a build, an update) kept the CPUs busy through the speed check: on a shared
        // machine it ends. Wait for it, holding nothing, and run the block again; an hour of it refuses.
        if busy_waits < BUSY_WAITS && r.as_ref().err().is_some_and(|e| e.message.starts_with("Background: ")) {
            busy_waits += 1;
            progress(format!("  waiting: other work keeps the CPUs busy ({}); strobe runs the {} block again once it ends", r.as_ref().err().map(|e| e.message.rsplit("busiest processes (% of a core): ").next().unwrap_or("").to_string()).unwrap_or_default(), v.role));
            release_machine(ctx);
            ctx.reg.set("waiting", format!("{} {} block: other work keeps the CPUs busy", s.label(), v.role));
            let waiting = Instant::now();
            nap(Duration::from_secs(60))?;
            crate::validity::wait_quiet(None, Duration::from_secs(60));
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
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

/// Whether to run a block again after a launch that failed for a passing reason (`transient`): up to
/// `LAUNCH_RETRIES` times per block, after the machine has a minute to calm down, holding nothing.
fn retry_launch(ctx: &mut Ctx, s: &Series, v: &Variant, r: &Result<Block>, retries: &mut u32) -> Result<bool> {
    let Err(e) = r else { return Ok(false) };
    if stop_requested() {
        return Err(stop_error());
    }
    if *retries >= LAUNCH_RETRIES || !transient(e) {
        return Ok(false);
    }
    *retries += 1;
    progress(format!("  the {} launch failed ({}): runs again in 30 s ({} of {LAUNCH_RETRIES})", v.role, e.message.lines().next().unwrap_or(""), retries));
    release_machine(ctx);
    ctx.reg.set("waiting", format!("{} {} block: the launch failed and runs again", s.label(), v.role));
    let waiting = Instant::now();
    chromium::reap_orphans();
    nap(Duration::from_secs(30))?;
    crate::validity::wait_quiet(None, Duration::from_secs(60));
    ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
    Ok(true)
}

/// Puts the machine in the state blocks run in: the device's bench slot (SPEC §10.1
/// `ConcurrentBench`; queued for it with nothing else held), then the run's power plan, settled, and
/// keep-awake. Kept from one block to the next until the bench waits, builds, or another bench
/// queues (`release_machine`).
fn hold_machine(ctx: &mut Ctx, what: &str) -> Result<()> {
    if ctx.slot.is_some() && slot::others_queued(&ctx.exp) {
        release_machine(ctx);
    }
    match ctx.slot {
        Some(_) => ctx.reg.set("running", what),
        None => {
            let (s, waited) = ctx.reg.acquire(what)?;
            ctx.waited_ms += waited.as_secs_f64() * 1e3;
            ctx.slot = Some(s);
        }
    }
    if ctx.power.is_none() {
        let p = power_for(ctx.args)?;
        if let Some(why) = &p.kept
            && !ctx.warnings.iter().any(|w| w.id == "PowerPlan")
        {
            ctx.warnings.push(validity::info("PowerPlan", why.clone()));
        }
        ctx.power_used.get_or_insert_with(|| p.used.clone());
        let settle = p.switched();
        ctx.power = Some(p);
        if settle {
            // Cores unpark and clocks follow the new plan's policy within a second or two.
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    ctx.awake.hold(true);
    Ok(())
}

/// Gives back what `hold_machine` took: the user's power plan, sleep as their settings say, and the
/// slot for the next bench in line.
fn release_machine(ctx: &mut Ctx) {
    if ctx.power.take().is_some_and(|p| p.switched()) {
        progress("  power plan: the user's own again");
    }
    ctx.awake.hold(false);
    ctx.slot = None;
}

/// Runs `f` with the machine held for a block; hands the slot over after it when another bench waits.
fn in_slot<T>(ctx: &mut Ctx, what: &str, f: impl FnOnce(&mut Ctx) -> Result<T>) -> Result<T> {
    hold_machine(ctx, what)?;
    let r = f(ctx);
    if slot::others_queued(&ctx.exp) {
        release_machine(ctx);
    } else {
        ctx.reg.set("between", "between blocks; holds the slot until the next one or a wait");
    }
    r
}

/// One block in the bench slot, with this user's other processes kept off the target's CPUs while it
/// runs (`--reserve-cpus`, SPEC §9.4).
fn slotted_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    in_slot(ctx, &format!("{} {} block", s.label(), v.role), |ctx| {
        let reserve = reserve_for(ctx, set);
        ctx.target_qos = None;
        let r = any_block(ctx, s, v, pair, set, keep).and_then(|b| priority_state(ctx, s, set, b));
        let Some(report) = reserve.map(host::reserve::Reserve::finish) else { return r };
        ctx.reserved.get_or_insert_with(ReserveTotal::default).add(&report);
        r.map(|mut b| {
            b.reserve = Some(json!({ "moved": report.moved_total(), "started": report.started, "failed": report.failed.len(), "restored": report.restored, "overhead_pct": report.overhead_pct, "setup_ms": report.setup_ms }));
            b
        })
    })
}

/// Starts the reservation a block gets: `auto` for timed blocks (not `--check`, attribution, memory
/// or pixels blocks), `on` for every block, `off` never. It needs a pinned target.
fn reserve_for(ctx: &mut Ctx, set: Set) -> Option<host::reserve::Reserve> {
    let timed = !ctx.check && matches!(set, Set::Lean | Set::Timing) && ctx.pixels_dir.is_none();
    let wanted = match ctx.reserve {
        "on" => true,
        "off" => false,
        _ => timed,
    };
    if !wanted {
        return None;
    }
    let noted = ctx.warnings.iter().any(|w| w.id == "ReservedCpus");
    let Some(cpus) = ctx.pin else {
        if !noted {
            ctx.warnings.push(validity::info("ReservedCpus", "other processes were not kept off the target's CPUs: the target is not pinned (--pin none, or no V-Cache CCD for --pin auto)"));
        }
        return None;
    };
    match host::reserve::Reserve::start(cpus) {
        Ok(r) => Some(r),
        Err(why) => {
            if !noted {
                ctx.warnings.push(validity::info("ReservedCpus", format!("other processes were not kept off the target's CPUs: {why}")));
            }
            None
        }
    }
}

/// One block of whatever kind the target and case are.
fn any_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    let native = is_native(&ctx.describe["targets"][&s.target]);
    if s.launch.is_object() && native {
        native::launch(ctx, s, v, pair)
    } else if s.launch.is_object() {
        launch::block(ctx, s, v, pair)
    } else if native {
        native::block(ctx, s, v, pair, set, keep)
    } else if ctx.describe["targets"][&s.target]["kind"] == "script" {
        script::block(ctx, s, v, pair, set, keep)
    } else {
        run_block(ctx, s, v, pair, set, keep)
    }
}

/// `phase.<name>` for one run (ms): the summed spans between its `…:phase:<name>:start` and `:end`
/// marks on one clock; NaN when the run never entered the phase.
fn phase_ms(marks: &[(String, u64)], name: &str) -> f64 {
    let (start, end) = (format!(":phase:{name}:start"), format!(":phase:{name}:end"));
    let mut open: Option<u64> = None;
    let mut total: Option<u64> = None;
    for (key, at) in marks {
        if key.ends_with(&start) {
            open = Some(*at);
        } else if key.ends_with(&end)
            && let Some(from) = open.take()
        {
            *total.get_or_insert(0) += at.saturating_sub(from);
        }
    }
    total.map_or(f64::NAN, |ns| ns as f64 / 1e6)
}

/// Whether a series can run while the session is locked (SPEC §10.1 `SessionLocked`): nothing it
/// measures needs a presenting window. Script targets; leak runs; and a native target with
/// `window: false` (a tray app), or any native target under `--check` (no verdict: it validates the
/// file), whose metrics are all CPU (`ui.*` too), memory (GPU memory too), IO, counters, its run
/// time, GPU use as a secondary (refused while locked) and launch milestones (not pixels).
fn runs_locked(ctx: &Ctx, s: &Series, set: Set) -> bool {
    let t = &ctx.describe["targets"][&s.target];
    let offscreen = |m: &str| {
        ["cpu.", "ui.", "memory.", "io.", "count.", "gpu.", "phase."].iter().any(|p| m.starts_with(p)) || m == "run.duration" || (m.starts_with("launch.") && m != "launch.painted" && m != "launch.settled")
    };
    match t["kind"].as_str().unwrap_or("") {
        "script" => true,
        _ if set == Set::Memory => true,
        "native" if t["window"] == false || ctx.check || t["app"].as_str().and_then(|id| isolate::app(id, &ctx.config).ok()).is_some_and(|a| a.windowless) => {
            !gpu_use(&s.primary) && std::iter::once(&s.primary).chain(&s.metrics).all(|m| offscreen(m))
        }
        _ => false,
    }
}

/// GPU use (engine time, compiles), as opposed to GPU memory (`gpu.vram`, `gpu.shared`): what the app
/// allocated is the same whether or not a compositor shows it, but the work a GPU does for a screen
/// nobody sees is not.
fn gpu_use(metric: &str) -> bool {
    metric.starts_with("gpu.") && metric != "gpu.vram" && metric != "gpu.shared"
}

/// Records a block's lock state. On a locked screen the compositor draws nothing, so GPU use there
/// describes no person's screen: its values go, and a series that declares one shows it refused. GPU
/// memory stays: allocations need no presenting window, and both blocks of a pair share the state.
fn session_state(s: &Series, mut b: Block, locked: bool) -> Block {
    b.locked = locked;
    if locked {
        b.values.retain(|k, _| !gpu_use(k));
        for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| gpu_use(m)) {
            b.refused.insert(m.clone(), "SessionLocked: the session was locked, so the compositor drew nothing and GPU use describes no screen".into());
        }
    }
    b
}

/// Frames on screen and input latencies: what background priority makes meaningless (SPEC §10.1
/// `BackgroundPriority`). At background QoS a drag read 83-91% dropped frames that dropped 0.4% at
/// normal priority.
pub(crate) fn foreground_metric(m: &str) -> bool {
    ["frames.", "interaction.", "scroll.", "canvas.", "input."].iter().any(|p| m.starts_with(p)) || m == "launch.painted" || m == "launch.settled"
}

fn background_refusal(why: &str, what: &str) -> Error {
    Error::refused(
        "BackgroundPriority",
        format!("the target runs at {why}: the OS keeps it on slow cores or behind other work, so {what}, a frame or input-latency metric, describes no person's machine"),
        "run strobe at normal priority (not under taskpolicy -b, nice 19, SCHED_IDLE, an idle priority class or efficiency mode), or compare a CPU metric (cpu.work, main.work)",
    )
}

/// `BackgroundPriority`: a block whose target ran at background priority (inherited from strobe, or
/// its own) keeps no frame or input-latency values; declared ones show refused, and a primary among
/// them refuses the block (a check run lists it refused).
fn priority_state(ctx: &mut Ctx, s: &Series, set: Set, mut b: Block) -> Result<Block> {
    let Some(why) = ctx.target_qos.take().or_else(|| ctx.qos.clone()).filter(|_| !matches!(set, Set::Attribution | Set::Memory)) else { return Ok(b) };
    if foreground_metric(&s.primary) && !ctx.check {
        return Err(background_refusal(&why, &format!("the primary {}", s.primary)));
    }
    b.values.retain(|k, _| !foreground_metric(k));
    for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| foreground_metric(m)) {
        b.refused.insert(m.clone(), format!("BackgroundPriority: the target ran at {why}"));
    }
    if ctx.qos.is_none() && !ctx.warnings.iter().any(|w| w.id == "BackgroundPriority") {
        ctx.warnings.push(validity::warn("BackgroundPriority", format!("a {} block's target ran at {why}: its frame and input-latency metrics are refused", b.variant)));
    }
    Ok(b)
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
        crate::target::command::launch(&crate::target::command::Spec::from_target(&target, &root), &isolate::new_home(), &window, ctx.pin)?.0
    } else if app {
        let launch = variant_app(v, &target, &ctx.config)?;
        let seed = seed_dir(ctx, s, &target);
        let (b, g) = electron::launch(&launch, seed.as_deref(), &window, ctx.pin)?;
        guard = Some(g);
        b
    } else {
        chromium::launch(&window, false, ctx.pin, &[])?
    };
    let steps = RefCell::new(vec![("launch".to_string(), launched.elapsed().as_secs_f64() * 1e3)]);
    ctx.target_qos = host::background_qos(Some(browser.pid));
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
                let times: Vec<f64> =
                    page.eval("JSON.stringify(globalThis.__strobeCanvas?.splice(0) ?? [])").ok().and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())).unwrap_or_default();
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
                .map_err(|e| crate::runner::incomplete("prepare", "prepare", e, "", "fix the target's prepare in the bench file"))?;
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
        // A leak run reads heaps, not pixels: on a locked screen the hidden page is still measured.
        let hidden_ok = set == Set::Memory && host::session_locked();
        if !hidden_ok {
            validity::window_visible(&page)?;
        }
        lap("checks");
        ctx.runner
            .call("setup", json!({}), t, &serve)
            .map_err(|e| crate::runner::incomplete("setup", "setup", e, &snapshot(&page, &ctx.exp, "setup"), "fix the bench file's setup"))?;
        lap("setup");
        let procs0 = (host::now_ns(), host::all_processes());
        // A memory block (a leak run, SPEC §8.6) takes no trace: a live trace subscription would copy
        // every heap snapshot chunk, and nothing in it is timed. Its recording holds the snapshots.
        // What invalidated style (`query <rec> style`): attribution blocks of a series about style, or `--style`.
        let style = set == Set::Attribution && (ctx.args.has("style") || declared.iter().any(|m| *m == "main.style" || *m == "style.count"));
        let extra_categories: Vec<&str> = [
            (shift, "loading"),
            (gc, "disabled-by-default-v8.gc"),
            (gpu_compile, "gpu.dawn"),
            (gpu_compile, "disabled-by-default-gpu.dawn"),
            (style, "disabled-by-default-devtools.timeline.invalidationTracking"),
        ]
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
            driver.drags.borrow_mut().clear();
            driver.mark(&format!("{key}:start"))?;
            let t0 = host::now_ns();
            ctx.runner.call("run", json!({ "case": s.case }), run_timeout, &serve).map_err(|e| {
                crate::runner::incomplete(&format!("{} run", s.case), "run", e, &snapshot(&page, &ctx.exp, "run"), "the run threw or timed out; fix the bench file or the app")
            })?;
            driver.dispatch("frame", &[])?;
            let t1 = host::now_ns();
            let input = crate::driver::input_pacing(&driver.drags.take());
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
                    .map_err(|e| crate::runner::incomplete(&format!("{} reset", s.case), "reset", e, &snapshot(&page, &ctx.exp, "reset"), "fix the case's reset"))?;
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
            Ok(Run { ms: wall / 1e6, cpu, task: c1[0] + carry[0] - c0[0], parts, roles, t0, t1, total: began.elapsed().as_secs_f64() * 1e3, input })
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
        if !hidden_ok {
            validity::window_visible(&page)?;
        }
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
            let build = crate::target::isolate::build_for_os(&ctx.config["build"]);
            let served = v.root.as_ref().zip(ctx.config["serve"]["dir"].as_str().or(crate::target::isolate::per_os(&build["output"]))).map(|(r, d)| r.join(d));
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
        // `phase.<name>`: the run's phase windows from the trace's marks.
        for name in std::iter::once(&s.primary).chain(&s.metrics).filter_map(|m| m.strip_prefix("phase.")) {
            let per: BTreeMap<String, f64> = (0..measured.len())
                .filter_map(|j| {
                    let suffix = format!(":run:{}:phase:{name}", first + j);
                    let spans: Vec<f64> = data.windows.iter().filter(|w| w.key.ends_with(&suffix)).map(|w| (w.t1 - w.t0) as f64 / 1e6).collect();
                    (!spans.is_empty()).then(|| (key(j), spans.iter().sum()))
                })
                .collect();
            extra.insert(format!("phase.{name}"), per);
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
        // Drags: the input strobe gave, beside the frames it drew (a starved driver shows here).
        if measured.iter().any(|r| r.input.is_some()) {
            extra.insert("input.rate".to_string(), measured.iter().enumerate().filter_map(|(j, r)| Some((key(j), r.input?.0))).collect());
            extra.insert("input.gap.p95".to_string(), measured.iter().enumerate().filter_map(|(j, r)| Some((key(j), r.input?.1))).collect());
        }
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
            refused: BTreeMap::new(),
            locked: false,
            display: None,
            reserve: None,
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

/// The run's power plan (SPEC §9.4): `--power high` (the default) switches to the fastest plan the
/// device offers and puts the user's back afterwards, even after a kill (`host::power`); `--power
/// keep` keeps the user's. strobe pins the target to its CPUs itself, so it needs no core parking.
pub(crate) fn power_for(args: &Args) -> Result<host::power::Power> {
    let p = host::power::Power::for_run(power_keep(args)?);
    if p.kept.is_none() {
        progress(format!("  power plan: {} for this run", p.used));
    }
    Ok(p)
}

fn power_keep(args: &Args) -> Result<bool> {
    match args.get("power").unwrap_or("high") {
        "keep" => Ok(true),
        "high" => Ok(false),
        other => Err(Error::usage(format!("--power: high or keep, not {other}"))),
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

/// The target's `home` seed folder for this series (`{param}` filled in), relative to the bench file.
fn seed_dir(ctx: &Ctx, s: &Series, target: &Value) -> Option<PathBuf> {
    target["home"].as_str().map(|h| {
        let h = s.params.iter().fold(h.to_string(), |h, (k, val)| h.replace(&format!("{{{k}}}"), &value_text(val)));
        Path::new(ctx.describe["file"].as_str().unwrap_or(".")).parent().unwrap_or(Path::new(".")).join(h)
    })
}

/// Native targets (`native.rs`).
fn is_native(target: &Value) -> bool {
    target["kind"] == "native"
}

/// What an app target launches for a variant. `app:<id>` is that app's own profile; `exe:<path>` is
/// the target's profile (`app(id)`, or `exe(path, { app })`) with another build.
fn variant_app(v: &Variant, target: &Value, config: &Value) -> Result<isolate::App> {
    if let Kind::App(id) = &v.kind {
        return isolate::app(id, config);
    }
    let profile = match target["kind"].as_str() {
        Some("app") => target["id"].as_str(),
        Some("exe") => target["app"].as_str(),
        _ => return Err(Error::usage("not an app target")),
    };
    let base = match profile {
        Some(id) => isolate::app(id, config)?,
        None => isolate::App::bare(Path::new(target["path"].as_str().unwrap_or(""))),
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
        if filter.is_some_and(|f| !case_matches(f, &case)) {
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

/// `--filter`: comma-separated case names, each exact unless it holds `*` (a glob over the whole name).
fn case_matches(filter: &str, case: &str) -> bool {
    filter.split(',').map(str::trim).filter(|f| !f.is_empty()).any(|f| glob(f, case))
}

/// Which cases `--filter` picked and which it left out, before anything builds. Cases left out
/// that start with an exact filter are named with the glob that would take them too.
fn filter_report(describe: &Value, filter: &str) -> (String, Option<Error>) {
    let cases: Vec<String> = describe["cases"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    let (picked, left): (Vec<&String>, Vec<&String>) = cases.iter().partition(|c| case_matches(filter, c));
    let list = |v: &[&String]| v.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
    let near: Vec<&String> = left.iter().copied().filter(|c| filter.split(',').map(str::trim).any(|f| !f.contains('*') && !f.is_empty() && c.starts_with(f))).collect();
    let mut text = format!("filter \"{filter}\" (exact names, or globs with *): runs {}", if picked.is_empty() { "nothing".to_string() } else { list(&picked) });
    if !left.is_empty() {
        text += &format!(" · skips {}", list(&left));
    }
    let glob_hint = if near.is_empty() { String::new() } else { format!("to run {}: --filter \"{}*\"", list(&near), filter.trim()) };
    if !glob_hint.is_empty() {
        text += &format!(" · {glob_hint} too");
    }
    let err = picked.is_empty().then(|| {
        let cases = format!("cases: {} (exact names, or globs with *)", list(&cases.iter().collect::<Vec<_>>()));
        Error::usage(format!("--filter \"{filter}\" matches no case")).hint(if glob_hint.is_empty() { cases } else { format!("{glob_hint} · {cases}") })
    });
    (text, err)
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
        isolate::installed(&isolate::app(id, config)?)?;
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
            let mut v = Variant { role, spec: spec.to_string(), kind: Kind::Dir, label: if sha.is_empty() { name } else { format!("{name}@{sha}{}", if dirty { " (dirty)" } else { "" }) }, root: Some(root.clone()), base_url: None, server: None, exe: None, commit: None, dirty, state: tree_state(&root), changed: Vec::new() };
            v.commit = (!sha.is_empty()).then_some(sha);
            return Ok(v);
        } else {
            (Kind::Git(spec.to_string()), spec.to_string(), None)
        }
    };
    let mut v = Variant { role, spec: spec.to_string(), kind, label, root, base_url: None, server: None, exe: None, commit: None, dirty: false, state: None, changed: Vec::new() };
    if let Kind::Git(r) = &v.kind {
        let sha = host::output(Command::new("git").arg("-C").arg(project).args(["rev-parse", "--short=10", r]))
            .ok_or_else(|| Error::usage(format!("{spec} is neither a directory nor a git ref in {}", project.display())))?;
        v.label = format!("{r}@{sha}");
        v.commit = Some(sha);
    }
    Ok(v)
}

/// How a report names a variant: its commit (a ref that is not the commit itself in front, a
/// directory's uncommitted changes after), else its label.
fn variant_name(v: &Value) -> String {
    let Some(commit) = v["commit"].as_str() else { return v["label"].as_str().unwrap_or("").to_string() };
    let spec = v["spec"].as_str().unwrap_or("");
    let mut name = if v["kind"] == "git" && !commit.starts_with(spec) && !spec.starts_with(commit) { format!("{spec} ({commit})") } else { commit.to_string() };
    if v["dirty"] == true {
        name += " with uncommitted changes";
    }
    name
}

/// The bench's harness: the bench file's folder, the strobe.json in use and strobe.json's `harness` paths, in
/// the checkout that holds the bench file. Every variant builds and serves with it (`overlay_harness`).
struct Harness {
    from: PathBuf,
    paths: Vec<PathBuf>,
}

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
fn overlay_harness(v: &Variant, root: &Path, h: &Harness) -> Result<()> {
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
fn prepare_variant(v: &mut Variant, target: &Value, config: &Value, h: &Harness, project: &Path) -> Result<()> {
    // A directory is built when each series starts, from whatever it holds then: say so when that is
    // not what it held when the bench started (another branch checked out, edits saved meanwhile).
    if let (Kind::Dir, Some(root), Some(was)) = (&v.kind, v.root.clone(), v.state.clone()) {
        let now = tree_state(&root);
        if now.as_ref() != Some(&was) {
            let short = |s: &str| s.split(' ').next().unwrap_or("").chars().take(10).collect::<String>();
            let text = format!(
                "{}'s folder {} changed since the bench started (commit {} then, {} now{}): later series build what it holds now; bench a commit (a git ref) to pin it",
                v.role,
                root.display(),
                short(&was),
                now.as_deref().map(short).unwrap_or_else(|| "?".into()),
                if now.as_deref().map(short) == Some(short(&was)) { ", its uncommitted changes differ" } else { "" }
            );
            progress(format!("  ! {text}"));
            v.changed.push(text);
            v.state = now;
        }
    }
    // Command and script targets run in the variant's folder: a git ref gets its worktree, and the
    // target's own `build`, if any, runs there. strobe.json's web build and server are not theirs.
    if matches!(target["kind"].as_str(), Some("command") | Some("script")) {
        if let Kind::Git(r) = &v.kind
            && v.root.is_none()
        {
            v.root = Some(worktree(r, project)?);
        }
        if let Some(root) = v.root.clone() {
            overlay_harness(v, &root, h)?;
        }
        if let (Some(root), Some(cmd)) = (v.root.clone(), target["build"].as_str()) {
            build_variant(v.role, &root, &json!({ "command": cmd }), None)?;
        }
        return Ok(());
    }
    // A native target from a directory or a git ref is built with strobe.json's `build`, and its output
    // (the executable) copied into strobe's own folder for the variant, so base and head coexist even
    // when the build writes to one shared place (`{cache}`). Without a `build`, both run the profile's
    // or the target's executable.
    if is_native(target) {
        // `native(path)` names a fixed executable: no build replaces it.
        if !matches!(v.kind, Kind::Dir | Kind::Git(_)) || v.exe.is_some() || target["path"].is_string() {
            return Ok(());
        }
        let build = config.get("build").map(crate::target::isolate::build_for_os).unwrap_or(Value::Null);
        let build = &build;
        if !build["command"].is_string() {
            return Ok(());
        }
        if let Kind::Git(r) = &v.kind {
            v.root = Some(worktree(r, project)?);
        }
        let root = v.root.clone().ok_or_else(|| Error::msg("variant has no directory"))?;
        overlay_harness(v, &root, h)?;
        let output = crate::target::isolate::per_os(&build["output"]).ok_or_else(|| Error::usage(format!("a native target's build in strobe.json needs output for {}: the executable it builds", crate::target::isolate::os())))?;
        let dest = host::paths::sub("builds").join("out").join(hash_str(&root.to_string_lossy()));
        let built = root.join(cache_fill(output, &root));
        let exe = dest.join(built.file_name().ok_or_else(|| Error::usage(format!("build output {output} is not a file")))?);
        build_variant(v.role, &root, build, Some((&built, &exe)))?;
        v.exe = Some(exe);
        return Ok(());
    }
    if target["kind"].as_str() != Some("web") || v.base_url.is_some() || matches!(v.kind, Kind::Url(_)) {
        return Ok(());
    }
    if let Kind::Git(r) = &v.kind {
        v.root = Some(worktree(r, project)?);
    }
    let root = v.root.clone().ok_or_else(|| Error::msg("variant has no directory"))?;
    overlay_harness(v, &root, h)?;
    if let Some(build) = config.get("build").filter(|b| b.is_object()) {
        build_variant(v.role, &root, &crate::target::isolate::build_for_os(build), None)?;
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

/// `{cache}` in a build's command, output or env: a folder that lasts across builds and is shared by
/// every worktree of the repository (`<state>/builds/cache/<repo>`), such as a compiler's target folder,
/// so base and head reuse each other's dependency builds.
fn cache_fill(s: &str, root: &Path) -> String {
    if !s.contains("{cache}") {
        return s.to_string();
    }
    let common = host::output(Command::new("git").arg("-C").arg(root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))
        .and_then(|d| std::fs::canonicalize(d).ok())
        .unwrap_or_else(|| root.to_path_buf());
    let dir = host::paths::sub("builds").join("cache").join(hash_str(&common.to_string_lossy().to_lowercase()));
    let _ = std::fs::create_dir_all(&dir);
    s.replace("{cache}", &dir.display().to_string())
}

/// Runs `build` in `root` unless its key is unchanged. With `copy`, the built file (and a `.pdb` beside
/// it) goes to the second path: a build that writes to a shared place (`{cache}`) leaves the other
/// variant's output there, so a missing copy always rebuilds.
fn build_variant(role: &str, root: &Path, build: &Value, copy: Option<(&Path, &Path)>) -> Result<()> {
    let cmd = cache_fill(build["command"].as_str().unwrap_or(""), root);
    if cmd.is_empty() {
        return Ok(());
    }
    let cwd = root.join(build["cwd"].as_str().unwrap_or("."));
    let key = build_key(root, build);
    let marker = host::paths::sub("builds").join(format!("{}.key", hash_str(&root.to_string_lossy())));
    let output = crate::target::isolate::per_os(&build["output"]).map(|o| root.join(cache_fill(o, root)));
    let ready = match copy {
        Some((_, dest)) => dest.exists(),
        None => output.as_ref().is_none_or(|o| o.exists()),
    };
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(key.as_str()) && ready {
        progress(format!("  build {role}: unchanged, reusing"));
        return Ok(());
    }
    // Builds run on every CPU: strobe itself is pinned to the runner's CPUs, and children inherit
    // that. The pin comes back after it, and the machine settles before any timed block.
    let _unpinned = Unpinned::start();
    // A package with its own lockfile below the repository root (a site in a Rust repo) gets its own install:
    // a fresh worktree has none.
    if (cwd.join("bun.lock").exists() || cwd.join("bun.lockb").exists()) && !cwd.join("node_modules").exists() {
        progress(format!("  bun install ({role}, {})", build["cwd"].as_str().unwrap_or(".")));
        Command::new(crate::runner::find_bun().unwrap_or_else(|| "bun".into())).args(["install", "--frozen-lockfile"]).current_dir(&cwd).stdout(Stdio::null()).status()?;
    }
    progress(format!("  build {role}: {cmd}"));
    let mut c = shell(&cmd);
    c.current_dir(&cwd);
    // Source maps for attribution without changing what ships: a bundler config that reads this (Vite:
    // `build: { sourcemap: process.env.STROBE_SOURCEMAP }`) writes each map beside its script without the
    // `sourceMappingURL` comment, so the served scripts stay byte for byte the production ones.
    c.env("STROBE_SOURCEMAP", "hidden");
    if let Some(env) = build["env"].as_object() {
        for (k, v) in env {
            c.env(k, cache_fill(v.as_str().unwrap_or(""), root));
        }
    }
    let out = c.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        // A command that reports its failure on stdout (cmd's own commands do) shows that instead.
        let text = if out.stderr.iter().all(u8::is_ascii_whitespace) { &out.stdout } else { &out.stderr };
        let tail = String::from_utf8_lossy(text).lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        let log = host::paths::sub("builds").join("logs").join(format!("{}.log", hash_str(&root.to_string_lossy())));
        let saved = log.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())
            && std::fs::write(&log, [format!("$ {cmd}\n(in {})\n\n--- stdout\n", cwd.display()).as_bytes(), &out.stdout, b"\n--- stderr\n", &out.stderr].concat()).is_ok();
        let code = out.status.code().map(|c| format!(" (exit {c})")).unwrap_or_default();
        let whole = if saved { format!("; all of it: {}", log.display()) } else { String::new() };
        return Err(Error::msg(format!("build {role} failed{code}: {cmd}\n{tail}")).hint(format!(
            "it ran in {} with this machine's defaults{whole}. A toolchain or variable the build needs goes in strobe.json build.env, or build.{}.env for this OS only",
            cwd.display().to_string().trim_start_matches(r"\\?\"),
            crate::target::isolate::os()
        )));
    }
    if let Some((built, dest)) = copy {
        if !built.is_file() {
            return Err(Error::msg(format!("build {role}: the build succeeded but wrote no {}", built.display())).hint("set build.output in strobe.json to the executable the command writes"));
        }
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::copy(built, dest)?;
        // Debug symbols beside the executable, for native stacks (Cargo names a `my-app` binary's `my_app.pdb`).
        let stem = built.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        for name in [stem.clone(), stem.replace('-', "_")] {
            let pdb = built.with_file_name(format!("{name}.pdb"));
            if pdb.is_file() {
                std::fs::copy(&pdb, dest.with_file_name(format!("{name}.pdb")))?;
                break;
            }
        }
    }
    std::fs::write(marker, key)?;
    Ok(())
}

/// While alive, strobe (and what it starts) may use every CPU; dropping it restores strobe's pin and
/// waits up to 30 s for the CPUs to go quiet (a build's last processes, a scan of what it wrote).
struct Unpinned(Option<u64>);

impl Unpinned {
    fn start() -> Unpinned {
        let n = host::topology().logical.min(64);
        let all = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        Unpinned(host::set_affinity(all))
    }
}

impl Drop for Unpinned {
    fn drop(&mut self) {
        if let Some(mask) = self.0 {
            host::set_affinity(mask);
        }
        validity::wait_quiet(None, Duration::from_secs(30));
    }
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

/// A worktree of `project` at ref `r`, strobe's own (`<state>/builds/<sha12>`). One kept from an
/// earlier bench is reset to the ref first: a harness overlaid then (or anything a build wrote
/// into tracked files) must not carry into this bench. Ignored files (dependencies, build output)
/// stay, so builds and installs are reused.
fn worktree(r: &str, project: &Path) -> Result<PathBuf> {
    let git = || {
        let mut c = Command::new("git");
        c.arg("-C").arg(project);
        c
    };
    let sha = host::output(git().args(["rev-parse", r])).ok_or_else(|| Error::usage(format!("unknown git ref {r} in {}", project.display())))?;
    let dir = host::paths::sub("builds").join(&sha[..12.min(sha.len())]);
    if dir.join(".git").exists() {
        let at = |a: &[&str]| Command::new("git").arg("-C").arg(&dir).args(a).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        if !(at(&["checkout", "--force", "--detach", &sha]) && at(&["clean", "-fd"])) {
            return Err(Error::msg(format!("could not reset the worktree for {r} at {}", dir.display())).hint("remove that folder; strobe makes it again"));
        }
    } else {
        progress(format!("  worktree {r} → {}", dir.display()));
        let ok = git().args(["worktree", "add", "--detach"]).arg(&dir).arg(&sha).stdout(Stdio::null()).status()?.success();
        if !ok {
            return Err(Error::msg(format!("git worktree add for {r} failed")));
        }
        if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
            let _unpinned = Unpinned::start();
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

pub(crate) fn pinning(spec: &str, topo: &host::Topology) -> Result<(Option<u64>, Option<u64>, String)> {
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
    let plan = if s["primary"]["metric"] == "memory.app.host" {
        format!("warmup, then {} runs with the processes' memory read before the first and after each", 2 * n)
    } else {
        format!("warmup, then S0 · {n} runs · S1 · {n} runs · S2 · a post-GC census after every run")
    };
    out += &format!("plan   {plan} · {}\n", r["plan"]["pin"].as_str().unwrap_or(""));
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
        // A native target: its processes' memory read between runs, no heap snapshots.
        if l["native_host"].is_object() {
            out += &format!("  host      {}  · {}\n", l["native_host"]["verdict"].as_str().unwrap_or(""), range(&l["native_host"], &bytes));
            if l["native_private"].is_object() {
                out += &format!("  private   {}  · {}\n", l["native_private"]["verdict"].as_str().unwrap_or(""), range(&l["native_private"], &bytes));
            }
            out += &format!("  next      strobe query {} memory --at 0 · strobe query {} memory\n", l["recording"].as_str().unwrap_or(""), l["recording"].as_str().unwrap_or(""));
            continue;
        }
        out += &format!("  js heap   {}  · {}\n", l["js_heap"]["verdict"].as_str().unwrap_or(""), range(&l["js_heap"], &bytes));
        out += &format!("  dom       {}  · {}\n", l["dom_nodes"]["verdict"].as_str().unwrap_or(""), range(&l["dom_nodes"], &nodes));
        let sus = &l["suspects"];
        out += &format!("  suspects  {} objects from runs 1–{n} still alive after run {} · {} retained\n", sus["count"], 2 * n, fmt::bytes(sus["retained_size"].as_f64().unwrap_or(0.0)));
        for c in sus["classes"].as_array().into_iter().flatten().take(5) {
            out += &format!("            {:>9}  {:>6}  {}\n", fmt::bytes(c["retained_size"].as_f64().unwrap_or(0.0)), c["count"], c["class"].as_str().unwrap_or(""));
        }
        out += &format!("  next      strobe query {} leaks · strobe query {} memory\n", l["recording"].as_str().unwrap_or(""), l["recording"].as_str().unwrap_or(""));
    }
    out += if s["primary"]["metric"] == "memory.app.host" {
        "\n  slopes: Theil–Sen over the readings, Sen's 95% interval; bound max(16 KB, 0.1% of host memory) per run\n  host is memory.app.host (heap, stacks, anonymous memory), read before run 1 and after every run; private is memory.app.private\n\n"
    } else {
        "\n  slopes: Theil–Sen over the censuses, Sen's 95% interval; bound max(16 KB, 0.1% of the heap) and 1 DOM node per run\n\n"
    };
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
        let cpus = env["logical_cpus"].as_u64().map(|n| format!("{n} logical CPUs")).unwrap_or_default();
        out += &format!("env    {}\n", fmt::fields([env["os"].as_str().unwrap_or(""), env["cpu"].as_str().unwrap_or(""), &cpus]));
        let gpu = r["gpu"]["devices"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
        let window = Some(fmt::size(&r["window"])).filter(|s| !s.is_empty()).map(|s| format!("window {s}")).unwrap_or_default();
        let refresh = r["refresh_hz"].as_f64().map(|h| format!("display {h:.0} Hz")).unwrap_or_default();
        let display = fmt::fields([r["browser"].as_str().unwrap_or(""), gpu, &window, &refresh]);
        if !display.is_empty() {
            out += &format!("       {display}\n");
        }
        let screen = match s["session"].as_str() {
            Some("locked") => "session locked (nothing measured needs a presenting window)",
            Some("mixed") => "session locked in some pairs only: nothing decided",
            _ => "visible",
        };
        let reserved = r["plan"]["reserve"]["cpus"].as_u64().map(|c| format!("others kept off CPUs {}", cpu_list(c))).unwrap_or_default();
        let power = format!("power plan {}", env["power_plan"].as_str().filter(|p| !p.is_empty()).unwrap_or("?"));
        out += &format!("plan   {}\n", fmt::fields([r["plan"]["pin"].as_str().unwrap_or(""), &reserved, &power, &format!("{screen}, unthrottled")]));
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
                "\nwhy    attribution blocks: base {} · head {}\n       strobe query {} {}\n",
                attr["base"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("rec_…"),
                why_view(&s["target_kind"])
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
    let id = r["id"].as_str().unwrap_or("");
    if let Some(st) = r["stopped"].as_object() {
        let not_run: Vec<&str> = st.get("not_run").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let why = st.get("why").and_then(Value::as_str).unwrap_or("");
        out += &format!("{}{}\n", if st.get("code").and_then(Value::as_i64) == Some(130) { "stopped: kept what finished".to_string() } else { format!("failed: {why}; kept what finished") }, if not_run.is_empty() { String::new() } else { format!("; not run: {}", not_run.join(", ")) });
        out += &format!("resume  strobe bench --resume {id}\n");
    }
    out += &format!("{id} · strobe report experiment {id} --open\n");
    out
}

#[cfg(test)]
mod phase_tests {
    use super::phase_ms;

    #[test]
    fn a_phase_sums_its_spans_and_is_unknown_when_never_entered() {
        let marks: Vec<(String, u64)> = [("run:0:phase:finish:start", 1_000_000), ("run:0:phase:other:start", 1_500_000), ("run:0:phase:finish:end", 4_000_000), ("run:0:phase:finish:start", 10_000_000), ("run:0:phase:finish:end", 12_500_000)]
            .into_iter()
            .map(|(k, t)| (k.to_string(), t))
            .collect();
        assert_eq!(phase_ms(&marks, "finish"), 5.5);
        assert!(phase_ms(&marks, "other").is_nan());
        assert!(phase_ms(&marks, "start").is_nan());
    }
}
