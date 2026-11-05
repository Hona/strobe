//! `strobe bench`: experiments with verdicts (SPEC §9). Blocks of fresh launches, alternating A B, B A.

use super::{Output, fmt, progress, stop_error, stop_requested};
use crate::cli::{Args, duration_ms};
use crate::config;
use crate::driver::Driver;
use crate::host::{self, git};
use crate::metrics;
use crate::model::{self, Error, Hidden, MetricDef, Refusal, Result, ValidatorId, VerdictKind};
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
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod block;
mod launch;
mod native;
pub mod plan;
mod script;
mod shown;
pub mod slot;

pub use shown::shown_line;

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
    /// The case's `timeout` (ms) for a run or reset, when it sets one.
    timeout: Option<u64>,
    /// Non-inferiority margins by metric (`--noworse`, the case's `noworse`): these metrics answer "is
    /// head at most this much worse?" instead of "did it change?" (SPEC §9.3).
    noworse: BTreeMap<String, f64>,
    /// What the block must show (`expect`: the target's, with the case's keys over it): a block on
    /// another screen is refused `WrongScreen`.
    expect: Value,
    /// The case's `gpuCache` (`warm` or `cold`), over its target's and its app profile's.
    gpu_cache: Option<String>,
}

impl Series {
    /// How long a run or reset may take (the case's `timeout`, default 60 s).
    fn run_timeout(&self) -> Duration {
        Duration::from_millis(self.timeout.unwrap_or(60_000).max(1))
    }

    /// How long setup and other hooks may take: 120 s, or the case's run timeout when longer.
    fn hook_timeout(&self) -> Duration {
        self.run_timeout().max(Duration::from_secs(120))
    }

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
    /// The variant's server with its process group (`host::spawn_owned`): ended with the variant.
    server: Option<host::spawn::Owned>,
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
    let sha = git::out(root, &["rev-parse", "HEAD"])?;
    let diff = git::bytes(root, &["diff", "HEAD", "--no-ext-diff", "--binary"])?;
    Some(format!("{sha} {:x}", sha2::Sha256::digest(diff)))
}

/// What a recording says about the variant it ran: the commit, uncommitted changes and their hash
/// (what the build read), so a kept or `--check` recording still names its code later.
fn variant_git(v: &Variant) -> Value {
    let changes = v.state.as_deref().and_then(|s| s.split_once(' ')).map(|(_, h)| &h[..12.min(h.len())]).filter(|_| v.dirty);
    json!({ "spec": v.spec, "commit": v.commit, "dirty": v.dirty, "changes": changes })
}

/// What a variant is for the rerun ledger: a commit (with a hash of a folder's uncommitted changes),
/// an app or executable with its file's size and time, or a URL. A folder and a git ref at the same
/// clean commit are the same variant.
fn variant_identity(v: &Variant, project: &Path, config: &Value) -> String {
    let stamp = |p: &Path| {
        std::fs::metadata(p).ok().map(|m| format!("@{}-{}", m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()))).unwrap_or_default()
    };
    match &v.kind {
        Kind::Dir => match v.state.as_deref().and_then(|s| s.split_once(' ')) {
            Some((sha, diff)) if v.dirty => format!("{sha}+{}", &diff[..12.min(diff.len())]),
            Some((sha, _)) => sha.to_string(),
            None => format!("dir:{}", v.root.as_deref().unwrap_or(Path::new(&v.spec)).display()),
        },
        Kind::Git(r) => git::commit(project, r).unwrap_or_else(|| v.commit.clone().unwrap_or_default()),
        Kind::App(id) => format!("app:{id}{}", isolate::app(id, config).map(|a| stamp(&a.exe)).unwrap_or_default()),
        Kind::Exe(p) => format!("exe:{}{}", p.display(), stamp(p)),
        Kind::Url(h) => format!("url:{h}"),
    }
}

/// The experiment's identity for the rerun ledger (`store::ledger`, SPEC §14.2): its variants, the bench
/// file and harness by content, its series with their primaries and margins, and the flags that chose
/// what ran and how it was judged.
fn identity(variants: &[Variant], file: &Path, harness: &Harness, args: &Args, series: &[Series], project: &Path, config: &Value) -> Value {
    use sha2::Digest;
    let short = |d: &[u8]| d.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>();
    let file_hash = std::fs::read(file).map(|b| short(&sha2::Sha256::digest(&b))).unwrap_or_default();
    // The harness by content; large files (seed models) by size and time, which is enough to see a change.
    let mut h = sha2::Sha256::new();
    let mut files: Vec<PathBuf> = harness
        .paths
        .iter()
        .flat_map(|p| {
            let src = harness.from.join(p);
            if src.is_dir() { walk(&src) } else { vec![src] }
        })
        .collect();
    files.sort();
    for f in files {
        let Ok(m) = std::fs::metadata(&f) else { continue };
        h.update(f.strip_prefix(&harness.from).unwrap_or(&f).to_string_lossy().replace('\\', "/").as_bytes());
        if m.len() <= 1 << 20 {
            h.update(std::fs::read(&f).unwrap_or_default());
        } else {
            h.update(format!("{}-{:?}", m.len(), m.modified().ok()).as_bytes());
        }
    }
    const SKIP: &[&str] = &["base", "head", "aa", "id", "json", "quiet", "verbose", "resume", "stop", "keep", "open", "progress", "max-time", "plan"];
    let flags: serde_json::Map<String, Value> = args.to_json()["flags"].as_object().into_iter().flatten().filter(|(k, _)| !SKIP.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut ids: Vec<String> = variants.iter().map(|v| variant_identity(v, project, config)).collect();
    if ids.len() == 1 {
        ids.insert(0, ids[0].clone());
    }
    json!({
        "variants": ids,
        "file": file.file_name().map(|f| f.to_string_lossy().to_string()),
        "file_hash": file_hash,
        "harness": short(&h.finalize()),
        "flags": flags,
        "series": series.iter().map(|s| json!({ "label": s.label(), "case": s.case, "primary": s.primary, "noworse": s.noworse })).collect::<Vec<_>>(),
    })
}

thread_local! {
    /// The saved state `--resume` hands to the bench it starts.
    static RESUMED: RefCell<Option<Value>> = const { RefCell::new(None) };
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
    // Unreachable (255): no debugging port, a connection that closed. A timeout (124) only from CDP: a
    // call with no reply; a hook or a wait that timed out is the bench's own.
    e.validator().is_none() && (e.code == 255 || (e.code == 124 && e.message.starts_with("cdp ")))
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
    /// What the OS counted for the target's processes (`energy.cpu`, `cpu.instructions`, …).
    counted: probe::wakes::Counted,
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
    /// Metrics this block cannot defend, with the validator and why (kept as "WindowHidden: …"):
    /// they get no value, and a series that declares one shows it refused.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", with = "model::refusal_lines")]
    refused: BTreeMap<String, Refusal>,
    /// The block ran on a locked screen (a series that needs no presenting window, `runs_locked`).
    locked: bool,
    /// The console display's state through the block (0 off, 1 on, 2 dimmed), when known: a display
    /// that sleeps under a locked series changes what the machine does (the compositor parks, the GPU
    /// idles deeper), so both blocks of a pair share it.
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<u8>,
    /// The display, session and compositor over the block (`screen_state`), where strobe read them.
    #[serde(skip_serializing_if = "Option::is_none")]
    screen: Option<Value>,
    /// What `--reserve-cpus` did in this block: processes moved, started during it, not movable, and
    /// the sweep's own CPU (% of one core).
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve: Option<Value>,
    /// What the window or page showed before the runs and after them (`shown`).
    #[serde(skip_serializing_if = "Option::is_none")]
    shown: Option<shown::Shown>,
    /// What strobe itself cost over the runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    overhead: Option<Overhead>,
}

/// What an experiment does, decided from its flags before anything runs and never changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `--check`: one untimed block per series on head, one run, no statistics.
    Check,
    /// Pairs of timed blocks, base and head alternating, with verdicts (`--base`, `--aa`).
    Compare,
    /// `--blocks N` without a base: N timed blocks of head alone and their values, no comparison
    /// (what `track` records for a commit).
    Blocks(usize),
    /// `--probes memory`: a leak run per variant.
    Leak,
    /// `--pixels-only`: base twice and head once, untimed, at the case's snapshots.
    Pixels,
}

/// How one block runs, fixed before it starts: what it traces, how many runs it measures, whether
/// it warms up first and counts as timed, whether its recording is kept, and where its snapshots go.
#[derive(Clone, Debug)]
struct Exec {
    set: Set,
    /// Measured runs (a leak run: per half).
    runs: usize,
    /// One run, no warmup: a block that asks whether a case works or what it draws (`--check`,
    /// pixels), not what it costs. It reserves no CPUs, discards no first launch, and keeps frame
    /// metrics only as refused at background priority instead of refusing the block.
    untimed: bool,
    keep: bool,
    /// `--pixels`: the folder its `page.snapshot()`s go to.
    pixels: Option<PathBuf>,
    /// A counter block (Linux, SPEC §7.5): perf events open on the target's threads, no trace or
    /// present capture, and its time metrics dropped: the events slow every context switch.
    counters: bool,
}

impl Exec {
    fn timed(set: Set, runs: usize, keep: bool) -> Exec {
        Exec { set, runs, untimed: false, keep, pixels: None, counters: false }
    }

    fn untimed(keep: bool, pixels: Option<PathBuf>) -> Exec {
        Exec { set: Set::Lean, runs: 1, untimed: true, keep, pixels, counters: false }
    }

    /// This block as a counter block, or as it is.
    fn counting(self, counters: bool) -> Exec {
        Exec { counters, ..self }
    }

    /// A timing block whose numbers become a verdict: what `--reserve-cpus auto` reserves for.
    fn measures_time(&self) -> bool {
        !self.untimed && matches!(self.set, Set::Lean | Set::Timing)
    }
}

/// Where a series' counter metrics come from (SPEC §7.5). Timing blocks never open Linux's perf
/// events; a series that declares `cpu.instructions` there gets counter blocks of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Counting {
    /// No perf events: macOS and Windows (rusage is free; Windows refuses), or no `cpu.instructions`.
    InTiming,
    /// Only counter metrics (and untimed ones) declared: every block is a counter block.
    Only,
    /// Counter and time metrics declared: timing blocks stay clean and a counter pass follows (or,
    /// with `cpu.instructions` the primary, comes first and a timing pass follows).
    Separate,
}

/// The series' declared metrics: its primary, its metrics and those it gives a no-worse margin.
fn declared_metrics(s: &Series) -> Vec<&String> {
    let mut out: Vec<&String> = std::iter::once(&s.primary).chain(&s.metrics).collect();
    for m in s.noworse.keys() {
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// A metric that perf events disturb: wall and CPU time, CPU shares, frames and input latency.
fn time_metric(m: &str) -> bool {
    !probe::wakes::COUNTER_METRICS.contains(&m)
        && !m.starts_with("cpu.foreign")
        && (model::metric(m).is_some_and(|d| d.unit == "ms" || d.unit == "%") || ["frames.", "interaction.", "scroll.", "canvas.", "input."].iter().any(|p| m.starts_with(p)))
}

fn counting(s: &Series) -> Counting {
    let declared = declared_metrics(s);
    // A launch reads no counters (`counter_state` says so).
    if !cfg!(target_os = "linux") || s.launch.is_object() || !probe::wakes::wants_perf(declared.iter().copied()) {
        Counting::InTiming
    } else if declared.iter().any(|m| time_metric(m)) {
        Counting::Separate
    } else {
        Counting::Only
    }
}

/// Copies what `from` measured of the metrics `pick` names (values and refusals) into `into`: a
/// counter block's counts into its timing block, or the other way round.
fn take_metrics(into: &mut Block, from: &Block, pick: impl Fn(&str) -> bool) {
    into.values.retain(|k, _| !pick(k));
    into.refused.retain(|k, _| !pick(k));
    into.values.extend(from.values.iter().filter(|(k, _)| pick(k)).map(|(k, v)| (k.clone(), *v)));
    into.refused.extend(from.refused.iter().filter(|(k, _)| pick(k)).map(|(k, r)| (k.clone(), r.clone())));
}

/// A counter metric that only a counter block measures on this OS.
fn counted_apart(m: &str) -> bool {
    cfg!(target_os = "linux") && m == "cpu.instructions"
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
    /// Per target, how its launches found the GPU shader caches (`gpuCache`), for the plan.
    gpu_cache: BTreeMap<String, String>,
    /// What the experiment does, fixed when it starts.
    mode: Mode,
    /// `--check --headless`: pages run in a headless browser, which draws nothing to a display.
    headless: bool,
    /// Series and part of strobe already warned over its §19 budget (`OverheadBudget`).
    overhead_warned: std::collections::BTreeSet<String>,
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
    /// Per series, the planned time of the series after it (ms), for the ETA (`plan`).
    eta_after: BTreeMap<String, f64>,
    /// Per series, variant and moment, the first block's screen and hash: a thumbnail is kept for it
    /// and for blocks that showed something else (`shown`).
    shown_seen: BTreeMap<String, (String, Option<u64>)>,
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
        let mut t = format!(
            "other processes of this user kept off CPUs {cpus} during {} timed block{}: {:.0} moved per block ({names}{})",
            self.blocks,
            if self.blocks == 1 { "" } else { "s" },
            self.moved_per_block,
            if top.len() > 4 { ", …" } else { "" }
        );
        if self.started > 0 {
            t += &format!(", {} more started during blocks and moved within 200 ms", self.started);
        }
        if !self.failed.is_empty() {
            let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (n, why) in &self.failed {
                by.entry(why.as_str()).or_default().push(n.as_str());
            }
            let failed = by
                .iter()
                .map(|(why, names)| format!("{} ({why})", names.iter().take(4).copied().collect::<Vec<_>>().join(", ") + if names.len() > 4 { ", …" } else { "" }))
                .collect::<Vec<_>>()
                .join("; ");
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

/// What strobe itself cost over a block's runs (rule 2, SPEC §19), each as % of one core: its own
/// process (every thread: DevTools, samplers, accessibility events, the reservation sweep), the
/// bench runner that runs the file's hooks, and the parts that measure themselves.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
#[serde(default)]
struct Overhead {
    #[serde(skip_serializing_if = "Option::is_none")]
    strobe_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_pct: Option<f64>,
    /// The OS and GPU sampler's thread (§19: 0.5%).
    #[serde(skip_serializing_if = "Option::is_none")]
    sampler_pct: Option<f64>,
    /// The OS present session's consumer (§19: 0.5%).
    #[serde(skip_serializing_if = "Option::is_none")]
    present_pct: Option<f64>,
    /// The reserved-CPU sweep (§19: 5%, on strobe's own CPUs).
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve_pct: Option<f64>,
}

impl Overhead {
    /// `strobe 1.2% · runner 0.3% · sampler 0.08% of a core`.
    fn text(&self) -> String {
        let parts: Vec<String> = [("strobe", self.strobe_pct), ("runner", self.runner_pct), ("sampler", self.sampler_pct), ("presents", self.present_pct), ("reserve", self.reserve_pct)]
            .into_iter()
            .filter_map(|(n, p)| Some(format!("{n} {}", pct_of_core(p?))))
            .collect();
        if parts.is_empty() { String::new() } else { format!("{} of a core", parts.join(" · ")) }
    }

    /// Where a part went over its §19 budget: (part, its share, the budget).
    fn over(&self) -> Vec<(&'static str, f64, f64)> {
        [("the sampler", self.sampler_pct, 0.5), ("the present session's consumer", self.present_pct, 0.5), ("the reserved-CPU sweep", self.reserve_pct, 5.0)]
            .into_iter()
            .filter_map(|(n, p, b)| p.filter(|p| *p > b).map(|p| (n, p, b)))
            .collect()
    }
}

/// The blocks' overhead, each part's mean over the blocks that measured it.
fn mean_overhead(blocks: &[Block]) -> Option<Overhead> {
    let all: Vec<&Overhead> = blocks.iter().filter_map(|b| b.overhead.as_ref()).collect();
    let mean = |f: fn(&Overhead) -> Option<f64>| {
        let v: Vec<f64> = all.iter().filter_map(|o| f(o)).filter(|x| x.is_finite()).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    };
    (!all.is_empty()).then(|| Overhead {
        strobe_pct: mean(|o| o.strobe_pct),
        runner_pct: mean(|o| o.runner_pct),
        sampler_pct: mean(|o| o.sampler_pct),
        present_pct: mean(|o| o.present_pct),
        reserve_pct: mean(|o| o.reserve_pct),
    })
}

fn pct_of_core(p: f64) -> String {
    if p < 1.0 { format!("{p:.2}%") } else { format!("{p:.1}%") }
}

/// One run: its span (TSC units, or ns), strobe's and the runner's CPU in it (the same units).
type RunCost = (u64, Option<u64>, Option<u64>);

/// strobe's own process and the runner's CPU inside the runs only (rule 2): read just before each
/// run begins and just after it ends, so the work strobe does between runs (reading the target's
/// processes, snapshots, resets) is never counted, and the share is over the measured runs' time.
/// Each read is of the one process (`host::process_cycles`), not a list of the system's.
struct SelfCost {
    runner: u32,
    open: Option<(u64, Option<u64>, Option<u64>)>,
    runs: Vec<RunCost>,
}

impl SelfCost {
    fn new(runner: u32) -> SelfCost {
        SelfCost { runner, open: None, runs: Vec::new() }
    }

    fn read(&self) -> (Option<u64>, Option<u64>, Option<u64>) {
        (host::process_cycles(std::process::id()), host::process_cycles(self.runner), host::tsc())
    }

    /// A run begins now.
    fn start(&mut self) {
        let (strobe, runner, tsc) = self.read();
        self.open = tsc.map(|t| (t, strobe, runner));
    }

    /// The run that began ended now.
    fn end(&mut self) {
        let (strobe, runner, tsc) = self.read();
        let Some((t0, s0, r0)) = self.open.take() else {
            self.runs.push((0, None, None));
            return;
        };
        let span = tsc.map_or(0, |t| t.saturating_sub(t0));
        let delta = |a: Option<u64>, b: Option<u64>| a.zip(b).filter(|(a, b)| b >= a).map(|(a, b)| b - a);
        self.runs.push((span, delta(s0, strobe), delta(r0, runner)));
    }

    /// strobe's and the runner's share of one core over the runs from `first` on.
    fn share(&self, first: usize) -> (Option<f64>, Option<f64>) {
        let runs = &self.runs[first.min(self.runs.len())..];
        let span: u64 = runs.iter().map(|r| r.0).sum();
        let part = |f: fn(&RunCost) -> Option<u64>| {
            let used: Option<u64> = runs.iter().map(f).sum();
            used.filter(|_| span > 0).map(|u| 100.0 * u as f64 / span as f64)
        };
        (part(|r| r.1), part(|r| r.2))
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
        if args.has("resume") {
            return Err(Error::usage("--stop or --resume, not both"));
        }
        return slot::stop(exp);
    }
    // `--resume <exp>`: the same command line, from the pairs and series it had finished.
    if let Some(handle) = args.get("resume") {
        let (dir, exp) = slot::experiment(handle)?;
        let saved = store::read_json(&dir.join("state.json"))
            .map_err(|_| Error::usage(format!("{exp} has no saved state to resume (state.json): only experiments started by this strobe or later can resume")))?;
        if let Some(b) = slot::list().into_iter().find(|b| b["exp"] == exp.as_str()) {
            return Err(Error::usage(format!("{exp} is still running (pid {}, {}): strobe bench --stop {exp} first", b["pid"], b["state"].as_str().unwrap_or(""))));
        }
        let mut resumed = Args::from_json(&saved["args"]).ok_or_else(|| Error::usage(format!("{exp}: state.json holds no command line")))?;
        resumed.set("id", &exp);
        resumed.remove("resume");
        RESUMED.with(|r| *r.borrow_mut() = Some(saved));
        let out = run(&resumed);
        // Taken when the bench starts; a run that failed before that must not hand it to the next.
        RESUMED.with(|r| r.borrow_mut().take());
        return out;
    }
    // `--blocks N`: one build's timed blocks, nothing compared, so nothing that compares or decides goes with it.
    if args.get("blocks").is_some() {
        if let Some(f) = ["require-verdict", "plan", "noworse", "pairs", "pixels", "pixels-only"].iter().find(|f| args.has(f)) {
            return Err(Error::usage(format!("--blocks N times one build alone and decides nothing; --{f} needs a comparison: drop one of them")));
        }
        if args.num("blocks")?.is_none_or(|b| !b.is_finite() || b < 1.0 || b.fract() != 0.0) {
            return Err(Error::usage(format!("--blocks: a whole number of blocks from 1, not {}", args.get("blocks").unwrap_or(""))));
        }
    }
    if args.has("plan") {
        return plan::run(args);
    }
    if let Some(range) = args.get("bisect") {
        if args.has("noworse") {
            return Err(Error::usage("--bisect looks for the commit that moved the primary; --noworse asks whether head is at most a margin worse: run them separately"));
        }
        return super::bisect::run(args, range);
    }
    let file = args.pos.first().ok_or_else(|| Error::usage("bench needs a bench file: strobe bench perf/x.bench.ts --base <variant> --head <variant>"))?;
    let file = std::fs::canonicalize(file).map_err(|_| Error::usage(format!("no such bench file: {file}")))?;
    let head_spec = args.get("head").unwrap_or(".").to_string();
    let aa = args.has("aa");
    // `--probes memory`: a leak run per variant (SPEC §8.6); `--base` is optional there.
    let leak_run = args.get("probes") == Some("memory");
    let check = args.has("check");
    let blocks = args.num("blocks")?.map(|b| b as usize);
    let base_spec = match (args.get("base"), aa) {
        _ if check && (aa || args.get("base").is_some()) => return Err(Error::usage("--check runs each case once on --head, with nothing to compare; drop --base and --aa")),
        _ if check && blocks.is_some() => return Err(Error::usage("--check runs one untimed block per case; --blocks N runs N timed ones: pick one")),
        _ if check => None,
        _ if blocks.is_some() && (aa || args.get("base").is_some() || leak_run) => {
            return Err(Error::usage("--blocks N measures --head alone, N timed blocks with no comparison; drop --base, --aa and --probes memory"));
        }
        _ if blocks.is_some() => None,
        (Some(b), false) => Some(b.to_string()),
        (None, true) => Some(head_spec.clone()),
        (Some(_), true) => return Err(Error::usage("--aa runs --head against itself; drop --base")),
        (None, false) if leak_run => None,
        (None, false) if args.get("head").is_some() => {
            return Err(Error::usage("--head alone compares nothing: add --base <variant>, --blocks N for N timed blocks of it alone, or --check to run each case once and see that it passes"));
        }
        (None, false) => return Err(Error::usage("say what to compare: --base <variant> (or --aa for the same build on both sides, or --check to validate the file)")),
    };
    let mode = match blocks {
        _ if check => Mode::Check,
        Some(n) => Mode::Blocks(n),
        None if args.has("pixels-only") => Mode::Pixels,
        None if leak_run => Mode::Leak,
        None => Mode::Compare,
    };
    // A headless browser presents nothing and paints off screen: a timed block there would put a
    // number on a pipeline no person runs. It validates a file, as --check does, and nothing more.
    if args.has("headless") && mode != Mode::Check {
        return Err(Error::usage("--headless runs pages in a headless browser for --check only: no timed block runs headless (its frames reach no display)"));
    }
    let exp = args.get("id").map(str::to_string).unwrap_or_else(|| store::new_id("exp"));
    let dir = store::experiment_dir(&exp)?;
    // Before it registers: a `--stop` that sees the registration must find its stop file kept.
    let _scope = super::experiment_scope(&dir);
    if let Some(d) = args.get("unlock-wait") {
        duration_ms(d)?;
    }
    let reserve = match args.get("reserve-cpus").unwrap_or("auto") {
        "auto" => "auto",
        "on" => "on",
        "off" => "off",
        other => return Err(Error::usage(format!("--reserve-cpus: auto, on or off, not {other}"))),
    };
    // Benches share the device by taking turns per block (the slot); this one registers what it does.
    let reg = slot::Registration::new(&exp, &file.display().to_string());
    for l in crate::target::isolate::reap_dead_owners() {
        progress(l);
    }
    let project = config::project_root(file.parent().unwrap_or(Path::new(".")));
    let config = config::load(&project)?;
    // The plan switches before the first block, not now: builds and waits run on the user's own.
    power_keep(args)?;
    let mut env = host::env();
    let topo = host::topology();
    let (pin, runner_pin, pin_label) = pinning(args.get("pin").unwrap_or("auto"), &topo)?;
    let _own = OwnAffinity::start(runner_pin);
    progress(format!("strobe bench {} · {exp} · {}", file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(), super::pr::version()));
    // The display state comes from a callback that first reports a moment after it is registered:
    // read now, the first block reads a state rather than none (a pair whose base saw none ran again).
    let _ = host::display_state();
    let busy = host::system_busy(1000);
    isolate::prune_gpu_caches();
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
        gpu_cache: BTreeMap::new(),
        mode,
        headless: args.has("headless"),
        overhead_warned: Default::default(),
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
        eta_after: BTreeMap::new(),
        shown_seen: BTreeMap::new(),
    };
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
    if let Some(why) = host::timer_coalescing() {
        // Every target inherits it: its timer-driven work and wakes are not a terminal-started app's.
        ctx.warnings.push(validity::warn(
            "TimerCoalescing",
            format!("strobe has {why}; the target inherits it, so its timers fire late and its wakes are fewer than an app started from a terminal; compare only with runs started the same way (run strobe from a terminal, or give that job ProcessType Interactive)"),
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
    preflight(&series, &ctx.describe, &variants, &config, ctx.headless)?;
    // The rerun ledger (SPEC §14.2): the other experiments on these two variants, said before anything
    // builds, so rerunning until one says "faster" shows.
    let compares = !check && !leak_run && variants.len() == 2;
    let ident = if compares { identity(&variants, &file, &ctx.harness, args, &series, &project, &config) } else { Value::Null };
    let ledger = store::ledger::for_experiment(&exp, &super::record::now_iso(), &ident);
    if let Some(t) = ledger["text"].as_str() {
        progress(format!("ledger  {t}"));
    }
    if ledger["changed"] == true {
        ctx.warnings.push(validity::warn(
            "Rerun",
            "earlier experiments on these same variants ran another bench file, harness, case, primary, margin or flags (the ledger names them): a verdict reached by changing the setup until one says what you hoped is not a verdict",
        ));
    }
    // The plan (SPEC §9.1): pairs and minutes per series from earlier runs here, and the ETA it keeps.
    let estimate = if compares && !args.has("pixels-only") { plan::estimate(&series, &ctx.describe, &file, args, &config, &project, &variants, Some(&exp)) } else { Value::Null };
    let planned: Vec<f64> = estimate["series"].as_array().into_iter().flatten().map(|s| s["ms"].as_f64().unwrap_or(0.0)).collect();
    for (i, s) in series.iter().enumerate() {
        ctx.eta_after.insert(s.label(), planned.iter().skip(i + 1).sum());
    }
    if estimate.is_object() {
        for line in plan::summary(&estimate) {
            progress(format!("plan    {line}"));
        }
        ctx.reg.set_eta(estimate["blocks_ms"].as_f64().unwrap_or(0.0));
    }
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
        let r = match mode {
            Mode::Check => check_series(&mut ctx, s, &mut variants, &config),
            Mode::Pixels => (|| -> Result<Value> {
                let target = ctx.describe["targets"][&s.target].clone();
                release_machine(&mut ctx);
                ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
                for v in variants.iter_mut() {
                    prepare_variant(v, &target, &config, &ctx.harness, &ctx.project)?;
                }
                let pixels = pixels_series(&mut ctx, s, &variants)?;
                Ok(json!({ "label": s.label(), "case": s.case, "kind": "pixels", "pixels": pixels, "decided": pixels.as_array().is_some_and(|a| a.iter().all(|p| p["state"] == "same")) }))
            })(),
            Mode::Leak => leak_series(&mut ctx, s, &mut variants, &config),
            Mode::Blocks(n) => blocks_series(&mut ctx, s, &mut variants, &config, n),
            Mode::Compare => run_series(&mut ctx, s, &mut variants, &config, started, max_time),
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
            halted = Some(Error::new(h["code"].as_i64().unwrap_or(1) as i32, h["message"].as_str().unwrap_or("")));
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
    // The experiment's GPU cache stores go with it; a stopped one (Ctrl-C, `--stop`, `--unlock-wait`)
    // keeps them, so `--resume` launches as warm as the pairs it already has.
    if halted.as_ref().is_none_or(|h| !matches!(h.code, 130 | 124)) && args.get("gpu-store") != Some("exe") {
        for v in variants.iter() {
            let _ = std::fs::remove_dir_all(gpu_store(&exp, v));
        }
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
            "pin": pin_label, "runs_per_block": ctx.runs, "aa": aa, "reserve": ctx.reserved, "reserve_mode": ctx.reserve, "gpu_cache": ctx.gpu_cache,
            "looks": if args.get("pairs").is_none_or(|p| p == "auto") { LOOKS.to_vec() } else { vec![args.num("pairs")?.unwrap_or(0.0) as usize] },
            "alpha": if args.get("pairs").is_none_or(|p| p == "auto") { POCOCK_ALPHA } else { 0.05 },
            "estimate": estimate,
        },
        "file_repo": super::pr::repo_relative(&file.display().to_string()), "identity": ident, "ledger": ledger,
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
        "id": exp, "created": result_json["created"], "file": result_json["file"], "identity": result_json["identity"],
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
    // Kept, so its views (`query <rec> threads`, …) can follow up on what the check showed. A Linux
    // series that counts instructions checks them in an untimed counter block (SPEC §7.5).
    let split = counting(s);
    let main_counters = split == Counting::Only || (split == Counting::Separate && counted_apart(&s.primary));
    let r = block_on_screen(ctx, s, &variants[0], 0, &Exec::untimed(true, None).counting(main_counters)).and_then(|mut b| {
        if split == Counting::Separate {
            let other = block_on_screen(ctx, s, &variants[0], 0, &Exec::untimed(false, None).counting(!main_counters))?;
            take_metrics(&mut b, &other, |m| counted_apart(m) != main_counters);
        }
        Ok(b)
    });
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    // What the bench file's hooks printed during this case (console.log/error go to runner.log).
    let log: Vec<String> = std::fs::read(&ctx.runner.log)
        .ok()
        .and_then(|b| b.get(log_from as usize..).map(|b| String::from_utf8_lossy(b).lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).map(str::to_string).collect()))
        .unwrap_or_default();
    let overhead = r.as_ref().ok().and_then(|b| b.overhead.clone());
    let (passed, error, hint, values, refused, locked, recording, screen, shown) = match r {
        Ok(b) => {
            let shown = shown::summarize(std::slice::from_ref(&b)).0;
            (true, Value::Null, Value::Null, json!(b.values), refusal_map(&b.refused), b.locked, json!(b.recording), b.screen.unwrap_or(Value::Null), shown)
        }
        Err(e) => (false, json!(e.message), json!(e.hint), Value::Null, Value::Null, false, Value::Null, Value::Null, Value::Null),
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
        let runs = ctx.args.num("runs")?.unwrap_or(5.0).max(1.0) as usize;
        let attributed = if s.launch.is_object() {
            in_slot(ctx, &format!("{} {} attribution", s.label(), v.role), |ctx| launch::attribution(ctx, s, v))
        } else {
            block_on_screen(ctx, s, v, usize::MAX, &Exec::timed(Set::Attribution, runs, true))
        };
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
        "passed": passed, "decided": passed, "error": error, "hint": hint, "values": values, "refused": refused, "session_locked": locked, "headless": ctx.headless, "elapsed_ms": elapsed_ms, "attribution": attribution, "target_kind": target["kind"],
        "recording": recording, "screen": screen, "shown": shown, "overhead": overhead,
        "log": log, "log_path": ctx.runner.log,
    }))
}

fn check_output(exp: &str, dir: &Path, file: &Path, series: Vec<Value>, warnings: Vec<Warning>) -> Result<Output> {
    let failed = series.iter().filter(|s| s["passed"] != true).count();
    let mut text = String::new();
    for s in &series {
        // Why each refused metric was refused, said once per reason under the case: a missing
        // permission refuses every frame metric with the same paragraph.
        let mut reasons: Vec<(String, Vec<String>)> = Vec::new();
        for (name, why) in s["refused"].as_object().into_iter().flatten() {
            let why = why.as_str().unwrap_or("");
            let why = why.strip_prefix(&format!("{name}: ")).unwrap_or(why).to_string();
            let why = regex_lite::Regex::new(r"^(\w+): [a-z]+(\.[a-z0-9]+)+: ").ok().map(|re| re.replace(&why, "$1: ").to_string()).unwrap_or(why);
            match reasons.iter_mut().find(|r| r.0 == why) {
                Some(r) => r.1.push(name.clone()),
                None => reasons.push((why, vec![name.clone()])),
            }
        }
        // The declared metrics' values: one run's worth, enough to see that a counter counts.
        let shown = |name: &str| {
            s["values"][name].as_f64().map(|v| format!("{name} {}", fmt::value(v, model::metric(name).map_or("", |m| m.unit)))).or_else(|| s["refused"][name].is_string().then(|| format!("{name} refused")))
        };
        // A launch's milestones too, declared or not: when each line or pixel change came.
        let declared: Vec<&str> = s["declared"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let milestones = s["values"].as_object().into_iter().flatten().map(|(k, _)| k.as_str()).filter(|k| k.starts_with("launch.") && !declared.contains(k));
        // Every declared metric, with its value, its refusal, or that it got none: a metric left out
        // read as measured and fine.
        let value = declared
            .iter()
            .map(|n| shown(n).unwrap_or_else(|| format!("{n} no value (nothing in this block measured it)")))
            .chain(milestones.filter_map(shown))
            .collect::<Vec<_>>()
            .join(" · ");
        let secs = s["elapsed_ms"].as_f64().unwrap_or(0.0) / 1e3;
        if s["passed"] == true {
            text += &format!(
                "pass  {}  {value}  {secs:.1} s{}{}\n",
                s["label"].as_str().unwrap_or(""),
                if s["headless"] == true { " · headless" } else { "" },
                if s["session_locked"] == true { " · session locked" } else { "" }
            );
            if let Some(rec) = s["recording"].as_str() {
                text += &format!("      rec  {rec} · strobe query {rec} {}\n", if s["target_kind"] == "native" { "threads" } else { "summary" });
            }
            if let Some(line) = screen_line(&s["screen"]) {
                text += &format!("      screen  {line}\n");
            }
            if let Some(o) = serde_json::from_value::<Overhead>(s["overhead"].clone()).ok().map(|o| o.text()).filter(|t| !t.is_empty()) {
                text += &format!("      self  {o}\n");
            }
            let missing: Vec<&str> = s["shown"]["head"]["missing"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|m| !m.contains("before the runs")).collect();
            match shown_line(s) {
                Some(line) => text += &format!("      shown  {line}{}\n", if missing.is_empty() { String::new() } else { format!(" (not read: {})", missing.join("; ")) }),
                None if !missing.is_empty() => text += &format!("      shown  nothing read: {}\n", missing.join("; ")),
                None => {}
            }
            if let Some(p) = s["shown"]["head"]["thumb_path"].as_str() {
                text += &format!("      thumb  {p}\n");
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
        for (why, names) in &reasons {
            text += &format!("      refused  {}: {why}\n", names.join(", "));
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
        let b = block_on_screen(ctx, s, v, 0, &Exec::timed(Set::Memory, n, true))?;
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
        "primary": { "metric": metric, "verdict": head["verdict"], "verdict_kind": head["verdict_kind"], "decided": head["decided"] },
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
    // A metric with a no-worse margin is declared: it is tested.
    for m in s.noworse.keys() {
        if *m != s.primary && !names.contains(m) {
            names.push(m.clone());
        }
    }
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

/// `ETA ~12.5 min`: the pairs left to `target` at this series' mean pair time so far, plus the planned
/// time of the series after it; kept in the bench's registration for other benches' plans.
fn eta(ctx: &mut Ctx, s: &Series, blocks: &[Block], pairs: &[(usize, usize)], target: usize) -> String {
    let block = |b: &Block| b.steps.iter().map(|x| x.1).sum::<f64>();
    let pair_ms = pairs.iter().map(|&(a, b)| block(&blocks[a]) + block(&blocks[b])).sum::<f64>() / pairs.len().max(1) as f64;
    let ms = target.saturating_sub(pairs.len()) as f64 * pair_ms + ctx.eta_after.get(&s.label()).copied().unwrap_or(0.0);
    ctx.reg.set_eta(ms);
    format!("ETA ~{}", fmt::ms_duration(ms as u64))
}

/// Saves a series' finished blocks and pairs (or its result, once done) in `state.json`, so a bench
/// that stops or fails keeps them and `--resume` goes on from there.
fn checkpoint(ctx: &mut Ctx, label: &str, entry: Value) {
    ctx.saved["series"][label] = entry;
    if let Ok(dir) = store::experiment_dir(&ctx.exp) {
        let _ = store::write_json(&dir.join("state.json"), &ctx.saved);
    }
}

/// Warm GPU caches (`gpuCache`): discarded blocks per variant fill its store first, so no measured
/// block pays a driver compiling its shaders cold. A driver may compile in the background for
/// longer than a block (NVIDIA's Vulkan cache took two 40 s launches to fill), so they repeat
/// while the store still grew by 5% (at most 3). A launch case's discarded first launch warms too.
/// Returns the discarded blocks and their wall time; a stop sets `halted`.
fn warm_gpu(ctx: &mut Ctx, s: &Series, variants: &[Variant], target: &Value, halted: &mut Option<Error>) -> Result<(usize, f64)> {
    let (mut blocks, mut ms) = (0usize, 0.0f64);
    if !gpu_warming(ctx, s, target)? {
        return Ok((blocks, ms));
    }
    for v in variants.iter() {
        if !ctx.launched.insert(format!("{}\n{}\ngpu", s.label(), v.role)) {
            continue;
        }
        let store = gpu_store(&ctx.exp, v);
        for round in 1..=3 {
            let before = isolate::gpu_cache_bytes(&store);
            progress(format!("  {} warm-up block {round} (discarded): fills its GPU shader caches", v.role));
            let warm_started = Instant::now();
            match block_on_screen(ctx, s, v, 0, &Exec::timed(Set::Lean, ctx.runs, false)) {
                Ok(b) => {
                    blocks += 1;
                    ms += warm_started.elapsed().as_secs_f64() * 1e3;
                    if let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
                Err(e) if e.code == 130 || stop_requested() => {
                    *halted = Some(stop_error());
                    return Ok((blocks, ms));
                }
                Err(e) => return Err(e),
            }
            let after = isolate::gpu_cache_bytes(&store);
            let grew = after.saturating_sub(before);
            if grew < (after / 20).max(64 << 10) {
                break;
            }
            if round == 3 {
                ctx.warnings.push(validity::warn(
                    "GpuCacheGrowing",
                    format!("{}: {}'s GPU shader caches still grew by {} in its third warm-up block: its measured blocks may still compile shaders", s.label(), v.role, fmt::bytes(grew as f64)),
                ));
            }
        }
    }
    Ok((blocks, ms))
}

/// The set a timed block traces: only what the metrics read, or every timeline category (`--trace full`).
fn timed_set(ctx: &Ctx) -> Set {
    if ctx.args.get("trace") == Some("full") { Set::Timing } else { Set::Lean }
}

/// Whether the blocks of `blocks` ran on a locked screen in all, none or some of them.
fn session_of<'a>(blocks: impl Iterator<Item = &'a Block>) -> (&'static str, usize, usize) {
    let (mut locked, mut n) = (0, 0);
    for b in blocks {
        n += 1;
        locked += usize::from(b.locked);
    }
    let session = match locked {
        0 => "unlocked",
        l if l == n => "locked",
        _ => "mixed",
    };
    (session, locked, n)
}

/// `--blocks N` (what `track` records per commit): N timed blocks of head alone, each warmed until
/// steady like a bench's, and their values. Nothing is compared, so nothing is decided; a reader
/// takes the blocks' values (and their spread) as the commit's point.
fn blocks_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value, n: usize) -> Result<Value> {
    let def = model::metric(&s.primary)
        .ok_or_else(|| Error::usage(format!("{}: unknown primary metric {} (known: {})", s.case, s.primary, model::METRICS.iter().map(|m| m.name).collect::<Vec<_>>().join(", "))))?;
    let target = ctx.describe["targets"][&s.target].clone();
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    for v in variants.iter_mut() {
        prepare_variant(v, &target, config, &ctx.harness, &ctx.project)?;
    }
    progress(format!("\n{}  primary {} · {n} blocks of head alone", s.label(), s.primary));
    let series_started = Instant::now();
    let waited_before = ctx.waited_ms;
    let mut blocks: Vec<Block> = serde_json::from_value(ctx.saved["series"][s.label()]["blocks"].clone()).unwrap_or_default();
    if !blocks.is_empty() {
        progress(format!("  resumed with the {} blocks it had finished", blocks.len()));
    }
    let mut halted: Option<Error> = None;
    let (warmup_blocks, warmup_ms) = if blocks.is_empty() { warm_gpu(ctx, s, variants, &target, &mut halted)? } else { (0, 0.0) };
    let unit = model::metric(&s.primary).map_or("", |d| d.unit);
    // Counter blocks (SPEC §7.5): N of them for a series of counters (or one whose primary is
    // counted apart), and for a series of both, N of the other kind whose metrics join block i.
    let split = counting(s);
    let main_counters = split == Counting::Only || (split == Counting::Separate && counted_apart(&s.primary));
    if split == Counting::Separate {
        progress(format!("  counters: {} blocks of each kind (cpu.instructions in counter blocks, times in timing blocks)", n));
    }
    // The blocks of a point share one lock and display state, as a pair's do: a block in another state
    // starts the point again from it (a locked session's display goes to sleep once, early).
    let mut restarts = 0;
    while halted.is_none() && blocks.len() < n {
        if stop_requested() {
            halted = Some(stop_error());
            break;
        }
        let i = blocks.len();
        let keep = ctx.keep_all || i == 0;
        let both = |ctx: &mut Ctx| -> Result<Block> {
            let mut b = block_on_screen(ctx, s, &variants[0], i, &Exec::timed(timed_set(ctx), ctx.runs, keep).counting(main_counters))?;
            if split == Counting::Separate {
                let other = block_on_screen(ctx, s, &variants[0], i, &Exec::timed(timed_set(ctx), ctx.runs, false).counting(!main_counters))?;
                take_metrics(&mut b, &other, |m| counted_apart(m) != main_counters);
            }
            Ok(b)
        };
        match both(ctx) {
            Ok(b) => {
                progress(format!("  block {:>2}  {:>10}  {}{}", i + 1, fmt::value(val(&b, &s.primary), unit), steps_text(&b.steps), if b.locked { " · session locked" } else { "" }));
                if blocks.first().is_some_and(|f| (f.locked, f.display) != (b.locked, b.display)) && restarts < 3 {
                    restarts += 1;
                    progress("  the session locked or unlocked, or the display turned off or on, since the first block: the blocks start again in this state");
                    for old in blocks.drain(..) {
                        if let Some(dir) = old.recording.as_deref().and_then(|id| store::resolve(id).ok()) {
                            let _ = std::fs::remove_dir_all(dir);
                        }
                    }
                }
                blocks.push(b);
                checkpoint(ctx, &s.label(), json!({ "blocks": blocks }));
            }
            Err(e) if e.code == 130 || stop_requested() || !blocks.is_empty() => halted = Some(if e.code == 130 || stop_requested() { stop_error() } else { e }),
            Err(e) => return Err(e),
        }
    }
    if let Some(h) = halted.as_ref().filter(|_| blocks.is_empty()) {
        return Ok(json!({ "label": s.label(), "case": s.case, "not_run": true, "halt": { "code": h.code, "message": h.message } }));
    }
    // The primary, refused as a paired comparison would refuse it (rule 1): a block refused it, a
    // percentile's tail is too short, or no measured run entered its phase.
    let mut primary = json!({ "metric": s.primary });
    // (A block with no value of a declared phase or span refused it in `phase_state`, and a block that
    // refused its primary failed: a refusal here is one from a block's lock state.)
    let refusal = blocks.iter().find_map(|b| b.refused.get(&s.primary).cloned()).or_else(|| tail_refusal(def, &blocks));
    if let Some(r) = refusal {
        primary["verdict"] = json!(format!("refused ({}): {}", r.validator, r.summary));
        primary["verdict_kind"] = json!(VerdictKind::Refused);
        primary["refusal"] = json!(r);
    }
    let mut warnings: Vec<Warning> = Vec::new();
    // The analysers of one build's blocks: HighVariance, MultimodalDistribution, NotSteady,
    // Background, StateDrift (Drift is a trend of pair differences: none here).
    analyse(s, def, &blocks, &[], false, &mut warnings);
    let (session, locked, _) = session_of(blocks.iter());
    if session == "mixed" {
        warnings.push(validity::warn("SessionLocked", format!("{locked} of {} blocks ran on a locked screen and the rest unlocked: their values do not compare", blocks.len())));
    }
    if blocks.iter().any(|b| b.display == Some(0)) {
        warnings.push(validity::info("DisplayOff", "the blocks ran with the display off (a locked session's display sleeps): compare them only with runs whose display was off too"));
    }
    let (shown, screen_warnings) = shown::summarize(&blocks);
    warnings.extend(screen_warnings);
    let timing_recordings: Vec<Value> = blocks.iter().filter_map(|b| b.recording.clone().map(|r| json!({ "variant": b.variant, "pair": b.pair, "recording": r }))).collect();
    Ok(json!({
        "label": s.label(), "case": s.case, "target": s.target, "target_kind": target["kind"], "params": s.params, "kind": "blocks",
        "primary": primary, "decided": false, "session": session,
        "pairs": 0, "launches": blocks.len(), "stopped": if halted.is_some() { "stopped" } else { "blocks done" },
        "warmup_blocks": warmup_blocks, "warmup_ms": warmup_ms,
        "steps": mean_steps(&blocks), "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3 - (ctx.waited_ms - waited_before), "waited_ms": ctx.waited_ms - waited_before,
        "overhead": mean_overhead(&blocks),
        "blocks": blocks, "warnings": warnings, "attribution": {}, "timing_recordings": timing_recordings, "shown": shown,
        "halt": halted.map(|h| json!({ "code": h.code, "message": h.message })),
    }))
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
    // Non-inferiority (SPEC §9.3): the same interval at the same looks, judged against the margin.
    let margin = s.noworse.get(&s.primary).copied();
    let compare_primary = |pp: &[(f64, f64)]| match margin {
        Some(m) => stats::compare_noworse(&s.primary, def, pp, m, confidence),
        None => stats::compare(&s.primary, def, pp, bound, confidence),
    };
    progress(format!("\n{}  primary {}{}", s.label(), s.primary, margin.map(|m| format!(" · no worse than {}%?", (m * 1000.0).round() / 10.0)).unwrap_or_default()));
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
    // Discarded warm-up blocks and their wall time, for `plan` (SPEC §9.1).
    let (warmup_blocks, warmup_ms) = if pairs.is_empty() { warm_gpu(ctx, s, variants, &target, &mut halted)? } else { (0, 0.0) };
    // Linux's perf events never run in a timing block (SPEC §7.5): the paired loop runs on counter
    // blocks for a series of counters, or one whose primary is counted apart.
    let split = counting(s);
    let main_counters = split == Counting::Only || (split == Counting::Separate && counted_apart(&s.primary));
    let from = if halted.is_some() { max_pairs } else { pairs.len() };
    'pairs: for pair in from..max_pairs {
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
                let b = match block_on_screen(ctx, s, &variants[v], pair, &Exec::timed(timed_set(ctx), ctx.runs, keep).counting(main_counters)) {
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
            let need = holm_pairs(tested_names(s, script, &blocks, &pairs, bound).len());
            let eta = eta(ctx, s, &blocks, &pairs, need);
            progress(format!("  pair {n:>2}  for the secondaries (the primary was decided: {}) · {eta}", c.verdict));
            if n >= need {
                stopped = "primary decided".into();
                break;
            }
            continue;
        }
        let look = looks.contains(&n);
        let cmp = compare_primary(&primary_pairs);
        let next = looks.iter().find(|&&l| l > n).copied();
        let decided = look && cmp.decided;
        // Give up only at a look from 12 pairs, on a robust projection: early intervals are [min, max]
        // of few Walsh averages, so one odd pair would stop a series that more pairs decide.
        let d: Vec<f64> = primary_pairs.iter().map(|&(a, b)| if cmp.relative { (b / a).ln() } else { b - a }).collect();
        let mut cmp = cmp;
        if !cmp.decided {
            cmp.needed_pairs = Some(match cmp.noworse {
                Some(m) => stats::pairs_needed_noworse(&d, stats::margin_shift(def, cmp.relative, m), confidence),
                None => stats::pairs_needed(&d, if cmp.relative { (1.0 + bound).ln() } else { cmp.bound }, confidence),
            });
        }
        // Where this series likely ends: at the look its projection reaches, or where it gives up.
        let target = if decided || ctx.args.get("pairs").is_some() {
            if decided { n } else { max_pairs }
        } else {
            let need = cmp.needed_pairs.unwrap_or(max_pairs).max(n + 1);
            let at = if need > max_pairs { need.min(FUTILITY_FROM.max(n + 1)) } else { need };
            looks.iter().copied().find(|&l| l >= at).unwrap_or(max_pairs)
        };
        let eta = eta(ctx, s, &blocks, &pairs, target);
        progress(format!(
            "  pair {:>2}  base {:>10}  head {:>10}  {}  [{} … {}]  {} · {eta}",
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
        last = Some(compare_primary(&pp));
    }
    let primary = last.ok_or_else(|| Error::msg("no pairs completed"))?;
    let primary_decided = stopped == "primary decided" && pairs.len() >= min_pairs;
    let mut primary = primary;
    if !primary_decided {
        let (kind, verdict) = match stopped.as_str() {
            "saturated" => (
                VerdictKind::Saturated,
                format!(
                    "saturated: base {} and head {} sit near the ceiling, where the builds cannot differ; choose another primary (main.work, cpu.work)",
                    fmt::value(primary.base, def.unit),
                    fmt::value(primary.head, def.unit)
                ),
            ),
            "stopped" => (VerdictKind::Stopped, format!("stopped after {} pair{}: no verdict (resume to finish)", pairs.len(), if pairs.len() == 1 { "" } else { "s" })),
            s if s.starts_with("failed") => (VerdictKind::Unclear, format!("unclear: {} pair{}, then the bench failed", pairs.len(), if pairs.len() == 1 { "" } else { "s" })),
            "more pairs cannot decide it" | "max pairs" | "max time" => match primary.needed_pairs {
                Some(n) if n != usize::MAX && n > pairs.len() => (VerdictKind::Underpowered, format!("underpowered: ~{n} pairs needed")),
                _ if stopped == "max pairs" && ctx.args.get("pairs").is_none() => (
                    VerdictKind::Unclear,
                    format!("unclear after {} pairs, the last planned look: the difference sits on the {}", pairs.len(), if margin.is_some() { "margin" } else { "equivalence bound" }),
                ),
                _ => (VerdictKind::Unclear, "unclear".into()),
            },
            _ => (VerdictKind::Unclear, "unclear".into()),
        };
        primary.set_verdict(kind, verdict);
    }
    // Tail percentiles need enough frames in every block.
    let primary = tail_check(def, &blocks, primary);
    // A series of both counters and times (SPEC §7.5): K pairs of the other kind of block, the
    // fewest that let its secondaries clear Holm (at least the first look), never more than the
    // series ran: counter blocks after timing pairs, or timing blocks after counter pairs.
    let mut pass: Option<Pass> = None;
    let mut counters_note: Option<String> = None;
    if split == Counting::Separate && halted.is_none() && !pairs.is_empty() {
        let other = secondary_names(s, script, false).into_iter().filter(|m| counted_apart(m) != main_counters).count();
        let k = holm_pairs(other).max(min_pairs).min(pairs.len());
        counters_note = Some(format!("separate blocks: cpu.instructions in counter blocks (perf events), times in timing blocks; {} → {k} pairs", if main_counters { "times" } else { "cpu.instructions" }));
        progress(format!("  {} blocks: {k} pairs (Linux perf events never run in a timing block)", if main_counters { "timing" } else { "counter" }));
        match pass_pairs(ctx, s, variants, !main_counters, k) {
            Ok(p) => pass = Some(p),
            Err(e) if e.code == 130 || stop_requested() => halted = Some(stop_error()),
            Err(e) => counters_note = Some(format!("the {} blocks failed: {}", if main_counters { "timing" } else { "counter" }, e.message)),
        }
    }
    // Which blocks measure a metric: the pass's for what the paired loop's kind does not.
    let source = |name: &str| -> (&[Block], &[(usize, usize)]) {
        match &pass {
            Some((pb, pp)) if counted_apart(name) != main_counters => (pb, pp),
            _ => (&blocks, &pairs),
        }
    };
    // Secondaries: gated on the primary, Holm-corrected.
    let (tb, tp) = source("run.duration");
    let rate_context = !same(tb, tp, "run.duration", bound);
    let names = secondary_names(s, script, rate_context);
    let mut secondaries: Vec<Comparison> = Vec::new();
    for name in &names {
        let Some(d) = model::metric(name) else { continue };
        let (blocks, pairs) = source(name);
        if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
            // A metric the series declared that a block refused shows as refused, not as missing.
            if let Some(why) = blocks.iter().find_map(|b| b.refused.get(name)) {
                secondaries.push(refused(name, d, why));
            }
            continue;
        }
        let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], name), val(&blocks[b], name))).collect();
        let c = match s.noworse.get(name) {
            Some(&m) => stats::compare_noworse(name, d, &pp, m, 0.95),
            None => stats::compare(name, d, &pp, bound, 0.95),
        };
        secondaries.push(tail_check(d, blocks, c));
    }
    // An exact comparison (a stable counter) is a fact, not a test: it skips Holm and the primary's gate.
    let tested: Vec<usize> = (0..secondaries.len()).filter(|&i| !secondaries[i].exact).collect();
    let ps: Vec<f64> = tested.iter().map(|&i| secondaries[i].p).collect();
    let rejected = stats::holm(&ps, 0.05);
    for (&i, rej) in tested.iter().zip(rejected) {
        let c = &mut secondaries[i];
        if c.kind == VerdictKind::Refused {
            continue;
        }
        // A non-inferiority secondary's test is against its margin: rejected, its interval clears it.
        let directional = if c.noworse.is_some() { c.decided } else { c.lo > 0.0 || c.hi < 0.0 };
        let keep = primary_decided && ((rej && directional) || c.kind == VerdictKind::Same);
        if !keep {
            c.set_verdict(VerdictKind::Untested, "—");
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
        if c.kind != VerdictKind::Refused {
            c.set_verdict(VerdictKind::Descriptive, "·");
        }
        c.decided = false;
        descriptive.push(c);
    }
    let mut warnings: Vec<Warning> = Vec::new();
    if let Some(note) = counters_note {
        warnings.push(validity::info("Counters", note));
    }
    analyse(s, def, &blocks, &pairs, primary.relative, &mut warnings);
    let (shown, screen_warnings) = shown::summarize(&blocks);
    warnings.extend(screen_warnings);
    // A number from a locked screen is not comparable with one from an unlocked screen: pairs that
    // ran in both states decide nothing.
    let (session, locked_pairs, _) = session_of(pairs.iter().map(|&(a, _)| &blocks[a]));
    let mut primary = primary;
    let mut primary_decided = primary_decided;
    // A block that refused the primary (its lock state, after the block): refused, as for `--blocks`.
    if let Some(why) = blocks.iter().find_map(|b| b.refused.get(&s.primary)) {
        primary.refuse(why.clone());
        primary_decided = false;
    }
    if session == "mixed" {
        let why = Refusal::new(ValidatorId::SessionLocked, format!("{locked_pairs} of {} pairs ran on a locked screen, the rest unlocked", pairs.len()));
        primary.refuse(why.clone());
        primary_decided = false;
        for c in secondaries.iter_mut().chain(descriptive.iter_mut()) {
            c.refuse(why.clone());
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
            let attributed = if s.launch.is_object() {
                in_slot(ctx, &format!("{} {} attribution", s.label(), v.role), |ctx| launch::attribution(ctx, s, v))
            } else {
                block_on_screen(ctx, s, v, usize::MAX, &Exec::timed(Set::Attribution, ctx.runs, true))
            };
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
        "pairs": pairs.len(), "launches": blocks.len(), "stopped": stopped, "bound": bound, "noworse": s.noworse,
        "warmup_blocks": warmup_blocks, "warmup_ms": warmup_ms,
        "steps": mean_steps(&blocks), "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3 - (ctx.waited_ms - waited_before), "waited_ms": ctx.waited_ms - waited_before,
        "overhead": mean_overhead(&blocks),
        "blocks": blocks, "warnings": warnings, "attribution": recordings, "timing_recordings": timing_recordings, "shown": shown,
        "pass": pass.map(|(b, p)| json!({ "counters": !main_counters, "blocks": b, "pairs": p })),
        "halt": halted.map(|h| json!({ "code": h.code, "message": h.message })),
    }))
}

/// Blocks and their pairs (indexes into the blocks: base, head).
type Pass = (Vec<Block>, Vec<(usize, usize)>);

/// `k` pairs of the other kind of block for a series of both counters and times (SPEC §7.5):
/// counter blocks (`counters`) or timing blocks, base and head alternating as the series' own pairs,
/// both blocks of a pair in one lock and display state.
fn pass_pairs(ctx: &mut Ctx, s: &Series, variants: &[Variant], counters: bool, k: usize) -> Result<Pass> {
    let (mut blocks, mut pairs): (Vec<Block>, Vec<(usize, usize)>) = (Vec::new(), Vec::new());
    for pair in 0..k {
        if stop_requested() {
            return Err(stop_error());
        }
        let order: [usize; 2] = if pair % 2 == 0 { [0, 1] } else { [1, 0] };
        let mut redone = 0;
        loop {
            let start = blocks.len();
            let mut idx = [0usize; 2];
            for &v in &order {
                let x = Exec::timed(timed_set(ctx), ctx.runs, ctx.keep_all).counting(counters);
                let b = block_on_screen(ctx, s, &variants[v], pair, &x)?;
                idx[v] = blocks.len();
                blocks.push(b);
            }
            if blocks[start].locked == blocks[start + 1].locked && blocks[start].display == blocks[start + 1].display {
                pairs.push((idx[0], idx[1]));
                break;
            }
            for b in blocks.drain(start..) {
                if let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            redone += 1;
            if redone >= 5 {
                return Err(Error::refused("SessionLocked", "the session kept locking and unlocking between a pair's blocks", "bench while the session stays locked or stays unlocked"));
            }
        }
        progress(format!("  {} pair {:>2}", if counters { "counter" } else { "timing" }, pair + 1));
    }
    Ok((blocks, pairs))
}

/// `--pixels`: untimed blocks run the case once each with `page.snapshot(name)` capturing the page: base twice,
/// then head. The same build does not draw bit for bit the same twice (a GPU's blending and rasterization vary
/// between launches: an A/A once differed in 4 to 109 pixels by 1 or 2 of 255), so base's two runs measure that
/// noise per snapshot, and head differs only where it moves a pixel further than base moved against itself.
fn pixels_series(ctx: &mut Ctx, s: &Series, variants: &[Variant]) -> Result<Value> {
    let slug: String = s.label().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let root = store::experiment_dir(&ctx.exp)?.join("pixels").join(slug);
    progress("  pixels blocks (one run each, untimed: base twice for the noise between launches, then head)");
    for (v, folder) in [(&variants[0], "base"), (&variants[0], "base-again"), (&variants[1], "head")] {
        block_on_screen(ctx, s, v, usize::MAX, &Exec::untimed(false, Some(root.join(folder))))?;
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
                    store::write_atomic(&diff_path, &png::write(img)?)?;
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
    if let Some(r) = tail_refusal(def, blocks) {
        c.refuse(r);
    }
    c
}

/// `TailTooShort` when a block had fewer of a percentile's events than it needs.
fn tail_refusal(def: &MetricDef, blocks: &[Block]) -> Option<Refusal> {
    let (count, what) = match def.name.split('.').next() {
        Some("interaction") => ("interaction.n", "inputs"),
        Some("scroll") => ("scroll.n", "scroll updates"),
        Some("canvas") => ("canvas.n", "canvas frame gaps"),
        _ => ("frames.n", "frames"),
    };
    let least = blocks.iter().map(|b| b.values.get(count).copied().unwrap_or(0.0) as usize).min().unwrap_or(0);
    (def.min_n > 1 && least < def.min_n).then(|| Refusal::new(ValidatorId::TailTooShort, format!("{least} {what} < {}", def.min_n)))
}

/// What an attribution block records for this target.
fn why_text(target: &Value) -> &'static str {
    if is_native(target) {
        if crate::probe::present::access().helper || !cfg!(windows) {
            "OS samples and native stacks"
        } else {
            "OS samples and native stacks (sampled without the deep helper: user frames, a short stall per sample)"
        }
    } else {
        "CPU and allocation stacks, forced layouts, GC, a heap snapshot"
    }
}

/// The view that explains an attribution recording of a target of this kind.
fn why_view(kind: &Value) -> &'static str {
    if kind == "native" { "hotspots --by package · threads · frames" } else { "hotspots --by package" }
}

/// A metric a block refused.
fn refused(name: &str, def: &MetricDef, why: &Refusal) -> Comparison {
    let mut c = stats::compare(name, def, &[], 0.0, 0.95);
    c.refuse(why.clone());
    c
}

/// A block's refused metrics as result.json keeps them: `"<metric>": "<validator>: <why>"`.
fn refusal_map(refused: &BTreeMap<String, Refusal>) -> Value {
    refused.iter().map(|(k, r)| (k.clone(), Value::String(r.message()))).collect::<serde_json::Map<_, _>>().into()
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
    stats::compare(name, d, &pp, bound, 0.95).kind == VerdictKind::Same
}

fn change(c: &Comparison) -> String {
    if c.relative { fmt::pct(c.estimate) } else { fmt::signed(c.estimate, "count") }
}

fn bound_text(x: f64, c: &Comparison, def: &MetricDef) -> String {
    if c.relative { fmt::pct(x) } else { fmt::signed(x, def.unit) }
}

fn analyse(s: &Series, def: &MetricDef, blocks: &[Block], pairs: &[(usize, usize)], relative: bool, w: &mut Vec<Warning>) {
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
    // On the comparison's own scale: log ratios, or differences for counts and anything that read 0
    // (a ratio of 0 dropped those pairs, and a count's every pair).
    let d: Vec<f64> = pairs
        .iter()
        .map(|&(a, b)| (val(&blocks[a], &s.primary), val(&blocks[b], &s.primary)))
        .map(|(a, b)| if relative { (b / a).ln() } else { b - a })
        .filter(|x| x.is_finite())
        .collect();
    if d.len() >= 8 {
        let rho = validity::spearman(&d);
        // A trend smaller than 1% end to end (rounding of a near-constant difference) changes no verdict.
        let base = metrics::median(&pairs.iter().map(|&(a, _)| val(&blocks[a], &s.primary)).filter(|x| x.is_finite()).collect::<Vec<_>>()).abs();
        let range = d.iter().copied().fold(f64::NEG_INFINITY, f64::max) - d.iter().copied().fold(f64::INFINITY, f64::min);
        // A difference moves at least past a floor, as `stats::compare` floors its bound: 0.5 of the
        // unit, a whole one for counts (a count that went 0 → 1 once is no trend).
        let trend = if relative { range >= 0.01 } else { range > (0.01 * base).max(if def.unit == "count" { 1.0 } else { 0.5 }) };
        if rho.abs() > 0.6 && trend {
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
    let undecided_or_leak = js_v["verdict_kind"] == json!(VerdictKind::Worse) || js_v["verdict_kind"] == json!(VerdictKind::Unclear);
    if growing && undecided_or_leak && app_per_run <= bound {
        js_v["verdict"] = json!(format!(
            "no app leak: the heap grows {}/run in V8's compiled code and internals (the JIT warming up); app objects from the first {n} runs hold {}/run",
            bytes(js_v["slope"].as_f64().unwrap_or(0.0)),
            bytes(app_per_run)
        ));
        js_v["verdict_kind"] = json!(VerdictKind::Same);
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
    let Some((slope, lo, hi)) = stats::theil_sen(ys, 0.95) else { return json!({ "verdict": "unclear: too few censuses", "verdict_kind": VerdictKind::Unclear, "decided": false }) };
    let mid = n.min(ys.len().saturating_sub(1));
    let half = |h: &[f64]| stats::theil_sen(h, 0.95).map_or(f64::NAN, |x| x.0);
    let (first, second) = (half(&ys[..=mid]), half(&ys[mid..]));
    // As kinds: a leak is worse; no growth, or growth that levels off, is the same (not a leak).
    let (kind, verdict) = if lo > bound && second > bound {
        (VerdictKind::Worse, format!("leaks {}/run", show(slope)))
    } else if lo > bound {
        (VerdictKind::Same, format!("grows, then levels off ({}/run in the first {n} runs, {}/run in the last {n}): a cache filling up, not a leak so far", show(first), show(second)))
    } else if lo >= -bound && hi <= bound {
        (VerdictKind::Same, format!("no growth (within ±{}/run)", show(bound).trim_start_matches('+')))
    } else {
        (VerdictKind::Unclear, format!("unclear: {} … {}/run; more runs (--repeat) narrow it", show(lo), show(hi)))
    };
    json!({ "slope": slope, "lo": lo, "hi": hi, "bound": bound, "first_half": first, "second_half": second, "verdict": verdict, "verdict_kind": kind, "decided": kind.decided() })
}

/// Other work on the target's CPUs over a span (a page's speed check), as `validity`'s native check
/// measures it: their busy time from idle cycles, less the target browser's own CPU, and the busiest
/// processes outside strobe's tree (the browser is in it).
struct Foreign {
    idle: Vec<u64>,
    tsc: Option<u64>,
    browser: u64,
    procs: (u64, Vec<host::ProcInfo>),
}

impl Foreign {
    fn start(browser: &chromium::Browser) -> Foreign {
        let mut idle = Vec::new();
        host::idle_cycles(&mut idle);
        Foreign { idle, tsc: host::tsc(), browser: browser.processes().iter().map(|p| p.cycles).sum(), procs: (host::now_ns(), host::all_processes()) }
    }

    /// Other work on `cpus` (all when unpinned) as % of one core, and the busiest outside processes.
    fn stop(self, cpus: Option<u64>, browser: &chromium::Browser) -> (f64, Vec<(String, f64)>) {
        let mut idle = Vec::new();
        let ok = host::idle_cycles(&mut idle);
        let (tsc, own) = (host::tsc(), browser.processes().iter().map(|p| p.cycles).sum::<u64>());
        let busiest = busiest_others(&self.procs, &(host::now_ns(), host::all_processes()));
        let span = match (self.tsc, tsc) {
            (Some(a), Some(b)) if b > a && ok && idle.len() == self.idle.len() => (b - a) as f64,
            _ => return (0.0, busiest),
        };
        let busy: f64 = self
            .idle
            .iter()
            .zip(&idle)
            .enumerate()
            .filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1))
            .map(|(_, (a, b))| (100.0 * (1.0 - b.saturating_sub(*a) as f64 / span)).clamp(0.0, 100.0))
            .sum();
        ((busy - 100.0 * own.saturating_sub(self.browser) as f64 / span).max(0.0), busiest)
    }
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

/// What the screen was doing over a block or recording, for its meta and `query summary`: the
/// display (on, off, dimmed), the session (locked or not) and the compositor's CPU between two
/// process snapshots. A recording made with the display asleep or locked draws differently (a
/// composited spin, a parked compositor), and its numbers only compare with recordings in that state.
pub(crate) fn screen_state(a: &(u64, Vec<host::ProcInfo>), b: &(u64, Vec<host::ProcInfo>)) -> Value {
    let before: BTreeMap<u32, u64> = a.1.iter().map(|p| (p.pid, p.cpu_ns)).collect();
    let span = b.0.saturating_sub(a.0).max(1) as f64;
    let compositor = b.1.iter().filter(|p| host::COMPOSITORS.iter().any(|c| p.name.eq_ignore_ascii_case(c))).fold(None, |acc: Option<(String, f64)>, p| {
        let pct = 100.0 * p.cpu_ns.saturating_sub(before.get(&p.pid).copied().unwrap_or(p.cpu_ns)) as f64 / span;
        Some(match acc {
            Some((n, x)) => (n, x + pct),
            None => (p.name.clone(), pct),
        })
    });
    json!({
        "display": host::display_state().map(|d| match d.0 { 0 => "off", 1 => "on", _ => "dimmed" }),
        "session": if host::session_locked() { "locked" } else { "unlocked" },
        "compositor": compositor.map(|(name, pct)| json!({ "name": name, "cpu_pct": pct })),
    })
}

/// `display on · session locked · dwm.exe 3.1% of a core`, from `screen_state`.
pub(crate) fn screen_line(s: &Value) -> Option<String> {
    let session = s["session"].as_str()?;
    let mut parts = vec![format!("display {}", s["display"].as_str().unwrap_or("?")), format!("session {session}")];
    if let Some(c) = s["compositor"].as_object() {
        parts.push(format!("{} {:.1}% of a core", c["name"].as_str().unwrap_or("compositor"), c["cpu_pct"].as_f64().unwrap_or(f64::NAN)));
    }
    Some(parts.join(" · "))
}

/// The commit subject of a git variant, so a report still says what was compared days later.
fn subject(v: &Variant) -> Option<String> {
    let root = v.root.as_ref()?;
    Some(git::subject(root, "HEAD")).filter(|s| !s.is_empty())
}

/// How many times a block waits for other work to end before `Background` refuses it: each wait
/// and speed check takes about 4 minutes.
const BUSY_WAITS: u32 = 15;

/// How many times a macOS block whose app lost the front while nobody used the Mac runs again.
const HIDDEN_RETRIES: u32 = 5;

/// Runs a block while the screen is unlocked and lit: waits out a locked session, redoes a block the
/// lock screen touched (Chromium hides its windows and nothing reaches the display, so its numbers
/// describe nothing), and wakes a display that went dark on idle, once, before a block refuses
/// `WindowHidden`. It also waits out someone using a Mac whose window it would cover, and other work
/// that keeps the CPUs busy (`Background`) for up to about an hour. The block's own checks still refuse
/// anything else.
fn block_on_screen(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    // The validator that refused the block, if one did.
    let refused_by = |r: &Result<Block>| r.as_ref().err().and_then(Error::validator);
    // macOS: the app's window was covered, or the app not the active one.
    let behind = |r: &Result<Block>| r.as_ref().err().is_some_and(|e| matches!(e.hidden(), Some(Hidden::Covered | Hidden::NotActive)));
    // A series that needs no presenting window runs through a lock, in one lock state per block.
    let mut retries = 0;
    // A series that may run locked but met a hidden window on an unlocked screen (a `--check` or
    // `record --bench` of a windowed native case): it waits and retries as on-screen blocks do.
    let mut hidden_unlocked = false;
    if runs_locked(ctx, s, x) {
        loop {
            let (start, locks, unlocks) = (host::session_locked(), ctx.awake.locks(), ctx.awake.unlocks());
            let display = host::display_state();
            let r = slotted_block(ctx, s, v, pair, x);
            if retry_launch(ctx, s, v, &r, &mut retries)? {
                continue;
            }
            let end = host::session_locked();
            let steady = start == end && if start { ctx.awake.unlocks() == unlocks } else { ctx.awake.locks() == locks };
            // The display turned off (or on) during the block: it ran in two machine states.
            let lit = display.is_none_or(|(_, n)| host::display_state().is_some_and(|(_, m)| m == n));
            if steady && lit && !start && refused_by(&r) == Some(ValidatorId::WindowHidden) {
                hidden_unlocked = true;
                break;
            }
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
    let mut hidden_retries = 0;
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
            // `--unlock-wait`: how long one wait may last before the bench ends with what it finished.
            let limit = ctx.args.get("unlock-wait").map(duration_ms).transpose()?.map(Duration::from_millis);
            while host::session_locked() {
                if let Some(l) = limit.filter(|l| waiting.elapsed() >= *l) {
                    ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
                    return Err(Error::timeout(format!("the session stayed locked for {} (--unlock-wait), and the series needs a presenting window", fmt::ms_duration(l.as_millis() as u64)))
                        .hint(format!("strobe bench --resume {} once the session is unlocked", ctx.exp)));
                }
                nap(Duration::from_secs(2))?;
            }
            // The desktop and the GPU settle after an unlock.
            nap(Duration::from_secs(5))?;
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
        }
        let before = ctx.awake.locks();
        let r = slotted_block(ctx, s, v, pair, x);
        if retry_launch(ctx, s, v, &r, &mut retries)? {
            continue;
        }
        if !woken && refused_by(&r) == Some(ValidatorId::WindowHidden) {
            progress(format!("  the {} window was hidden (a display off on idle?): waking the display and running the block again", v.role));
            woken = true;
            continue;
        }
        // macOS: someone is using the Mac, so strobe did not take their focus to uncover the window.
        // Wait for them to leave it, as for a locked session, holding nothing meanwhile.
        if behind(&r) && host::person_idle_s().is_some_and(|i| i < 60.0) {
            progress(format!("  waiting: someone is using this Mac, and strobe measures the {} app as the active one; it runs the block again once nobody has touched the Mac for a minute", v.role));
            release_machine(ctx);
            ctx.reg.set("waiting", format!("{} {} block: someone is using this Mac (the app must be the active one)", s.label(), v.role));
            let waiting = Instant::now();
            while host::person_idle_s().is_some_and(|i| i < 60.0) {
                nap(Duration::from_secs(5))?;
            }
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
            continue;
        }
        // macOS, nobody at the Mac: another app came forward during the block (one that opens a window
        // or takes focus by itself). The block is discarded and runs again, a few times.
        if hidden_retries < HIDDEN_RETRIES && behind(&r) {
            hidden_retries += 1;
            progress(format!("  another app came in front of the {} app during the block: discarded, it runs again ({hidden_retries} of {HIDDEN_RETRIES})", v.role));
            nap(Duration::from_secs(5))?;
            continue;
        }
        // Other work (a build, an update) kept the CPUs busy through the speed check: on a shared
        // machine it ends. Wait for it, holding nothing, and run the block again; an hour of it refuses.
        if busy_waits < BUSY_WAITS && refused_by(&r) == Some(ValidatorId::Background) {
            busy_waits += 1;
            let who = r.as_ref().err().and_then(|e| e.refusal.as_ref()).and_then(|f| f.detail.clone()).unwrap_or_default();
            progress(format!("  waiting: other work keeps the CPUs busy ({who}); strobe runs the {} block again once it ends", v.role));
            release_machine(ctx);
            ctx.reg.set("waiting", format!("{} {} block: other work keeps the CPUs busy", s.label(), v.role));
            let waiting = Instant::now();
            nap(Duration::from_secs(60))?;
            crate::validity::wait_quiet(None, Duration::from_secs(60));
            ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
            continue;
        }
        if ctx.awake.locks() == before && !host::session_locked() {
            return if hidden_unlocked {
                r.map(|mut b| {
                    b.display = host::display_state().map(|d| d.0);
                    session_state(s, b, false)
                })
            } else {
                r
            };
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
    for l in crate::target::isolate::reap_dead_owners() {
        progress(format!("  {l}"));
    }
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
    if ctx.slot.is_some() && ctx.mode != Mode::Check && slot::others_queued(&ctx.exp) {
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
    // A `--check` keeps the slot from one short untimed block to the next (its builds and waits still
    // hand it over): taking turns per block made a 9-case check take 25 minutes beside a long bench.
    if ctx.mode != Mode::Check && slot::others_queued(&ctx.exp) {
        release_machine(ctx);
    } else {
        ctx.reg.set("between", "between blocks; holds the slot until the next one or a wait");
    }
    r
}

/// One block in the bench slot, with this user's other processes kept off the target's CPUs while it
/// runs (`--reserve-cpus`, SPEC §9.4).
fn slotted_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    in_slot(ctx, &format!("{} {} block", s.label(), v.role), |ctx| {
        let reserve = reserve_for(ctx, x);
        ctx.target_qos = None;
        let r = any_block(ctx, s, v, pair, x).and_then(|b| priority_state(ctx, s, x, b)).map(|b| headless_state(ctx, s, b)).map(|b| phase_state(ctx, s, b)).map(|b| counter_state(s, x, b)).inspect(|b| restate_recording(ctx, b));
        // A timed or check block that refused its primary refuses the block, as a native one does:
        // pairs of nothing would end `unclear` instead of saying why.
        let r = r.and_then(|b| match b.refused.get(&s.primary).filter(|_| x.set != Set::Attribution && x.set != Set::Memory) {
            Some(why) => Err(Refusal {
                summary: if why.summary.starts_with(&format!("{}:", s.primary)) { why.summary.clone() } else { format!("{}: {}", s.primary, why.summary) },
                fix: why.fix.clone().or_else(|| Some("pick a primary this target can measure".into())), ..why.clone() }.into()),
            None => Ok(b),
        });
        let r = match reserve.map(host::reserve::Reserve::finish) {
            Some(report) => {
                ctx.reserved.get_or_insert_with(ReserveTotal::default).add(&report);
                r.map(|mut b| {
                    b.reserve = Some(json!({ "moved": report.moved_total(), "started": report.started, "failed": report.failed.len(), "restored": report.restored, "overhead_pct": report.overhead_pct, "setup_ms": report.setup_ms }));
                    b.overhead.get_or_insert_with(Overhead::default).reserve_pct = Some(report.overhead_pct);
                    b
                })
            }
            None => r,
        };
        // A part of strobe over its §19 budget in a timed block competes with the app it measures:
        // said once per series and part.
        let over = r.as_ref().ok().and_then(|b| b.overhead.as_ref()).filter(|_| x.measures_time()).map(Overhead::over).unwrap_or_default();
        for (part, pct, budget) in over {
            if ctx.overhead_warned.insert(format!("{}\n{part}", s.label())) {
                ctx.warnings.push(validity::warn("OverheadBudget", format!("{} · {} block: {part} used {} of a core, over its {budget}% budget (SPEC §19)", s.label(), v.role, pct_of_core(pct))));
            }
        }
        r
    })
}

/// Starts the reservation a block gets: `auto` for timed blocks (not `--check`, attribution, memory
/// or pixels blocks), `on` for every block, `off` never. It needs a pinned target.
fn reserve_for(ctx: &mut Ctx, x: &Exec) -> Option<host::reserve::Reserve> {
    let wanted = match ctx.reserve {
        "on" => true,
        "off" => false,
        _ => x.measures_time(),
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
fn any_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let native = is_native(&ctx.describe["targets"][&s.target]);
    if s.launch.is_object() && native {
        native::launch(ctx, s, v, pair, x)
    } else if s.launch.is_object() {
        launch::block(ctx, s, v, pair, x)
    } else if native {
        native::block(ctx, s, v, pair, x)
    } else if ctx.describe["targets"][&s.target]["kind"] == "script" {
        script::block(ctx, s, v, pair, x)
    } else {
        run_block(ctx, s, v, pair, x)
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
fn runs_locked(ctx: &Ctx, s: &Series, x: &Exec) -> bool {
    let t = &ctx.describe["targets"][&s.target];
    let offscreen = |m: &str| {
        ["cpu.", "ui.", "memory.", "io.", "count.", "gpu.", "phase.", "energy."].iter().any(|p| m.starts_with(p))
            || m == "run.duration"
            || (m.starts_with("launch.") && m != "launch.painted" && m != "launch.settled")
    };
    match t["kind"].as_str().unwrap_or("") {
        "script" => true,
        // A headless page draws to no display: the lock screen covers nothing of it.
        "web" | "url" if ctx.headless => true,
        _ if x.set == Set::Memory => true,
        "native" if t["window"] == false || x.untimed || t["app"].as_str().and_then(|id| isolate::app(id, &ctx.config).ok()).is_some_and(|a| a.windowless) => {
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
            b.refused.insert(m.clone(), Refusal::new(ValidatorId::SessionLocked, "the session was locked, so the compositor drew nothing and GPU use describes no screen"));
        }
    }
    b
}

/// Frames on screen and input latencies: what background priority makes meaningless (SPEC §10.1
/// `BackgroundPriority`). At background QoS a drag read 83-91% dropped frames that dropped 0.4% at
/// normal priority.
pub(crate) fn foreground_metric(m: &str) -> bool {
    ["frames.", "interaction.", "scroll.", "canvas.", "input.", "display."].iter().any(|p| m.starts_with(p)) || m == "launch.painted" || m == "launch.settled"
}

/// A kept recording's values and refusals as the block ended with them: the block wrote its
/// `meta.json` before `priority_state` and `headless_state` refused what the screen or the priority
/// make meaningless, and the recording must not hold those values under its handle.
fn restate_recording(ctx: &Ctx, b: &Block) {
    let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) else { return };
    let Ok(mut meta) = store::read_json(&dir.join("meta.json")) else { return };
    meta["values"] = json!(b.values);
    meta["refused"] = refusal_map(&b.refused);
    if ctx.headless {
        meta["headless"] = json!(true);
    }
    let _ = store::write_json(&dir.join("meta.json"), &meta);
}

/// `--headless`: what reached no display is not kept. Frames, input latencies, scrolling, canvas
/// frames and GPU use go, and the declared ones show refused; CPU, main-thread work, counters,
/// memory and milestones stay.
fn headless_state(ctx: &Ctx, s: &Series, mut b: Block) -> Block {
    if !ctx.headless {
        return b;
    }
    // GPU memory too when the headless browser drew in software: no GPU held it.
    let software = ctx.gpu["software"] == true;
    let drawn = |m: &str| foreground_metric(m) || gpu_use(m) || (software && matches!(m, "gpu.vram" | "gpu.shared"));
    b.values.retain(|k, _| !drawn(k));
    for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| drawn(m)) {
        b.refused.insert(m.clone(), Refusal::new(ValidatorId::NoPresentSource, format!("{m}: the page ran headless (--headless), so nothing it drew reached a display")));
    }
    b
}

/// A declared `phase.<name>` that a block which measured its runs has no value for: no measured run
/// entered the phase, so it is refused `NoPhase` rather than left without a value (native blocks say
/// so themselves; page and script blocks did not). Likewise a declared `span.<name>` (`NoSpan`): no
/// measured run's page called `performance.measure(name)`, or the target has no page.
fn phase_state(ctx: &Ctx, s: &Series, mut b: Block) -> Block {
    if b.values.is_empty() {
        return b;
    }
    let page = !is_native(&ctx.describe["targets"][&s.target]) && ctx.describe["targets"][&s.target]["kind"] != "script";
    for m in declared_metrics(s) {
        if b.values.get(m).is_some_and(|v| v.is_finite()) || b.refused.contains_key(m) {
            continue;
        }
        let refusal = if let Some(phase) = m.strip_prefix("phase.") {
            Refusal::new(ValidatorId::NoPhase, format!("{m}: no measured run entered phase(\"{phase}\")"))
        } else if let Some(span) = m.strip_prefix("span.") {
            let why = if page { format!("no measured run's page called performance.measure(\"{span}\")") } else { "spans are a page's performance.measure() calls; this target has no page".into() };
            Refusal::new(ValidatorId::NoSpan, format!("{m}: {why}"))
        } else {
            continue;
        };
        b.values.remove(m);
        b.refused.insert(m.clone(), refusal);
    }
    b
}

/// Linux's per-thread perf events (opened for a series that declares `cpu.instructions`) cost the
/// target at each of its context switches: 4.4 µs a switch in a VM, which made a run of 2000 short
/// sleeps cost 36.8 ms of CPU instead of 19.3 ms. That series' times describe a disturbed run, so its
/// time metrics (ms, CPU shares, frames, input latency) are dropped, and refused `OverheadBudget`
/// where declared (rule 3). The instruction count itself is user mode only: the switches' cost is
/// not in it. macOS counts always, at no cost to the target.
fn counter_state(s: &Series, x: &Exec, mut b: Block) -> Block {
    if x.counters {
        // A counter block's times describe a run whose every context switch the events slowed: its
        // timing blocks hold them.
        b.values.retain(|m, _| !time_metric(m));
        b.refused.retain(|m, _| !time_metric(m));
        return b;
    }
    // A launch block (and a leak or pixels block) reads no counters: said, not left blank. A clean
    // timing block of a series counted apart leaves them to its counter blocks.
    let apart = counting(s) != Counting::InTiming;
    for m in declared_metrics(s).into_iter().filter(|m| probe::wakes::COUNTER_METRICS.contains(&m.as_str())) {
        if b.values.contains_key(m) || b.refused.contains_key(m) || (apart && counted_apart(m)) || b.values.is_empty() {
            continue;
        }
        let what = if s.launch.is_object() { "a launch case" } else { "this kind of block" };
        b.refused.insert(m.clone(), Refusal::new(ValidatorId::NoCounter, format!("{what} reads no counters: they are read around a case's runs")).fix("count in a case with runs"));
    }
    b
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
fn priority_state(ctx: &mut Ctx, s: &Series, x: &Exec, mut b: Block) -> Result<Block> {
    let Some(why) = ctx.target_qos.take().or_else(|| ctx.qos.clone()).filter(|_| !matches!(x.set, Set::Attribution | Set::Memory)) else { return Ok(b) };
    if foreground_metric(&s.primary) && !x.untimed {
        return Err(background_refusal(&why, &format!("the primary {}", s.primary)));
    }
    b.values.retain(|k, _| !foreground_metric(k));
    for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| foreground_metric(m)) {
        b.refused.insert(m.clone(), Refusal::new(ValidatorId::BackgroundPriority, format!("the target ran at {why}")));
    }
    if ctx.qos.is_none() && !ctx.warnings.iter().any(|w| w.id == "BackgroundPriority") {
        ctx.warnings.push(validity::warn("BackgroundPriority", format!("a {} block's target ran at {why}: its frame and input-latency metrics are refused", b.variant)));
    }
    Ok(b)
}

fn run_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let (set, keep, n) = (x.set, x.keep, x.runs);
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
    // As before a native block's speed check: a timed block launches once the target's CPUs are quiet
    // (the last block's browser still frees its memory and GPU objects there), waiting at most 10 s.
    // Only CPUs strobe keeps for the target: an unpinned machine's every CPU is never all quiet (on
    // air every block waited the full 10 s).
    let settled = Instant::now();
    if x.measures_time() && ctx.pin.is_some() {
        validity::wait_quiet(ctx.pin, Duration::from_secs(10));
    }
    let quiet_ms = settled.elapsed().as_secs_f64() * 1e3;
    let launched = Instant::now();
    let mut guard = None;
    let browser = if kind == "command" {
        let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a command target runs in a variant's folder (a directory or a git ref)", v.role)))?;
        crate::target::command::launch(&crate::target::command::Spec::from_target(&target, &root), &isolate::new_home(), true, &window, ctx.pin)?.0
    } else if app {
        let launch = with_gpu_cache(ctx, s, v, &target, variant_app(v, &target, &ctx.config)?)?;
        let seed = seed_dir(ctx, s, &target);
        let (b, g) = electron::launch(&launch, seed.as_deref(), &window, ctx.pin)?;
        guard = Some(g);
        b
    } else {
        chromium::launch(&window, ctx.headless, ctx.pin, &[])?
    };
    let steps = RefCell::new(vec![("quiet".to_string(), quiet_ms), ("launch".to_string(), launched.elapsed().as_secs_f64() * 1e3)]);
    ctx.target_qos = host::background_qos(Some(browser.pid));
    let lapped = std::cell::Cell::new(Instant::now());
    let lap = |name: &str| {
        steps.borrow_mut().push((name.to_string(), lapped.get().elapsed().as_secs_f64() * 1e3));
        lapped.set(Instant::now());
    };
    let result = (|| -> Result<Block> {
        if !ctx.gpu_checked {
            // Headless, a software rasterizer refuses nothing: frames and GPU use are refused anyway.
            let gpu = if ctx.headless { browser.gpu()? } else { validity::software_gpu(&browser)? };
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
        driver.pixels = x.pixels.clone();
        // This run's `count.<name>` values (the bench file's `ctx.count` and the page's counter global)
        // and the run's key, which names its phase marks.
        let hooks = block::Hooks::default();
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
        let page_counts =
            || -> BTreeMap<String, f64> { page.eval("JSON.stringify(globalThis.__strobeCounts ?? {})").ok().and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())).unwrap_or_default() };
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
                    if call == "goto" && hooks.in_run() {
                        let before = counters();
                        let mut c = carried.borrow_mut();
                        for (sum, v) in c.iter_mut().zip(before) {
                            *sum += v;
                        }
                        if !counted.is_empty() {
                            hooks.add(&page_counts());
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
                    hooks.count(a);
                    Ok(Value::Bool(true))
                }
                // A page's phases are marks in its own trace, read from there with the run's frames.
                "phase" => {
                    if let Some(key) = hooks.phase_key(a) {
                        driver.mark(&key)?;
                    }
                    Ok(Value::Bool(true))
                }
                "line" => Err(Error::usage("waitForLine reads a native app's output or the log files under its home; a page has neither (wait for the page instead: page.waitFor)")),
                other => Err(Error::msg(format!("unknown runner request {other}"))),
            }
        };
        let t = s.hook_timeout();
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
        let during = Foreign::start(&browser);
        let speed = validity::speed_check(&page)?;
        let ratio = match validity::throttled(speed, ctx.baseline) {
            // A slow check while other work kept the target's CPUs busy (the browser's own processes
            // not counted) is that work's doing, as for a native block: `Background`, which the bench
            // waits out, not `Throttled`.
            Err(e) => {
                let (foreign, busiest) = during.stop(ctx.pin, &browser);
                return Err(if foreign >= BACKGROUND_PCT {
                    let who = busiest.iter().map(|(n, p)| format!("{n} {p:.0}%")).collect::<Vec<_>>().join(", ");
                    Refusal::new(ValidatorId::Background, format!("the speed check ran slow while other work kept the target's CPUs {foreign:.0}% of a core busy ({})", e.message))
                        .detail(format!("busiest processes (% of a core): {}", if who.is_empty() { "none named (system work)".into() } else { who }))
                        .fix("let that work finish (an antivirus scanning new files, an update, a build), or exclude it from the bench's CPUs")
                        .into()
                } else {
                    e
                });
            }
            Ok(r) => r,
        };
        // A leak run reads heaps, not pixels: on a locked screen the hidden page is still measured.
        let hidden_ok = set == Set::Memory && host::session_locked();
        if !hidden_ok {
            validity::window_visible(&page)?;
        }
        lap("checks");
        ctx.runner.call("setup", json!({}), t, &serve).map_err(|e| crate::runner::incomplete("setup", "setup", e, &snapshot(&page, &ctx.exp, "setup"), "fix the bench file's setup"))?;
        let start = shown::page(ctx, s, v, pair, "start", &page);
        shown::expect(&s.expect, &start, "before the runs", true, &|sel| shown::page_shows(&page, sel))?;
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
        // A counter block traces nothing: its times are dropped, and tracing would add the browser's
        // own instructions to what the counters count.
        let tracing = if set == Set::Memory || x.counters { None } else { Some(probe::trace::start_with(&browser.cdp, set, &extra_categories)?) };
        let mem_dir = if set == Set::Memory { Some(store::recording_dir(&id)?) } else { None };
        // A leak run measures 2 × N runs (`Warmup`): N to S1 and N more to S2.
        let mut census: Vec<Value> = Vec::new();
        // GPU engines and VRAM of the target's process tree, read from the OS (SPEC §13.6).
        let sampler = probe::os::Sampler::start(browser.pid);
        let (sampled, runner_pid) = (Instant::now(), ctx.runner.pid());
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

        let run_timeout = s.run_timeout();
        let perf = x.counters.then(probe::wakes::Perf::new);
        let mut cost = SelfCost::new(runner_pid);
        let mut run_once = |key: &str| -> Result<Run> {
            let began = Instant::now();
            hooks.begin(key);
            // The target's CPU in TSC cycles over TSC time: process times move in 15.6 ms ticks, which
            // made an idle 5 s run read 0, 0.31% or 0.62% and nothing between. One read of the
            // target's processes at each end of the run gives its cycles, times and roles.
            let read = || {
                let procs = browser.processes();
                let tsc = host::tsc();
                let (cycles, cpu) = procs.iter().fold((0u64, 0u64), |(c, t), p| (c + p.cycles, t + p.cpu_ns));
                (procs, cycles, tsc, cpu)
            };
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
            let listed = probe::wakes::wall_us();
            let (procs_start, cyc0, tsc0, cpu0) = read();
            let k0 = probe::wakes::Counters::take(listed, &procs_start.iter().map(|p| p.pid).collect::<Vec<_>>(), perf.as_ref());
            *carried.borrow_mut() = [0.0; 6];
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
            // strobe's own cost over the run as its trace marks bound it, from its start mark to its end mark.
            cost.start();
            driver.mark(&format!("{key}:start"))?;
            let t0 = host::now_ns();
            ctx.runner
                .call("run", json!({ "case": s.case }), run_timeout, &serve)
                .map_err(|e| crate::runner::incomplete(&format!("{} run", s.case), "run", e, &snapshot(&page, &ctx.exp, "run"), "the run threw or timed out; fix the bench file or the app"))?;
            driver.dispatch("frame", &[])?;
            let t1 = host::now_ns();
            let input = crate::driver::input_pacing(&driver.drags.take());
            let c1 = counters();
            driver.mark(&format!("{key}:end"))?;
            cost.end();
            let (procs_end, cyc1, tsc1, cpu1) = read();
            let k1 = probe::wakes::Counters::take(probe::wakes::wall_us(), &procs_end.iter().map(|p| p.pid).collect::<Vec<_>>(), perf.as_ref());
            roles.extend(process_roles(&browser));
            let (role0, role1) = (by_role(&procs_start, &roles), by_role(&procs_end, &roles));
            if !counted.is_empty() {
                hooks.add(&page_counts());
            }
            take_canvas();
            run_canvas.borrow_mut().push(canvas_docs.take());
            let index = run_counts.borrow().len();
            run_counts.borrow_mut().push(hooks.counts());
            if let Some(n) = &net {
                n.borrow_mut().drain(Some(index));
            }
            hooks.end();
            // A key or button the run left held would be down in the next one: let go, outside its span.
            driver.release_all()?;
            if s.reset {
                ctx.runner
                    .call("reset", json!({ "case": s.case }), run_timeout, &serve)
                    .map_err(|e| crate::runner::incomplete(&format!("{} reset", s.case), "reset", e, &snapshot(&page, &ctx.exp, "reset"), "fix the case's reset"))?;
                driver.release_all()?;
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
            let counted = probe::wakes::counted(&k0, &k1);
            Ok(Run { ms: wall / 1e6, cpu, task: c1[0] + carry[0] - c0[0], parts, roles, t0, t1, total: began.elapsed().as_secs_f64() * 1e3, input, counted })
        };
        // Warm until two consecutive runs do the same main-thread work (within 5%); both of them count
        // as measured (SPEC §9.2). Wall time says nothing here: a fixed-speed scroll or paced typing
        // takes the same time cold or warm, while the work it costs settles over 1–4 runs. Attribution
        // blocks start their profiler at the steady point, so they measure the runs after it.
        let mut runs: Vec<Run> = Vec::new();
        let mut nodes_all = Vec::new();
        let mut warmup = block::Warmup::new(x);
        let mut starts = shown::Starts::new(set);
        while warmup.more(runs.len()) {
            let i = runs.len();
            nodes_all.push(driver.nodes().unwrap_or(0));
            starts.check(i + 1, false, &|| shown::page_start(&page))?;
            runs.push(run_once(&format!("run:{i}"))?);
            if let Some(f) = warmup.first() {
                // Leak run: a post-GC census after every run; S1 after N runs, S2 after 2 × N.
                if let Some(dir) = &mem_dir {
                    let mut c = probe::census::take(&browser, &page);
                    c["os_after_snapshot"] = json!(true);
                    let done = runs.len() - f;
                    if done == 2 * n {
                        c["allocators"] = probe::census::allocators(&browser, &dir.join("memdump-s2.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
                    }
                    census.push(c);
                    if done == n {
                        probe::heap::snapshot(&page, &dir.join("heap-s1.heapsnapshot.gz"))?;
                    }
                    if done == 2 * n {
                        probe::heap::snapshot(&page, &dir.join("heap-0.heapsnapshot.gz"))?;
                    }
                }
                continue;
            }
            // The work that must hold steady is the renderer's main-thread time. `--check` measures the
            // first run: it asks whether a case works, not what it costs.
            if warmup.after(&runs.iter().map(|r| r.task).collect::<Vec<_>>()) && !x.untimed {
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
        starts.check(runs.len() + 1, true, &|| shown::page_start(&page))?;
        let unsteady = warmup.unsteady();
        let first = warmup.first().unwrap_or(0);
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
        let (strobe_pct, runner_pct) = cost.share(first);
        let samples = sampler.stop();
        let sampler_pct = samples.cpu_ms.map(|c| 100.0 * c / (sampled.elapsed().as_secs_f64() * 1e3).max(1.0));
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
        let screen = screen_state(&procs0, &procs1);
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
        let leak = if set == Set::Memory { Some(leak_analysis(&census, &dir, n)?) } else { None };
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
        // What the page showed after the runs, once the trace and profilers have stopped.
        let end = shown::page(ctx, s, v, pair, "end", &page);
        shown::expect(&s.expect, &end, "after the runs", true, &|sel| shown::page_shows(&page, sel))?;
        if let Err(e) = ctx.runner.call("close", json!({}), t, &serve)
            && !ctx.warnings.iter().any(|w| w.id == "Teardown")
        {
            ctx.warnings.push(validity::warn("Teardown", format!("{} block: {}", v.role, e.message)));
        }
        let mut data = if set == Set::Memory || x.counters { metrics::BlockData::default() } else { metrics::read(&trace, &token)? };
        data.warm_below(first);
        lap("analyse");
        let mut extra = BTreeMap::new();
        let run_counts = run_counts.into_inner();
        for name in &counted {
            extra.insert(format!("count.{name}"), (0..measured.len()).map(|j| (key(j), run_counts.get(first + j).and_then(|c| c.get(name)).copied().unwrap_or(0.0))).collect());
        }
        // `phase.<name>`: the run's phase windows from the trace's marks.
        for name in declared_metrics(s).into_iter().filter_map(|m| m.strip_prefix("phase.")) {
            let per: BTreeMap<String, f64> = (0..measured.len())
                .filter_map(|j| {
                    let suffix = format!(":run:{}:phase:{name}", first + j);
                    let spans: Vec<f64> = data.windows.iter().filter(|w| w.key.ends_with(&suffix)).map(|w| (w.t1 - w.t0) as f64 / 1e6).collect();
                    (!spans.is_empty()).then(|| (key(j), spans.iter().sum()))
                })
                .collect();
            extra.insert(format!("phase.{name}"), per);
        }
        // `span.<name>`: the page's own performance.measure() spans per run, from the trace.
        for name in declared_metrics(s).into_iter().filter_map(|m| m.strip_prefix("span.")) {
            if let Some(per) = data.span_per_run("run", name) {
                extra.insert(format!("span.{name}"), per);
            }
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
        let counted: Vec<(String, &probe::wakes::Counted)> = measured.iter().enumerate().map(|(j, r)| (key(j), &r.counted)).collect();
        let (counts, uncounted) = probe::wakes::counter_metrics(&counted, declared_metrics(s));
        extra.extend(counts);
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
                "id": id, "kind": "block", "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label, "variant_git": variant_git(v),
                "target": url, "probes": set, "token": token, "runs": measured.len(), "values": values, "duration_ms": run_ms.iter().sum::<f64>() as u64,
                "created": super::record::now_iso(), "valid": true, "env": host::env(), "browser": browser.version, "background": background, "screen": screen,
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
            refused: uncounted,
            locked: false,
            display: None,
            screen: Some(screen),
            reserve: None,
            shown: Some(shown::Shown { start, end }),
            overhead: Some(Overhead { strobe_pct, runner_pct, sampler_pct, ..Overhead::default() }),
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
/// drop. In the daemon, which runs other invocations in the same process, only this bench's thread
/// moves (on Linux the threads and processes it starts inherit that), never the process.
struct OwnAffinity(Option<u64>);

thread_local! {
    /// The CPUs this thread's bench keeps it on inside the daemon (`OwnAffinity`), for `Unpinned`.
    static THREAD_PIN: Cell<Option<u64>> = const { Cell::new(None) };
}

impl OwnAffinity {
    fn start(mask: Option<u64>) -> OwnAffinity {
        let Some(m) = mask else { return OwnAffinity(None) };
        if super::in_daemon() {
            host::set_thread_affinity(m);
            THREAD_PIN.with(|p| p.set(Some(m)));
            return OwnAffinity(None);
        }
        OwnAffinity(host::set_affinity(m))
    }
}

impl Drop for OwnAffinity {
    fn drop(&mut self) {
        if let Some(mask) = self.0 {
            host::set_affinity(mask);
        }
        THREAD_PIN.with(|p| p.set(None));
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

/// Where a variant's launches keep their GPU shader caches during an experiment (`gpuCache: "warm"`).
fn gpu_store(exp: &str, v: &Variant) -> PathBuf {
    host::paths::sub("gpu-cache").join(format!("{exp}-{}", v.role))
}

/// The `gpuCache` a series' launches take: its case's, else its target's, else its app profile's
/// (`app`), default `warm`.
fn gpu_cache_mode<'a>(s: &'a Series, target: &'a Value, app: Option<&'a isolate::App>) -> &'a str {
    s.gpu_cache.as_deref().or(target["gpuCache"].as_str()).or(app.and_then(|a| a.gpu_cache_mode.as_deref())).unwrap_or("warm")
}

/// The series' `gpuCache` (`gpu_cache_mode`) set on `app`: `warm` lends each
/// launch the GPU shader caches the variant's earlier launches compiled (its own store, never the
/// other variant's or the user's), as a person's machine has them; `cold` starts every launch
/// without any. `--check` has no discarded block: its first launch per variant is cold, the next
/// (the next case, `--why always`'s attribution block) warm. Noted per target for the plan (per
/// series when a case sets its own).
fn with_gpu_cache(ctx: &mut Ctx, s: &Series, v: &Variant, target: &Value, mut app: isolate::App) -> Result<isolate::App> {
    let mode = gpu_cache_mode(s, target, Some(&app)).to_string();
    let note = match mode.as_str() {
        _ if cfg!(target_os = "macos") => "os: macOS keeps Metal's compiled shaders per app in the user's own cache folder, which a launch cannot move: warm, and shared by both variants",
        "warm" if ctx.args.get("gpu-store") == Some("exe") => {
            app.gpu_cache = Some(isolate::exe_gpu_store(&app.exe));
            "warm: recordings of one executable share its GPU shader caches (the first recording's launches compile them)"
        }
        "warm" => {
            app.gpu_cache = Some(gpu_store(&ctx.exp, v));
            "warm: each variant keeps the driver's and Chromium's GPU shader caches across its own launches, from discarded first blocks"
        }
        "cold" => "cold: every launch compiles its shaders again (gpuCache: \"cold\")",
        other => return Err(Error::usage(format!("{}: gpuCache is warm or cold, not {other}", s.target))),
    };
    let key = if s.gpu_cache.is_some() { s.label() } else { s.target.clone() };
    ctx.gpu_cache.entry(key).or_insert_with(|| note.to_string());
    Ok(app)
}

/// Whether a series' launches of `target` take warm GPU caches that its first block of each variant
/// fills (the block before the pairs, discarded).
fn gpu_warming(ctx: &Ctx, s: &Series, target: &Value) -> Result<bool> {
    let kind = target["kind"].as_str().unwrap_or("");
    if !matches!(ctx.mode, Mode::Compare | Mode::Blocks(_)) || cfg!(target_os = "macos") || !s.launch.is_null() || !matches!(kind, "native" | "app" | "exe") {
        return Ok(false);
    }
    let profile = profile_of(target).and_then(|id| isolate::app(id, &ctx.config).ok());
    Ok(gpu_cache_mode(s, target, profile.as_ref()) == "warm")
}

/// The app profile a target launches from: `native(appId)`, `app(id)`, `exe(path, { app })`.
fn profile_of(target: &Value) -> Option<&str> {
    match target["kind"].as_str()? {
        "app" => target["id"].as_str(),
        "native" | "exe" => target["app"].as_str(),
        _ => None,
    }
}

/// What a bench can tell is wrong before it waits for the slot or builds anything: an app profile that
/// does not resolve, a guarded file strobe may not read, a `gpuCache` that is neither warm nor cold.
fn preflight(series: &[Series], describe: &Value, variants: &[Variant], config: &Value, headless: bool) -> Result<()> {
    for s in series {
        let target = &describe["targets"][&s.target];
        if headless && !matches!(target["kind"].as_str(), Some("web" | "url")) {
            return Err(Error::usage(format!("{}: --headless runs pages in a headless browser; this target is a {} target, which opens its own window", s.label(), target["kind"].as_str().unwrap_or("?"))));
        }
        let apps = profile_of(target).into_iter().chain(variants.iter().filter_map(|v| if let Kind::App(id) = &v.kind { Some(id.as_str()) } else { None }));
        let mut profiles = Vec::new();
        for id in apps {
            let app = isolate::app(id, config).map_err(|e| Error { message: format!("{}: {}", s.label(), e.message), ..e })?;
            isolate::Guard::take(&app).unreadable()?;
            profiles.push(app);
        }
        for app in profiles.iter().map(Some).chain(std::iter::once(None)) {
            let mode = gpu_cache_mode(s, target, app);
            if !matches!(mode, "warm" | "cold") {
                return Err(Error::usage(format!("{}: gpuCache is warm or cold, not {mode}", s.label())));
            }
        }
    }
    Ok(())
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
            // `--target`: target names as `--filter` takes cases (comma-separated, globs with *).
            if only_target.is_some_and(|t| !case_matches(t, target)) || on.as_ref().is_some_and(|on| !on.contains(&target.as_str())) {
                continue;
            }
            let primary = args.get("primary").map(str::to_string).unwrap_or_else(|| c["primary"].as_str().unwrap_or("frames.presented.p95").to_string());
            // The case's margins, then `--noworse` over them (a bare margin is the primary's).
            let mut noworse = margins(&c["noworse"], &primary).map_err(|e| Error::usage(format!("{case}: noworse: {e}")))?;
            if let Some(flag) = args.get("noworse") {
                noworse.extend(margins(&json!(flag), &primary).map_err(|e| Error::usage(format!("--noworse: {e}")))?);
            }
            let mut expect = targets[target]["expect"].clone();
            if let (Some(t), Some(c)) = (expect.as_object_mut(), c["expect"].as_object()) {
                t.extend(c.clone());
            } else if c["expect"].is_object() {
                expect = c["expect"].clone();
            }
            for p in &combos {
                out.push(Series {
                    expect: expect.clone(),
                    case: case.clone(),
                    target: target.clone(),
                    params: p.clone(),
                    primary: primary.clone(),
                    noworse: noworse.clone(),
                    metrics: c["metrics"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                    reset: c["reset"].as_bool().unwrap_or(false),
                    launch: c["launch"].clone(),
                    timeout: c["timeout"].as_u64(),
                    gpu_cache: c["gpuCache"].as_str().map(str::to_string),
                });
            }
        }
    }
    Ok(out)
}

/// Non-inferiority margins as fractions by metric, from `"2%"` or `2` (the primary's), or
/// `"main.work=2%,cpu.wakes=5%"`, or `{ "main.work": "2%" }`.
fn margins(v: &Value, primary: &str) -> std::result::Result<BTreeMap<String, f64>, String> {
    let pct = |v: &Value| -> std::result::Result<f64, String> {
        let x = match v {
            Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
            Value::String(s) => s.trim().trim_end_matches('%').trim().parse::<f64>().map_err(|_| format!("not a margin: {s} (use a percentage such as 2%)"))?,
            other => return Err(format!("not a margin: {other}")),
        };
        if !(0.0..100.0).contains(&x) {
            return Err(format!("{x}% is not a margin from 0% up to (not including) 100%"));
        }
        Ok(x / 100.0)
    };
    let mut out = BTreeMap::new();
    match v {
        Value::Null => {}
        Value::Object(o) => {
            for (k, x) in o {
                out.insert(k.clone(), pct(x)?);
            }
        }
        Value::String(s) if s.contains('=') => {
            for part in s.split(',').filter(|p| !p.trim().is_empty()) {
                let (k, x) = part.split_once('=').ok_or_else(|| format!("{part}: use metric=margin"))?;
                out.insert(k.trim().to_string(), pct(&json!(x.trim()))?);
            }
        }
        other => {
            out.insert(primary.to_string(), pct(other)?);
        }
    }
    if let Some(unknown) = out.keys().find(|k| model::metric(k).is_none()) {
        return Err(format!("{unknown} is not a metric strobe knows"));
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
            let sha = git::out(&root, &["rev-parse", "--short=10", "HEAD"]).unwrap_or_default();
            let dirty = git::out(&root, &["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let mut v = Variant {
                role,
                spec: spec.to_string(),
                kind: Kind::Dir,
                label: if sha.is_empty() { name } else { format!("{name}@{sha}{}", if dirty { " (dirty)" } else { "" }) },
                root: Some(root.clone()),
                base_url: None,
                server: None,
                exe: None,
                commit: None,
                dirty,
                state: tree_state(&root),
                changed: Vec::new(),
            };
            v.commit = (!sha.is_empty()).then_some(sha);
            return Ok(v);
        } else {
            (Kind::Git(spec.to_string()), spec.to_string(), None)
        }
    };
    let mut v = Variant { role, spec: spec.to_string(), kind, label, root, base_url: None, server: None, exe: None, commit: None, dirty: false, state: None, changed: Vec::new() };
    if let Kind::Git(r) = &v.kind {
        let sha = git::out(project, &["rev-parse", "--short=10", r])
            .ok_or_else(|| Error::usage(format!("{spec} is neither a directory nor a git ref in {}", project.display())))?;
        v.label = format!("{r}@{sha}");
        v.commit = Some(sha);
    }
    Ok(v)
}

/// The bench's harness: the bench file's folder, the strobe.json in use and strobe.json's `harness` paths, in
/// the checkout that holds the bench file. Every variant builds and serves with it (`overlay_harness`).
struct Harness {
    from: PathBuf,
    paths: Vec<PathBuf>,
}

fn harness(file: &Path, project: &Path, config: &Value) -> Harness {
    let dir = file.parent().unwrap_or(Path::new("."));
    let from = git::toplevel(dir)
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

/// `look app:<id> --build`: the native executable strobe.json's `build` makes of the checkout `dir`
/// is in, built as a bench of that directory builds it (unchanged since the last build: reused),
/// as strobe's own copy.
pub(crate) fn build_native(dir: &Path) -> Result<PathBuf> {
    let project = config::project_root(dir);
    let config = config::load(&project)?;
    let mut v = resolve_variant("head", &dir.display().to_string(), &project, &config)?;
    let root = v.root.clone().ok_or_else(|| Error::usage(format!("{} is not a folder to build", dir.display())))?;
    prepare_variant(&mut v, &json!({ "kind": "native" }), &config, &Harness { from: root, paths: Vec::new() }, &project)?;
    v.exe.take().ok_or_else(|| Error::usage(format!("strobe.json has no build.command for {} in {}: nothing to build", isolate::os(), project.display())))
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
        let output = crate::target::isolate::per_os(&build["output"])
            .ok_or_else(|| Error::usage(format!("a native target's build in strobe.json needs output for {}: the executable it builds", crate::target::isolate::os())))?;
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
    let mut serve_cmd = shell(&cmd);
    serve_cmd.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = host::spawn_owned(&mut serve_cmd, None, 0, None).map_err(|e| Error::msg(format!("serve {}: {e}", v.role)))?;
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
    let common = git::out(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
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
    let build_started = Instant::now();
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
    // How long this build took here, for `plan` (SPEC §9.1).
    let times = host::paths::sub("builds").join("times.json");
    let mut all = store::read_json(&times).unwrap_or_else(|_| json!({}));
    all[build_time_key(build)] = json!({ "ms": build_started.elapsed().as_secs_f64() * 1e3, "command": build["command"], "at": super::record::now_iso() });
    let _ = store::write_json(&times, &all);
    Ok(())
}

/// A build's entry in `builds/times.json`: its command and folder as written in strobe.json.
fn build_time_key(build: &Value) -> String {
    hash_str(&format!("{}\n{}", build["command"], build["cwd"]))
}

/// While alive, strobe (and what it starts) may use every CPU; dropping it restores strobe's pin and
/// waits up to 30 s for the CPUs to go quiet (a build's last processes, a scan of what it wrote). In
/// the daemon only this bench's thread widens and comes back, as `OwnAffinity` pinned only it.
struct Unpinned(Option<u64>);

impl Unpinned {
    fn start() -> Unpinned {
        let n = host::topology().logical.min(64);
        let all = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        if super::in_daemon() {
            if THREAD_PIN.with(|p| p.get()).is_some() {
                host::set_thread_affinity(all);
            }
            return Unpinned(None);
        }
        Unpinned(host::set_affinity(all))
    }
}

impl Drop for Unpinned {
    fn drop(&mut self) {
        if let Some(mask) = self.0 {
            host::set_affinity(mask);
        }
        if let Some(mask) = THREAD_PIN.with(|p| p.get()).filter(|_| super::in_daemon()) {
            host::set_thread_affinity(mask);
        }
        validity::wait_quiet(None, Duration::from_secs(30));
    }
}

fn build_key(root: &Path, build: &Value) -> String {
    let git = |a: &[&str]| git::out(root, a).unwrap_or_default();
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
    let sha = git::out(project, &["rev-parse", r]).ok_or_else(|| Error::usage(format!("unknown git ref {r} in {}", project.display())))?;
    let dir = host::paths::sub("builds").join(&sha[..12.min(sha.len())]);
    if dir.join(".git").exists() {
        if !(git::ok(&dir, &["checkout", "--force", "--detach", &sha]) && git::ok(&dir, &["clean", "-fd"])) {
            return Err(Error::msg(format!("could not reset the worktree for {r} at {}", dir.display())).hint("remove that folder; strobe makes it again"));
        }
    } else {
        progress(format!("  worktree {r} → {}", dir.display()));
        git::run(project, &["worktree", "add", "--detach", &dir.to_string_lossy(), &sha]).map_err(|e| Error::msg(format!("git worktree add for {r} failed: {}", e.message)))?;
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

/// `self   strobe 1.2% · runner 0.3% … of a core (mean over the blocks' runs)`, when the series measured it.
fn self_line(s: &Value) -> String {
    match serde_json::from_value::<Overhead>(s["overhead"].clone()).ok().map(|o| o.text()).filter(|t| !t.is_empty()) {
        Some(t) => format!("self   {t} (mean over the blocks' runs)\n"),
        None => String::new(),
    }
}

/// A `--blocks` series' text: the primary in each block, and the blocks' median with a 95% interval
/// (Hodges–Lehmann over the blocks; log scale when every value is positive). Nothing is compared.
fn blocks_text(r: &Value, s: &Value, file: &str) -> String {
    let metric = s["primary"]["metric"].as_str().unwrap_or("");
    let def = model::metric(metric);
    let unit = def.map_or("", |d| d.unit);
    let values: Vec<f64> = s["blocks"].as_array().into_iter().flatten().filter_map(|b| b["values"][metric].as_f64()).filter(|v| v.is_finite()).collect();
    let blocks = s["blocks"].as_array().map_or(0, Vec::len);
    let mut out = format!("strobe bench {file} · {} · head {} · {blocks} blocks, nothing compared\n", s["label"].as_str().unwrap_or(""), r["variants"][0]["label"].as_str().unwrap_or(""));
    out += &format!("plan   {} · warmup until steady · {} runs per block\n", r["plan"]["pin"].as_str().unwrap_or(""), r["plan"]["runs_per_block"]);
    let steps: Vec<(String, f64)> = serde_json::from_value(s["steps"].clone()).unwrap_or_default();
    out += &format!("time   {} · per block {}\n", fmt::ms_duration(s["elapsed_ms"].as_f64().unwrap_or(0.0) as u64), steps_text(&steps));
    out += &self_line(s);
    let ratio = def.is_none_or(|d| d.kind != model::Kind::Exact && d.unit != "count" && d.unit != "score") && !values.is_empty() && values.iter().all(|v| *v > 0.0);
    if stats::kind_of(&s["primary"]) == VerdictKind::Refused {
        out += &format!("\n★ {metric}  {}\n", s["primary"]["verdict"].as_str().unwrap_or("refused"));
    } else if values.len() >= 2 {
        let (lo, hi, conf) = if ratio {
            let (_, lo, hi, c) = stats::hodges_lehmann(&values.iter().map(|v| v.ln()).collect::<Vec<_>>(), 0.95);
            (lo.exp(), hi.exp(), c)
        } else {
            let (_, lo, hi, c) = stats::hodges_lehmann(&values, 0.95);
            (lo, hi, c)
        };
        out += &format!(
            "\n★ {metric}  median {}  [{} … {}] ({:.0}% over the blocks)\n",
            fmt::value(metrics::median(&values), unit),
            fmt::value(lo, unit),
            fmt::value(hi, unit),
            100.0 * conf
        );
    }
    let each = s["blocks"].as_array().into_iter().flatten().map(|b| b["values"][metric].as_f64().filter(|v| v.is_finite()).map_or("—".to_string(), |v| fmt::value(v, unit)));
    out += &format!("  blocks  {}\n", each.collect::<Vec<_>>().join(" · "));
    if s["session"] == "mixed" {
        out += "  ! some blocks ran on a locked screen and the rest unlocked: their values do not compare\n";
    }
    let warns: Vec<&Value> = r["warnings"].as_array().into_iter().flatten().chain(s["warnings"].as_array().into_iter().flatten()).collect();
    if !warns.is_empty() {
        out += "\nwarnings\n";
        for w in warns {
            out += &format!("{} {:<24}{}\n", if w["level"] == "warn" { "!" } else { "i" }, w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or(""));
        }
    }
    out + "\n"
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
        if s["kind"] == "blocks" {
            out += &blocks_text(r, &s, &file);
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
        let cache = &r["plan"]["gpu_cache"];
        if let Some(gpu) = cache[s["label"].as_str().unwrap_or("")].as_str().or(cache[s["target"].as_str().unwrap_or("")].as_str()) {
            out += &format!("gpu    shader caches {gpu}\n");
        }
        let per_block = if s["kind"] == "launch" {
            "one timed launch per block · first launch of each variant discarded".to_string()
        } else {
            format!("warmup until steady · {} runs per block", r["plan"]["runs_per_block"])
        };
        out += &format!("blocks {} pairs ({} fresh launches) · {per_block} · stopped: {}\n", s["pairs"], s["launches"], s["stopped"].as_str().unwrap_or(""));
        let steps: Vec<(String, f64)> = serde_json::from_value(s["steps"].clone()).unwrap_or_default();
        out += &format!("time   {} · per block {}\n", fmt::ms_duration(s["elapsed_ms"].as_f64().unwrap_or(0.0) as u64), steps_text(&steps));
        out += &self_line(&s);
        if let Some(line) = shown_line(&s) {
            out += &format!("shown  {line}{}\n", if s["shown"]["changed"] == true { "  ! different screens (ScreenChanged)" } else { "" });
        }
        out.push('\n');
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
        if s["primary"]["noworse"].is_number() {
            out += "  no worse: the primary's interval against its margin, not 0: \"no worse\" once its worse end is inside it, \"worse\" once its better end is beyond it; each one-sided, 2.5% overall\n";
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
        out += &format!(
            "{}{}\n",
            if st.get("code").and_then(Value::as_i64) == Some(130) { "stopped: kept what finished".to_string() } else { format!("failed: {why}; kept what finished") },
            if not_run.is_empty() { String::new() } else { format!("; not run: {}", not_run.join(", ")) }
        );
        out += &format!("resume  strobe bench --resume {id}\n");
    }
    if let Some(t) = store::ledger::line(r) {
        out += &format!("ledger  {t}\n");
    }
    // `--blocks` compares nothing, so it has no report: its blocks are in its result.json.
    if r["series"].as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|s| s["kind"] == "blocks")) {
        out += &format!("{id} · strobe query {id}\n");
    } else {
        out += &format!("{id} · strobe report experiment {id} --open\n");
    }
    out
}
