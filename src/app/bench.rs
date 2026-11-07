//! `strobe bench`: experiments with verdicts (SPEC §9). Blocks of fresh launches, alternating A B, B A.

use super::{Output, fmt, progress, stop_error, stop_requested};
use crate::cli::{Args, duration_ms};
use crate::config;
use crate::driver::Driver;
use crate::host::{self, git};
use crate::metrics;
use crate::model::{self, Error, Hidden, MetricDef, Refusal, Result, ValidatorId, VerdictKind, WebEngine};
use crate::png;
use crate::probe::{self, Set};
use crate::runner::Runner;
use crate::stats::{self, Comparison};
use crate::store;
use crate::target::chromium::{self, Window};
use crate::target::motion::{self, Motion};
use crate::target::web::{self, WebBrowser, WebPage as Page};
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
mod cli;
mod fixture;
mod journey;
mod launch;
mod mocks;
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
    "main.paint",
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
    /// Whether every run must start from the same state (`ResetIncomplete`); the case's `resetCheck`.
    reset_check: bool,
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
    /// The case's own `expect`, which each side's target's adds to (`expect` is the merge for `target`).
    case_expect: Value,
    /// An implementation comparison (SPEC §9.10): the base's and the head's targets. The series is
    /// case × params; each block runs its side's target (`side_series`).
    sides: Option<(String, String)>,
    /// A parity gap: the side that cannot run this journey (its role), why, and the adapter entry it
    /// lacks (`@find`) when that is why. Nothing runs.
    gap: Option<(&'static str, String, Option<String>)>,
    /// `steps`: the case gives each target its own run (`run: { <target>: fn }`).
    adapted: Option<String>,
    /// Refused before anything builds (`check_engines`): a primary this series' engine cannot measure, or
    /// a headless page cannot show; a launch case in an engine without one. Listed; the rest still runs.
    refused: Option<Refusal>,
    /// The case says `hero: true`: reports lead with it when it is decided.
    hero: bool,
    /// The sides run on different engines: metrics of one engine (`Across::PerSide`) are kept per side
    /// and never compared.
    cross_engine: bool,
}

impl Series {
    /// A metric this series keeps per side instead of comparing (SPEC §9.10).
    fn per_side(&self, metric: &str) -> bool {
        self.cross_engine && model::metric(metric).is_some_and(|d| d.across == model::Across::PerSide)
    }
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
        // An implementation comparison's series is the journey: its sides name the targets.
        if self.sides.is_some() {
            return self.journey();
        }
        let mut s = format!("{} · {}", self.case, self.target);
        for (k, v) in &self.params {
            s += &format!(" · {k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
        }
        s
    }

    /// The case and its params, whatever target runs it: a row of the parity matrix.
    fn journey(&self) -> String {
        let mut s = self.case.clone();
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
    /// `cmd:<line>`: a command line, for a `cli()` target (`strobe run --base … --head …`); it runs in
    /// the folder strobe was started in.
    Cmd(String),
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Dir => "dir",
            Kind::Git(_) => "git",
            Kind::Url(_) => "url",
            Kind::App(_) => "app",
            Kind::Exe(_) => "exe",
            Kind::Cmd(_) => "cmd",
        }
    }
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
    /// An implementation comparison's side: the bench file's target it runs (`--base <target>[=<variant>]`).
    target: Option<String>,
    /// What every text calls this side: its target's `name`, else the target's key; the label otherwise.
    name: String,
}

impl Variant {
    /// The target this variant's blocks run: its side's own, else the series'.
    fn target_of<'a>(&'a self, s: &'a Series) -> &'a str {
        self.target.as_deref().unwrap_or(&s.target)
    }
}

/// What a target runs on: a page's browser engine, Chromium for apps on it, or the kind. Metrics that
/// are a browser engine's own (`Across::PerSide`) compare only between sides of one engine, and two
/// pages on different engines are measured from outside as a page against a native app is: Chromium's
/// trace and another engine's OS presents are not one instrument.
fn engine(target: &Value) -> &'static str {
    match target["kind"].as_str().unwrap_or("") {
        "native" => "native",
        "script" => "script",
        "cli" => "cli",
        _ => engine_of(target).name(),
    }
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
        Kind::Cmd(l) => format!("cmd:{l}"),
    }
}

/// The experiment's identity for the rerun ledger (`store::ledger`, SPEC §14.2): its variants, the bench
/// file and harness by content, its series with their primaries and margins, and the flags that chose
/// what ran and how it was judged.
#[allow(clippy::too_many_arguments)]
fn identity(variants: &[Variant], file: &Path, harness: &Harness, args: &Args, series: &[Series], project: &Path, config: &Value, projects: &BTreeMap<String, (PathBuf, Value)>) -> Value {
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
    // `--seed` is the workload only where a hook used it (`used_random`, set after the runs): the ledger
    // compares `seed` on its own. `--video` films untimed blocks after the timed ones: no verdict moves.
    const SKIP: &[&str] = &["base", "head", "aa", "id", "json", "quiet", "verbose", "resume", "stop", "keep", "open", "progress", "max-time", "plan", "seed", "video"];
    let flags: serde_json::Map<String, Value> = args.to_json()["flags"].as_object().into_iter().flatten().filter(|(k, _)| !SKIP.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    // An implementation comparison's side is its target at a build: both are its identity, the build
    // resolved in that target's own project (its `cwd`'s repository and app profiles).
    let own = |v: &Variant| v.target.as_deref().and_then(|t| projects.get(t)).map_or((project, config), |(p, c)| (p.as_path(), c));
    let mut ids: Vec<String> = variants
        .iter()
        .map(|v| {
            let (p, c) = own(v);
            format!("{}{}", v.target.as_deref().map(|t| format!("{t}=")).unwrap_or_default(), variant_identity(v, p, c))
        })
        .collect();
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

/// The `Rerun` warning: the ledger found earlier attempts on these variants with another setup.
const RERUN: &str = "earlier experiments on these same variants ran another bench file, harness, case, primary, margin, seed or flags (the ledger names them): a verdict reached by changing the setup until one says what you hoped is not a verdict";

/// The experiment's seed: `--seed <n>`, else one drawn from the OS's random source.
pub(crate) fn seed_of(args: &Args) -> Result<u32> {
    match args.get("seed") {
        Some(s) => parse_seed(s),
        None => Ok(u32::from_str_radix(&host::paths::csprng_hex(4)?, 16).unwrap_or(0)),
    }
}

/// `--seed <n>`: a 32-bit unsigned integer.
pub(crate) fn parse_seed(s: &str) -> Result<u32> {
    s.trim().parse::<u32>().map_err(|_| Error::usage(format!("--seed: a whole number from 0 to 4294967295, not {s}")))
}

/// A block's recording keeps what its hooks did to the target (`actions.json.gz`, SPEC §14.1).
fn keep_actions(ctx: &Ctx, dir: &Path) -> Result<()> {
    let log = ctx.runner.take_actions();
    if log.is_empty() { Ok(()) } else { log.write(&dir.join(store::actions::FILE)) }
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
    // Unreachable (255): no debugging port, a connection that closed. A timeout (124) only from CDP or
    // BiDi: a call with no reply; a hook or a wait that timed out is the bench's own.
    e.validator().is_none() && (e.code == 255 || (e.code == 124 && (e.message.starts_with("cdp ") || e.message.starts_with("bidi "))))
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
/// task time (ms), all without its untimed stretches, and its span on the host clock.
#[derive(Clone, Debug)]
struct Run {
    ms: f64,
    cpu: f64,
    /// The target's CPU (% of a core) over the whole span, untimed stretches too: what other work on
    /// its CPUs is told apart from.
    cpu_whole: f64,
    task: f64,
    /// The target's processes' CPU in the run (ms, every thread): what warms up on an engine with no
    /// main-thread task counter (Firefox), as for native targets.
    work: f64,
    /// Per run, the Performance domain's other counters in `WORK_PARTS` order.
    parts: [f64; 5],
    /// CPU (% of a core) of the browser's renderer, GPU and browser processes, in `PROCESS_ROLES` order.
    roles: [f64; 3],
    t0: u64,
    t1: u64,
    /// Its span with the stretches `ctx.untimed` left out (host clock).
    timed: metrics::Timed,
    /// The run with its reset (ms): what it cost the block.
    total: f64,
    /// Drags in the run: moves strobe sent per second and the 95th percentile gap (ms).
    input: Option<(f64, f64)>,
    /// What the OS counted for the target's processes (`energy.cpu`, `cpu.instructions`, …).
    counted: probe::wakes::Counted,
    /// strobe's reads of the page inside the run and their time (ms): `driver.reads`, `driver.read_ms`.
    reads: (u64, f64),
}

/// A browser's process roles (`WebBrowser::process_roles`) and the metric each one's CPU goes to.
const PROCESS_ROLES: [(&str, &str); 3] = [("renderer", "cpu.renderer"), ("gpu", "cpu.gpu"), ("browser", "cpu.browser")];

/// Each browser process's index in `PROCESS_ROLES`, by pid; empty where the browser does not say.
fn process_roles(browser: &WebBrowser) -> BTreeMap<u32, usize> {
    browser.process_roles().into_iter().filter_map(|(pid, role)| Some((pid, PROCESS_ROLES.iter().position(|(r, _)| *r == role)?))).collect()
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
    /// The target's processes at the end of the runs, by program name: what its metrics summed over.
    #[serde(skip_serializing_if = "Option::is_none")]
    process_set: Option<BTreeMap<String, u32>>,
    /// With a fixture: whether the target's processes held a connection to it (None: not read).
    #[serde(skip_serializing_if = "Option::is_none")]
    fixture_connected: Option<bool>,
    /// The main window's client area after the runs, in device-independent pixels, and its scale.
    #[serde(skip_serializing_if = "Option::is_none")]
    client: Option<[f64; 3]>,
    /// `client` is the window's whole frame, title bar included: a native window on macOS, whose
    /// window list does not separate the title bar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    frame: bool,
    /// A page's browser as this block launched it: its version, and whether it drew in software.
    #[serde(skip_serializing_if = "Option::is_none")]
    browser: Option<BrowserSeen>,
    /// The direction (`lower`, `higher`) each `metric.<name>` the block measured is better in, as the
    /// command's lines said it (`cli()` targets).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    directions: BTreeMap<String, String>,
    /// What each `expect.same` element read (SPEC §9.10), by the name the file gives it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    same: BTreeMap<String, journey::Reads>,
    /// The input calls of the measured runs, each one's median per run (`InputMismatch`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    ops: BTreeMap<String, f64>,
    /// The input devices the block's page saw (`validity::InputSeen`): a page target's, checked to be a
    /// desktop's; an app's page (Electron, a webview, a command's browser), the machine's as it saw them.
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<validity::InputClass>,
    /// Whether the block's page matched `(prefers-reduced-motion: reduce)`, read with its input
    /// devices (`validity::InputSeen`): a target's `motion` or the OS's own setting (`admit_motion`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reduced_motion: Option<bool>,
}

/// What a page block's browser was (`Block::browser`): read once per target and headless mode, since
/// two targets of one file may run two engines (or one headed and one headless).
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
struct BrowserSeen {
    version: String,
    software: bool,
    /// Its adapter, for the block's own `SoftwareGpu` refusals (`software_state`).
    #[serde(skip)]
    gpu: Option<chromium::Gpu>,
}

/// What a page block's browser is, read once per side's target and mode (`ctx.browsers`): one file may
/// run a headless Chromium target and a headed Firefox one, and an implementation comparison's sides
/// two. Headed, a software rasterizer refuses only what it drew (`SoftwareGpu` per metric,
/// `software_state`): with `timed`, a series whose primary is a frame or GPU metric refuses the block
/// (an attribution or memory block measures no primary), and the rest say once that their CPU holds
/// the rasterization. Headless refuses nothing here (frames and GPU use are refused anyway). The first
/// one read is the experiment's GPU and browser.
fn browser_seen(ctx: &mut Ctx, s: &Series, v: &Variant, headless: bool, timed: bool, browser: &WebBrowser) -> Result<BrowserSeen> {
    let key = format!("{}{}", v.target_of(s), if headless { ":headless" } else { "" });
    let seen = match ctx.browsers.get(&key) {
        Some(b) => b.clone(),
        None => {
            let gpu = browser.gpu()?;
            if !ctx.gpu_checked {
                ctx.gpu = serde_json::to_value(&gpu)?;
                ctx.browser_version = browser.version().to_string();
                ctx.gpu_checked = true;
            }
            let b = BrowserSeen { version: browser.version().to_string(), software: gpu.software, gpu: Some(gpu) };
            ctx.browsers.insert(key, b.clone());
            b
        }
    };
    if let Some(gpu) = seen.gpu.as_ref().filter(|g| g.software && !headless) {
        if timed {
            validity::software_refusals(gpu, false, Some(&s.primary), [])?;
        }
        if !ctx.warnings.iter().any(|w| w.id == "SoftwareGpu") {
            ctx.warnings.push(validity::software_warning(gpu));
        }
    }
    Ok(seen)
}

/// A headed page block drawn by a software rasterizer (`BrowserSeen::software`): its frame and GPU
/// values go, declared ones refused `SoftwareGpu` (`validity::software_refusals`); CPU, main-thread work,
/// memory and milestones stay (`browser_seen` warned that the CPU holds the rasterization).
fn software_state(ctx: &Ctx, s: &Series, mut b: Block) -> Block {
    let Some(gpu) = b.browser.as_ref().filter(|br| br.software && !headless_of(ctx, s)).and_then(|br| br.gpu.clone()) else { return b };
    b.values.retain(|k, _| !validity::software_drawn(k));
    b.frames.clear();
    b.intervals.clear();
    b.phases.clear();
    for (m, r) in validity::software_refusals(&gpu, false, None, declared_metrics(s).into_iter().map(String::as_str)).unwrap_or_default() {
        b.refused.insert(m, r);
    }
    b
}

/// The browsers a series' blocks ran, for its result and track's fingerprint (`a / b` when the sides
/// of an implementation comparison ran two).
fn series_browser(blocks: &[Block]) -> Value {
    let mut seen: Vec<&str> = Vec::new();
    for b in blocks.iter().filter_map(|b| b.browser.as_ref()) {
        if !seen.contains(&b.version.as_str()) {
            seen.push(&b.version);
        }
    }
    if seen.is_empty() { Value::Null } else { json!(seen.join(" / ")) }
}

/// The definition `name` compares under in these blocks: a `metric.<name>` is better in the direction
/// its lines gave, and blocks (a variant's lines, or a run's) that disagree refuse it (`NoMetric`).
fn def_in(name: &str, blocks: &[Block]) -> std::result::Result<Option<&'static MetricDef>, Refusal> {
    if !name.starts_with("metric.") {
        return Ok(model::metric(name));
    }
    let dirs: std::collections::BTreeSet<&str> = blocks.iter().filter_map(|b| b.directions.get(name).map(String::as_str)).collect();
    match dirs.into_iter().collect::<Vec<_>>().as_slice() {
        [] | ["lower"] => Ok(model::metric(name)),
        ["higher"] => Ok(Some(&model::METRIC_HIGHER)),
        _ => Err(Refusal::new(ValidatorId::NoMetric, format!("{name}: base's and head's lines disagree on its direction (lower and higher)")).fix("print one direction for a name on both sides")),
    }
}

/// A window's client area in device-independent pixels, and its scale.
fn client_of(w: &host::TopWindow) -> [f64; 3] {
    let sc = w.scale();
    [(w.client.0 as f64 / sc).round(), (w.client.1 as f64 / sc).round(), sc]
}

/// Every target the series runs (both sides') opens no window: a script, a command line, or an app
/// with `window: false` (on the target or its profile).
fn series_windowless(ctx: &Ctx, s: &Series) -> bool {
    let names: Vec<&str> = match &s.sides {
        Some((b, h)) => vec![b.as_str(), h.as_str()],
        None => vec![s.target.as_str()],
    };
    names.iter().all(|n| {
        let t = &ctx.describe["targets"][*n];
        matches!(t["kind"].as_str(), Some("script" | "cli")) || t["window"] == false || profile_of(t).and_then(|id| isolate::app(id, &ctx.config).ok()).is_some_and(|a| a.windowless)
    })
}

/// How a note names a side: an implementation's own name in an implementation comparison, else its
/// role (`base`, `head`), which reports word as `before` and `this PR`.
fn side_of<'a>(ctx: &Ctx, v: &'a Variant) -> &'a str {
    if ctx.implementations { &v.name } else { v.role }
}

/// The tallest title bar a macOS window's frame may add to its content (a unified toolbar's is taller
/// and counts as content): what a native frame on macOS may exceed a page's height by.
const TITLE_BAR_PT: f64 = 32.0;

/// Every block of a run case (a `--check`, one target's, either side's): a target's declared viewport
/// must be what the block drew in, within 2 device-independent pixels (a native window's frame on
/// macOS, which strobe sizes to it). In an implementation comparison also `VariantMismatch` when the
/// sides showed different windows (SPEC §9.10): widths over 2 DIP apart, heights too (or, a macOS
/// native frame against a page, outside the title bar's 0 to 32 points), or another scale. A
/// difference there is not the implementation. Each refusal names the side.
fn admit_window(ctx: &mut Ctx, s: &Series, v: &Variant, b: &Block) -> Result<()> {
    let target = &ctx.describe["targets"][v.target_of(s)];
    let windowed = !matches!(target["kind"].as_str(), Some("script" | "cli")) && target["window"] != false;
    let asked = target["viewport"].as_object().map(|vp| (vp.get("width").and_then(Value::as_f64).unwrap_or(0.0), vp.get("height").and_then(Value::as_f64).unwrap_or(0.0)));
    // A launch is timed in the window the app opens (never resized): its sides are held to each other.
    match (asked.filter(|_| windowed && !s.launch.is_object()), b.client) {
        (Some((w, h)), Some(c)) if (c[0] - w).abs() > 2.0 || (c[1] - h).abs() > 2.0 => {
            let what = if b.frame { "window (its frame, title bar included)" } else { "page" };
            return Err(Error::refused(
                "VariantMismatch",
                format!("{}'s {what} was {}×{}, not the viewport {w}×{h} its target asks for", v.name, c[0], c[1]),
                "drop viewport from the targets (each app's own size is used then), or let the app take the size",
            ));
        }
        (Some((w, h)), None) => {
            return Err(Error::refused(
                "VariantMismatch",
                format!("strobe measured no window for {}, so it cannot hold it to the viewport {w}×{h} its target asks for", v.name),
                "drop viewport from the targets: each app opens at its own size",
            ));
        }
        _ => {}
    }
    if !ctx.implementations {
        return Ok(());
    }
    let Some(mine) = b.client else { return Ok(()) };
    let other = if v.role == "base" { "head" } else { "base" };
    if let Some((theirs, their_frame)) = ctx.windows.get(&format!("{}\n{other}", s.label())).copied() {
        let tall = match (b.frame, their_frame) {
            (true, false) => mine[1] - theirs[1],
            (false, true) => theirs[1] - mine[1],
            _ => 0.0,
        };
        let height_ok = if b.frame != their_frame { (-2.0..=TITLE_BAR_PT + 2.0).contains(&tall) } else { (mine[1] - theirs[1]).abs() <= 2.0 };
        if (mine[0] - theirs[0]).abs() > 2.0 || !height_ok || (mine[2] - theirs[2]).abs() > 0.01 {
            let shown = |c: [f64; 3], frame: bool| format!("{}×{}{} at scale {}", c[0], c[1], if frame { " (its frame, title bar included)" } else { "" }, c[2]);
            return Err(Error::refused(
                "VariantMismatch",
                format!("{}'s window showed {}, the other side's {}", v.name, shown(mine, b.frame), shown(theirs, their_frame)),
                "give both targets the same viewport (and let both apps take it): an implementation comparison varies the app, not its window",
            ));
        }
    }
    ctx.windows.insert(format!("{}\n{}", s.label(), v.role), (mine, b.frame));
    Ok(())
}

/// `VariantMismatch` when base's and head's pages saw other input devices (`Block.input`): a page
/// target's is always a desktop's, an app's page the machine's, so a page against an app on a
/// touch-screen laptop (or one without a mouse) compares two workloads, not two implementations.
fn admit_input(ctx: &mut Ctx, s: &Series, v: &Variant, b: &Block) -> Result<()> {
    let Some(mine) = b.input else { return Ok(()) };
    let other = if v.role == "base" { "head" } else { "base" };
    if let Some(theirs) = ctx.inputs.get(&format!("{}\n{other}", s.label())).copied()
        && theirs != mine
    {
        return Err(Error::refused(
            "VariantMismatch",
            format!("{}'s page saw {} input, the other side's {}", v.name, mine.name(), theirs.name()),
            "a page target always sees a desktop's input and an app's page the machine's: compare them on a machine with a mouse and no touch screen",
        ));
    }
    ctx.inputs.insert(format!("{}\n{}", s.label(), v.role), mine);
    Ok(())
}

/// `VariantMismatch` when base's and head's pages saw other motion (`Block.reduced_motion`): one took
/// its reduced-motion path and the other its animations (an app's page that keeps the OS's setting
/// against a page target with `motion`, or the other way round), two workloads. A native side has no page
/// to read: its motion is the target's declaration.
fn admit_motion(ctx: &mut Ctx, s: &Series, v: &Variant, b: &Block) -> Result<()> {
    let Some(mine) = b.reduced_motion else { return Ok(()) };
    let other = if v.role == "base" { "head" } else { "base" };
    let said = |r: bool| if r { "matched (prefers-reduced-motion: reduce)" } else { "did not match (prefers-reduced-motion: reduce)" };
    if let Some(theirs) = ctx.motions.get(&format!("{}\n{other}", s.label())).copied()
        && theirs != mine
    {
        return Err(Error::refused(
            "VariantMismatch",
            format!("{}'s page {}, the other side's {}", v.name, said(mine), said(theirs)),
            "give both targets the same motion (motion: \"reduce\" on both, or on neither on an OS that does not reduce motion)",
        ));
    }
    ctx.motions.insert(format!("{}\n{}", s.label(), v.role), mine);
    Ok(())
}

/// Program names and counts of a target's processes.
fn programs(procs: &[host::ProcTimes]) -> BTreeMap<String, u32> {
    let mut by = BTreeMap::new();
    for p in procs {
        *by.entry(p.name.clone()).or_insert(0) += 1;
    }
    by
}

/// A side's process set over its timed blocks: the median count and the last block's programs.
fn process_set_of(series: &[Value], role: &str) -> Value {
    let sets: Vec<BTreeMap<String, u32>> = series
        .iter()
        .flat_map(|s| s["blocks"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["variant"] == role)
        .filter_map(|b| serde_json::from_value(b["process_set"].clone()).ok())
        .collect();
    match sets.last() {
        Some(last) => json!({ "median_count": metrics::median(&sets.iter().map(|s| s.values().sum::<u32>() as f64).collect::<Vec<_>>()), "by_program": last }),
        None => Value::Null,
    }
}

/// What both sides of an implementation comparison shared, as measured (SPEC §9.10).
fn fairness(ctx: &Ctx, variants: &[Variant], series: &[Value]) -> Value {
    let w = ctx.used.as_ref().unwrap_or(&ctx.window);
    let frames = series.iter().flat_map(|s| s["blocks"].as_array().cloned().unwrap_or_default()).any(|b| b["values"]["frames.presented.count"].is_number());
    let source = if cfg!(windows) {
        "ETW present events"
    } else if cfg!(target_os = "macos") {
        "ScreenCaptureKit screen times"
    } else {
        "X11 Present"
    };
    let observer = |v: Option<&Variant>| match v.and_then(|v| v.target.as_deref()).map(|t| engine(&ctx.describe["targets"][t])) {
        Some("chromium") => "devtools",
        Some("firefox" | "webkit") => "webdriver",
        Some("native") => "accessibility",
        _ => "none",
    };
    // What the sides drew in, as measured (`Block.client`, which `admit_window` held equal on both),
    // only when both sides were measured: a side strobe could not measure leaves it unknown.
    // A macOS native side's size is its window's frame (title bar included): the content side's is
    // given when there is one, and `frame` names the sides measured by their frame.
    let measured = |role: &str| {
        series
            .iter()
            .flat_map(|s| s["blocks"].as_array().cloned().unwrap_or_default())
            .filter(|b| b["variant"] == role)
            .find_map(|b| serde_json::from_value::<[f64; 3]>(b["client"].clone()).ok().map(|c| (c, b["frame"] == true)))
    };
    let (client, scale, frame) = match (measured("base"), measured("head")) {
        (Some(b), Some(h)) => {
            let (c, _) = if b.1 && !h.1 { h } else { b };
            let framed: Vec<&str> = [("base", b.1), ("head", h.1)].into_iter().filter(|x| x.1).map(|x| x.0).collect();
            (json!([c[0], c[1]]), json!(c[2]), json!(framed))
        }
        _ => (Value::Null, Value::Null, json!([])),
    };
    let gpu_cache = ctx.gpu_cache.values().next().map(|n| n.split(':').next().unwrap_or("").to_string());
    let some = |v: &Value| v["ops"].as_object().is_some_and(|o| !o.is_empty());
    let gave_input = series.iter().any(|s| some(s) || s["blocks"].as_array().into_iter().flatten().any(some));
    json!({
        "window": { "client_dip": client, "scale": scale, "frame": frame, "display": Value::Null, "hz": host::refresh_at(w.x + 20, w.y + 20) },
        "adapter": ctx.gpu["devices"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null),
        // Pages against a native implementation get OS input, as native apps always do; two pages DevTools
        // input; runs that gave no input (scripts, commands, journeys without steps) none.
        "input": if !gave_input { "none" } else if ctx.cross_engine || variants.iter().all(|v| observer(Some(v)) == "accessibility") { "os" } else { "devtools" },
        "frames": frames.then_some(source),
        "gpu_cache": gpu_cache,
        "priority": ctx.qos.clone().unwrap_or_else(|| "normal".into()),
        "observer": { "base": observer(variants.iter().find(|v| v.role == "base")), "head": observer(variants.iter().find(|v| v.role == "head")), "reads_per_run": journey::reads_per_run(series) },
    })
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
    /// `--video` (SPEC §8.8): the block films its runs, so nothing it measures is timed: a `--check`
    /// block, or the one block per variant after a comparison's timed pairs, never in them.
    video: bool,
}

impl Exec {
    fn timed(set: Set, runs: usize, keep: bool) -> Exec {
        Exec { set, runs, untimed: false, keep, pixels: None, counters: false, video: false }
    }

    fn untimed(keep: bool, pixels: Option<PathBuf>) -> Exec {
        Exec { set: Set::Lean, runs: 1, untimed: true, keep, pixels, counters: false, video: false }
    }

    /// This block as a counter block, or as it is.
    fn counting(self, counters: bool) -> Exec {
        Exec { counters, ..self }
    }

    /// The video block a comparison runs per variant after its timed pairs: warmed, one measured run,
    /// kept, its numbers in no verdict.
    fn filming() -> Exec {
        Exec { video: true, ..Exec::timed(Set::Lean, 1, true) }
    }

    /// A timing block whose numbers become a verdict: what `--reserve-cpus auto` reserves for.
    fn measures_time(&self) -> bool {
        !self.untimed && !self.video && matches!(self.set, Set::Lean | Set::Timing)
    }
}

/// Whether a recording kept a video (`video.json`).
fn has_video(rec: &str) -> bool {
    store::resolve(rec).is_ok_and(|d| d.join("video.json").is_file())
}

/// Why the target `name` (one side's, in an implementation comparison) gets no video with `--video`
/// (it is said, the bench goes on): it opens no window, or it is a headless page in an engine without
/// a screencast.
fn no_video(ctx: &Ctx, name: &str) -> Option<String> {
    let t = &ctx.describe["targets"][name];
    let windowless = matches!(t["kind"].as_str(), Some("script" | "cli")) || t["window"] == false || profile_of(t).and_then(|id| isolate::app(id, &ctx.config).ok()).is_some_and(|a| a.windowless);
    if windowless {
        return Some(format!("{name} opens no window (a script, a command, or window: false)"));
    }
    let engine = engine_of(t);
    if (ctx.headless || t["headless"] == true) && engine != WebEngine::Chromium && !is_native(t) {
        return Some(format!("{name} is a headless {engine} page: no window to film, and stock {engine} has no screencast (Playwright films only its own patched builds)"));
    }
    None
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

/// Takes out of a block's `values` every metric this machine cannot read (`outside::not_on_this_os`:
/// cycles that are not time, what the OS has no source for) and refuses the declared ones, before
/// anything (meta.json, the result) is written: no number made from a clock strobe does not have.
fn refuse_not_on_this_os(s: &Series, values: &mut BTreeMap<String, f64>, refused: &mut BTreeMap<String, Refusal>) {
    let declared = declared_metrics(s);
    let names: Vec<String> = values.keys().cloned().chain(declared.iter().map(|m| m.to_string())).collect();
    for m in names {
        if let Some(r) = super::outside::not_on_this_os(&m) {
            values.remove(&m);
            if declared.iter().any(|d| **d == m) {
                refused.insert(m, r);
            }
        }
    }
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
    /// The experiment's seed (`--seed`, or drawn): every case's `ctx.seed` comes from it.
    seed: u32,
    describe: Value,
    runner: Runner,
    window: Window,
    /// The window the last block actually used (a target's viewport overrides the default).
    used: Option<Window>,
    pin: Option<u64>,
    /// What `--pin` picked and why (`pinning`): the plan's line, and why an unpinned target reserves nothing.
    pin_label: String,
    gpu_checked: bool,
    gpu: Value,
    /// Each page target's browser by target and headless mode (`BrowserSeen`), checked at its first block.
    browsers: BTreeMap<String, BrowserSeen>,
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
    /// Base and head run different targets of the bench file (SPEC §9.10).
    implementations: bool,
    /// The journeys an implementation comparison compares (its series without a parity gap): the
    /// family a decided primary runs on for (`stats::family_min_pairs`, SPEC §9.3).
    journeys: usize,
    /// ... and on different engines: one engine's own metrics are not compared, and Chromium sides are
    /// measured from outside as a native app is (`outside_in`).
    cross_engine: bool,
    /// The bench file's `fixture()`: started for every block, outside the target's process set.
    fixture: Option<fixture::Spec>,
    /// Targets with a `cwd` of their own: that project's root and strobe.json, for their builds and app
    /// profiles (`as_side`).
    projects: BTreeMap<String, (PathBuf, Value)>,
    /// The fixture of the block that runs now (`any_block`), for its hooks.
    fixture_running: Option<std::rc::Rc<fixture::Running>>,
    /// Per series and side, the window its first block showed (`admit_window`).
    windows: BTreeMap<String, ([f64; 3], bool)>,
    /// Per series and side, the input devices its first page block saw (`admit_input`).
    inputs: BTreeMap<String, validity::InputClass>,
    /// Per series and side, whether its first page block matched reduced motion (`admit_motion`).
    motions: BTreeMap<String, bool>,
    /// The sides whose first launch has run: a later launch notes what appears in the user's
    /// Documents or Desktop (`folder_notes`) instead of stopping (SPEC §13.2).
    folders_first: std::collections::BTreeSet<String>,
    folder_notes: isolate::Notes,
    /// The CPUs the target is not pinned to (the runner's): a fixture runs there.
    runner_pin: Option<u64>,
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

/// A series' untimed stretches (SPEC §9.6), from its blocks' `untimed.n` and `untimed.ms` (each the
/// median per measured run): `{ stretches, ms }` per run, the median over the blocks; null when no
/// run left one out.
fn untimed_of(blocks: &[(Option<f64>, Option<f64>)]) -> Value {
    let n: Vec<f64> = blocks.iter().filter_map(|b| b.0).collect();
    let ms: Vec<f64> = blocks.iter().filter_map(|b| b.1).collect();
    if n.is_empty() {
        return Value::Null;
    }
    json!({ "stretches": metrics::median(&n), "ms": metrics::median(&ms) })
}

/// `2 stretches a run, 603 ms of each run left out`: a series' `untimed`.
fn untimed_line(u: &Value) -> Option<String> {
    let (n, ms) = (u["stretches"].as_f64()?, u["ms"].as_f64().unwrap_or(f64::NAN));
    Some(format!("{n:.0} stretch{} a run, {} of each run left out (ctx.untimed)", if n == 1.0 { "" } else { "es" }, fmt::value(ms, "ms")))
}

/// `untimed.n` and `untimed.ms` of a block (SPEC §9.6): its measured runs' untimed stretches and the
/// time they left out, each the median per run; none when no run left one out.
fn untimed_values(runs: &[&metrics::Timed], values: &mut BTreeMap<String, f64>) {
    if runs.iter().all(|r| r.untimed.is_empty()) {
        return;
    }
    values.insert("untimed.n".into(), metrics::median(&runs.iter().map(|r| r.untimed.len() as f64).collect::<Vec<_>>()));
    values.insert("untimed.ms".into(), metrics::median(&runs.iter().map(|r| r.untimed_in(r.t0, r.t1) as f64 / 1e6).collect::<Vec<_>>()));
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
/// processes, snapshots, resets) is never counted, and the share is over the measured runs' time;
/// a run's `ctx.untimed` stretches pause it (`pause`, `resume`). A `cli()` run counts only while its
/// commands run (`begin`, then `resume` at each spawn and `pause` at its exit): its measured values
/// are the commands', and strobe's launch and accounting of each sit outside them. Each read is of
/// the one process (`host::process_cycles`), not a list of the system's.
struct SelfCost {
    runner: u32,
    /// The readings when the run (or its stretch after a pause) began: the cycle clock, strobe's and
    /// the runner's cycles.
    open: Cell<Option<RunCost>>,
    /// The run so far, over its stretches before a pause.
    sum: Cell<Option<RunCost>>,
    runs: RefCell<Vec<RunCost>>,
}

impl SelfCost {
    fn new(runner: u32) -> SelfCost {
        SelfCost { runner, open: Cell::new(None), sum: Cell::new(None), runs: RefCell::new(Vec::new()) }
    }

    /// A run begins now.
    fn start(&self) {
        self.begin();
        self.resume();
    }

    /// A run begins now, counting nothing until `resume`.
    fn begin(&self) {
        self.open.set(None);
        self.sum.set(Some((0, Some(0), Some(0))));
    }

    /// A run's untimed stretch ends now: the run counts again. strobe's own process is read last here
    /// and first in `pause`, so reading the runner is not in its count.
    fn resume(&self) {
        let runner = host::process_cycles(self.runner);
        let tsc = host::tsc();
        let strobe = host::process_cycles(std::process::id());
        self.open.set(tsc.map(|t| (t, strobe, runner)));
    }

    /// A run's untimed stretch begins now: what the run used so far is kept, and nothing until `resume`.
    /// Paused already (or no cycle clock, so nothing opened): nothing to add.
    fn pause(&self) {
        let (Some(sum), Some((t0, s0, r0))) = (self.sum.get(), self.open.take()) else { return };
        let strobe = host::process_cycles(std::process::id());
        let tsc = host::tsc();
        let runner = host::process_cycles(self.runner);
        let span = tsc.map_or(0, |t| t.saturating_sub(t0));
        let delta = |a: Option<u64>, b: Option<u64>, so_far: Option<u64>| a.zip(b).filter(|(a, b)| b >= a).and_then(|(a, b)| Some(so_far? + b - a));
        self.sum.set(Some((sum.0 + span, delta(s0, strobe, sum.1), delta(r0, runner, sum.2))));
    }

    /// The run that began ended now. A run that counted no time (all of it untimed) adds nothing to
    /// the share; with no cycle clock no run counts any, and the share is None.
    fn end(&self) {
        self.pause();
        let run = self.sum.take().map_or((0, None, None), |s| if s.0 == 0 { (0, Some(0), Some(0)) } else { s });
        self.runs.borrow_mut().push(run);
    }

    /// strobe's and the runner's share of one core over the measured runs.
    fn share(&self, measured: std::ops::Range<usize>) -> (Option<f64>, Option<f64>) {
        let runs = self.runs.borrow();
        let runs = &runs[measured.start.min(runs.len())..measured.end.min(runs.len())];
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
    // `--video` films a check run or a comparison's untimed blocks (SPEC §8.8); these modes have none.
    if args.has("video") {
        let why = if args.has("blocks") {
            Some("--blocks N times one build and keeps no recording to film")
        } else if args.has("pixels-only") {
            Some("--pixels-only compares snapshots and runs no blocks to film")
        } else if args.get("probes") == Some("memory") {
            Some("--probes memory is a leak run: heap snapshots between runs, nothing to watch")
        } else if args.has("bisect") {
            Some("--bisect runs many builds' blocks to find a commit; film the culprit's own comparison")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(Error::usage(format!("--video: {why}: drop one of them")));
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
        _ if check && aa => return Err(Error::usage("--check runs each case once on --head, with nothing to compare; drop --aa")),
        // `--base <target> --head <target> --check`: each case once on each of two targets, checked as
        // their implementation comparison measures them (`expand`).
        _ if check && blocks.is_none() && args.get("base").is_some() => None,
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
    let seed = seed_of(args)?;
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
    progress(format!("strobe bench {} · {exp} · {} · seed {seed}", file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(), super::pr::version()));
    // The display state comes from a callback that first reports a moment after it is registered:
    // read now, the first block reads a state rather than none (a pair whose base saw none ran again).
    let _ = host::display_state();
    let busy = host::system_busy(1000);
    isolate::prune_gpu_caches();
    let runner = Runner::spawn(&file, dir.join("runner.log"), runner_pin)?;
    let mut ctx = Ctx {
        args,
        exp: exp.clone(),
        seed,
        describe: Value::Null,
        runner,
        window: Window::default(),
        used: None,
        pin,
        pin_label: pin_label.clone(),
        gpu_checked: false,
        gpu: Value::Null,
        browsers: BTreeMap::new(),
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
        implementations: false,
        journeys: 0,
        cross_engine: false,
        fixture: None,
        projects: BTreeMap::new(),
        fixture_running: None,
        windows: BTreeMap::new(),
        inputs: BTreeMap::new(),
        motions: BTreeMap::new(),
        folders_first: Default::default(),
        folder_notes: Default::default(),
        runner_pin,
    };
    // What `--resume` needs from another folder: the bench file and directory variants as full paths.
    ctx.saved["args"]["pos"] = json!([file]);
    for k in ["head", "base"] {
        if let Some(p) = args.get(k).filter(|p| Path::new(p).is_dir()).and_then(|p| std::fs::canonicalize(p).ok()) {
            ctx.saved["args"]["flags"][k] = json!(p.display().to_string().trim_start_matches(r"\\?\"));
        }
    }
    // A drawn seed too: `--resume` and the rerun line replay the same workload.
    ctx.saved["args"]["flags"]["seed"] = json!(seed.to_string());
    let _ = store::write_json(&dir.join("state.json"), &ctx.saved);
    ctx.awake.hold(false);
    ctx.describe = ctx.runner.describe()?;
    ctx.runner.seed(seed)?;
    apply_engines(&mut ctx.describe, args)?;
    if let Some(f) = args.get("filter") {
        let (text, err) = filter_report(&ctx.describe, f);
        if let Some(e) = err {
            return Err(e);
        }
        progress(text);
    }
    let (mut series, mut variants) = series_and_sides(&ctx.describe, args, base_spec.as_deref(), &head_spec, &project, &config)?;
    if series.is_empty() {
        return Err(Error::usage("no series matches --filter / --target / --params"));
    }
    ctx.projects = target_projects(&ctx.describe)?;
    ctx.implementations = variants.iter().filter_map(|v| v.target.as_deref()).collect::<std::collections::BTreeSet<_>>().len() == 2;
    check_engines(&mut series, &ctx.describe, args, &variants, &config, &ctx.projects)?;
    // A series the display cannot show refuses before anything counts it (journeys, the Holm family).
    let (describe, headless) = (ctx.describe.clone(), ctx.headless);
    display_fit(Some(&mut ctx), &mut series, &describe, headless)?;
    // A check of two targets refuses what their comparison would before anything builds.
    if let Some((a, b)) = check_pair(&ctx.describe, args, &project)? {
        let sides = [resolve_side("base", a, None, &ctx.describe, &project, &config)?, resolve_side("head", b, None, &ctx.describe, &project, &config)?];
        implementation_preflight(&ctx.describe, &series, &sides, &config, &ctx.projects)?;
    }
    ctx.journeys = series.iter().filter(|s| s.gap.is_none() && s.refused.is_none()).count();
    if ctx.implementations {
        for v in &variants {
            let t = v.target.as_deref().unwrap_or("");
            progress(format!("{:<5} {}: target {t} ({}) at {}", v.role, v.name, ctx.describe["targets"][t]["kind"].as_str().unwrap_or("?"), v.label));
        }
        implementation_preflight(&ctx.describe, &series, &variants, &config, &ctx.projects)?;
        ctx.cross_engine = {
            let kinds: Vec<&str> = variants.iter().filter_map(|v| v.target.as_deref()).map(|t| engine(&ctx.describe["targets"][t])).collect();
            kinds[0] != kinds[1]
        };
    }
    ctx.fixture = fixture::Spec::from(&ctx.describe)?;
    let mut engines: Vec<WebEngine> = Vec::new();
    for e in series.iter().filter(|s| s.refused.is_none()).map(|s| &ctx.describe["targets"][&s.target]).filter(|t| matches!(t["kind"].as_str(), Some("web" | "url"))).map(engine_of) {
        if !engines.contains(&e) {
            engines.push(e);
        }
    }
    for why in engines.into_iter().filter_map(web::patched) {
        ctx.warnings.push(validity::warn("PatchedBrowser", why));
    }
    if busy > 0.10 {
        ctx.warnings.push(validity::warn("Background", format!("{:.0}% of all CPUs were busy before the run; close heavy apps for tighter intervals", busy * 100.0)));
    }
    if env.on_battery {
        ctx.warnings.push(validity::warn("Battery", "on battery: clocks may drop mid-run; plug in"));
    }
    // The OS reduces motion, and a series asked for nothing: its pages and apps follow the OS (a native
    // app may not), so it measures their reduced-motion paths, as no person with animations on sees them.
    if env.reduced_motion == Some(true) {
        let ambient: Vec<String> = series
            .iter()
            .filter(|s| s.refused.is_none() && s.gap.is_none())
            .filter(|s| std::iter::once(&s.target).chain(s.sides.iter().flat_map(|(b, h)| [b, h])).any(|t| ctx.describe["targets"][t]["motion"].is_null()))
            .map(|s| s.label())
            .collect();
        if !ambient.is_empty() {
            ctx.warnings.push(validity::warn(
                "ReducedMotion",
                format!(
                    "the OS reduces motion (its own setting, which strobe never changes) and {} ask for none: their pages and apps follow the OS and take their reduced-motion paths, and a native side may not; turn the OS's setting off to measure animations, or give the targets motion: \"reduce\" to measure reduced motion on purpose",
                    ambient.join(", ")
                ),
            ));
        }
    }
    if let Some(why) = ctx.qos.clone() {
        // Frames and input latencies at background priority describe no person's machine: refused
        // (before anything builds when a primary is one). CPU metrics stay, with this warning.
        if let Some(s) = series.iter().find(|s| s.refused.is_none() && foreground_metric(&s.primary)).filter(|_| !check) {
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
    // Headed runs rely on a calibration that checked frames on a display; a run whose every series is
    // headless (a target's own `headless` too) or windowless accepts a headless one.
    let headed = series.iter().filter(|s| s.refused.is_none() && s.gap.is_none()).any(|s| !headless_of(&ctx, s) && !series_windowless(&ctx, s));
    ctx.warnings.extend(super::up::calibration_warning(!headed));
    preflight(&series, &ctx.describe, &variants, &config, &ctx.projects, ctx.headless)?;
    // A native app's reduced motion is what its profile declares: no page to read it back from.
    let natives: Vec<String> = series
        .iter()
        .filter(|s| s.gap.is_none() && s.refused.is_none())
        .flat_map(|s| std::iter::once(&s.target).chain(s.sides.iter().flat_map(|(b, h)| [b, h])))
        .filter(|t| ctx.describe["targets"][*t]["kind"] == "native" && ctx.describe["targets"][*t]["motion"] == "reduce")
        .map(|t| profile_of(&ctx.describe["targets"][t]).unwrap_or(t).to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for id in natives {
        ctx.warnings.push(validity::info(
            "MotionContract",
            format!("reduced motion in native app {id} is the contract its app profile declares (apps.{id}.motion.reduce: its args, env and files): no page tells strobe whether the app follows it"),
        ));
    }
    // The rerun ledger (SPEC §14.2): the other experiments on these two variants, said before anything
    // builds, so rerunning until one says "faster" shows.
    let compares = !check && !leak_run && variants.len() == 2;
    let mut ident = if compares { identity(&variants, &file, &ctx.harness, args, &series, &project, &config, &ctx.projects) } else { Value::Null };
    if compares {
        // The browser a page ran in is part of what was measured: another engine, version, a headless
        // run or other input devices is another experiment, never the same one rerun.
        // Both sides' targets in an implementation comparison.
        let pages: BTreeMap<String, Value> = series
            .iter()
            .flat_map(|s| std::iter::once(&s.target).chain(s.sides.iter().flat_map(|(b, h)| [b, h])))
            .filter(|t| matches!(ctx.describe["targets"][*t]["kind"].as_str(), Some("web" | "url")))
            .map(|t| {
                let target = &ctx.describe["targets"][t];
                // The input its page is held to (`validity::emulated_input`), and what it emulates.
                let input = match target["emulate"].as_str() {
                    Some("mobile") => validity::InputClass::Touch,
                    Some(_) => validity::InputClass::Hybrid,
                    None => validity::InputClass::Desktop,
                };
                let mut page = json!({ "engine": engine_of(target), "browser": web::browser_identity(engine_of(target), target["headless"] == true), "headless": target["headless"] == true, "input": input });
                if !target["emulate"].is_null() {
                    page["emulate"] = target["emulate"].clone();
                }
                (t.clone(), page)
            })
            .collect();
        if !pages.is_empty() {
            ident["engines"] = json!(pages);
        }
        // Reduced motion is another workload: the targets that asked for it (none: the OS's own setting).
        let reduced: BTreeMap<String, Value> = series
            .iter()
            .flat_map(|s| std::iter::once(&s.target).chain(s.sides.iter().flat_map(|(b, h)| [b, h])))
            .filter(|t| ctx.describe["targets"][*t]["motion"] == "reduce")
            .map(|t| (t.clone(), json!("reduce")))
            .collect();
        ident["motion"] = json!(reduced);
        // And what the targets without it followed: the OS's own setting.
        ident["os_reduced_motion"] = json!(env.reduced_motion);
        ident["seed"] = json!(seed);
    }
    let ledger_at = super::record::now_iso();
    let mut ledger = store::ledger::for_experiment(&exp, &ledger_at, &ident);
    if let Some(t) = ledger["text"].as_str() {
        progress(format!("ledger  {t}"));
    }
    if ledger["changed"] == true {
        ctx.warnings.push(validity::warn("Rerun", RERUN));
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
    // Series `--max-time` left no time to start.
    let mut out_of_time: Vec<String> = Vec::new();
    for (i, s) in series.iter().enumerate() {
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
        if s.gap.is_some() {
            let g = gap_series(s, &variants, &ctx.describe);
            progress(format!("\n{}  {}: {}", s.label(), if check { "gap" } else { "not compared" }, g["primary"]["refusal"]["summary"].as_str().unwrap_or("")));
            series_out.push(g);
            continue;
        }
        if let Some(r) = &s.refused {
            progress(format!("\n{}  refused: {}", s.label(), refused_text(&json!(r))));
            series_out.push(refused_series(&ctx, s, r));
            continue;
        }
        let r = match mode {
            Mode::Check => check_series(&mut ctx, s, &mut variants, &config),
            Mode::Pixels => (|| -> Result<Value> {
                release_machine(&mut ctx);
                ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
                prepare_all(&mut ctx, s, &mut variants, &config)?;
                let pixels = pixels_series(&mut ctx, s, &variants)?;
                Ok(json!({ "label": s.label(), "case": s.case, "kind": "pixels", "pixels": pixels, "decided": pixels.as_array().is_some_and(|a| a.iter().all(|p| p["state"] == "same")) }))
            })(),
            Mode::Leak => leak_series(&mut ctx, s, &mut variants, &config),
            Mode::Blocks(n) => blocks_series(&mut ctx, s, &mut variants, &config, n),
            // `--max-time` ran out: no series starts after it.
            Mode::Compare if max_time.is_some_and(|t| started.elapsed() > t) => {
                out_of_time.push(s.label());
                continue;
            }
            Mode::Compare => {
                let remaining = series[i..].iter().filter(|s| s.gap.is_none() && s.refused.is_none() && !ctx.saved["series"][s.label()]["done"].is_object()).count();
                run_series(&mut ctx, s, &mut variants, &config, started, max_time, remaining)
            }
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
    if !out_of_time.is_empty() {
        all_decided = false;
        ctx.warnings.push(validity::warn("MaxTime", format!("--max-time ran out before these series started; not run: {}", out_of_time.join(", "))));
    }
    for text in variants.iter().flat_map(|v| v.changed.iter()) {
        ctx.warnings.push(validity::warn("VariantChanged", text.clone()));
    }
    // Now it is known whether a hook used the seed: only then is another `--seed` another workload.
    // A resumed bench keeps what the runs before the resume found (`checkpoint`).
    let used_random = ctx.runner.used_random() || ctx.saved["used_random"] == true;
    if ident.is_object() {
        ident["used_random"] = json!(used_random);
        ledger = store::ledger::for_experiment(&exp, &ledger_at, &ident);
        if ledger["changed"] == true && !ctx.warnings.iter().any(|w| w.id == "Rerun") {
            ctx.warnings.push(validity::warn("Rerun", RERUN));
        }
    }
    if let Some(h) = halted.as_ref().filter(|_| check || pixels_only) {
        ctx.warnings.push(validity::warn("Stopped", format!("{}; not run: {}", h.message, not_run.join(", "))));
    }
    if check {
        let mut series_out = series_out;
        let matrix = journey::matrix(&mut series_out);
        return check_output(&exp, &dir, &file, series_out, ctx.warnings, matrix, args.has("require-parity"), (seed, used_random));
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
        let result = json!({ "id": exp, "kind": "pixels", "file": file, "created": super::record::now_iso(), "plan": { "seed": seed, "used_random": used_random }, "series": series_out, "warnings": ctx.warnings, "text": text });
        store::write_json(&dir.join("result.json"), &result)?;
        let mut out = Output::new(text, result);
        if !all_decided {
            out.code = 1;
        }
        return Ok(out);
    }
    let refresh = host::refresh_at(ctx.window.x + 20, ctx.window.y + 20);
    let series_out_gaps = series_out.iter().filter(|s| s["kind"] == "gap").count();
    let series_out_refused = series_out.iter().filter(|s| s["kind"] == "refused").count();
    let result_json = json!({
        "id": exp, "kind": "experiment", "file": file, "created": super::record::now_iso(),
        // A refresh only where some series drew to a display (a headless page has none).
        "env": env, "browser": ctx.browser_version, "gpu": ctx.gpu, "window": ctx.used.as_ref().unwrap_or(&ctx.window),
        "refresh_hz": refresh.filter(|_| series_out.iter().any(|s| s["headless"] != true)),
        "plan": {
            "pin": pin_label, "runs_per_block": ctx.runs, "aa": aa, "reserve": ctx.reserved, "reserve_mode": ctx.reserve, "gpu_cache": ctx.gpu_cache,
            "looks": if args.get("pairs").is_none_or(|p| p == "auto") { LOOKS.to_vec() } else { vec![args.num("pairs")?.unwrap_or(0.0) as usize] },
            "alpha": if args.get("pairs").is_none_or(|p| p == "auto") { POCOCK_ALPHA } else { 0.05 },
            "estimate": estimate,
            "seed": seed, "used_random": used_random,
        },
        "file_repo": super::pr::repo_relative(&file.display().to_string()), "identity": ident, "ledger": ledger,
        "comparison": if ctx.implementations { "implementations" } else { "builds" },
        "variants": variants.iter().map(|v| {
            let kind = v.target.as_deref().map(|t| ctx.describe["targets"][t]["kind"].clone()).unwrap_or(Value::Null);
            json!({
                "role": v.role, "name": v.name, "spec": v.spec, "label": v.label, "subject": subject(v), "commit": v.commit, "dirty": v.dirty,
                "kind": v.kind.name(),
                "target": v.target, "engine": v.target.as_deref().map(|t| engine(&ctx.describe["targets"][t])), "target_kind": kind,
                "repo": v.target.as_deref().and_then(|t| ctx.projects.get(t)).map(|(p, _)| p.display().to_string()),
                "process_set": process_set_of(&series_out, v.role),
            })
        }).collect::<Vec<_>>(),
        "fixture": ctx.fixture.as_ref().map(|f| f.summary(&series_out)),
        "fairness": if ctx.implementations { fairness(&ctx, &variants, &series_out) } else { Value::Null },
        "scoreboard": Value::Null,
        // The command line, for the rerun line under a PR table.
        "args": ctx.saved["args"],
        "series": series_out, "warnings": ctx.warnings, "decided": all_decided && halted.is_none(),
        "stopped": halted.as_ref().map(|h| json!({ "code": h.code, "why": h.message, "not_run": not_run })),
    });
    let mut result_json = result_json;
    // The journeys as one family (SPEC §9.3): `scoreboard`, and each primary's `p_repeated` and
    // `holm_decided`, as every report reads them.
    if ctx.implementations {
        super::family::Board::of(&result_json).write(&mut result_json);
    }
    let text = render(&result_json);
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
    // A parity gap is listed and exits 0, unless the bench guards a finished port.
    if (args.has("require-parity") || args.has("require-verdict")) && series_out_gaps > 0 {
        out.code = 4;
    }
    // A series refused before it ran is listed and exits 0, as a gap is, unless every series must decide.
    if args.has("require-verdict") && series_out_refused > 0 {
        out.code = 4;
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
    // `record --bench` (`--check --why always`) of a command: nothing profiles a process that ends
    // inside each run (SPEC §9.6).
    if target["kind"] == "cli" && ctx.args.get("why") == Some("always") {
        return Err(Error::usage(format!("{}: a cli() case has no attribution recording: its commands end inside each run (profile a long-running command with strobe record pid:<n> --probes deep)", s.case)));
    }
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    prepare_all(ctx, s, variants, config)?;
    let started = Instant::now();
    let log_from = std::fs::metadata(&ctx.runner.log).map(|m| m.len()).unwrap_or(0);
    // Kept, so its views (`query <rec> threads`, …) can follow up on what the check showed. A Linux
    // series that counts instructions checks them in an untimed counter block (SPEC §7.5).
    let split = counting(s);
    let main_counters = split == Counting::Only || (split == Counting::Separate && counted_apart(&s.primary));
    // `--video`: the check block films its run (a counter block films nothing: its times are dropped anyway).
    let film = ctx.args.has("video") && !main_counters && {
        let skip = no_video(ctx, variants[0].target_of(s));
        if let Some(why) = &skip {
            ctx.warnings.push(validity::info("Video", format!("{}: no video: {why}", s.label())));
        }
        skip.is_none()
    };
    let r = block_on_screen(ctx, s, &variants[0], 0, &Exec { video: film, ..Exec::untimed(true, None).counting(main_counters) }).and_then(|mut b| {
        if split == Counting::Separate {
            let other = block_on_screen(ctx, s, &variants[0], 0, &Exec::untimed(false, None).counting(!main_counters))?;
            take_metrics(&mut b, &other, |m| counted_apart(m) != main_counters);
        }
        Ok(b)
    });
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    // An adapter entry the target lacks, met as the case ran: a gap in the matrix, not a failure.
    if let Err(e) = &r
        && e.validator() == Some(ValidatorId::ParityGap)
    {
        return Ok(runtime_gap(ctx, s, variants, 0, e, &[]));
    }
    // What the bench file's hooks printed during this case (console.log/error go to runner.log).
    let log: Vec<String> = std::fs::read(&ctx.runner.log)
        .ok()
        .and_then(|b| b.get(log_from as usize..).map(|b| String::from_utf8_lossy(b).lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).map(str::to_string).collect()))
        .unwrap_or_default();
    let overhead = r.as_ref().ok().and_then(|b| b.overhead.clone());
    let (same, ops) = r.as_ref().map(|b| (json!(b.same), json!(b.ops))).unwrap_or_default();
    let input = r.as_ref().map(|b| input_of(std::slice::from_ref(b))).unwrap_or(Value::Null);
    let window = series_window(ctx, s, r.as_ref().map(std::slice::from_ref).unwrap_or_default());
    let (passed, error, hint, values, refused, locked, recording, screen, shown) = match r {
        Ok(b) => {
            let shown = shown::summarize(std::slice::from_ref(&b), false).0;
            (true, Value::Null, Value::Null, json!(b.values), refusal_map(&b.refused), b.locked, json!(b.recording), b.screen.unwrap_or(Value::Null), shown)
        }
        Err(e) => (false, json!(e.message), json!(e.hint), Value::Null, Value::Null, false, Value::Null, Value::Null, Value::Null),
    };
    progress(format!("  {} {}{}", if passed { "pass" } else { "FAIL" }, s.label(), error.as_str().map(|e| format!("  {e}")).unwrap_or_default()));
    // A case without a recording (it failed, or its kind keeps none: a script, a command, a launch, a
    // page measured from outside) keeps what its hooks sent beside the result. A recording took its own.
    let mut actions = Value::Null;
    let sent = ctx.runner.take_actions();
    if recording.is_null() && !sent.is_empty() {
        // A journey's label names no target: each side's file is its own.
        let name = if s.sides.is_some() { format!("{} {}", s.label(), s.target) } else { s.label() };
        let slug: String = name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect::<Vec<_>>().join("-");
        let path = store::experiment_dir(&ctx.exp)?.join(format!("{}{slug}.actions.json.gz", if passed { "" } else { "failed-" }));
        match sent.write(&path) {
            Ok(()) => actions = json!({ "path": path, "count": sent.actions.len(), "failed": !passed }),
            Err(e) => ctx.warnings.push(validity::warn("Actions", format!("{}: {}", s.label(), e.message))),
        }
    }
    // `--why always` profiles the case on head too: one attribution block, warmed like a bench's, for finding
    // where a single build spends its time before there is anything to compare.
    let (mut attribution, mut sampling) = (Value::Null, Value::Null);
    let unprofiled = no_profiler(&target, s.launch.is_object()).filter(|_| matches!(target["kind"].as_str(), Some("web" | "url")));
    if let (true, Some(no)) = (passed && ctx.args.get("why") == Some("always"), &unprofiled) {
        ctx.warnings.push(validity::info("Attribution", format!("{}: {no}: no attribution block (profile it in Chromium)", s.label())));
    }
    if passed && ctx.args.get("why") == Some("always") && unprofiled.is_none() {
        progress(format!("  attribution block ({}; not timed)", why_text(&target)));
        let v = &variants[0];
        // Warmed and run as in a bench (`--runs`, default 5): `--check`'s single cold run would start no
        // profiler, and one measured run is too few to read per-run numbers from.
        let runs = ctx.args.num("runs")?.unwrap_or(5.0).max(1.0) as usize;
        let attributed = if s.launch.is_object() {
            in_slot(ctx, &format!("{} {} attribution", s.label(), v.role), |ctx| as_side(ctx, s, v, |ctx, s| launch::attribution(ctx, s, v)))
        } else {
            block_on_screen(ctx, s, v, usize::MAX, &Exec::timed(Set::Attribution, runs, true))
        };
        match attributed {
            Ok(b) => {
                progress(format!("  {} attribution  {}", b.variant, steps_text(&b.steps)));
                let rec = b.recording.as_deref().and_then(|r| super::query::Rec::resolve(r).ok());
                sampling = json!(attribution_sampling(rec.as_ref(), &b.variant, &mut ctx.warnings));
                if target["kind"] != "script" && !is_native(&target) {
                    observer_profile(rec.as_ref(), &s.label(), &mut ctx.warnings);
                }
                attribution = json!(b.recording);
            }
            Err(e) => ctx.warnings.push(validity::warn("Attribution", format!("{} attribution block failed: {}", side_of(ctx, v), e.message))),
        }
    }
    let declared: Vec<&String> = std::iter::once(&s.primary).chain(&s.metrics).collect();
    let mut out = json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "check", "until": launch::until_of(&s.launch), "primary": { "metric": s.primary }, "declared": declared,
        "passed": passed, "decided": passed, "error": error, "hint": hint, "values": values, "refused": refused, "session_locked": locked, "headless": headless_of(ctx, s), "input": input, "elapsed_ms": elapsed_ms, "attribution": attribution, "attribution_sampling": sampling, "target_kind": target["kind"], "engine": target["engine"],
        "recording": recording, "screen": screen, "shown": shown, "overhead": overhead, "untimed": untimed_of(&[(values["untimed.n"].as_f64(), values["untimed.ms"].as_f64())]),
        "video": recording.as_str().filter(|r| film && has_video(r)).map(|r| json!({ "head": r })),
        "log": log, "log_path": ctx.runner.log, "actions": actions,
        // For the parity matrix (`journey::matrix`).
        "journey": s.journey(), "side_name": target["name"].as_str().unwrap_or(&s.target), "same": same, "ops": ops, "adapted": s.adapted,
    });
    series_setup(ctx, s, window, &mut out);
    Ok(out)
}

/// `seeded`: the experiment's seed, and whether a hook used it.
#[allow(clippy::too_many_arguments)]
fn check_output(exp: &str, dir: &Path, file: &Path, series: Vec<Value>, warnings: Vec<Warning>, matrix: Option<String>, require_parity: bool, seeded: (u32, bool)) -> Result<Output> {
    // A gap is listed, not failed; journeys whose sides differ (`journey_refusal`) fail.
    let gaps = series.iter().filter(|s| s["kind"] == "gap").count();
    // Refused before it ran (`check_engines`): listed, not failed, as a gap is; the rest of the file ran.
    let before = series.iter().filter(|s| s["kind"] == "refused").count();
    let refused = series.iter().filter(|s| s["journey_refusal"].is_object()).count();
    let failed = series.iter().filter(|s| !matches!(s["kind"].as_str(), Some("gap" | "refused")) && (s["passed"] != true || s["journey_refusal"].is_object())).count();
    let mut text = String::new();
    for s in &series {
        if s["kind"] == "gap" {
            text += &format!("gap   {}  {}\n", s["label"].as_str().unwrap_or(""), s["gap"]["reason"].as_str().unwrap_or(""));
            continue;
        }
        if s["kind"] == "refused" {
            let r = &s["primary"]["refusal"];
            text += &format!("refused  {}  {}: {}\n", s["label"].as_str().unwrap_or(""), r["validator"].as_str().unwrap_or(""), r["summary"].as_str().unwrap_or(""));
            if let Some(f) = r["fix"].as_str() {
                text += &format!("      fix: {f}\n");
            }
            continue;
        }
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
        // A percentile with fewer events than a timed block needs prints none: one run's tail is no tail.
        let shown = |name: &str| {
            s["values"][name]
                .as_f64()
                .map(|v| {
                    let def = model::metric(name);
                    let (count, what) = tail_of(name);
                    match (def.filter(|d| d.min_n > 1), s["values"][count].as_f64()) {
                        (Some(d), n) if (n.unwrap_or(0.0) as usize) < d.min_n => format!("{name} — ({} {what} < {} in 1 check run)", n.unwrap_or(0.0) as usize, d.min_n),
                        _ => format!("{name} {}", fmt::value(v, def.map_or("", |m| m.unit))),
                    }
                })
                // Which edge the input samples were timed from (SPEC §7.4).
                .map(|t| match (name, s["values"]["interaction.n"].as_f64(), s["values"]["interaction.n.press"].as_f64()) {
                    ("interaction.p50" | "interaction.p95", Some(n), Some(p)) if !t.contains('—') => format!("{t} ({n:.0} inputs, {p:.0} at a press)"),
                    ("interaction.p95", _, Some(p)) => format!("{t} ({p:.0} at a press)"),
                    _ => t,
                })
                .or_else(|| s["refused"][name].is_string().then(|| format!("{name} refused")))
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
                "pass  {}  {value}  {secs:.1} s{}{}{}{}\n",
                s["label"].as_str().unwrap_or(""),
                s["engine"].as_str().filter(|e| *e != "chromium").map(|e| format!(" · {e}")).unwrap_or_default(),
                // An emulated device's window says what it was (`390×844 @3x · emulated mobile`), and
                // reduced motion (`1400×900 · reduced motion`).
                match s["window"].as_str().filter(|_| !s["emulate"].is_null() || !s["motion"].is_null()) {
                    Some(w) => format!(" · {w}"),
                    None if s["motion"] == "reduce" => format!(" · {}", motion::LABEL),
                    None => String::new(),
                },
                if s["headless"] == true { " · headless" } else { "" },
                if s["session_locked"] == true { " · session locked" } else { "" }
            );
            if let Some(rec) = s["recording"].as_str() {
                text += &format!("      rec  {rec} · strobe query {rec} {}\n", if s["target_kind"] == "native" { "threads" } else { "summary" });
            }
            if let Some(n) = s["actions"]["count"].as_u64() {
                text += &format!("      actions  {n} sent · strobe query {exp} actions\n");
            }
            if let Some(rec) = s["video"]["head"].as_str() {
                let meta = store::resolve(rec).ok().and_then(|d| store::read_json(&d.join("meta.json")).ok()).unwrap_or(Value::Null);
                text += &format!("      {}", super::video::line(rec, &meta["video"]));
            }
            if let Some(line) = screen_line(&s["screen"]) {
                text += &format!("      screen  {line}\n");
            }
            if let Some(o) = serde_json::from_value::<Overhead>(s["overhead"].clone()).ok().map(|o| o.text()).filter(|t| !t.is_empty()) {
                text += &format!("      self  {o}\n");
            }
            if let Some(u) = untimed_line(&s["untimed"]) {
                text += &format!("      untimed  {u}\n");
            }
            let missing: Vec<&str> = s["shown"]["head"]["missing"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|m| !m.contains("before the runs")).collect();
            match shown_line(s, ("base", "head")) {
                Some(line) => text += &format!("      shown  {line}{}\n", if missing.is_empty() { String::new() } else { format!(" (not read: {})", missing.join("; ")) }),
                None if !missing.is_empty() => text += &format!("      shown  nothing read: {}\n", missing.join("; ")),
                None => {}
            }
            if let Some(p) = s["shown"]["head"]["thumb_path"].as_str() {
                text += &format!("      thumb  {p}\n");
            }
            if let Some(rec) = s["attribution"].as_str() {
                text += &format!("      why  {rec} · strobe query {rec} {}\n", why_view(&s["target_kind"]));
                if let Some(at) = sampled(&s["attribution_sampling"]) {
                    text += &format!("           JavaScript {at}\n");
                }
            }
        } else {
            text += &format!("FAIL  {}  {secs:.1} s\n      {}\n", s["label"].as_str().unwrap_or(""), s["error"].as_str().unwrap_or("").replace('\n', "\n      "));
            if let Some(h) = s["hint"].as_str() {
                text += &format!("      fix: {h}\n");
            }
            if let Some(n) = s["actions"]["count"].as_u64() {
                text += &format!("      actions  {n} sent before it failed · strobe query {exp} actions\n");
            }
        }
        for (why, names) in &reasons {
            text += &format!("      refused  {}: {why}\n", names.join(", "));
        }
        if let Some(r) = s["journey_refusal"].as_object() {
            text += &format!("      refused  this journey: {}: {}\n", r.get("validator").and_then(Value::as_str).unwrap_or(""), r.get("summary").and_then(Value::as_str).unwrap_or(""));
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
    if let Some(m) = &matrix {
        text += &format!("\nparity\n{m}");
    }
    let ran = series.len() - gaps - before;
    text += &format!(
        "\n{} of {ran} cases passed{}{}{} (one cold run each, no warmup: values include first-run work such as shader compiles; a case with a reset runs once more after it, unmeasured; nothing compared)\n",
        ran - failed,
        if refused > 0 { format!(", {refused} where the targets' journeys differ") } else { String::new() },
        if gaps > 0 { format!(", {gaps} parity gap{}", if gaps == 1 { "" } else { "s" }) } else { String::new() },
        if before > 0 { format!(", {before} refused before running") } else { String::new() }
    );
    // A failure of a seeded workload is replayed by its seed.
    if failed > 0 && seeded.1 {
        text += &format!("seed  {0} (the hooks used ctx.random() or ctx.seed): add --seed {0} to replay the same workload\n", seeded.0);
    }
    for w in &warnings {
        text += &format!("{} {:<24}{}\n", if w.level == "warn" { "!" } else { "i" }, w.id, w.text);
    }
    text += &format!("\n{exp}\n");
    let result = json!({
        "id": exp, "kind": "check", "file": file, "created": super::record::now_iso(), "plan": { "seed": seeded.0, "used_random": seeded.1 },
        "series": series, "warnings": warnings, "passed": failed == 0, "parity": matrix, "text": text,
    });
    store::write_json(&dir.join("result.json"), &result)?;
    let mut out = Output::new(text, result);
    if failed > 0 {
        out.code = 1;
    } else if gaps > 0 && require_parity {
        out.code = 4;
    }
    Ok(out)
}

/// A leak run (`--probes memory`, SPEC §8.6): one memory block per variant, nothing paired. Each gets
/// growth verdicts for its post-GC JS heap and DOM nodes, and its suspects.
fn leak_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value) -> Result<Value> {
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    prepare_all(ctx, s, variants, config)?;
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
    let mut out = json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "leak", "engine": ctx.describe["targets"][&s.target]["engine"],
        "headless": headless_of(ctx, s),
        "primary": { "metric": metric, "verdict": head["verdict"], "verdict_kind": head["verdict_kind"], "decided": head["decided"] },
        "decided": decided, "leak": leak, "runs_per_half": n, "warnings": [], "blocks": [], "attribution": {},
        "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3,
    });
    series_setup(ctx, s, series_window(ctx, s, &[]), &mut out);
    Ok(out)
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
        let skip = names.iter().any(|n| n == m) || m == s.primary || (s.primary == "cpu.process.total" && m == "cpu.app.total") || (m == "cpu.app.total" && rate_context) || s.per_side(m);
        if !skip {
            names.push(m.to_string());
        }
    }
    names
}

/// The secondaries that will be tested after these pairs: those every block has a value of.
fn tested_names(s: &Series, script: bool, blocks: &[Block], pairs: &[(usize, usize)], bound: f64) -> Vec<String> {
    let rate_context = !same(blocks, pairs, "run.duration", bound);
    secondary_names(s, script, rate_context).into_iter().filter(|n| model::metric(n).is_some() && !s.per_side(n) && blocks.iter().all(|b| b.values.get(n).is_some_and(|v| v.is_finite()))).collect()
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
    // Whether the hooks so far used the seed: a resumed bench's runner starts without knowing.
    if ctx.runner.used_random() {
        ctx.saved["used_random"] = json!(true);
    }
    if let Ok(dir) = store::experiment_dir(&ctx.exp) {
        let _ = store::write_json(&dir.join("state.json"), &ctx.saved);
    }
}

/// Warm GPU caches (`gpuCache`): discarded blocks per variant fill its store first, so no measured
/// block pays a driver compiling its shaders cold. A driver may compile in the background for
/// longer than a block (NVIDIA's Vulkan cache took two 40 s launches to fill), so they repeat
/// while the store still grew by 5% (at most 3). A launch case's discarded first launch warms too.
/// Returns the discarded blocks and their wall time; a stop sets `halted`.
fn warm_gpu(ctx: &mut Ctx, s: &Series, variants: &[Variant], halted: &mut Option<Error>) -> Result<(usize, f64)> {
    let (mut blocks, mut ms) = (0usize, 0.0f64);
    for v in variants.iter() {
        // Each side's own target (an implementation comparison's two warm separately).
        let warming = as_side(ctx, s, v, |ctx, s| {
            let t = ctx.describe["targets"][&s.target].clone();
            gpu_warming(ctx, s, &t)
        })?;
        if !warming || !ctx.launched.insert(format!("{}\n{}\ngpu", s.label(), v.role)) {
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
                    format!(
                        "{}: {}'s GPU shader caches still grew by {} in its third warm-up block: its measured blocks may still compile shaders",
                        s.label(),
                        side_of(ctx, v),
                        fmt::bytes(grew as f64)
                    ),
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
    prepare_all(ctx, s, variants, config)?;
    progress(format!("\n{}  primary {} · {n} blocks of head alone", s.label(), s.primary));
    let series_started = Instant::now();
    let waited_before = ctx.waited_ms;
    let mut blocks: Vec<Block> = serde_json::from_value(ctx.saved["series"][s.label()]["blocks"].clone()).unwrap_or_default();
    if !blocks.is_empty() {
        progress(format!("  resumed with the {} blocks it had finished", blocks.len()));
    }
    let mut halted: Option<Error> = None;
    let (warmup_blocks, warmup_ms) = if blocks.is_empty() { warm_gpu(ctx, s, variants, &mut halted)? } else { (0, 0.0) };
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
    analyse(s, def, &blocks, &[], false, ["base", "head"], &mut warnings);
    warnings.extend(observer_reads(&blocks, "head", "head", is_native(&target)));
    let (session, locked, _) = session_of(blocks.iter());
    if session == "mixed" {
        warnings.push(validity::warn("SessionLocked", format!("{locked} of {} blocks ran on a locked screen and the rest unlocked: their values do not compare", blocks.len())));
    }
    if blocks.iter().any(|b| b.display == Some(0)) {
        warnings.push(validity::info("DisplayOff", "the blocks ran with the display off (a locked session's display sleeps): compare them only with runs whose display was off too"));
    }
    let (shown, screen_warnings) = shown::summarize(&blocks, false);
    warnings.extend(screen_warnings);
    let timing_recordings: Vec<Value> = blocks.iter().filter_map(|b| b.recording.clone().map(|r| json!({ "variant": b.variant, "pair": b.pair, "recording": r }))).collect();
    let window = series_window(ctx, s, &blocks);
    let mut out = json!({
        "label": s.label(), "case": s.case, "target": s.target, "target_kind": target["kind"], "params": s.params, "kind": "blocks",
        "until": launch::until_of(&s.launch),
        "engine": target["engine"], "headless": headless_of(ctx, s), "input": input_of(&blocks), "browser": series_browser(&blocks),
        "primary": primary, "decided": false, "session": session,
        "pairs": 0, "launches": blocks.len(), "stopped": if halted.is_some() { "stopped" } else { "blocks done" },
        "warmup_blocks": warmup_blocks, "warmup_ms": warmup_ms,
        "steps": mean_steps(&blocks), "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3 - (ctx.waited_ms - waited_before), "waited_ms": ctx.waited_ms - waited_before,
        "overhead": mean_overhead(&blocks), "untimed": untimed_of(&blocks.iter().map(|b| (b.values.get("untimed.n").copied(), b.values.get("untimed.ms").copied())).collect::<Vec<_>>()),
        "blocks": blocks, "warnings": warnings, "attribution": {}, "timing_recordings": timing_recordings, "shown": shown,
        "halt": halted.map(|h| json!({ "code": h.code, "message": h.message })),
    });
    series_setup(ctx, s, window, &mut out);
    Ok(out)
}

/// `remaining`: the series still to run, this one included (its `--max-time` share).
#[allow(clippy::too_many_arguments)]
fn run_series(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value, started: Instant, max_time: Option<Duration>, remaining: usize) -> Result<Value> {
    let def = model::metric(&s.primary)
        .ok_or_else(|| Error::usage(format!("{}: unknown primary metric {} (known: {})", s.case, s.primary, model::METRICS.iter().map(|m| m.name).collect::<Vec<_>>().join(", "))))?;
    let target = ctx.describe["targets"][&s.target].clone();
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    prepare_all(ctx, s, variants, config)?;
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
    // An implementation comparison's journeys are one family (SPEC §9.3): a primary decided better or
    // worse runs on to the pairs at which its repeated p can clear Holm across them.
    let journeys = ctx.journeys;
    let family_looks = looks.clone();
    let implementations = ctx.implementations;
    let family = move |c: &Comparison| {
        if implementations && matches!(c.kind, VerdictKind::Better | VerdictKind::Worse) { stats::family_min_pairs(journeys, &family_looks) } else { 0 }
    };
    // Non-inferiority (SPEC §9.3): the same interval at the same looks, judged against the margin.
    let margin = s.noworse.get(&s.primary).copied();
    let compare_primary = |pp: &[(f64, f64)], def: &MetricDef| match margin {
        Some(m) => stats::compare_noworse(&s.primary, def, pp, m, confidence),
        None => stats::compare(&s.primary, def, pp, bound, confidence),
    };
    progress(format!("\n{}  primary {}{}", s.label(), s.primary, margin.map(|m| format!(" · no worse than {}%?", (m * 1000.0).round() / 10.0)).unwrap_or_default()));
    let series_started = Instant::now();
    // This series' deadline under `--max-time`: an equal share of the time left among the series still
    // to run (this one included); a series that ends early leaves its time to the ones after it.
    let share = max_time.map(|t| series_started + t.saturating_sub(started.elapsed()) / remaining.max(1) as u32);
    let waited_before = ctx.waited_ms;
    // A resumed experiment (`--resume`) starts from the pairs it had finished.
    let saved = &ctx.saved["series"][s.label()];
    let mut blocks: Vec<Block> = serde_json::from_value(saved["blocks"].clone()).unwrap_or_default();
    let mut pairs: Vec<(usize, usize)> = serde_json::from_value(saved["pairs"].clone()).unwrap_or_default();
    pairs.retain(|&(a, b)| a < blocks.len() && b < blocks.len());
    if !pairs.is_empty() {
        progress(format!("  resumed with the {} pairs it had finished", pairs.len()));
    }
    // Outside-in CPU (`cpu.work`) is a tested secondary where the target is measured from outside: a
    // script, a command, a native app, both sides of an implementation comparison, and a page in an
    // engine without the renderer's task time.
    let script = target["kind"] == "script" || target["kind"] == "cli" || is_native(&target) || s.sides.is_some() || engine_of(&target) != WebEngine::Chromium;
    let mut stopped = String::from("max pairs");
    let mut last: Option<Comparison> = None;
    // Why the series ended early (a stop, or an error after some pairs), as the error to report.
    let mut halted: Option<Error> = None;
    // The primary's verdict at the look that decided it, kept while pairs continue for the secondaries.
    let mut decided_at: Option<Comparison> = None;
    // Why the two sides' journeys are not one (`NotEquivalent`, `InputMismatch`), from the pair that showed it.
    let mut journey_refused: Option<Refusal> = None;
    // Discarded warm-up blocks and their wall time, for `plan` (SPEC §9.1).
    let (warmup_blocks, warmup_ms) = if pairs.is_empty() { warm_gpu(ctx, s, variants, &mut halted)? } else { (0, 0.0) };
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
                    // A side that reached for an adapter entry it lacks, which the runner's scan did not
                    // see (a name built at run time): at any pair the journey is a gap, never half run.
                    Err(e) if e.validator() == Some(ValidatorId::ParityGap) => return Ok(runtime_gap(ctx, s, variants, v, &e, &blocks)),
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
        // The two sides did not do the same journey (SPEC §9.10): nothing of the series compares.
        if s.sides.is_some()
            && let Some(r) = journey::pair_refusal(s.adapted.is_some(), (&variants[0].name, &variants[1].name), &blocks[idx[0]], &blocks[idx[1]])
        {
            progress(format!("  pair {n}: {}", r.message()));
            last = Some(compare_primary(&primary_pairs, def));
            journey_refused = Some(r);
            stopped = "journeys differ".into();
            break;
        }
        // Decided at a look, then run on to the pairs the secondaries need to clear Holm (and the
        // journeys' family, `family`).
        if let Some(c) = &decided_at {
            let need = holm_pairs(tested_names(s, script, &blocks, &pairs, bound).len()).max(family(c));
            let eta = eta(ctx, s, &blocks, &pairs, need);
            progress(format!("  pair {n:>2}  for the secondaries (the primary was decided: {}) · {eta}", c.verdict));
            // `--max-time` ends the secondaries' pairs too: the primary keeps its verdict.
            if n >= need || max_time.is_some_and(|t| started.elapsed() > t) || share.is_some_and(|d| Instant::now() > d) {
                stopped = "primary decided".into();
                break;
            }
            continue;
        }
        let look = looks.contains(&n);
        // A `metric.<name>` primary is better in the direction its lines gave (known from the blocks).
        let def = def_in(&s.primary, &blocks)?.unwrap_or(def);
        let cmp = compare_primary(&primary_pairs, def);
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
        let side = |i: usize| if ctx.implementations { variants[i].name.clone() } else { variants[i].role.to_string() };
        progress(format!(
            "  pair {:>2}  {} {:>10}  {} {:>10}  {}  [{} … {}]  {} · {eta}",
            n,
            side(0),
            fmt::value(val(&blocks[idx[0]], &s.primary), def.unit),
            side(1),
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
            let holm = holm_pairs(tested_names(s, script, &blocks, &pairs, bound).len());
            let fam = last.as_ref().map(&family).unwrap_or(0);
            let need = holm.max(fam);
            if n < need && n < max_pairs && ctx.args.get("pairs").is_none() {
                let why = if fam > holm { format!("its verdict can clear Holm across the {journeys} journeys") } else { "the secondaries can clear Holm".to_string() };
                progress(format!("  the primary is decided; {} more pair{} so {why} ({need} pairs)", need - n, if need - n == 1 { "" } else { "s" }));
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
        // `--max-time`: the experiment's deadline, and from the first look this series' share of what
        // was left when it began, so the first series cannot spend the others' time.
        if max_time.is_some_and(|t| started.elapsed() > t) || (n >= min_pairs && share.is_some_and(|d| Instant::now() > d)) {
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
    let def = def_in(&s.primary, &blocks)?.unwrap_or(def);
    // Pairs a resumed series had finished: the comparison at the last of them.
    if last.is_none() && !pairs.is_empty() {
        let pp: Vec<(f64, f64)> = pairs.iter().map(|&(a, b)| (val(&blocks[a], &s.primary), val(&blocks[b], &s.primary))).collect();
        last = Some(compare_primary(&pp, def));
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
    // Tail percentiles need enough frames in every block: a primary refused so decides nothing (its
    // secondaries stay untested, no attribution, `--require-verdict` exits 4).
    let primary = tail_check(def, &blocks, primary);
    let primary_decided = primary_decided && primary.decided;
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
        // One engine's own measure, declared in an implementation comparison: a row that says why
        // it is not compared (its values are in per_side).
        if s.per_side(name) {
            secondaries.push(refused(name, d, &not_comparable(name, d)));
            continue;
        }
        let (blocks, pairs) = source(name);
        if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
            // A metric the series declared that a block refused shows as refused, not as missing.
            if let Some(why) = blocks.iter().find_map(|b| b.refused.get(name)) {
                secondaries.push(refused(name, d, why));
            }
            continue;
        }
        let d = match def_in(name, blocks) {
            Ok(dd) => dd.unwrap_or(d),
            Err(why) => {
                secondaries.push(refused(name, d, &why));
                continue;
            }
        };
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
    // A launch's milestones and CPU, a process's CPU, and a command's launch, are context like the fixed list.
    let found: Vec<String> = blocks
        .first()
        .map(|b| b.values.keys().filter(|k| k.starts_with("launch.") || k.starts_with("metric.") || *k == "memory.peak" || *k == "cpu.work" || *k == "cli.launch" || (*k == "cpu.app.total" && rate_context)).cloned().collect())
        .unwrap_or_default();
    for name in DESCRIPTIVE.iter().map(|n| n.to_string()).chain(found) {
        let name = name.as_str();
        if name == s.primary || names.iter().any(|n| n == name) || s.per_side(name) {
            continue;
        }
        let Some(d) = model::metric(name) else { continue };
        if !blocks.iter().all(|b| b.values.get(name).is_some_and(|v| v.is_finite())) {
            continue;
        }
        let d = match def_in(name, &blocks) {
            Ok(dd) => dd.unwrap_or(d),
            Err(why) => {
                descriptive.push(refused(name, d, &why));
                continue;
            }
        };
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
    let names = match &*variants {
        [b, h] if ctx.implementations => [b.name.as_str(), h.name.as_str()],
        _ => ["base", "head"],
    };
    analyse(s, def, &blocks, &pairs, primary.relative, names, &mut warnings);
    let (shown, screen_warnings) = shown::summarize(&blocks, ctx.implementations);
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
    if let Some(why) = &journey_refused {
        primary.refuse(why.clone());
        primary_decided = false;
        for c in secondaries.iter_mut().chain(descriptive.iter_mut()) {
            c.refuse(why.clone());
        }
    }
    // Latency samples taken by one rule on both sides, but in numbers too far apart to be the same inputs.
    if s.sides.is_some() {
        let names = (variants[0].name.as_str(), variants[1].name.as_str());
        for (prefix, why) in journey::latency_skew(&blocks, names) {
            for c in std::iter::once(&mut primary).chain(secondaries.iter_mut()).chain(descriptive.iter_mut()).filter(|c| c.metric.starts_with(prefix) && c.kind != VerdictKind::Refused) {
                c.refuse(why.clone());
            }
            if s.primary.starts_with(prefix) {
                primary_decided = false;
            }
        }
        warnings.extend(journey::observer_cost(&blocks, names));
    } else {
        for (role, who) in ["base", "head"].into_iter().zip(names) {
            warnings.extend(observer_reads(&blocks, role, who, is_native(&target)));
        }
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
    let (mut recordings, mut sampling) = (json!({}), serde_json::Map::new());
    let differs = primary_decided && (primary.lo > 0.0 || primary.hi < 0.0);
    // Each side is profiled with its own engine's tools: an implementation comparison's two may differ,
    // and a side strobe has no profiler for gets no block while the other still gets its own.
    let side_targets: Vec<Value> = variants.iter().map(|v| ctx.describe["targets"][v.target_of(s)].clone()).collect();
    let unprofiled: Vec<(usize, String)> = side_targets.iter().enumerate().filter_map(|(i, t)| Some((i, no_profiler(t, s.launch.is_object())?))).collect();
    let why = match ctx.args.get("why").unwrap_or(if ctx.args.get("probes") == Some("timing") { "never" } else { "auto" }) {
        _ if halted.is_some() => false,
        // A command ends inside each run: there is nothing to profile it with.
        _ if side_targets.iter().all(|t| t["kind"] == "cli") => false,
        "always" | "auto" if unprofiled.len() == variants.len() => {
            progress(format!("  no attribution blocks: {}", unprofiled[0].1));
            false
        }
        "always" => true,
        "never" => false,
        "auto" => differs,
        other => return Err(Error::usage(format!("--why: auto, always or never, not {other}"))),
    };
    if !why && !differs && halted.is_none() {
        progress("  no attribution blocks: nothing to explain (--why always runs them anyway)");
    }
    if why {
        if !ctx.implementations {
            progress(format!("  attribution blocks ({}; not timed)", why_text(&target)));
        }
        for (i, v) in variants.iter().enumerate() {
            if let Some((_, no)) = unprofiled.iter().find(|u| u.0 == i) {
                progress(format!("  no attribution block for {}: {no}", v.name));
                warnings.push(validity::info("Attribution", format!("{}: no attribution block: {no}", v.name)));
                continue;
            }
            if ctx.implementations {
                progress(format!("  {} attribution block ({}; not timed)", v.name, why_text(&side_targets[i])));
            }
            // A launch's attribution is a launch of its own, profiled from the first statement.
            let attributed = if s.launch.is_object() {
                in_slot(ctx, &format!("{} {} attribution", s.label(), v.role), |ctx| as_side(ctx, s, v, |ctx, s| launch::attribution(ctx, s, v)))
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
                    // `Unresolved` (SPEC §10.2): script samples the stored source maps could not name,
                    // native samples no symbols named. A script target's profile names its own source
                    // files: nothing to map.
                    let who = if ctx.implementations { v.name.as_str() } else { v.role };
                    let script = side_targets[i]["kind"] == "script";
                    let rec = b.recording.as_deref().and_then(|r| super::query::Rec::resolve(r).ok());
                    if let Some(sm) = attribution_sampling(rec.as_ref(), who, &mut warnings) {
                        sampling.insert(v.role.to_string(), json!(sm));
                    }
                    if !script && !is_native(&side_targets[i]) {
                        observer_profile(rec.as_ref(), who, &mut warnings);
                    }
                    let hot = rec.as_ref().filter(|_| !script).and_then(|r| super::query::hotspots_of(r, "function", 1).ok());
                    if let Some(h) = hot.map(|o| o.json) {
                        let (scripts, mapped) = (h["script_samples"].as_f64().unwrap_or(0.0), h["mapped_samples"].as_f64().unwrap_or(0.0));
                        if scripts > 0.0 && mapped / scripts < 0.95 {
                            warnings.push(validity::warn(
                                "Unresolved",
                                format!(
                                    "{who}: {:.0}% of script samples have no source map, so their names and files stay minified; strobe builds with STROBE_SOURCEMAP=hidden: let the bundler read it (Vite: build: {{ sourcemap: process.env.STROBE_SOURCEMAP }}), which ships the same scripts",
                                    100.0 * (1.0 - mapped / scripts)
                                ),
                            ));
                        }
                        let resolved = h["resolved_share"].as_f64().filter(|_| h["native"] == true && h["samples"].as_u64().unwrap_or(0) > 0);
                        if let Some(share) = resolved.filter(|r| *r < 0.95) {
                            let missing: Vec<String> = h["unsymbolized"].as_array().into_iter().flatten().take(3).map(|m| m["module"].as_str().unwrap_or("").to_string()).collect();
                            warnings.push(validity::warn(
                                "Unresolved",
                                format!(
                                    "{who}: {:.0}% of native samples have no symbol ({}), so their time names no function; build the app with symbols (a PDB beside it, a dSYM or DWARF)",
                                    100.0 * (1.0 - share),
                                    if missing.is_empty() { "modules without symbols".to_string() } else { missing.join(", ") }
                                ),
                            ));
                        }
                    }
                    recordings[v.role] = json!(b.recording);
                }
                // A stop or an unlock wait that ran out ends the bench here, as in the pairs.
                Err(e) if e.code == 130 || e.code == 124 || stop_requested() => {
                    halted = Some(if e.code == 124 { e } else { stop_error() });
                    break;
                }
                Err(e) => warnings.push(validity::warn("Attribution", format!("{} attribution block failed: {}", side_of(ctx, v), e.message))),
            }
        }
    }
    // `--video` (SPEC §8.8): after every timed pair, one untimed block per variant films a warmed run,
    // through the same OS capture (or renderer) on both sides. Its numbers join no verdict.
    let mut videos = json!({});
    if ctx.args.has("video") && halted.is_none() {
        if s.launch.is_object() {
            warnings.push(validity::info("Video", format!("{}: no video: a launch case starts before its window or page exists, which every source films from, so a video would miss the launch it times", s.label())));
        } else {
            progress("  video blocks (one warmed run per variant; not timed)");
            for v in variants.iter() {
                // Each side by its own target: an implementation comparison's may differ.
                if let Some(why) = no_video(ctx, v.target_of(s)) {
                    warnings.push(validity::info("Video", format!("{}: no video of {}: {why}", s.label(), side_of(ctx, v))));
                    continue;
                }
                match block_on_screen(ctx, s, v, usize::MAX, &Exec::filming()) {
                    Ok(b) => match b.recording.filter(|r| has_video(r)) {
                        Some(r) => videos[v.role] = json!(r),
                        None => warnings.push(validity::warn("Video", format!("{} video block kept no video", side_of(ctx, v)))),
                    },
                    Err(e) if e.code == 130 || e.code == 124 || stop_requested() => {
                        halted = Some(if e.code == 124 { e } else { stop_error() });
                        break;
                    }
                    Err(e) => warnings.push(validity::warn("Video", format!("{} video block failed: {}", side_of(ctx, v), e.message))),
                }
            }
            // Side by side only when both are the same kind of video (both the screen's, or both a renderer's).
            let kind = |r: &Value| r.as_str().and_then(|r| store::resolve(r).ok()).and_then(|d| store::read_json(&d.join("video.json")).ok()).map(|j| j["kind"].clone());
            if let (Some(a), Some(b)) = (kind(&videos["base"]), kind(&videos["head"]))
                && a != b
            {
                videos["mixed"] = json!(true);
                warnings.push(validity::warn(
                    "Video",
                    format!("base's video is the {a}'s and head's the {b}'s: not comparable side by side (one shows what reached the display, the other what a renderer produced)"),
                ));
            }
        }
    }
    let timing_recordings: Vec<Value> = blocks.iter().filter_map(|b| b.recording.clone().map(|r| json!({ "variant": b.variant, "pair": b.pair, "recording": r }))).collect();
    let pixels = if ctx.args.has("pixels") && variants.len() > 1 && halted.is_none() { pixels_series(ctx, s, variants)? } else { Value::Null };
    if pixels.as_array().is_some_and(|a| a.iter().any(|p| p["state"] != "same")) {
        warnings.push(validity::warn("PixelsDiffer", "base and head drew different pixels at a snapshot: see pixels"));
    }
    let window = series_window(ctx, s, &blocks);
    let mut out = json!({
        "pixels": pixels,
        "label": s.label(), "case": s.case, "target": s.target, "target_kind": target["kind"], "params": s.params, "kind": if s.launch.is_object() { "launch" } else { "runs" },
        "until": launch::until_of(&s.launch),
        "engine": target["engine"], "headless": headless_of(ctx, s), "browser": series_browser(&blocks),
        "primary": primary, "decided": primary_decided, "session": session, "secondaries": secondaries, "descriptive": descriptive, "holm_pairs": holm,
        "pairs": pairs.len(), "launches": blocks.len(), "stopped": stopped, "bound": bound, "noworse": s.noworse,
        "warmup_blocks": warmup_blocks, "warmup_ms": warmup_ms,
        "steps": mean_steps(&blocks), "elapsed_ms": series_started.elapsed().as_secs_f64() * 1e3 - (ctx.waited_ms - waited_before), "waited_ms": ctx.waited_ms - waited_before,
        "overhead": mean_overhead(&blocks), "untimed": untimed_of(&blocks.iter().map(|b| (b.values.get("untimed.n").copied(), b.values.get("untimed.ms").copied())).collect::<Vec<_>>()),
        "blocks": blocks, "warnings": warnings, "attribution": recordings, "attribution_sampling": if sampling.is_empty() { Value::Null } else { Value::Object(sampling) }, "timing_recordings": timing_recordings, "shown": shown,
        "pass": pass.map(|(b, p)| json!({ "counters": !main_counters, "blocks": b, "pairs": p })),
        "halt": halted.map(|h| json!({ "code": h.code, "message": h.message })),
    });
    out["video"] = if videos.as_object().is_some_and(|o| !o.is_empty()) { videos } else { Value::Null };
    out["input"] = input_of(&blocks);
    series_setup(ctx, s, window, &mut out);
    if ctx.implementations {
        implementation_series(ctx, s, variants, &mut out);
    }
    Ok(out)
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
    let mut blocks = Vec::new();
    for (v, folder) in [(&variants[0], "base"), (&variants[0], "base-again"), (&variants[1], "head")] {
        blocks.push(block_on_screen(ctx, s, v, usize::MAX, &Exec::untimed(false, Some(root.join(folder))))?);
    }
    // The input devices the snapshots' pages saw.
    let input = input_of(&blocks);
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
                    "name": name, "state": if d.differing == 0 { "same" } else { "differ" }, "differing": d.differing, "total": d.total, "max_delta": d.max_delta, "noise": noise, "input": input,
                    "diff": d.image.as_ref().map(|_| diff_path.display().to_string()), "base": root.join("base").join(format!("{name}.png")), "head": root.join("head").join(format!("{name}.png")),
                })
            }
            (b, _) => json!({ "name": name, "state": if b.is_some() { "only base" } else { "only head" }, "input": input }),
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

/// The count a percentile is a percentile of, and its events in words.
fn tail_of(name: &str) -> (&'static str, &'static str) {
    match name.split('.').next() {
        Some("interaction") => ("interaction.n", "inputs"),
        Some("scroll") => ("scroll.n", "scroll updates"),
        Some("canvas") => ("canvas.n", "canvas frame gaps"),
        _ if metrics::split_metric(name) => ("frames.split.n", "frames with their stages"),
        _ => ("frames.n", "frames"),
    }
}

/// `TailTooShort` when a block had fewer of a percentile's events than it needs.
fn tail_refusal(def: &MetricDef, blocks: &[Block]) -> Option<Refusal> {
    let (count, what) = tail_of(def.name);
    let least = blocks.iter().map(|b| b.values.get(count).copied().unwrap_or(0.0) as usize).min().unwrap_or(0);
    (def.min_n > 1 && least < def.min_n).then(|| Refusal::new(ValidatorId::TailTooShort, format!("{least} {what} < {}", def.min_n)))
}

/// `TailTooShort` per timed block: a percentile with fewer events than its minimum n in this block has
/// no value, and is refused when the series declared it, so a primary like that fails the first block
/// instead of pairing values no comparison could use.
fn tail_state(x: &Exec, mut b: Block) -> Block {
    if !x.measures_time() {
        return b;
    }
    let short: Vec<(String, Refusal)> = b
        .values
        .keys()
        .filter_map(|name| {
            let def = model::metric(name).filter(|d| d.min_n > 1)?;
            tail_refusal(def, std::slice::from_ref(&b)).map(|r| (name.clone(), r))
        })
        .collect();
    for (name, r) in short {
        b.values.remove(&name);
        b.refused.entry(name).or_insert(r);
    }
    b
}

/// Why strobe has no attribution block for this target (`launch`: of a launch case): a command ends
/// inside each run, and only Chromium's DevTools and Firefox's own profiler profile a page; a WebKit
/// page has none in strobe, and Firefox's profile names the page by strobe's run marks, which no
/// document carries from a launch's navigation. None when its own engine's tools profile it.
fn no_profiler(target: &Value, launch: bool) -> Option<String> {
    match (target["kind"].as_str(), engine_of(target)) {
        (Some("cli"), _) => Some("a command ends inside each run: there is nothing to profile it with".into()),
        (Some("web" | "url"), WebEngine::Webkit) => Some(format!("{} pages have no profilers in strobe yet", engine_of(target))),
        (Some("web" | "url"), WebEngine::Firefox) if launch => Some("a Firefox page's launch has no profile in strobe yet".into()),
        _ => None,
    }
}

/// What an attribution block records for this target.
fn why_text(target: &Value) -> &'static str {
    if is_native(target) {
        if crate::probe::present::access().helper || !cfg!(windows) {
            "OS samples and native stacks"
        } else {
            "OS samples and native stacks (sampled without the deep helper: user frames, a short stall per sample)"
        }
    } else if matches!(target["kind"].as_str(), Some("web" | "url")) && engine_of(target) == WebEngine::Firefox {
        "Firefox's profiler: CPU stacks, style, layout and GC markers; about:memory"
    } else {
        "CPU and allocation stacks, forced layouts, GC, a heap snapshot"
    }
}

/// What an attribution recording's JavaScript sampler kept against what it was asked for
/// (`probe::cpu::Sampling`), said under the block's line: `head JavaScript sampled every 2.0 ms (asked 1 ms)`.
/// A coarser one is also an `Attribution` note, so it reaches `--json`, `--quiet` and the summary.
fn attribution_sampling(rec: Option<&super::query::Rec>, who: &str, warnings: &mut Vec<Warning>) -> Option<probe::cpu::Sampling> {
    let s = rec.and_then(|r| r.sampling())?;
    progress(format!("  {who} JavaScript {}", s.text()));
    if s.coarse() {
        warnings.push(validity::info("Attribution", format!("{who}: JavaScript {}: {}", s.text(), super::query::COARSE)));
    }
    Some(s)
}

/// `ObserverCost` for one side (SPEC §10.2), in every bench: its timed blocks' reads of the page or tree
/// inside the measured runs (`driver.reads`, `driver.read_ms`) over 2% of a run's wall time; for a
/// native app read through its accessibility tree (`a11y`), whose provider works for each read, also
/// over 8 a run. An implementation comparison also weighs the sides against each other
/// (`journey::observer_cost`).
fn observer_reads(blocks: &[Block], role: &str, who: &str, a11y: bool) -> Option<Warning> {
    let (reads, share) = journey::reads_of(blocks, role)?;
    let why = if share > 0.02 {
        "reading took over 2% of a run, time the page or app spent answering strobe"
    } else if a11y && reads > 8.0 {
        "over 8 reads a run of the app's accessibility tree: each costs its provider work a person's use does not (wait on until: \"frame\" or \"idle\", which read nothing)"
    } else {
        return None;
    };
    Some(validity::warn("ObserverCost", format!("strobe read {who} {reads:.1} times a run ({:.1}% of its wall time) inside the measured runs: {why}", 100.0 * share)))
}

/// `ObserverCost` from a page's attribution recording: its profile's samples in strobe's page driver,
/// its own script (`(strobe driver)`) and the browser's work it called (`(native, via strobe driver)`,
/// the layouts its checks forced), over 2% of the main thread's active time (every sample but
/// `(idle)`), when that time is at least 2 ms a run; said with where it went.
fn observer_profile(rec: Option<&super::query::Rec>, who: &str, warnings: &mut Vec<Warning>) {
    let Some(rec) = rec else { return };
    let Some(h) = super::query::hotspots_of(rec, "package", usize::MAX).ok().map(|o| o.json) else { return };
    let rows: Vec<(&str, f64)> = h["rows"].as_array().into_iter().flatten().map(|r| (r["name"].as_str().unwrap_or(""), r["weight"].as_f64().unwrap_or(0.0))).collect();
    let active: f64 = rows.iter().filter(|r| r.0 != "(idle)").map(|r| r.1).sum();
    let mine: Vec<&(&str, f64)> = rows.iter().filter(|r| matches!(r.0, super::query::DRIVER | super::query::VIA_DRIVER)).collect();
    let driven: f64 = mine.iter().map(|r| r.1).sum();
    let runs = rec.meta["runs"].as_f64().unwrap_or(1.0).max(1.0);
    // µs: under 2 ms of work a run, a share of it says nothing.
    if active / runs < 2_000.0 || driven / active <= 0.02 {
        return;
    }
    let parts = mine.iter().map(|(n, w)| format!("{n} {}", fmt::value(w / 1000.0, "ms"))).collect::<Vec<_>>().join(", ");
    warnings.push(validity::warn(
        "ObserverCost",
        format!(
            "{who}: strobe's page driver took {:.1}% of the attribution block's active main-thread time ({parts}): waits whose checks run often or force layout cost the page work a person's use does not; wait on until: \"frame\" or \"idle\", or for an element that appears once",
            100.0 * driven / active
        ),
    ));
}

/// `sampled every 2.0 ms (asked 1 ms)` from a result's `attribution_sampling` entry.
fn sampled(v: &Value) -> Option<String> {
    serde_json::from_value::<probe::cpu::Sampling>(v.clone()).ok().map(|s| s.text())
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

/// Why one engine's own measure is not compared between two implementations.
fn not_comparable(name: &str, def: &MetricDef) -> Refusal {
    Refusal::new(ValidatorId::NotComparable, format!("{name}: one engine's own measure ({}), and the sides run on different engines: kept per side, never compared", def.about))
        .fix("compare a measure both sides take the same way (cpu.work, run.duration, launch.ready, frames from the OS)")
}

/// What an implementation comparison adds to a series (SPEC §9.10): the sides' targets, each
/// comparison's class across implementations (with its note), and the measures kept per side.
fn implementation_series(ctx: &Ctx, s: &Series, variants: &[Variant], out: &mut Value) {
    let (Some(b), Some(h)) = (variants.first().and_then(|v| v.target.as_deref()), variants.get(1).and_then(|v| v.target.as_deref())) else { return };
    out["targets"] = json!({ "base": b, "head": h });
    out["target_kinds"] = json!({ "base": ctx.describe["targets"][b]["kind"], "head": ctx.describe["targets"][h]["kind"] });
    out["hero"] = json!(s.hero);
    out["adapted"] = json!(s.adapted);
    let blocks: Vec<Block> = serde_json::from_value(out["blocks"].clone()).unwrap_or_default();
    out["equivalence"] = journey::equivalence(&blocks);
    let mark = |c: &mut Value| {
        if let Some(d) = c["metric"].as_str().and_then(model::metric) {
            c["across"] = json!(d.across.name());
            if let Some(n) = d.across.note() {
                c["care_note"] = json!(n);
            }
        }
    };
    mark(&mut out["primary"]);
    for k in ["secondaries", "descriptive"] {
        for c in out[k].as_array_mut().into_iter().flatten() {
            mark(c);
        }
    }
    if !s.cross_engine {
        return;
    }
    // Per side: each per-side measure's block values, their median and a Hodges–Lehmann interval
    // over the blocks (on a log scale when all are positive), as `--blocks` gives one build's.
    let blocks: Vec<Block> = serde_json::from_value(out["blocks"].clone()).unwrap_or_default();
    let mut per = json!({ "base": {}, "head": {} });
    for role in ["base", "head"] {
        let mine: Vec<&Block> = blocks.iter().filter(|b| b.variant == role).collect();
        let names: std::collections::BTreeSet<&String> = mine.iter().flat_map(|b| b.values.keys()).filter(|k| s.per_side(k)).collect();
        for name in names {
            let xs: Vec<f64> = mine.iter().filter_map(|b| b.values.get(name).copied()).filter(|x| x.is_finite()).collect();
            if xs.is_empty() {
                continue;
            }
            let unit = model::metric(name).map_or("", |d| d.unit);
            let (lo, hi) = match xs.len() {
                1 => (Value::Null, Value::Null),
                _ if xs.iter().all(|x| *x > 0.0) => {
                    let (_, lo, hi, _) = stats::hodges_lehmann(&xs.iter().map(|x| x.ln()).collect::<Vec<_>>(), 0.95);
                    (json!(lo.exp()), json!(hi.exp()))
                }
                _ => {
                    let (_, lo, hi, _) = stats::hodges_lehmann(&xs, 0.95);
                    (json!(lo), json!(hi))
                }
            };
            per[role][name] = json!({ "median": metrics::median(&xs), "lo": lo, "hi": hi, "unit": unit, "n": xs.len() });
        }
    }
    out["per_side"] = per;
}

/// A journey one side cannot run (SPEC §9.10 parity gap): listed with the side and why, never run on
/// either side and never counted as better or worse.
fn gap_series(s: &Series, variants: &[Variant], describe: &Value) -> Value {
    let (role, why, missing) = s.gap.clone().unwrap_or(("head", String::new(), None));
    // A `--check` of every target lists the target itself (no sides).
    let side = variants.iter().find(|v| v.role == role && v.target.is_some());
    let name = side.map_or_else(|| describe["targets"][&s.target]["name"].as_str().unwrap_or(&s.target).to_string(), |v| v.name.clone());
    let r = Refusal::new(ValidatorId::ParityGap, format!("{name} cannot run {}: {why}", s.case)).fix("give that target the journey (or drop it from both)");
    let mut primary = json!({ "metric": s.primary, "verdict": format!("refused (ParityGap): {}", r.summary), "verdict_kind": VerdictKind::Refused, "decided": false });
    primary["refusal"] = json!(r);
    let (b, h) = s.sides.clone().unwrap_or_default();
    json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "gap",
        "targets": { "base": b, "head": h }, "hero": s.hero, "primary": primary, "decided": false, "pairs": 0, "launches": 0,
        "gap": { "side": role, "name": name, "missing": missing, "reason": why },
        "journey": s.journey(), "side_name": name,
    })
}

/// A series whose side `v` met a missing adapter entry while it ran (`ParityGap` from the runner): a
/// gap on that side, its blocks and their recordings discarded.
fn runtime_gap(ctx: &Ctx, s: &Series, variants: &[Variant], v: usize, e: &Error, blocks: &[Block]) -> Value {
    for b in blocks {
        if let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    let summary = e.refusal.as_ref().map_or_else(|| e.message.clone(), |r| r.summary.clone());
    let missing = summary.split_whitespace().find(|w| w.starts_with('@')).map(str::to_string);
    let g = gap_series(&Series { gap: Some((variants[v].role, summary, missing)), ..s.clone() }, variants, &ctx.describe);
    progress(format!("  gap: {}", g["primary"]["refusal"]["summary"].as_str().unwrap_or("")));
    g
}

/// A series refused before anything built (`check_engines`): listed with why and the closest primary it
/// can measure, never run, and undecided (`--require-verdict` exits 4 on it).
fn refused_series(ctx: &Ctx, s: &Series, r: &Refusal) -> Value {
    let target = &ctx.describe["targets"][&s.target];
    let primary = json!({ "metric": s.primary, "verdict": format!("refused ({}): {}", r.validator, r.summary), "verdict_kind": VerdictKind::Refused, "decided": false, "refusal": r });
    let (b, h) = s.sides.clone().unwrap_or_default();
    let mut out = json!({
        "label": s.label(), "case": s.case, "target": s.target, "params": s.params, "kind": "refused",
        "targets": { "base": b, "head": h }, "hero": s.hero, "primary": primary, "decided": false, "passed": false, "pairs": 0, "launches": 0,
        "headless": headless_of(ctx, s), "engine": engine_of(target), "target_kind": target["kind"],
        "journey": s.journey(), "side_name": target["name"].as_str().unwrap_or(&s.target),
    });
    // It drew in no window.
    series_setup(ctx, s, Value::Null, &mut out);
    out
}

/// `<validator>: <summary> · fix: <fix>`, for a refusal as result.json keeps it.
pub(super) fn refused_text(r: &Value) -> String {
    let fix = r["fix"].as_str().map(|f| format!(" · fix: {f}")).unwrap_or_default();
    format!("{}: {}{fix}", r["validator"].as_str().unwrap_or(""), r["summary"].as_str().unwrap_or(""))
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

/// The analysers of a series' blocks; `names` are how its notes name base and head (`side_of`).
fn analyse(s: &Series, def: &MetricDef, blocks: &[Block], pairs: &[(usize, usize)], relative: bool, names: [&str; 2], w: &mut Vec<Warning>) {
    for (role, name) in ["base", "head"].into_iter().zip(names) {
        let v: Vec<f64> = blocks.iter().filter(|b| b.variant == role).map(|b| val(b, &s.primary)).filter(|x| x.is_finite()).collect();
        if v.len() >= 3 {
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt();
            // A CV means nothing near zero: 0.1% vs 0% dropped frames is not variance worth a warning.
            let tiny = matches!(def.unit, "%" | "count") && mean < 1.0;
            if mean > 0.0 && !tiny && sd / mean > 0.10 {
                w.push(validity::warn("HighVariance", format!("{name}: {} varies {:.0}% between launches (CV)", def.name, 100.0 * sd / mean)));
            }
        }
        if let Some(m) = stats::mvalue(&v)
            && m >= 3.2
        {
            w.push(validity::warn("MultimodalDistribution", format!("{name}: {} looks multimodal (mValue {m:.1}); check pinning and background load", def.name)));
        }
    }
    // Why a block never got steady: its work grew run after run (`warmup.unsteady` 2: state the runs
    // leave behind), or swung back and forth (1: noise, not state).
    let unsteady = |grew: bool| blocks.iter().filter(|b| b.values.get("warmup.unsteady").is_some_and(|u| (*u >= 2.0) == grew)).count();
    let (grew, swung) = (unsteady(true), unsteady(false));
    if grew > 0 {
        let why = if s.reset {
            "the work still grew run after run despite the case's reset: check what the reset leaves behind (DOM, listeners, caches)"
        } else {
            "the work grew run after run: the case's state grows across runs; add a reset"
        };
        w.push(validity::warn("NotSteady", format!("{grew} block(s) never did the same work twice in {MAX_WARMUP} warmup runs: {why}")));
    }
    if swung > 0 {
        w.push(validity::warn(
            "NotSteady",
            format!("{swung} block(s) never did the same work twice in {MAX_WARMUP} warmup runs: the work swung from run to run without growing (jitter, not state): pin the target's CPUs (--pin), quiet the machine, or give each run more work"),
        ));
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
                let fix = if s.reset { "despite the case's reset: check what the reset leaves behind (DOM, listeners, caches)" } else { "add a reset to the case" };
                w.push(validity::warn("StateDrift", format!("DOM nodes grew {:.0} → {:.0} across runs in a {} block: {fix}", first, last, b.variant)));
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
    // Firefox's census is about:memory's: the page's JavaScript from SpiderMonkey's reporters, which hold
    // no JIT code, and no heap snapshot to name what grew.
    if census.first().is_some_and(|c| c["engine"] == "firefox") {
        // The page's JS memory needs the page's process: a census that could not name it has none.
        if let Some(c) = census.iter().find(|c| !c["js_heap_bytes"].is_number()) {
            let when = c["taken"].as_str().unwrap_or("one census");
            return Err(Refusal::new(ValidatorId::RunIncomplete, format!("about:memory {when}: the page's content process could not be named, so its JavaScript memory is unknown")).into());
        }
        store::write_json(&dir.join("census.json"), &json!(census))?;
        let none = "Firefox gives strobe no heap snapshot: the growth is about:memory's, with no objects named";
        return Ok(json!({ "runs": 2 * n, "censuses": census.len(), "js_heap": js_v, "dom_nodes": dom_v, "js_heap_bytes": js, "dom_node_counts": dom, "suspects": { "refused": none } }));
    }
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

/// What `Foreign::stop` read: other work on the target's CPUs (% of one core, None when unreadable) and
/// the busiest processes on the machine.
type OtherWork = (Option<f64>, Vec<(String, f64)>);

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
    fn start(browser: &WebBrowser) -> Foreign {
        let mut idle = Vec::new();
        host::idle_cycles(&mut idle);
        Foreign { idle, tsc: host::tsc(), browser: browser.processes().iter().map(|p| p.cycles).sum(), procs: (host::now_ns(), host::all_processes()) }
    }

    /// Other work on `cpus` (all when unpinned) as % of one core (None: the idle counters or the time
    /// stamp counter could not be read), and the busiest processes on the machine outside strobe's
    /// tree and the target browser, as % of one core: their cycles over the span's time stamp counter,
    /// a process that started meanwhile counted from zero.
    fn stop(self, cpus: Option<u64>, browser: &WebBrowser) -> OtherWork {
        let mut idle = Vec::new();
        let ok = host::idle_cycles(&mut idle);
        let theirs = browser.processes();
        let (tsc, now) = (host::tsc(), (host::now_ns(), host::all_processes()));
        let own = theirs.iter().map(|p| p.cycles).sum::<u64>();
        let span = match (self.tsc, tsc) {
            (Some(a), Some(b)) if b > a => Some((b - a) as f64),
            _ => None,
        };
        let busiest = match span {
            Some(span) => {
                let skip: std::collections::BTreeSet<u32> = host::tree_pids(std::process::id()).into_iter().chain(theirs.iter().map(|p| p.pid)).collect();
                let before: BTreeMap<u32, u64> = self.procs.1.iter().map(|p| (p.pid, p.cycles)).collect();
                let mut out: Vec<(String, f64)> = now
                    .1
                    .iter()
                    .filter(|p| p.pid != 0 && !skip.contains(&p.pid))
                    .map(|p| (format!("{} ({})", p.name, p.pid), 100.0 * p.cycles.saturating_sub(before.get(&p.pid).copied().unwrap_or(0)) as f64 / span))
                    .filter(|(_, pct)| *pct >= 5.0)
                    .collect();
                out.sort_by(|x, y| y.1.total_cmp(&x.1));
                out.truncate(3);
                out
            }
            None => busiest_others(&self.procs, &now),
        };
        let foreign = span.filter(|_| ok && idle.len() == self.idle.len()).map(|span| {
            let busy: f64 = self
                .idle
                .iter()
                .zip(&idle)
                .enumerate()
                .filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1))
                .map(|(_, (a, b))| (100.0 * (1.0 - b.saturating_sub(*a) as f64 / span)).clamp(0.0, 100.0))
                .sum();
            (busy - 100.0 * own.saturating_sub(self.browser) as f64 / span).max(0.0)
        });
        (foreign, busiest)
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
    let root = v.root.as_ref().filter(|_| !matches!(v.kind, Kind::Cmd(_)))?;
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
    as_side(ctx, s, v, |ctx, s| on_screen(ctx, s, v, pair, x))
}

/// The series as one side runs it (SPEC §9.10): its own target, that target's `expect` with the
/// case's over it, and the strobe.json of the target's own project (`cwd`) as `ctx.config` while `f`
/// runs. A series of builds runs as it is, with its target's project.
fn as_side<T>(ctx: &mut Ctx, s: &Series, v: &Variant, f: impl FnOnce(&mut Ctx, &Series) -> Result<T>) -> Result<T> {
    // Its `expect` and a launch's `until` through that target's adapter (`@name`).
    let side = match v.target.as_deref().filter(|t| *t != s.target) {
        Some(t) => {
            let targets = &ctx.describe["targets"];
            let launch = journey::side_launch(&ctx.describe["cases"][&s.case]["launch"], targets, t)?;
            Some(Series { target: t.to_string(), expect: journey::side_expect(targets, &s.case_expect, t)?, launch, ..s.clone() })
        }
        None => None,
    };
    let s = side.as_ref().unwrap_or(s);
    let saved = ctx.projects.get(&s.target).map(|(_, c)| c.clone()).map(|c| std::mem::replace(&mut ctx.config, c));
    let r = f(ctx, s);
    if let Some(c) = saved {
        ctx.config = c;
    }
    r
}

fn on_screen(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
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
            let why = needs_screen(ctx, s, x).unwrap_or_else(|| "it met a hidden window".into());
            progress(format!("  waiting: the session is locked and {} needs a presenting window: {why}; strobe resumes when it is unlocked", s.label()));
            // Nothing held while waiting: the user's power plan, sleep as they set it, and the slot, so
            // other benches run what needs no presenting window meanwhile.
            release_machine(ctx);
            ctx.reg.set("waiting", format!("{} {} block: the session is locked and the series needs a presenting window ({why})", s.label(), v.role));
            let waiting = Instant::now();
            // `--unlock-wait`: how long one wait may last before the bench ends with what it finished.
            let limit = ctx.args.get("unlock-wait").map(duration_ms).transpose()?.map(Duration::from_millis);
            while host::session_locked() {
                if let Some(l) = limit.filter(|l| waiting.elapsed() >= *l) {
                    ctx.waited_ms += waiting.elapsed().as_secs_f64() * 1e3;
                    return Err(Error::timeout(format!("the session stayed locked for {} (--unlock-wait), and the series needs a presenting window ({why})", fmt::ms_duration(l.as_millis() as u64)))
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
        let r = any_block(ctx, s, v, pair, x).and_then(|b| priority_state(ctx, s, x, b)).map(|b| headless_state(ctx, s, b)).map(|b| software_state(ctx, s, b)).map(|b| phase_state(ctx, s, b)).map(|b| counter_state(s, x, b)).map(|b| tail_state(x, b)).inspect(|b| restate_recording(ctx, s, b));
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
                ctx.warnings
                    .push(validity::warn("OverheadBudget", format!("{} · {} block: {part} used {} of a core, over its {budget}% budget (SPEC §19)", s.label(), side_of(ctx, v), pct_of_core(pct))));
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
    // macOS has no CPU affinity: nothing is pinned or kept off anything, and nothing to say.
    let noted = cfg!(target_os = "macos") || ctx.warnings.iter().any(|w| w.id == "ReservedCpus");
    let Some(cpus) = ctx.pin else {
        if !noted {
            let why = match ctx.args.get("pin").unwrap_or("auto") {
                "auto" => format!("--pin auto left the target {}", ctx.pin_label),
                _ => "the target is not pinned (--pin none)".to_string(),
            };
            ctx.warnings.push(validity::info("ReservedCpus", format!("other processes were not kept off the target's CPUs: {why}")));
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

/// One block, with the bench file's fixture when it has one (SPEC §9.10): started before the target
/// launches, in its own group and home (outside the target's process set), its `{fixture.url}`,
/// `{fixture.port}` and `{fixture.pid}` filled into the target's options and given to the hooks as
/// `ctx.fixture`, and ended with the block. The block is refused `FixtureMismatch` when the target
/// never held a connection to it or ran its executable.
fn any_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let b = fixture_block(ctx, s, v, pair, x);
    // What appeared in the user's Documents or Desktop during a later launch: a caution, once each.
    let noted: Vec<String> = std::mem::take(&mut *ctx.folder_notes.lock().unwrap_or_else(|e| e.into_inner()));
    for n in noted {
        let text = format!("{n} while {}'s copy ran ({}): yours, or an isolation gap", v.name, s.label());
        if !ctx.warnings.iter().any(|w| w.id == "UserFolders" && w.text == text) {
            ctx.warnings.push(validity::warn("UserFolders", text));
        }
    }
    let b = b?;
    admit_window(ctx, s, v, &b)?;
    admit_input(ctx, s, v, &b)?;
    admit_motion(ctx, s, v, &b)?;
    Ok(b)
}

fn fixture_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let Some(spec) = &ctx.fixture else { return kind_block(ctx, s, v, pair, x) };
    let running = spec.start(&s.params, &store::experiment_dir(&ctx.exp)?.join("fixture.log"), ctx.runner_pin)?;
    // The target this variant runs (its side's, in an implementation comparison).
    let name = v.target_of(s).to_string();
    let unfilled = ctx.describe["targets"][&name].clone();
    ctx.describe["targets"][&name] = running.fill(&unfilled);
    ctx.fixture_running = Some(running.clone());
    let info = json!({ "url": running.url(), "port": running.port, "pid": running.pid, "home": running.home() });
    let r = ctx.runner.call("fixture", info, Duration::from_secs(10), launch::no_page).and_then(|_| kind_block(ctx, s, v, pair, x));
    ctx.describe["targets"][&name] = unfilled;
    ctx.fixture_running = None;
    let r = r.and_then(|mut b| {
        b.fixture_connected = running.verdict(&v.name)?;
        Ok(b)
    });
    drop(running);
    r
}

/// One block of whatever kind the target and case are.
fn kind_block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    // Only a block that keeps its actions logs them (`--check`, `--keep`, attribution and memory
    // blocks): an ordinary timed block reads no clock and copies nothing for them.
    ctx.runner.collect_actions(x.untimed || x.keep || matches!(x.set, Set::Attribution | Set::Memory));
    let native = is_native(&ctx.describe["targets"][&s.target]);
    if ctx.describe["targets"][&s.target]["kind"] == "cli" {
        cli::block(ctx, s, v, pair, x)
    } else if s.launch.is_object() && native {
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
/// marks on one clock, without the run's untimed stretches inside them; NaN when the run never
/// entered the phase.
fn phase_ms(marks: &[(String, u64)], name: &str, run: &metrics::Timed) -> f64 {
    let (start, end) = (format!(":phase:{name}:start"), format!(":phase:{name}:end"));
    let mut open: Option<u64> = None;
    let mut total: Option<u64> = None;
    for (key, at) in marks {
        if key.ends_with(&start) {
            open = Some(*at);
        } else if key.ends_with(&end)
            && let Some(from) = open.take()
        {
            *total.get_or_insert(0) += at.saturating_sub(from).saturating_sub(run.untimed_in(from, *at));
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
    needs_screen(ctx, s, x).is_none()
}

/// What makes a series need a presenting window, said for the wait on a locked session (None: it runs
/// locked, `runs_locked`). A metric a native app can never have (a browser engine's, frames of a
/// windowless app) needs nothing: its block refuses it `NotForTarget`.
fn needs_screen(ctx: &Ctx, s: &Series, x: &Exec) -> Option<String> {
    let t = &ctx.describe["targets"][&s.target];
    let offscreen = |m: &str| {
        ["cpu.", "ui.", "memory.", "io.", "count.", "gpu.", "phase.", "energy."].iter().any(|p| m.starts_with(p))
            || m == "run.duration"
            || (m.starts_with("launch.") && m != "launch.painted" && m != "launch.settled")
    };
    let windowless = t["window"] == false || t["app"].as_str().and_then(|id| isolate::app(id, &ctx.config).ok()).is_some_and(|a| a.windowless);
    // A pixels block compares what the window shows; only a headless page draws it without a screen.
    if x.pixels.is_some() && !(matches!(t["kind"].as_str(), Some("web" | "url")) && headless_of(ctx, s)) {
        return Some("it compares the case's snapshots (--pixels), which are pixels on screen".into());
    }
    match t["kind"].as_str().unwrap_or("") {
        "script" | "cli" => None,
        // A headless page draws to no display: the lock screen covers nothing of it.
        "web" | "url" if headless_of(ctx, s) => None,
        _ if x.set == Set::Memory => None,
        "native" if windowless || x.untimed => {
            let on: Vec<&str> = declared_metrics(s)
                .into_iter()
                .map(String::as_str)
                .filter(|m| !native::browser_only(m) && !(windowless && native::on_screen_area(m)))
                .filter(|m| !offscreen(m) || (*m == s.primary && gpu_use(m)))
                .collect();
            (!on.is_empty()).then(|| format!("it measures {} on screen", on.join(", ")))
        }
        "web" | "url" => Some("a page draws in a browser window (--headless runs page cases without one)".into()),
        _ => Some("its window is timed on screen".into()),
    }
}

pub(crate) use crate::validity::{display_metric, gpu_use};

/// Records a block's lock state. On a locked screen the compositor draws nothing, so GPU use there
/// describes no person's screen: its values go, and a series that declares one shows it refused. GPU
/// memory stays: allocations need no presenting window, and both blocks of a pair share the state.
fn session_state(s: &Series, mut b: Block, locked: bool) -> Block {
    b.locked = locked;
    if locked {
        b.values.retain(|k, _| !gpu_use(k));
        for m in declared_metrics(s).into_iter().filter(|m| gpu_use(m)) {
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
fn restate_recording(ctx: &Ctx, s: &Series, b: &Block) {
    let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) else { return };
    let Ok(mut meta) = store::read_json(&dir.join("meta.json")) else { return };
    meta["values"] = json!(b.values);
    meta["refused"] = refusal_map(&b.refused);
    if headless_of(ctx, s) {
        meta["headless"] = json!(true);
    }
    // Why the recording's frames are gone, as a `record` says it (`query summary`).
    let why = if headless_of(ctx, s) {
        Some("Headless: the page ran headless, so nothing it drew reached a display".to_string())
    } else if b.browser.as_ref().is_some_and(|br| br.software) {
        Some("SoftwareGpu: a software adapter drew the page; the browser's CPU includes the rasterization a GPU would do".to_string())
    } else {
        b.refused.iter().find(|(k, r)| display_metric(k) && r.validator == ValidatorId::BackgroundPriority).map(|(_, r)| r.message())
    };
    if let Some(why) = why {
        meta["frames_refused"] = json!(why);
    }
    let _ = store::write_json(&dir.join("meta.json"), &meta);
}

/// A headless page (`--headless`, a target's `headless`): what reached no display is not kept. Frames,
/// input-to-screen latencies, scrolling, canvas frames and GPU use go, and the declared ones show
/// refused (`Headless`); CPU, main-thread work, counters, memory, milestones and strobe's own input
/// pacing stay.
fn headless_state(ctx: &Ctx, s: &Series, mut b: Block) -> Block {
    if !headless_of(ctx, s) {
        return b;
    }
    // GPU memory too when this block's headless browser drew in software: no GPU held it.
    let software = b.browser.as_ref().is_some_and(|br| br.software);
    let drawn = |m: &str| display_metric(m) || gpu_use(m) || (software && matches!(m, "gpu.vram" | "gpu.shared"));
    b.values.retain(|k, _| !drawn(k));
    // The frames' own lists too: a report must not read late frames or pacing from them.
    b.frames.clear();
    b.intervals.clear();
    b.phases.clear();
    for m in declared_metrics(s).into_iter().filter(|m| drawn(m)) {
        b.refused.insert(m.clone(), Refusal::new(ValidatorId::Headless, format!("{m}: the page ran headless, so nothing it drew reached a display")));
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
    let page = !is_native(&ctx.describe["targets"][&s.target]) && !matches!(ctx.describe["targets"][&s.target]["kind"].as_str(), Some("script" | "cli"));
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
    b.frames.clear();
    b.intervals.clear();
    b.phases.clear();
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
    let app = matches!(kind, "app" | "exe" | "webview" | "command");
    let engine = engine_of(&target);
    let headless = headless_of(ctx, s);
    // What the series declared beyond the default probes: counters, network counts, layout shift.
    let declared: Vec<&String> = std::iter::once(&s.primary).chain(&s.metrics).collect();
    let counted: Vec<String> = declared.iter().filter_map(|m| m.strip_prefix("count.")).map(str::to_string).collect();
    let nets: Vec<String> = declared.iter().filter(|m| m.starts_with("net.")).map(|m| m.to_string()).collect();
    let shift = declared.iter().any(|m| *m == "layout.shift");
    let gc = declared.iter().any(|m| *m == "main.gc");
    let gpu_compile = declared.iter().any(|m| *m == "gpu.compile");
    let canvas = declared.iter().any(|m| m.starts_with("canvas."));
    // Against a native implementation (SPEC §9.10) a timed block measures the page from outside, as
    // the other side is: no trace, frames from the OS's present events, runs and phases stamped on
    // the host clock, input from the OS. The engine's own counters are read at run boundaries, kept
    // per side. A `--check` checks it the same way.
    let outside = s.cross_engine && !x.counters && matches!(set, Set::Lean | Set::Timing) && x.pixels.is_none();
    // The block's OS present capture, which the OS input's waits read live.
    let presents_rc: crate::driver::os::Presents = Default::default();
    // How the block's notes name its side (`side_of`), read before the runs' closures borrow ctx.
    let side = side_of(ctx, v).to_string();
    let url = if app { String::new() } else { variant_url(v, &target)? };
    let fixed_page = kind == "url" && !matches!(v.kind, Kind::Url(_));
    let mut window = ctx.window.clone();
    if let Some(vp) = target.get("viewport").filter(|v| v.is_object()) {
        window.width = vp["width"].as_u64().unwrap_or(1400) as u32;
        window.height = vp["height"].as_u64().unwrap_or(900) as u32;
    }
    window.scale = target["scale"].as_f64();
    let emulation = if app { chromium::Emulation::default() } else { emulation_of(&target, &s.target)? };
    let reduce = Motion::of(&target, &s.target)?;
    phone_window(&mut window, &target, &emulation, headless);
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
    // An app's hooks get its home, environment and executable (SPEC §9.6), as a launch case's do.
    let mut hook_app = json!({});
    let browser: WebBrowser = if kind == "command" {
        let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a command target runs in a variant's folder (a directory or a git ref)", v.role)))?;
        crate::target::command::launch(&crate::target::command::Spec::from_target(&target, &root), &isolate::new_home(), true, &window, ctx.pin)?.0.into()
    } else if app {
        let launch = with_gpu_cache(ctx, s, v, &target, variant_app(v, &target, &ctx.config)?)?;
        if !matches!(x.set, Set::Attribution | Set::Memory) && x.pixels.is_none() {
            dev_build(&launch.exe)?;
        }
        let seed = seed_dir(ctx, s, &target);
        let home = isolate::prepare_home(&launch, seed.as_deref(), None)?;
        hook_app = json!({ "home": home.dir, "env": home.environment(&launch), "exe": launch.exe });
        let (b, g, _) = electron::launch_with(&launch, seed.as_deref(), &window, ctx.pin, &electron::Extra { prepared: Some(home), ..Default::default() })?;
        guard = Some(g);
        b.into()
    } else {
        web::launch_as(engine, &window, headless, ctx.pin, &emulation, reduce)?
    };
    let steps = RefCell::new(vec![("quiet".to_string(), quiet_ms), ("launch".to_string(), launched.elapsed().as_secs_f64() * 1e3)]);
    ctx.target_qos = host::background_qos(Some(browser.pid()));
    let lapped = std::cell::Cell::new(Instant::now());
    let lap = |name: &str| {
        steps.borrow_mut().push((name.to_string(), lapped.get().elapsed().as_secs_f64() * 1e3));
        lapped.set(Instant::now());
    };
    let result = (|| -> Result<Block> {
        let seen = browser_seen(ctx, s, v, headless, !matches!(set, Set::Attribution | Set::Memory), &browser)?;
        let page: Page = match (&browser, app) {
            (WebBrowser::Chromium(b), true) => electron::main_page(b)?.into(),
            (WebBrowser::Chromium(_), false) | (WebBrowser::Firefox(_) | WebBrowser::Webkit(_), _) => browser.page()?,
        };
        // No trace to carry runs, phases and frames: measured from outside (against a native
        // implementation), or a page whose engine has none for strobe (Firefox).
        let traceless = outside || page.cdp().is_none();
        // Frames from the OS's presents: from outside, or headed in such an engine. Then `interaction.*`
        // too (a default secondary), from the page's own changes (`CHANGES_SCRIPT`, in every document
        // from its start). From outside the input is the OS's (`os::OsInput`), in every engine; a page
        // the OS's input cannot reach (`os::unreachable`) takes none in its runs there.
        let os_frames_on = !headless && (outside || (traceless && !x.counters && set != Set::Memory));
        let no_os_input = if outside { crate::driver::os::unreachable(engine, headless) } else { None };
        let os_input = outside && no_os_input.is_none();
        let changes = os_frames_on;
        if changes {
            page.add_init_script(CHANGES_SCRIPT)?;
            page.eval(CHANGES_SCRIPT)?;
        }
        // The page's changes in the run so far, harvested before a `goto` takes the document with them.
        let harvested: RefCell<PageChanges> = RefCell::new(PageChanges::default());
        // Keys the measured runs typed in a burst (`type(…, { burst: true })`).
        let burst_keys = std::cell::Cell::new(0usize);
        if let Some(g) = &guard {
            g.check("after the app opened its window")?;
        }
        let viewport = target.get("viewport").is_some_and(Value::is_object);
        // An emulated device's touch and user agent before anything sizes or loads the page; a phone's
        // viewport once the window is its size (headed, the window must have reached it:
        // `check_viewport`; after the override a page without a viewport tag lays out 980 wide).
        if !app {
            page.emulate(&emulation)?;
        }
        // A command's app was started by its command: only DevTools' media emulation reaches its page.
        if reduce.reduced()
            && kind == "command"
            && let Some(p) = page.cdp()
        {
            motion::emulate(p)?;
        }
        if !app && (viewport || emulation.mobile) {
            browser.fit_viewport(&page, window.width, window.height, headless)?;
        }
        if emulation.mobile {
            if !headless {
                page.check_viewport((window.width, window.height))?;
            }
            page.emulate_viewport(&emulation, (window.width, window.height), window.scale)?;
        }
        // An app's window (a webview's, an Electron app's) is placed like a native app's and sized to
        // a viewport (DevTools resizes no app window: the OS does). The block records the page's own
        // size, not the one asked for.
        if let (WebBrowser::Chromium(b), Page::Chromium(p)) = (&browser, &page)
            && (b.embedded || app)
        {
            ctx.used = Some(crate::target::webview::fit(b, p, Some(&window), viewport)?);
        }
        // The driver's frame waits use the browser's own rAF, kept before the page's scripts can replace it.
        page.add_init_script(crate::driver::NATIVE_RAF)?;
        // `count.<name>` metrics: the page counts through a global installed in every document it loads.
        if !counted.is_empty() {
            page.add_init_script(COUNT_SCRIPT)?;
            page.eval(COUNT_SCRIPT)?;
        }
        if canvas {
            page.add_init_script(CANVAS_SCRIPT)?;
            page.eval(CANVAS_SCRIPT)?;
        }
        // Profilers, heap snapshots and the memory census over CDP are Chromium's; Firefox's attribution
        // and memory blocks use its own profiler and about:memory (`probe::gecko`).
        let no_cdp = || Error::refused("EngineUnsupported", format!("{engine} pages have no DevTools profilers or heap snapshots"), "attribute in Chromium");
        let firefox = if let WebBrowser::Firefox(b) = &browser { Some(b) } else { None };
        // A Firefox attribution block marks its runs in the page, so its profile names the page's process.
        let gecko_on = set == Set::Attribution && firefox.is_some();
        let cp = || page.cdp().ok_or_else(no_cdp);
        let cb = || browser.cdp().ok_or_else(no_cdp);
        lap("attach");
        let id = store::new_id("rec");
        let token = id.trim_start_matches("rec_").to_string();
        let mut driver = Driver::new(page.clone(), token.clone());
        driver.pixels = x.pixels.clone();
        // This run's `count.<name>` values (the bench file's `ctx.count` and the page's counter global)
        // and the run's key, which names its phase marks.
        let hooks = block::Hooks::new(ctx, || browser.processes().iter().map(|p| p.pid).collect());
        // TaskDuration (ms), then WORK_PARTS: cumulative per document (`Performance.enable` comes before the runs).
        let counters = || -> [f64; 6] {
            // Chromium's Performance domain; an engine without it has no such counters (NaN, never 0).
            let m = page.cdp().map_or(Value::Null, |p| p.send("Performance.getMetrics", json!({})).unwrap_or(Value::Null));
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
        // A Chromium attribution block's profiles, one CPU and allocation pair per measured run, each over
        // exactly the run (never its reset): both profilers go on through the run's navigations in one
        // renderer. `profiling` while one is on; the scripts' function ranges, read after the last
        // measured run and before each `goto` inside a profiled run (`probe::symbols::Ranges`).
        let profiling = Cell::new(false);
        let profiles: RefCell<Vec<(Value, Value)>> = RefCell::new(Vec::new());
        let ranges: RefCell<probe::symbols::Ranges> = RefCell::new(Default::default());
        let isolate = || page.cdp().and_then(|p| p.send("Runtime.getIsolateId", json!({})).ok()).and_then(|v| v["id"].as_str().map(str::to_string));
        // The renderer the profilers run in (its isolate): a navigation that moved the page to another
        // (a cross-site one under site isolation) left them with the first, so the profile would miss it.
        let renderer: RefCell<Option<String>> = RefCell::new(None);
        // Profiling paused for a `ctx.untimed` stretch inside a profiled run.
        let paused = Cell::new(false);
        let same_renderer = || -> Result<()> {
            let now = isolate();
            if renderer.borrow().is_some() && now == *renderer.borrow() {
                return Ok(());
            }
            Err(Refusal::new(
                ValidatorId::RunIncomplete,
                "the page moved to another renderer process during a profiled run (a cross-site navigation), or strobe could not read which one it is in, and the profilers stay with the first: the profile would not cover the run",
            )
            .fix("profile a run that stays on one site (load the first page in setup), or attribute the other site's page from its own case")
            .into())
        };
        let profile_start = || -> Result<()> {
            *renderer.borrow_mut() = isolate();
            same_renderer()?;
            probe::cpu::start(cp()?)?;
            probe::alloc::start(cp()?)?;
            profiling.set(true);
            Ok(())
        };
        let profile_stop = || -> Result<()> {
            profiling.set(false);
            same_renderer()?;
            profiles.borrow_mut().push((probe::cpu::stop(cp()?)?, probe::alloc::stop(cp()?)?));
            Ok(())
        };
        let mocks = mocks::Mocks::default();
        // An init script in an engine without preload scripts (WebKit): it ran after the first document's
        // own scripts, which strobe counts (`init_docs`, after the block's first load). A document loaded
        // after that (a `goto`, a click that navigates, the page's own navigation) would run its scripts
        // before the init script: refused, whatever loaded it.
        let late_init = Cell::new(false);
        let init_docs: Cell<Option<u64>> = Cell::new(None);
        let prepared = Cell::new(false);
        let late_check = || -> Result<()> {
            match (init_docs.get(), page.documents()) {
                (Some(first), Some(now)) if late_init.get() && now > first => Err(Refusal::new(
                    ValidatorId::EngineUnsupported,
                    format!("the block loaded another document after ctx.addInitScript in {engine}: it has no preload script, so that document's own scripts ran before the init script"),
                )
                .fix("measure the case in Chromium or Firefox, or keep it on the first document (no navigation after the init script)")
                .into()),
                _ => Ok(()),
            }
        };
        let cost = SelfCost::new(ctx.runner.pid());
        // The run's untimed stretches, read inside each (`Hooks::untimed`): the browser's processes and
        // the page's counters (with what a departed document carried) at each edge, the page's counter
        // global (what it counted in a stretch leaves the run's counts), and strobe's reads of the page
        // before the first. A trace marks each stretch, which its readers take out of its run
        // (`metrics::read`).
        let edges: RefCell<Vec<WebEdge>> = RefCell::new(Vec::new());
        let reads_before = Cell::new((0u64, 0.0f64));
        let counted_before: RefCell<BTreeMap<String, f64>> = RefCell::new(BTreeMap::new());
        // strobe's own calls into the page at an edge (its reads, marks, counts and the OS input's
        // checks) stay inside the stretch: after the processes' readings at its start, before them at
        // its end, so none of their round trips is the run's CPU.
        let reading = || {
            let procs = browser.processes();
            let tsc = host::tsc();
            let (c, carry) = (counters(), *carried.borrow());
            edges.borrow_mut().push((super::record::per_process(&procs), tsc, std::array::from_fn(|i| c[i] + carry[i])));
        };
        // The OS input posted on each side of an edge reached the page, counted on its own: input in a
        // stretch never covers for a timed one lost (or the other way round).
        let os_edge = || -> Result<()> {
            if let Some(os) = driver.os.get() {
                os.delivered(&page)?;
                os.placed(&page, "run")?;
                os.begin(&page)?;
            }
            Ok(())
        };
        // A trace's mark goes nearest the stretch's start: the trace's readers cut the run there.
        let edge = |start: bool, n: usize| -> Result<()> {
            if start {
                cost.pause();
            }
            // The profilers leave the stretch out: stopped at its start (with the scripts' ranges, as the
            // stretch may leave the page), started again in the page's renderer at its end.
            if start && profiling.get() {
                profile_stop()?;
                ranges.borrow_mut().merge(probe::symbols::Ranges::take(&page)?);
                paused.set(true);
            }
            if !start && paused.take() {
                profile_start()?;
            }
            if !traceless {
                driver.mark(&hooks.untimed_mark(n, start))?;
            }
            if start {
                reading();
            }
            let (k, ms) = driver.take_reads();
            if start {
                reads_before.set((reads_before.get().0 + k, reads_before.get().1 + ms));
            }
            if !counted.is_empty() {
                let now = page_counts();
                if start {
                    *counted_before.borrow_mut() = now;
                } else {
                    let before = counted_before.take();
                    hooks.add(&now.iter().map(|(k, v)| (k.clone(), before.get(k).copied().unwrap_or(0.0) - v)).collect());
                }
            }
            os_edge()?;
            if !start {
                reading();
                cost.resume();
            }
            Ok(())
        };
        // `--video`: the block's film, and the inputs strobe sent while it ran (for its markers).
        let video: RefCell<Option<super::video::Recorder>> = RefCell::new(None);
        let video_sent: RefCell<Vec<super::video::Sent>> = RefCell::new(Vec::new());
        // With a trace, the phases' host-clock edges for the video (traceless blocks keep them in `run_marks`).
        let video_marks: RefCell<Vec<(String, u64)>> = RefCell::new(Vec::new());
        let serve = |op: &str, a: &Value| -> Result<Value> {
            match op {
                "driver" => {
                    let call = a["call"].as_str().unwrap_or("");
                    let args = a["args"].as_array().cloned().unwrap_or_default();
                    hooks.op(call, &args);
                    // From outside a page gets the OS's input (`os::OsInput`); one it cannot reach would get
                    // its driver's, where the other side gets the OS's (SPEC §9.10).
                    if let Some(r) = no_os_input.as_ref().filter(|_| hooks.in_run() && crate::driver::os::INPUT.contains(&call)) {
                        return Err(
                            Refusal { summary: format!("page.{call}() in a run of a {engine} page measured from outside: {}, and the other side gets the OS's", r.summary), ..r.clone() }.into()
                        );
                    }
                    if emulation.mobile
                        && let Some(r) = phone_pointer(call, &args)
                    {
                        return Err(r.into());
                    }
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
                        if changes {
                            harvested.borrow_mut().extend(page_changes(&page));
                        }
                    }
                    // The profilers go on through a navigation; the scripts of the document it leaves
                    // go with it, so their function ranges are read first, and the page must still be in
                    // the profilers' renderer after it (A → B → A would drop B's samples unseen).
                    let profiled_goto = call == "goto" && profiling.get();
                    if profiled_goto {
                        ranges.borrow_mut().merge(probe::symbols::Ranges::take(&page)?);
                    }
                    let (started, sent0) = (host::now_ns(), driver.inputs.borrow().len());
                    let r = driver.dispatch(call, &args);
                    if profiled_goto && r.is_ok() {
                        same_renderer()?;
                    }
                    if let Some(v) = video.borrow().as_ref() {
                        v.note(call, &args, started);
                        video_sent.borrow_mut().extend(driver.inputs.borrow().iter().skip(sent0).map(|s| super::video::Sent { t_ns: s.t_ns, press_ns: s.press }));
                    }
                    late_check()?;
                    r
                }
                // `ctx.mock` on an engine without the runner's CDP session: strobe answers through the page's
                // network interception (`mocks`), asking the runner's handlers.
                "mock" => mocks.add(&page, a),
                // `ctx.addInitScript` (prepare, setup) through strobe's own connection, so it works in every
                // engine: before the page's scripts in each new document. WebKit has no preload: the script runs
                // once strobe first touches the document, so only in `prepare` (the first document is still to
                // load), and a document loaded later refuses the block (`late_check`).
                "init" => {
                    let source = a["source"].as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| Error::usage("addInitScript(script): a function, or JavaScript as a string"))?;
                    if engine == WebEngine::Webkit && prepared.get() {
                        return Err(Refusal::new(
                            ValidatorId::EngineUnsupported,
                            format!("ctx.addInitScript in setup in {engine}: the first document already loaded, and it has no preload script for the next"),
                        )
                        .fix("add the init script in the target's prepare, or measure the case in Chromium or Firefox")
                        .into());
                    }
                    // A line comment at its end must not swallow what an engine wraps it in.
                    page.add_init_script(&format!("{source}\n"))?;
                    late_init.set(engine == WebEngine::Webkit);
                    Ok(Value::Bool(true))
                }
                "count" => {
                    hooks.count(a);
                    Ok(Value::Bool(true))
                }
                // A page's phases are marks in its own trace, read from there with the run's frames; from
                // outside, or in an engine without that trace, strobe's own marks on the host clock.
                "phase" => {
                    if traceless {
                        hooks.phase(a)?;
                    } else if let Some(key) = hooks.phase_key(a)? {
                        driver.mark(&key)?;
                        // The trace's marks are the page's clock: the video's phases need the host's.
                        if video.borrow().is_some() {
                            video_marks.borrow_mut().push((key, host::now_ns()));
                        }
                    }
                    Ok(Value::Bool(true))
                }
                "untimed" => hooks.untimed(a, edge),
                "line" => Err(Error::usage("waitForLine reads a native app's output or the log files under its home; a page has neither (wait for the page instead: page.waitFor)")),
                other => Err(Error::msg(format!("unknown runner request {other}"))),
            }
        };
        let t = s.hook_timeout();
        let mut block = json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": url, "root": v.root, "engine": engine });
        if let (Some(b), Some(a)) = (block.as_object_mut(), hook_app.as_object()) {
            b.extend(a.clone());
        }
        ctx.runner.call("block", block, t, &serve)?;
        // `prepare` (and a target `url` function) may name the first page, so a block loads one page, not two.
        let mut first = url.clone();
        if target["prepare"].as_bool() == Some(true) {
            // Playwright connects over CDP: Chromium only (the runner's `playwright` throws elsewhere).
            let (endpoint, target_id) = match (browser.cdp(), page.cdp()) {
                (Some(b), Some(p)) => (json!(b.ws), json!(p.target)),
                _ => (Value::Null, Value::Null),
            };
            let r = ctx
                .runner
                .call("prepare", json!({ "endpoint": endpoint, "target": target_id, "engine": engine }), t, &serve)
                .map_err(|e| crate::runner::incomplete("prepare", "prepare", e, "", "fix the target's prepare in the bench file"))?;
            if let Some(u) = r.get("url").and_then(Value::as_str) {
                first = join_url(origin(&url), u);
            }
        }
        lap("prepare");
        prepared.set(true);
        if !app {
            page.navigate(&first, Duration::from_secs(60))?;
        }
        init_docs.set(page.documents());
        lap("load");
        validity::dev_build(&page)?;
        let page_seen = if app { observed_input(ctx, s, &page) } else { Some(validity::emulated_input(&page, &emulation)?) };
        let (input, reduced_motion) = (page_seen.map(|s| s.0), page_seen.map(|s| s.1));
        if reduce.reduced() {
            motion::require(reduced_motion == Some(true), motion_how(kind, engine))?;
        }
        let mut during = Some(Foreign::start(&browser));
        let speed = validity::speed_check(&page)?;
        // The calibration is Chromium's: another engine's JS speed is compared with its own reference
        // only, which a check under other work never joins.
        let mut loaded: Option<OtherWork> = None;
        let checked = match engine {
            WebEngine::Chromium => validity::throttled(speed, ctx.baseline),
            WebEngine::Firefox | WebEngine::Webkit => {
                let f = during.take().map(|d| d.stop(ctx.pin, &browser)).unwrap_or_default();
                // Only a check known to have run without other work joins the engine's reference.
                let keep = f.0.is_some_and(|x| x < BACKGROUND_PCT);
                loaded = Some(f);
                validity::engine_throttled(engine, ctx.pin, speed, keep)
            }
        };
        let ratio = match checked {
            // A slow check while other work kept the target's CPUs busy (the browser's own processes
            // not counted) is that work's doing, as for a native block: `Background`, which the bench
            // waits out, not `Throttled`.
            Err(e) => {
                let (foreign, busiest) = loaded.or_else(|| during.map(|d| d.stop(ctx.pin, &browser))).unwrap_or_default();
                let who = busiest.iter().map(|(n, p)| format!("{n} {p:.0}%")).collect::<Vec<_>>().join(", ");
                return Err(match foreign {
                    Some(f) if f >= BACKGROUND_PCT => {
                        Refusal::new(ValidatorId::Background, format!("the speed check ran slow while other work kept the target's CPUs {f:.0}% of a core busy ({})", e.message))
                            .detail(format!("busiest processes on the machine (% of a core): {}", if who.is_empty() { "none named (system work)" } else { &who }))
                            .fix("let that work finish (an antivirus scanning new files, an update, a build), or exclude it from the bench's CPUs")
                            .into()
                    }
                    // Too little other work to be the cause (or none readable), but named, so a reader can tell.
                    _ => match e.refusal {
                        Some(r) => {
                            let on_cpus = foreign.map_or("could not be read".to_string(), |f| format!("{f:.0}% of a core"));
                            let busiest = if who.is_empty() { "nothing outside strobe".to_string() } else { format!("busiest processes on the machine (% of a core): {who}") };
                            (*r).detail(format!("other work on the target's CPUs meanwhile: {on_cpus}; {busiest}")).into()
                        }
                        None => e,
                    },
                });
            }
            Ok(r) => r,
        };
        // A leak run reads heaps, not pixels: on a locked screen the hidden page is still measured.
        let hidden_ok = set == Set::Memory && host::session_locked();
        if !hidden_ok {
            validity::window_visible(&page)?;
            front_page(s, &browser)?;
        }
        // From outside, every input of the block (setup's and reset's too) comes from the OS, as the
        // native side's does: set up once the window shows and is in front.
        if os_input {
            let os = crate::driver::os::OsInput::attach(&page, browser.pids(), presents_rc.clone())?;
            os.vouched.set(s.expect["same"].as_array().is_some_and(|a| !a.is_empty()));
            let _ = driver.os.set(os);
        } else if os_frames_on {
            // Headed in an engine without frames of its own: the page's clicks are held, and its measured
            // inputs wait, on the OS's presents of its browser (SPEC 7.4), as the OS input's are.
            let refresh = host::main_window(&browser.pids()).and_then(|w| host::display_of(w.hwnd)).map(|d| 1000.0 / d.hz);
            driver.watch(presents_rc.clone(), browser.pids(), refresh);
        }
        lap("checks");
        ctx.runner.call("setup", json!({}), t, &serve).map_err(|e| crate::runner::incomplete("setup", "setup", e, &snapshot(&page, &ctx.exp, "setup"), "fix the bench file's setup"))?;
        if late_init.get() && !ctx.warnings.iter().any(|w| w.id == "InitScript") {
            ctx.warnings.push(validity::info("InitScript", format!("{}: {engine} has no preload script: ctx.addInitScript ran after the first document's own scripts; a block that loads another document is refused", s.label())));
        }
        hooks.check_fixture();
        // A page without WebGPU (an engine or OS that does not ship it, or an origin that is not a secure
        // context) draws no frame the canvas hook sees: canvas.* refused, never 0. Read on the page the
        // runs start from, after setup.
        let no_webgpu = canvas && page.eval("typeof GPUCanvasContext === 'undefined'")?.as_bool() == Some(true);
        // Setup's OS input reached the page, and setup left the window where strobe mapped it (or it is
        // mapped again).
        if let Some(os) = driver.os.get() {
            os.delivered(&page)?;
            os.recheck(&page)?;
        }
        // `expect.same` (SPEC §9.10): read after setup, each run and each reset, outside the runs.
        let same = journey::Same::new(s, &ctx.describe["targets"])?;
        let read_same = |when| same.read(when, &|sel| crate::driver::text(&page, sel, Duration::from_secs(2)));
        if same.on() {
            read_same(journey::When::Setup)?;
        }
        let start = shown::page(ctx, s, v, pair, "start", &page);
        shown::expect(&s.expect, &start, "before the runs", true, &|sel| shown::page_shows(&page, sel))?;
        lap("setup");
        // `--video` (an untimed block): from before the first run to after the last.
        if x.video {
            let w = host::main_window(&browser.pids()).filter(|_| !headless);
            match super::video::Recorder::start(w.as_ref(), page.cdp(), headless) {
                Ok(r) => *video.borrow_mut() = Some(r),
                Err(e) => ctx.warnings.push(validity::warn("Video", format!("{} · {side} block: no video: {}", s.label(), e.message))),
            }
        }
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
        // Without a trace (`traceless`) the marks are strobe's own on the host clock, the frames the OS's.
        let tracing = match browser.cdp() {
            Some(b) if set != Set::Memory && !x.counters && !traceless => Some(probe::trace::start_with(&b.cdp, set, &extra_categories)?),
            _ => None,
        };
        // A kept recording is claimed for this block alone and discarded on any error before its meta.json.
        let mut draft: Option<store::Draft> = None;
        let mem_dir = if set == Set::Memory { Some(draft.insert(store::claim_recording(&id)?).dir().to_path_buf()) } else { None };
        // A Firefox attribution block takes its about:memory census before its profiler starts, into
        // the recording claimed now: the profiler's own memory (tens of MB per process on Linux) would
        // count as the page's.
        let gecko_dir = if gecko_on { Some(draft.insert(store::claim_recording(&id)?).dir().to_path_buf()) } else { None };
        // A leak run measures 2 × N runs (`Warmup`): N to S1 and N more to S2.
        let mut census: Vec<Value> = Vec::new();
        // GPU engines and VRAM of the target's process tree, read from the OS (SPEC §13.6).
        let sampler = probe::os::Sampler::start(browser.pid(), browser.profile());
        let sampled = Instant::now();
        // Renderer main-thread task time, a cumulative counter: the live work signal for warmup, and
        // main.busy when the trace carries no RunTask slices.
        if let Some(p) = page.cdp() {
            p.send("Performance.enable", json!({}))?;
        }
        // `net.*` metrics: requests and bytes per run from the Network domain (Firefox: BiDi's network
        // events), on only for such series.
        let net = match (&browser, &page) {
            _ if nets.is_empty() => None,
            (WebBrowser::Chromium(b), Page::Chromium(p)) => {
                let rx = b.cdp.events();
                p.send("Network.enable", json!({}))?;
                Some(RefCell::new(Net::new(rx, p.session.clone())))
            }
            (WebBrowser::Firefox(b), Page::Firefox(p)) => {
                let rx = b.bidi.events();
                p.enable_network()?;
                Some(RefCell::new(Net::new(rx, p.context.clone())))
            }
            // WebDriver Classic reports no requests: `net.*` is refused (`validity::engine_refusal`).
            (WebBrowser::Webkit(_), _) | (WebBrowser::Chromium(_), Page::Firefox(_) | Page::Webkit(_)) | (WebBrowser::Firefox(_), Page::Chromium(_) | Page::Webkit(_)) => None,
        };
        // What reached the screen comes from the OS's present events of the browser's processes (§7.2),
        // as for a native app (`os_frames_on`). The capture follows the browser's windows (macOS and X11
        // stream them by window).
        // The OS input's waits read it live (`presents_rc`).
        let presents = os_frames_on.then(|| {
            probe::present::PresentCapture::start().map(|mut c| {
                c.track(&browser.pids());
                *presents_rc.borrow_mut() = Some(c);
            })
        });
        let run_counts: RefCell<Vec<BTreeMap<String, f64>>> = RefCell::new(Vec::new());
        let run_canvas: RefCell<Vec<Vec<Vec<f64>>>> = RefCell::new(Vec::new());
        // Without a trace: each run's host-clock phase marks, and its inputs with the page's own changes,
        // joined to the OS's presents after the runs.
        let run_marks: RefCell<Vec<Vec<(String, u64)>>> = RefCell::new(Vec::new());
        // WebKit: a run loaded a document, in which strobe's page scripts ran only after the page's own.
        let loaded_document = Cell::new(false);
        let run_inputs: RefCell<Vec<RunInputs>> = RefCell::new(Vec::new());

        let run_timeout = s.run_timeout();
        let perf = x.counters.then(probe::wakes::Perf::new);
        // `profile`: a measured run of a Chromium attribution block; `last`: the last of them.
        let mut run_once = |key: &str, profile: bool, last: bool| -> Result<Run> {
            let began = Instant::now();
            // As a native app is before each run (`front_page`).
            if cfg!(target_os = "macos") && (s.sides.is_some() || s.cross_engine) {
                host::raise_windows(&browser.pids());
            }
            hooks.begin(key);
            // The target's CPU in TSC cycles over TSC time: process times move in 15.6 ms ticks, which
            // made an idle 5 s run read 0, 0.31% or 0.62% and nothing between. One read of the
            // target's processes at each end of the run gives its cycles, times and roles.
            // Each process's cycles by pid and start time (`record::per_process`), read at once.
            let read = || {
                let procs = browser.processes();
                let tsc = host::tsc();
                let (cycles, cpu) = procs.iter().fold((0u64, 0u64), |(c, t), p| (c + p.cycles, t + p.cpu_ns));
                (super::record::per_process(&procs), cycles, tsc, cpu)
            };
            let mut roles = process_roles(&browser);
            // Strobe's own calls into the page (resetting its counters, its marks, the Performance
            // domain's counters) stay outside the CPU window, which brackets the run alone: their
            // round trips are not the run's work.
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
            driver.inputs.borrow_mut().clear();
            driver.set_pids(browser.pids());
            // The capture follows windows that opened since (the drivers' settles read it without looking).
            if let Some(p) = presents_rc.borrow_mut().as_mut() {
                p.track(&browser.pids());
            }
            if changes {
                page_changes(&page);
                harvested.take();
            }
            let docs = page.documents();
            driver.take_reads();
            edges.borrow_mut().clear();
            reads_before.set((0, 0.0));
            if let Some(os) = driver.os.get() {
                os.set_pids(browser.pids());
                os.take_log();
                os.begin(&page)?;
            }
            // From outside the run is strobe's own span on the host clock, as a native app's: no page
            // mark, and no wait for the page's next frame.
            if !traceless || gecko_on {
                driver.mark(&format!("{key}:start"))?;
            }
            let listed = probe::wakes::wall_us();
            let (at0, cyc0, tsc0, _) = read();
            let k0 = probe::wakes::Counters::take(listed, &at0.keys().map(|k| k.0).collect::<Vec<_>>(), perf.as_ref());
            if profile {
                profile_start()?;
            }
            // strobe's own cost over the run.
            cost.start();
            let t0 = host::now_ns();
            ctx.runner
                .call("run", json!({ "case": s.case }), run_timeout, &serve)
                .map_err(|e| crate::runner::incomplete(&format!("{} run", s.case), "run", e, &snapshot(&page, &ctx.exp, "run"), "the run threw or timed out; fix the bench file or the app"))?;
            // The page's next frame ends the run (what its last step drew), except after an untimed
            // stretch, whose own frames are none of the run's.
            if !outside && !hooks.ended_untimed() {
                driver.dispatch("frame", &[])?;
            }
            let t1 = host::now_ns();
            cost.end();
            // A run that ended inside an untimed stretch left its profilers stopped already.
            paused.set(false);
            if profiling.get() {
                profile_stop()?;
            }
            // The page's scripts' function ranges, before the reset can leave the page.
            if profile && (last || s.reset) {
                ranges.borrow_mut().merge(probe::symbols::Ranges::take(&page)?);
            }
            let timed = metrics::Timed { t0, t1, untimed: hooks.untimed_stretches() };
            let (at1, cyc1, tsc1, cpu1) = read();
            let k1 = probe::wakes::Counters::take(probe::wakes::wall_us(), &at1.keys().map(|k| k.0).collect::<Vec<_>>(), perf.as_ref());
            // OS input's log: its inputs for latency, its drags for `input.rate` (those begun untimed left out).
            let sent = driver.os.get().map(|os| os.take_log());
            // The OS's input (from outside) is the video's markers' edges, as the driver's are.
            if let (Some(l), true) = (&sent, video.borrow().is_some()) {
                video_sent.borrow_mut().extend(l.sent(&metrics::Timed { t0, t1, untimed: Vec::new() }).into_iter().map(|s| super::video::Sent { t_ns: s.t_ns, press_ns: s.press }));
            }
            let mut drags = sent.as_ref().map_or_else(|| driver.drags.take(), |l| l.drags.clone());
            drags.retain(|d| d.first().is_none_or(|t| !timed.untimed_at(*t)));
            let input = crate::driver::input_pacing(&drags);
            let c1 = counters();
            if !traceless || gecko_on {
                driver.mark(&format!("{key}:end"))?;
            }
            // Read once the run's span and CPU window closed: the page's own count of its time.
            let reads = driver.take_reads();
            let reads = (reads.0 + reads_before.get().0, reads.1 + reads_before.get().1);
            roles.extend(process_roles(&browser));
            // The readings at the run's start, each untimed stretch's edges and the run's end: each pair
            // a timed part (a stretch the run never ended ends with it). The cycle clock's and the page
            // counters' untimed time is the sum over the stretches (`gap`).
            let carry_end = *carried.borrow();
            let mut e = edges.take();
            let end: WebEdge = (at1.clone(), tsc1, std::array::from_fn(|i| c1[i] + carry_end[i]));
            if e.len() % 2 == 1 {
                e.push(end.clone());
            }
            let mut gap = (0u64, [0.0f64; 6]);
            for [a, b] in e.as_chunks::<2>().0 {
                gap.0 += b.1.zip(a.1).map_or(0, |(y, x)| y.saturating_sub(x));
                for i in 0..6 {
                    gap.1[i] += b.2[i] - a.2[i];
                }
            }
            let mut cycles_at: Vec<&super::record::Cycles> = vec![&at0];
            cycles_at.extend(e.iter().map(|x| &x.0));
            cycles_at.push(&at1);
            let timed_cycles = |keep: &dyn Fn(u32) -> bool| super::record::cycles_timed(&cycles_at, keep);
            if !counted.is_empty() {
                hooks.add(&page_counts());
            }
            take_canvas();
            run_canvas.borrow_mut().push(canvas_docs.take());
            // The run ends before its counts are kept: a fixture's CPU over the run is one of them.
            hooks.end();
            run_marks.borrow_mut().push(hooks.take_marks());
            let index = run_counts.borrow().len();
            run_counts.borrow_mut().push(hooks.counts());
            let timed_input = |s: &probe::present::frames::Sent| !timed.untimed_at(s.t_ns) && !s.press.is_some_and(|p| timed.untimed_at(p));
            // Keys typed in a burst: a trace times each key's events, but no frame is any one key's.
            burst_keys.set(burst_keys.get() + driver.inputs.borrow().iter().filter(|s| s.burst && timed_input(s)).count());
            if changes {
                let mut seen = harvested.take();
                seen.extend(page_changes(&page));
                let mut inputs: Vec<probe::present::frames::Sent> = sent.as_ref().map_or_else(|| driver.inputs.take().into_iter().filter(timed_input).collect(), |l| l.sent(&timed));
                seen.acted(&mut inputs);
                run_inputs.borrow_mut().push((inputs, seen.changes, timed.clone()));
            }
            if page.documents() != docs {
                loaded_document.set(true);
            }
            late_check()?;
            if let Some(n) = &net {
                n.borrow_mut().drain(Some(index));
            }
            // Whether the OS input reached the page, and where strobe mapped it, read after the run.
            if let Some(os) = driver.os.get() {
                os.delivered(&page)?;
                os.placed(&page, "run")?;
            }
            // A key or button the run left held would be down in the next one: let go, outside its span.
            driver.release_all()?;
            if same.on() {
                read_same(journey::When::Run)?;
            }
            if s.reset {
                if let Some(os) = driver.os.get() {
                    os.begin(&page)?;
                }
                ctx.runner
                    .call("reset", json!({ "case": s.case }), run_timeout, &serve)
                    .map_err(|e| crate::runner::incomplete(&format!("{} reset", s.case), "reset", e, &snapshot(&page, &ctx.exp, "reset"), "fix the case's reset"))?;
                driver.release_all()?;
                // A reset's OS input is checked on its own: a click there must not cover a run's lost one.
                if let Some(os) = driver.os.get() {
                    os.delivered(&page)?;
                    os.placed(&page, "reset")?;
                }
                if same.on() {
                    read_same(journey::When::Reset)?;
                }
            }
            // The run's timed time, and its cycles over the cycle clock's timed time: the untimed
            // stretches leave both (`gap`).
            let wall = timed.ns() as f64;
            let share = |cycles: u64, untimed_tsc: u64| match (tsc0, tsc1) {
                (Some(a), Some(b)) if b > a + untimed_tsc => Some(100.0 * cycles as f64 / (b - a - untimed_tsc) as f64),
                _ => None,
            };
            let cpu = match share(super::record::cycles_used(&at0, &at1), 0) {
                // No process readings (an OS without a process sampler yet): unknown, never 0%.
                _ if cyc1 == 0 && cpu1 == 0 => (f64::NAN, f64::NAN),
                Some(whole) if cyc0 > 0 => (share(timed_cycles(&|_| true), gap.0).unwrap_or(f64::NAN), whole),
                // Never from process times, which move in 15.6 ms ticks on Windows (where cycles are not
                // time, as on Windows on Arm before 11 24H2, the metric is refused instead).
                _ => (f64::NAN, f64::NAN),
            };
            let (cpu, cpu_whole) = cpu;
            let carry = *carried.borrow();
            let parts = std::array::from_fn(|i| c1[i + 1] + carry[i + 1] - c0[i + 1] - gap.1[i + 1]);
            // Cycles only: a process's share needs the exact counter, and a process born during the run
            // (a renderer after a cross-site navigation) counts from zero. A role no process has (a browser
            // that composites in its own process: no GPU process) is absent, never 0.
            let roles = std::array::from_fn(|i| match share(timed_cycles(&|pid| roles.get(&pid) == Some(&i)), gap.0) {
                Some(pct) if roles.values().any(|&r| r == i) => pct,
                _ => f64::NAN,
            });
            let counted = probe::wakes::counted(&k0, &k1);
            let work = cpu * wall / 1e6 / 100.0;
            let task = c1[0] + carry[0] - c0[0] - gap.1[0];
            Ok(Run { ms: wall / 1e6, cpu, cpu_whole, task, work, parts, roles, t0, t1, timed, total: began.elapsed().as_secs_f64() * 1e3, input, counted, reads })
        };
        // Warm until two consecutive runs do the same main-thread work (within 5%); both of them count
        // as measured (SPEC §9.2). Wall time says nothing here: a fixed-speed scroll or paced typing
        // takes the same time cold or warm, while the work it costs settles over 1–4 runs. Attribution
        // blocks start their profiler at the steady point, so they measure the runs after it.
        let mut runs: Vec<Run> = Vec::new();
        let mut nodes_all = Vec::new();
        let mut warmup = block::Warmup::new(s, x);
        let mut starts = shown::Starts::new(s, x);
        let read_start = || shown::page_start(&page);
        let profiled = set == Set::Attribution && !gecko_on;
        let wait_start = |t: Duration| {
            let _ = crate::driver::wait_change(&page, t);
        };
        // Firefox's profiler, from the steady point to the end of the runs.
        let mut gecko: Option<probe::gecko::Profiler> = None;
        while warmup.more(runs.len()) {
            let i = runs.len();
            nodes_all.push(driver.nodes().unwrap_or(0));
            starts.check(i + 1, false, &read_start, &wait_start)?;
            let profile = profiled && warmup.measures(i);
            // A check's run after its reset is never measured: the trace names it a warmup run, so no
            // view of the recording counts it.
            let unmeasured = warmup.first().is_some() && i >= warmup.measured().end;
            let key = if unmeasured { format!("warm:{i}") } else { format!("run:{i}") };
            runs.push(run_once(&key, profile, profile && i + 1 == warmup.measured().end)?);
            if let Some(f) = warmup.first() {
                // Leak run: a post-GC census after every run; S1 after N runs, S2 after 2 × N. Firefox
                // takes about:memory's after its own GC and CC, and no heap snapshot.
                if let (Some(dir), Some(b)) = (&mem_dir, firefox) {
                    let done = runs.len() - f;
                    census.push(probe::gecko::census(b, &page, &dir.join(format!("memory-{done}.json.gz")), &format!("after run {done} of the leak run"))?);
                } else if let Some(dir) = &mem_dir {
                    let mut c = probe::census::take(cb()?, cp()?);
                    c["os_after_snapshot"] = json!(true);
                    let done = runs.len() - f;
                    if done == 2 * n {
                        c["allocators"] = probe::census::allocators(cb()?, &dir.join("memdump-s2.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
                    }
                    census.push(c);
                    if done == n {
                        probe::heap::snapshot(cp()?, &dir.join("heap-s1.heapsnapshot.gz"))?;
                    }
                    if done == 2 * n {
                        probe::heap::snapshot(cp()?, &dir.join("heap-0.heapsnapshot.gz"))?;
                    }
                }
                continue;
            }
            // The work that must hold steady is the renderer's main-thread time. `--check` measures the
            // first run: it asks whether a case works, not what it costs. From outside or in an engine
            // without that counter, the work is the processes' CPU, as a native app's; where this machine
            // gives no CPU time (cycles refused), the run's wall time.
            let work: Vec<f64> = runs.iter().map(|r| if !traceless { r.task } else if r.work.is_finite() { r.work } else { r.ms }).collect();
            if warmup.after(&work) && !x.untimed {
                if let (Some(dir), Some(b)) = (&gecko_dir, firefox) {
                    match probe::gecko::census(b, &page, &dir.join("memory-0.json.gz"), "after warmup, before the profiler started and the measured runs") {
                        Ok(c) => census.push(c),
                        Err(e) => ctx.warnings.push(validity::warn("Census", format!("{} about:memory: {}", side, e.message))),
                    }
                    gecko = Some(probe::gecko::start(b)?);
                }
                if let (Some(dir), Some(b)) = (&mem_dir, firefox) {
                    census.push(probe::gecko::census(b, &page, &dir.join("memory-s0.json.gz"), "after warmup, before the leak run's runs (S0)")?);
                } else if let Some(dir) = &mem_dir {
                    let mut c = probe::census::take(cb()?, cp()?);
                    c["allocators"] = probe::census::allocators(cb()?, &dir.join("memdump-s0.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
                    census.push(c);
                    probe::heap::snapshot(cp()?, &dir.join("heap-s0.heapsnapshot.gz"))?;
                }
            }
        }
        starts.check(runs.len() + 1, true, &read_start, &wait_start)?;
        let mut stopped = video.take().map(super::video::Recorder::stop);
        let unsteady = warmup.unsteady();
        let first = warmup.first().unwrap_or(0);
        // Warmup and measured runs interleave with the steadiness check: split the loop by run.
        // `+ 0.0`: an empty f64 sum is -0.0, which would print as "-0.0s".
        let warm_ms: f64 = runs[..first].iter().map(|r| r.total).sum::<f64>() + 0.0;
        steps.borrow_mut().push(("warmup".into(), warm_ms));
        steps.borrow_mut().push(("runs".into(), (lapped.get().elapsed().as_secs_f64() * 1e3 - warm_ms).max(0.0)));
        lapped.set(Instant::now());
        // A check's run after its reset is not measured (`Warmup::measured`).
        let range = first..warmup.measured().end.clamp(first, runs.len());
        let measured = &runs[range.clone()];
        let key = |j: usize| format!("run:{}", first + j);
        let run_ms: Vec<f64> = measured.iter().map(|r| r.ms).collect();
        let walls: BTreeMap<String, f64> = measured.iter().enumerate().map(|(j, r)| (key(j), r.ms)).collect();
        let cpu: BTreeMap<String, f64> = measured.iter().enumerate().map(|(j, r)| (key(j), r.cpu)).collect();
        let busy: BTreeMap<String, f64> = measured.iter().enumerate().filter(|(_, r)| r.task.is_finite() && r.ms > 0.0).map(|(j, r)| (key(j), 100.0 * r.task / r.ms)).collect();
        let work: BTreeMap<String, f64> = measured.iter().enumerate().filter(|(_, r)| r.task.is_finite()).map(|(j, r)| (key(j), r.task)).collect();
        let spans: Vec<(String, u64, u64)> = measured.iter().enumerate().map(|(j, r)| (key(j), r.t0, r.t1)).collect();
        let nodes = nodes_all.get(range.clone()).unwrap_or_default().to_vec();
        let warm = runs[..first].to_vec();
        let (strobe_pct, runner_pct) = cost.share(range.clone());
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
                r.cpu_whole.is_finite().then(|| (busy - r.cpu_whole).max(0.0))
            })
            .collect();
        let background = busiest_others(&procs0, &procs1);
        let screen = screen_state(&procs0, &procs1);
        let gpu_3d: BTreeMap<String, f64> = gpu_runs.iter().filter_map(|(k, s)| Some((k.clone(), s.gpu_3d_pct?))).collect();
        let vram = gpu_runs.last().and_then(|(_, s)| s.vram_dedicated_mb);
        // The window must have stayed visible for the whole block, and the browser strobe's alone.
        if !hidden_ok {
            validity::window_visible(&page)?;
            front_page(s, &browser)?;
        }
        browser.own()?;
        let private_mb = browser.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
        let pss = host::pss_bytes(&browser.processes().iter().map(|p| p.pid).collect::<Vec<_>>());
        // What the page had to draw in: its viewport in CSS pixels (device-independent at zoom 1) and
        // its scale, compared with the other side's content area (`admit_window`).
        let content = page.eval("[innerWidth, innerHeight, devicePixelRatio]").ok().and_then(|v| Some([v[0].as_f64()?, v[1].as_f64()?, v[2].as_f64()?]));
        let heap_mb = page.cdp().and_then(|p| p.send("Runtime.getHeapUsage", json!({})).ok()).and_then(|h| h["usedSize"].as_f64()).map(|b| b / (1024.0 * 1024.0));
        let dom_nodes = driver.nodes().ok();
        // What reached the screen in the measured runs (Firefox, headed), and the inputs it showed.
        let presents = presents.map(|r| r.and_then(|_| presents_rc.borrow_mut().take().ok_or_else(|| Error::msg("the present capture ended early"))).and_then(|c| c.stop()));
        let dir = match (&draft, keep) {
            (Some(d), _) => d.dir().to_path_buf(),
            (None, true) => draft.insert(store::claim_recording(&id)?).dir().to_path_buf(),
            (None, false) => host::paths::run().join(&id),
        };
        std::fs::create_dir_all(&dir)?;
        let leak = if set == Set::Memory { Some(leak_analysis(&census, &dir, n)?) } else { None };
        // The interval the page's sampler kept (SPEC §8.3), kept in the recording's meta.
        let mut sampling = None;
        // Firefox's profile becomes the recording's CPU profiles (each page thread's samples inside each
        // measured run's own span) and its style, layout and GC rows, then the page's source maps.
        if let Some(g) = gecko.take() {
            let raw = g.stop()?;
            let untimed: Vec<(u64, u64)> = measured.iter().flat_map(|r| r.timed.untimed.iter().copied()).collect();
            let got = probe::gecko::convert(&raw, &token, &spans, &untimed, &dir)?;
            sampling = got.sampling;
            let build = crate::target::isolate::build_for_os(&ctx.config["build"]);
            let served = v.root.as_ref().zip(ctx.config["serve"]["dir"].as_str().or(crate::target::isolate::per_os(&build["output"]))).map(|(r, d)| r.join(d));
            probe::symbols::store_maps(&page, &got.scripts, &dir, served.as_deref())?;
            lap("maps");
        } else if profiled {
            let profiles = profiles.take();
            sampling = probe::cpu::Sampling::of(probe::cpu::interval_us() as f64, profiles.iter().flat_map(|(p, _)| probe::cpu::gaps(p)).collect());
            for (k, (profile, allocs)) in profiles.iter().enumerate() {
                store::write_json(&dir.join(format!("cpu-{k}.cpuprofile")), profile)?;
                store::write_json(&dir.join(format!("alloc-{k}.heapprofile")), allocs)?;
            }
            // The page's source maps, kept with the recording while its server still runs, so any view
            // can resolve any frame later. The served folder (strobe.json `serve.dir`, else
            // `build.output`) turns original sources into files on disk.
            let build = crate::target::isolate::build_for_os(&ctx.config["build"]);
            let served = v.root.as_ref().zip(ctx.config["serve"]["dir"].as_str().or(crate::target::isolate::per_os(&build["output"]))).map(|(r, d)| r.join(d));
            let mut scripts = std::collections::BTreeSet::new();
            for (profile, allocs) in &profiles {
                probe::symbols::urls(profile, &mut scripts);
                probe::symbols::urls(allocs, &mut scripts);
            }
            probe::symbols::store_maps_with(&page, &scripts, ranges.take(), &dir, served.as_deref())?;
            lap("maps");
        }
        let trace = dir.join("trace.pftrace");
        if let Some(tracing) = tracing {
            tracing.stop(&trace)?;
            lap("trace");
        }
        // `layout.shift`'s explanation: the largest shifts and what moved, named while the page is open.
        if set == Set::Attribution && shift && page.cdp().is_some() {
            match probe::shifts::largest(&trace, &token, cp()?, first, 5) {
                Ok(v) => store::write_json(&dir.join("shifts.json"), &v)?,
                Err(e) => ctx.warnings.push(validity::warn("Shifts", format!("{} layout shifts: {}", side, e.message))),
            }
        }
        // A Firefox block's census came before its profiler; Firefox gives strobe no heap snapshot.
        if set == Set::Attribution && firefox.is_none() {
            // Memory by process and layer after the runs (`query memory`), before the snapshot raises it.
            let mut c = probe::census::take(cb()?, cp()?);
            c["allocators"] = probe::census::allocators(cb()?, &dir.join("memdump-0.pftrace")).unwrap_or_else(|e| json!({ "error": e.message }));
            census.push(c);
            // The live objects after the runs (post-GC), once no trace subscription copies every
            // snapshot chunk (SPEC §8.6).
            if let Err(e) = probe::heap::snapshot(cp()?, &dir.join("heap-0.heapsnapshot.gz")) {
                ctx.warnings.push(validity::warn("HeapSnapshot", format!("{} heap snapshot failed: {}", side, e.message)));
            }
            lap("heap");
        }
        // What the page showed after the runs, once the trace and profilers have stopped.
        let end = shown::page(ctx, s, v, pair, "end", &page);
        shown::expect(&s.expect, &end, "after the runs", true, &|sel| shown::page_shows(&page, sel))?;
        if let Err(e) = ctx.runner.call("close", json!({}), t, &serve)
            && !ctx.warnings.iter().any(|w| w.id == "Teardown")
        {
            ctx.warnings.push(validity::warn("Teardown", format!("{} block: {}", side, e.message)));
        }
        let mut data = if set == Set::Memory || x.counters || traceless { metrics::BlockData::default() } else { metrics::read(&trace, &token)? };
        data.measured_only(range.clone());
        lap("analyse");
        let mut extra = BTreeMap::new();
        let run_counts = run_counts.into_inner();
        for name in &counted {
            extra.insert(format!("count.{name}"), (0..measured.len()).map(|j| (key(j), run_counts.get(first + j).and_then(|c| c.get(name)).copied().unwrap_or(0.0))).collect());
        }
        let fixture_cpu: BTreeMap<String, f64> = (0..measured.len()).filter_map(|j| Some((key(j), *run_counts.get(first + j)?.get(block::FIXTURE_CPU)?))).collect();
        if !fixture_cpu.is_empty() {
            extra.insert("cpu.fixture".to_string(), fixture_cpu);
        }
        // `phase.<name>`: the run's phase windows from the trace's marks, or (traceless) strobe's own.
        let run_marks = run_marks.into_inner();
        for name in declared_metrics(s).into_iter().filter_map(|m| m.strip_prefix("phase.")) {
            let per: BTreeMap<String, f64> = (0..measured.len())
                .filter_map(|j| {
                    if traceless {
                        return Some((key(j), phase_ms(run_marks.get(first + j)?, name, &measured.get(j)?.timed))).filter(|(_, ms)| ms.is_finite());
                    }
                    let suffix = format!(":run:{}:phase:{name}", first + j);
                    let spans: Vec<f64> = data.windows.iter().filter(|w| w.key.ends_with(&suffix)).map(|w| (w.t1 - w.t0 - data.untimed_in(w)) as f64 / 1e6).collect();
                    (!spans.is_empty()).then(|| (key(j), spans.iter().sum()))
                })
                .collect();
            extra.insert(format!("phase.{name}"), per);
        }
        // The browser's processes' CPU per run: in every engine, so one primary compares across them.
        extra.insert("cpu.work".to_string(), measured.iter().enumerate().filter(|(_, r)| r.work.is_finite()).map(|(j, r)| (key(j), r.work)).collect());
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
        // The same CPU as time per run, as a native or script target's: what an implementation
        // comparison compares on both sides.
        extra.insert("cpu.work".to_string(), measured.iter().enumerate().filter(|(_, r)| r.cpu.is_finite()).map(|(j, r)| (key(j), r.cpu * r.ms / 100.0)).collect());
        extra.insert("driver.reads".to_string(), measured.iter().enumerate().map(|(j, r)| (key(j), r.reads.0 as f64)).collect());
        extra.insert("driver.read_ms".to_string(), measured.iter().enumerate().map(|(j, r)| (key(j), r.reads.1)).collect());
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
            let measured_canvas = run_canvas.get(range.clone()).unwrap_or_default();
            extra.insert("canvas.frames".to_string(), measured_canvas.iter().enumerate().map(|(j, docs)| (key(j), docs.iter().map(Vec::len).sum::<usize>() as f64)).collect());
            for doc in measured_canvas.iter().flatten() {
                canvas_gaps.extend(doc.windows(2).map(|w| w[1] - w[0]));
            }
            canvas_gaps.sort_by(f64::total_cmp);
        }
        let mut values = metrics::block_values(&data, "run", &extra);
        // Traceless, the frames are the OS's presents of the browser's processes (none headless), read
        // as a native app's are (§7.4) and refused with the reason where this OS cannot tell; headed in
        // another engine, `interaction.*` joins strobe's inputs to them (from outside a run takes none).
        let mut os_refused: BTreeMap<String, Refusal> = BTreeMap::new();
        let mut os_presents = None;
        let run_inputs_kept = run_inputs.into_inner();
        if traceless {
            values.remove("frames.n");
            if let Some(p) = presents {
                let timed: Vec<metrics::Timed> = measured.iter().map(|r| r.timed.clone()).collect();
                os_presents = os_frames(ctx, s, v, p, &browser.pids(), &timed, run_inputs_kept.get(range.clone()).unwrap_or_default(), &mut values, &mut os_refused);
            }
        }
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
            values.insert("warmup.unsteady".into(), if warmup.grew() { 2.0 } else { 1.0 });
        }
        if private_mb > 0.0 {
            values.insert("memory.app.private".into(), private_mb);
        }
        if let Some(b) = pss {
            values.insert("memory.app.pss".into(), b as f64 / (1024.0 * 1024.0));
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
        let mut refused = uncounted;
        refuse_not_on_this_os(s, &mut values, &mut refused);
        block::untimed_state(s, &measured.iter().map(|r| &r.timed).collect::<Vec<_>>(), &mut values, &mut refused);
        for (m, r) in os_refused {
            values.remove(&m);
            refused.insert(m, r);
        }
        for m in declared_metrics(s).into_iter().filter(|m| metrics::split_metric(m) && !values.contains_key(*m) && set != Set::Memory && !x.counters) {
            refused.insert(m.clone(), metrics::split_refusal(m, &values));
        }
        // What strobe's page scripts see from a document's start: WebDriver Classic runs them only after the
        // document's own, so a run that loaded one missed its counter calls, canvas frames and changes.
        let unseen_start = |m: &str| loaded_document.get() && ["count.", "canvas.", "interaction.", "scroll.latency."].iter().any(|p| m.starts_with(p));
        for m in declared_metrics(s) {
            let webgpu = (no_webgpu && m.starts_with("canvas."))
                .then(|| Refusal::new(ValidatorId::EngineUnsupported, format!("{m}: the page has no WebGPU here (no GPUCanvasContext: {engine} on this OS does not ship it, or the origin is not a secure context)")));
            let preload = unseen_start(m).then(|| {
                Refusal::new(ValidatorId::EngineUnsupported, format!("{m}: a run loaded a page, and WebDriver Classic ({engine}) runs strobe's page scripts only after the page's own, so what the new page did first went unseen"))
                    .fix("keep the run on one page (load it in prepare or reset), or measure it in Chromium or Firefox")
            });
            if let Some(r) = validity::engine_refusal(engine, m).or(webgpu).or(preload) {
                values.remove(m);
                refused.insert(m.clone(), r);
            }
        }
        if burst_keys.get() > 0 {
            let line = probe::present::frames::burst_refusal(burst_keys.get());
            let names: Vec<String> = declared_metrics(s).into_iter().cloned().chain(values.keys().cloned()).filter(|m| m.starts_with("interaction.")).collect();
            for m in names {
                values.remove(&m);
                if let Some(r) = Refusal::from_line(&line) {
                    refused.insert(m, r);
                }
            }
        }
        // Undeclared ones go too (default secondaries, descriptive rows): no value the engine cannot give.
        values.retain(|k, _| !(no_webgpu && k.starts_with("canvas.")) && !unseen_start(k) && validity::engine_refusal(engine, k).is_none());
        let phases = metrics::phase_frames(&data);
        let frames = metrics::run_frames(&data);
        // Traceless, the frame pacing per run (`intervals`) comes from the OS's presents where this OS
        // tells when a frame was handed over (Windows).
        let intervals = match (&os_presents, values.get("display.hz")) {
            (Some(p), Some(hz)) if !p.no_begin => present_intervals(p, &browser.pids(), &measured.iter().map(|r| r.timed.clone()).collect::<Vec<_>>(), 1000.0 / hz),
            _ => metrics::run_intervals(&data),
        };
        // From outside there is no trace to keep: the block's numbers are in result.json. A Firefox block
        // keeps its OS presents for `query <rec> frames`.
        let recording = if (keep && (!outside || x.video)) || set == Set::Memory {
            // The actions first: the video's markers take their measured reactions from them.
            keep_actions(ctx, &dir)?;
            // The video: every run on its time (warmup ones too), the inputs and, without a trace, the phases.
            let video = match stopped.take() {
                Some(Ok(st)) => {
                    let filmed: Vec<super::video::Run> = runs.iter().enumerate().map(|(i, r)| (format!("run:{i}"), r.t0, r.t1, range.contains(&i))).collect();
                    let marks: Vec<(String, u64)> = if traceless { run_marks.iter().flatten().cloned().collect() } else { video_marks.take() };
                    st.keep(&dir, &filmed, &video_sent.take(), &marks)
                        .map_err(|e| ctx.warnings.push(validity::warn("Video", format!("{} · {side} block: the video was not kept: {}", s.label(), e.message))))
                        .ok()
                }
                Some(Err(e)) => {
                    ctx.warnings.push(validity::warn("Video", format!("{} · {side} block: no video: {}", s.label(), e.message)));
                    None
                }
                None => None,
            };
            let mut meta = json!({
                // A `url()` target loads its address whatever the variant: no build of it ran.
                "id": id, "kind": "block", "experiment": ctx.exp, "seed": ctx.seed, "series": s.label(), "variant": v.role,
                "variant_label": if fixed_page { url.clone() } else { v.label.clone() }, "variant_git": if fixed_page { Value::Null } else { variant_git(v) },
                "target": url, "probes": set, "token": token, "runs": measured.len(), "values": values, "duration_ms": run_ms.iter().sum::<f64>() as u64,
                "created": super::record::now_iso(), "valid": true, "env": host::env(), "browser": browser.version(), "engine": engine, "headless": headless, "input": input, "background": background, "screen": screen,
                // Rule 3: a block with profilers, a census or a video describes the profiled (filmed) page.
                "timing": if (set == Set::Timing || set == Set::Lean) && !x.video { "clean" } else { "perturbed" },
                "os": spans.first().zip(spans.last()).map(|(a, b)| samples.window(a.1, b.2)), "video": video,
            });
            if !census.is_empty() {
                meta["census"] = json!(census);
            }
            if let Some(sm) = sampling {
                meta["sampling"] = json!(sm);
            }
            if let Some(l) = &leak {
                // What `query leaks` reads, as for a memory recording.
                meta["leaks"] = l["suspects"].clone();
                meta["leak"] = json!({ "js_heap": l["js_heap"], "dom_nodes": l["dom_nodes"], "runs": l["runs"] });
            }
            // A traceless block's frames are its OS presents: kept as a native block keeps them, with the
            // runs' spans, inputs and the page's changes, which `query <rec> frames` reads.
            if let Some(p) = &os_presents {
                crate::symbol::write_gz(&dir.join("presents.json.gz"), p)?;
                let inputs = &run_inputs_kept;
                let per: serde_json::Map<String, Value> = measured
                    .iter()
                    .enumerate()
                    .map(|(j, r)| {
                        let (sent, changes, _) = inputs.get(first + j).cloned().unwrap_or_default();
                        let sent: Vec<Value> = sent.iter().map(|s| json!({ "t_ns": s.t_ns, "kind": if s.scroll { "scroll" } else { "discrete" }, "first": true, "press_ns": s.press, "acted": s.acted, "burst": s.burst })).collect();
                        (key(j), json!({ "t0_ns": r.t0, "t1_ns": r.t1, "untimed": r.timed.untimed, "inputs": sent, "marks": run_marks.get(first + j), "changes": changes }))
                    })
                    .collect();
                store::write_json(&dir.join("frames.json"), &json!({ "pids": browser.pids(), "display_hz": values.get("display.hz"), "runs": per }))?;
            }
            store::write_json(&dir.join("meta.json"), &meta)?;
            if let Some(d) = draft.take() {
                d.keep();
            }
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
            refused,
            locked: false,
            display: None,
            screen: Some(screen),
            reserve: None,
            shown: Some(shown::Shown { start, end }),
            overhead: Some(Overhead { strobe_pct, runner_pct, sampler_pct, ..Overhead::default() }),
            process_set: Some(programs(&browser.processes())),
            fixture_connected: None,
            client: content,
            frame: false,
            browser: Some(seen),
            directions: BTreeMap::new(),
            same: same.take(range.clone()),
            ops: journey::ops(&run_counts.get(range.clone()).unwrap_or_default().iter().collect::<Vec<_>>()),
            input,
            reduced_motion,
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

/// macOS, an implementation comparison: a page's window is measured as a native app's is (SPEC §9.10,
/// §13.6): strobe makes its app the active one (once nobody has used the Mac for a minute) and refuses
/// `WindowHidden` when other apps' windows cover it or it is not the active app, which a bench waits
/// out as for a native block. A key window can cost several times an inactive one, so both sides run
/// in that one state (and a `--check` of a file whose pages get OS input, which needs the window in
/// front). Elsewhere, and for builds of one target, nothing.
fn front_page(s: &Series, browser: &WebBrowser) -> Result<()> {
    #[cfg(target_os = "macos")]
    if s.sides.is_some() || s.cross_engine {
        use crate::target::native::{COVERED, NOT_ACTIVE};
        let pids = browser.pids();
        host::raise_windows(&pids);
        let Some(w) = host::main_window(&pids) else { return Ok(()) };
        let why = if w.cloaked {
            Some((Hidden::Covered, COVERED))
        } else if !crate::target::a11y::ax::is_front(w.pid) {
            Some((Hidden::NotActive, NOT_ACTIVE))
        } else {
            None
        };
        if let Some((hidden, why)) = why {
            return Err(Refusal::window_hidden(hidden, format!("the browser's {why}")).fix("keep the window in front and uncovered while strobe measures").into());
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (s, browser);
    Ok(())
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
/// A progress line says so only when the plan changed: a plan already in use is the env line's.
pub(crate) fn power_for(args: &Args) -> Result<host::power::Power> {
    let p = power_quiet(args)?;
    if p.switched() {
        progress(format!("  power plan: {} for this run (the user's own comes back after it)", p.used));
    }
    Ok(p)
}

/// `power_for` without the progress line, for a command that says it in its result (`record`).
pub(crate) fn power_quiet(args: &Args) -> Result<host::power::Power> {
    Ok(host::power::Power::for_run(power_keep(args)?))
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
    let quick = Duration::from_secs(5);
    let url = page.eval_timeout("location.href", quick).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "unresponsive".into());
    let shot = page.screenshot(quick).ok();
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

/// `DevBuild`: an executable that imports a debug C runtime is a debug build (timing blocks only).
pub(crate) fn dev_build(exe: &Path) -> Result<()> {
    match host::debug_runtime(exe) {
        Some(crt) => Err(Error::refused(
            "DevBuild",
            format!("{} imports {crt}, a debug C runtime: its timings describe no build people run", exe.display()),
            "build the app optimized (strobe.json build: a release or profiling profile)",
        )),
        None => Ok(()),
    }
}

/// A native or webview target's directory and git-ref variants run strobe.json's `build`; without one
/// they all launch the profile's executable, so commits or folders that differ would be measured as
/// the same build (an A/A of one spec is the same build on purpose).
fn unbuilt(target: &Value, variants: &[Variant], label: &str) -> Result<()> {
    if !(is_native(target) || target["kind"] == "webview") || target["path"].is_string() {
        return Ok(());
    }
    let unbuilt: Vec<&Variant> = variants.iter().filter(|v| matches!(v.kind, Kind::Dir | Kind::Git(_)) && v.exe.is_none()).collect();
    let differ = unbuilt.iter().any(|v| matches!(v.kind, Kind::Git(_))) || unbuilt.windows(2).any(|w| w[0].spec != w[1].spec);
    if !differ {
        return Ok(());
    }
    Err(Error::usage(format!(
        "{label}: {} name{} a folder or git ref, but strobe.json has no build for {}, so each would launch the profile's same executable",
        unbuilt.iter().map(|v| v.role).collect::<Vec<_>>().join(" and "),
        if unbuilt.len() == 1 { "s" } else { "" },
        isolate::os()
    ))
    .hint("add a build (command and output) to strobe.json, or compare executables: --base exe:<path> --head exe:<path>"))
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
    // The user's Documents and Desktop: checked strictly around each side's first launch and in a
    // `--check` (a change stops it); past those a change is noted (`any_block`), since a person's
    // own save over a long bench is likelier than a copy's.
    if ctx.folders_first.insert(v.role.to_string()) || ctx.mode == Mode::Check {
        app.folder_notes = None;
    } else {
        app.folder_notes = Some(ctx.folder_notes.clone());
    }
    Ok(app)
}

/// Whether a series' launches of `target` take warm GPU caches that its first block of each variant
/// fills (the block before the pairs, discarded).
fn gpu_warming(ctx: &Ctx, s: &Series, target: &Value) -> Result<bool> {
    let kind = target["kind"].as_str().unwrap_or("");
    if !matches!(ctx.mode, Mode::Compare | Mode::Blocks(_)) || cfg!(target_os = "macos") || !s.launch.is_null() || !matches!(kind, "native" | "app" | "exe" | "webview") {
        return Ok(false);
    }
    let profile = profile_of(target).and_then(|id| isolate::app(id, &ctx.config).ok());
    Ok(gpu_cache_mode(s, target, profile.as_ref()) == "warm")
}

/// The app profile a target launches from: `native(appId)`, `app(id)`, `exe(path, { app })`.
fn profile_of(target: &Value) -> Option<&str> {
    match target["kind"].as_str()? {
        "app" => target["id"].as_str(),
        "native" | "exe" | "webview" => target["app"].as_str(),
        _ => None,
    }
}

/// What a bench can tell is wrong before it waits for the slot or builds anything: an app profile that
/// does not resolve, a guarded file strobe may not read, a `gpuCache` that is neither warm nor cold.
/// Each side's own target with its own project's strobe.json (an implementation comparison's two).
fn preflight(series: &[Series], describe: &Value, variants: &[Variant], config: &Value, projects: &BTreeMap<String, (PathBuf, Value)>, headless: bool) -> Result<()> {
    for s in series.iter().filter(|s| s.gap.is_none()) {
        for v in variants {
            let name = v.target_of(s);
            let target = &describe["targets"][name];
            let config = projects.get(name).map_or(config, |(_, c)| c);
            if headless && !matches!(target["kind"].as_str(), Some("web" | "url")) {
                return Err(Error::usage(format!(
                    "{}: --headless runs pages in a headless browser; this target is a {} target, which opens its own window",
                    s.label(),
                    target["kind"].as_str().unwrap_or("?")
                )));
            }
            let apps = profile_of(target).into_iter().chain(if let Kind::App(id) = &v.kind { Some(id.as_str()) } else { None });
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
    }
    Ok(())
}

/// The `gpuCache` a side's launches take for a series: the case's, its target's, else its app profile's
/// (from the target's own project), default warm.
fn side_gpu_cache(s: &Series, describe: &Value, name: &str, config: &Value, projects: &BTreeMap<String, (PathBuf, Value)>) -> String {
    let target = &describe["targets"][name];
    let config = projects.get(name).map_or(config, |(_, c)| c);
    let profile = profile_of(target).and_then(|id| isolate::app(id, config).ok());
    gpu_cache_mode(s, target, profile.as_ref()).to_string()
}

/// What an app target launches for a variant. `app:<id>` is that app's own profile; `exe:<path>` is
/// the target's profile (`app(id)`, or `exe(path, { app })`) with another build.
fn variant_app(v: &Variant, target: &Value, config: &Value) -> Result<isolate::App> {
    // The target's own `args`, `env` and `files` over the profile's.
    let args: Vec<String> = target["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
    let env: Vec<(String, String)> = target["env"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect();
    let app = variant_profile(v, target, config)?.with_extra(&args, &env).with_files(&target["files"].as_object().cloned().unwrap_or_default());
    // Reduced motion through the app's Chromium (Electron, WebView2), and its profile's declaration.
    if Motion::of(target, "")?.reduced() { app.with_reduced_motion(true) } else { Ok(app) }
}

fn variant_profile(v: &Variant, target: &Value, config: &Value) -> Result<isolate::App> {
    // `webview(…)` reaches the app through its webview whatever kind its profile names.
    let webview = |a: isolate::App| if target["kind"] == "webview" { a.as_kind(isolate::AppKind::Webview) } else { a };
    if let Kind::App(id) = &v.kind {
        return isolate::app(id, config).map(webview);
    }
    let profile = match target["kind"].as_str() {
        Some("app") => target["id"].as_str(),
        Some("exe" | "webview") => target["app"].as_str(),
        _ => return Err(Error::usage("not an app target")),
    };
    let base = webview(match profile {
        Some(id) => isolate::app(id, config)?,
        None => isolate::App::bare(Path::new(target["path"].as_str().unwrap_or(""))),
    });
    match (&v.kind, &v.exe) {
        (Kind::Exe(p), _) => Ok(base.with_exe(p)),
        _ if target["kind"] == "exe" => Ok(base.with_exe(Path::new(target["path"].as_str().unwrap_or("")))),
        // A directory or git-ref variant of a webview profile runs its own build.
        (_, Some(p)) if target["kind"] == "webview" && target["path"].is_null() => Ok(base.with_exe(p)),
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

/// The series and the variants of a bench (or plan): `--base`/`--head` as builds of every target, or
/// as sides that name targets (SPEC §9.10). Both sides naming different targets make an implementation
/// comparison; one target named (by both sides, or by `--head` alone) narrows the series to it.
fn series_and_sides(describe: &Value, args: &Args, base: Option<&str>, head: &str, project: &Path, config: &Value) -> Result<(Vec<Series>, Vec<Variant>)> {
    if let Some(pair) = check_pair(describe, args, project)? {
        let series = expand(describe, args, None, None, Some(pair))?;
        return Ok((series, vec![resolve_variant("head", ".", project, config)?]));
    }
    let base_side = base.map(|b| side_spec(b, describe, project)).transpose()?.flatten();
    let head_side = side_spec(head, describe, project)?;
    if base.is_some() && base_side.is_some() != head_side.is_some() {
        return Err(Error::usage(format!(
            "--base {} and --head {head}: name a target of the bench file on both sides (an implementation comparison: --base <target>[=<variant>] --head <target>[=<variant>]) or on neither (builds of every target)",
            base.unwrap_or("")
        )));
    }
    let sides = match (base_side, head_side) {
        (Some((b, _)), Some((h, _))) if b != h => Some((b, h)),
        _ => None,
    };
    if sides.is_some() && args.get("target").is_some() {
        return Err(Error::usage("--target picks targets of builds; an implementation comparison's sides name its two targets already: drop --target"));
    }
    let only = if sides.is_none() { head_side.map(|(t, _)| t) } else { None };
    let series = expand(describe, args, sides, only, None)?;
    let resolve = |role: &'static str, spec: &str, side: Option<(&str, Option<&str>)>| match side {
        Some((t, v)) => resolve_side(role, t, v, describe, project, config),
        None => resolve_variant(role, spec, project, config),
    };
    let mut variants = Vec::new();
    if let Some(b) = base {
        variants.push(resolve("base", b, base_side)?);
    }
    variants.push(resolve("head", head, head_side)?);
    Ok((series, variants))
}

/// `--base <target> --head <target> --check`: the two targets of the bench file it names, each run at
/// the bench file's own folder (a check builds no variant), or None without `--base`.
fn check_pair<'a>(describe: &Value, args: &'a Args, project: &Path) -> Result<Option<(&'a str, &'a str)>> {
    let Some(b) = args.get("base").filter(|_| args.has("check")) else { return Ok(None) };
    let h = args.get("head").unwrap_or(".");
    if args.get("target").is_some() {
        return Err(Error::usage("--check --base <target> --head <target> runs those two targets: drop --target"));
    }
    match (side_spec(b, describe, project)?, side_spec(h, describe, project)?) {
        (Some((b, None)), Some((h, None))) if b != h => Ok(Some((b, h))),
        _ => Err(Error::usage(format!(
            "--check --base {b} --head {h}: name two different targets of the bench file, with no =<variant> (a check runs the bench file's own folder), to check them as their comparison measures them; or drop --base"
        ))),
    }
}

/// What an implementation comparison refuses before anything builds (SPEC §9.10): a primary that is
/// one engine's own (`NotComparable`, exit 2), and sides set up unlike each other (`VariantMismatch`):
/// another viewport or scale, emulated device, motion or GPU shader cache mode.
fn implementation_preflight(describe: &Value, series: &[Series], variants: &[Variant], config: &Value, projects: &BTreeMap<String, (PathBuf, Value)>) -> Result<()> {
    let (Some(b), Some(h)) = (variants[0].target.as_deref(), variants.get(1).and_then(|v| v.target.as_deref())) else { return Ok(()) };
    let (tb, th) = (&describe["targets"][b], &describe["targets"][h]);
    let cross = engine(tb) != engine(th);
    // A page's launch is a navigation in a browser strobe started before it, timed in the page; an app's
    // is a process spawn timed to its window or tree (SPEC 9.6 "What a launch spans"): no one span.
    let page = |t: &Value| matches!(t["kind"].as_str(), Some("web" | "url"));
    if page(tb) != page(th)
        && let Some(s) = series.iter().find(|s| s.gap.is_none() && s.refused.is_none() && s.launch.is_object())
    {
        let (p, a) = if page(tb) { (&variants[0].name, &variants[1].name) } else { (&variants[1].name, &variants[0].name) };
        return Err(Error::usage_refused(
            Refusal::new(
                ValidatorId::NotComparable,
                format!(
                    "{}: {p}'s launch is a navigation in a browser that was already running, timed to its page, and {a}'s a process spawn timed to its window or tree: the two launches span different work",
                    s.case
                ),
            )
            .fix("compare the launch of two apps (a webview or app target for the page) or of two pages, or compare work after the launch in a run case"),
        ));
    }
    for s in series.iter().filter(|s| s.gap.is_none() && s.refused.is_none() && cross) {
        if let Some(d) = model::metric(&s.primary).filter(|d| d.across == model::Across::PerSide) {
            let why = format!(
                "{}'s primary {} is one engine's own measure ({}), and {} runs on {} while {} runs on {}",
                s.case,
                d.name,
                d.about,
                variants[0].name,
                engine(tb),
                variants[1].name,
                engine(th),
            );
            return Err(Error::usage_refused(Refusal::new(ValidatorId::NotComparable, why).fix("pick a primary both sides measure the same way, such as cpu.work, run.duration or launch.ready")));
        }
    }
    let size = |t: &Value| t["viewport"].is_object().then(|| (t["viewport"]["width"].as_f64().unwrap_or(0.0), t["viewport"]["height"].as_f64().unwrap_or(0.0)));
    let mismatch = |what: String| {
        Error::refused(
            "VariantMismatch",
            format!("{} and {} {what}", variants[0].name, variants[1].name),
            "give both targets the same window, device, motion and GPU cache setup: an implementation comparison varies the app, not its window, settings or caches",
        )
    };
    match (size(tb), size(th)) {
        (Some(x), Some(y)) if (x.0 - y.0).abs() > 2.0 || (x.1 - y.1).abs() > 2.0 => {
            return Err(mismatch(format!("ask for different viewports ({}×{} and {}×{} device-independent pixels)", x.0, x.1, y.0, y.1)));
        }
        (Some(_), None) | (None, Some(_)) => return Err(mismatch(format!("do not both set a viewport (only {} does)", if size(tb).is_some() { &variants[0].name } else { &variants[1].name }))),
        _ => {}
    }
    if tb["scale"].as_f64() != th["scale"].as_f64() {
        return Err(mismatch(format!("ask for different scales ({} and {})", tb["scale"], th["scale"])));
    }
    // A phone or a touch screen against a desktop is two workloads (the page takes its touch paths).
    if tb["emulate"] != th["emulate"] {
        let device = |t: &Value| t["emulate"].as_str().map_or("a desktop".to_string(), |e| format!("emulated {e}"));
        return Err(mismatch(format!("emulate different devices ({} and {})", device(tb), device(th))));
    }
    // Reduced motion against the OS's own setting is two workloads (the app skips its animations).
    let (mb, mh) = (Motion::of(tb, b)?, Motion::of(th, h)?);
    if mb != mh {
        let said = |m: Motion| if m.reduced() { motion::LABEL } else { "the OS's own motion setting" };
        return Err(mismatch(format!("ask for different motion ({} and {})", said(mb), said(mh))));
    }
    for s in series.iter().filter(|s| s.gap.is_none()) {
        let (cb, ch) = (side_gpu_cache(s, describe, b, config, projects), side_gpu_cache(s, describe, h, config, projects));
        if cb != ch {
            return Err(mismatch(format!("launch with different GPU shader caches for {} (gpuCache {cb} and {ch})", s.case)));
        }
    }
    Ok(())
}

/// A target's `expect` with the case's keys over it.
fn merged_expect(target: &Value, case: &Value) -> Value {
    let mut expect = target["expect"].clone();
    if let (Some(t), Some(c)) = (expect.as_object_mut(), case.as_object()) {
        t.extend(c.clone());
    } else if case.is_object() {
        expect = case.clone();
    }
    expect
}

/// Why a side cannot run a case, and the adapter entry it lacks when that is why: the case says it is
/// `unsupported` there, its hooks, `expect` or `until` name an `@name` the target's `ui` lacks (or it
/// gives other targets their own run and this one none: the runner's `gaps`), or it runs only on
/// other targets.
fn side_gap(c: &Value, target: &str) -> Option<(String, Option<String>)> {
    if let Some(why) = c["unsupported"][target].as_str() {
        return Some((why.to_string(), None));
    }
    if let Some(g) = c["gaps"][target].as_object() {
        return Some((g.get("reason").and_then(Value::as_str).unwrap_or("").to_string(), g.get("missing").and_then(Value::as_str).filter(|m| m.starts_with('@')).map(str::to_string)));
    }
    let on: Option<Vec<&str>> = c["targets"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect());
    on.filter(|on| !on.contains(&target)).map(|on| (format!("the case runs only on {}", on.join(", ")), None))
}

/// The series: case × target × params, or in an implementation comparison (`sides`: the base's and the
/// head's targets) case × params, each block on its side's target. `only`: the one target a side names.
/// `pair`: the two targets `--base`/`--head` name in a `--check` (`check_pair`), each case on each.
fn expand(describe: &Value, args: &Args, sides: Option<(&str, &str)>, only: Option<&str>, pair: Option<(&str, &str)>) -> Result<Vec<Series>> {
    let filter = args.get("filter");
    let only_target = only.or(args.get("target"));
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
    // `--check` of every target of a file checks each page of a case that runs on different engines the
    // way an implementation comparison measures it: from outside, with OS input (SPEC §9.10). Pages in
    // several browsers and nothing else are no such case: each checks in its own engine. `--base
    // <target> --head <target> --check` (`pair`) checks the two the way their comparison measures
    // them: from outside when their engines differ, two browsers' pages too.
    let check = args.has("check") && sides.is_none() && (only_target.is_none() || pair.is_some());
    let kind = |t: &Value| match pair {
        Some(_) => engine(t),
        None if WebEngine::ALL.iter().any(|e| e.name() == engine(t)) => "page",
        None => engine(t),
    };
    for (case, c) in describe["cases"].as_object().cloned().unwrap_or_default() {
        if filter.is_some_and(|f| !case_matches(f, &case)) {
            continue;
        }
        // A selector's `@name` that no target's ui has is a typo, not a gap.
        if let Some(t) = c["typos"].as_array().filter(|a| !a.is_empty()) {
            return Err(Error::usage(format!(
                "{case}: {} in no target's ui (a typo?): give the targets that run the case a ui entry of that name",
                t.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
            )));
        }
        // Steps per target prove nothing reaches the same place without reads that show it.
        if c["adapted"].is_string() && !c["expect"]["same"].as_array().is_some_and(|a| !a.is_empty()) {
            return Err(Error::usage(format!("{case}: run gives each target its own steps, so the case needs expect.same: elements both sides must read the same after setup and after each run")));
        }
        // A case may name the targets it runs on; by default it runs on every target.
        let on: Option<Vec<&str>> = c["targets"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect());
        // An implementation comparison runs each journey once, on both sides' targets.
        let names: Vec<String> = match (sides, pair) {
            (Some((_, head)), _) => vec![head.to_string()],
            (None, Some((a, b))) => vec![a.to_string(), b.to_string()],
            (None, None) => targets.keys().cloned().collect(),
        };
        // The engines of the targets this case runs on.
        let engines: std::collections::BTreeSet<&str> = names
            .iter()
            .filter(|t| on.as_ref().is_none_or(|on| on.contains(&t.as_str())) && !c["unsupported"][t.as_str()].is_string() && !c["gaps"][t.as_str()].is_object())
            .map(|t| kind(&targets[t.as_str()]))
            .collect();
        for target in &names {
            // `--target`: target names as `--filter` takes cases (comma-separated, globs with *).
            let excluded = on.as_ref().is_some_and(|on| !on.contains(&target.as_str())) || c["unsupported"][target].is_string() || c["gaps"][target].is_object();
            // The parity matrix lists a target the journey has no adapter for, or is unsupported on.
            let listed = check && excluded && on.as_ref().is_none_or(|on| on.contains(&target.as_str()));
            if sides.is_none() && (only_target.is_some_and(|t| !case_matches(t, target)) || excluded) && !listed {
                continue;
            }
            // A side that cannot run the journey: a parity gap, shown and never run on either side.
            let gap = match sides {
                Some((base, head)) => side_gap(&c, base).map(|(w, m)| ("base", w, m)).or_else(|| side_gap(&c, head).map(|(w, m)| ("head", w, m))),
                None if listed => side_gap(&c, target).map(|(w, m)| ("head", w, m)),
                None => None,
            };
            let primary = args.get("primary").map(str::to_string).unwrap_or_else(|| c["primary"].as_str().unwrap_or("frames.presented.p95").to_string());
            // The case's margins, then `--noworse` over them (a bare margin is the primary's).
            let mut noworse = margins(&c["noworse"], &primary).map_err(|e| Error::usage(format!("{case}: noworse: {e}")))?;
            if let Some(flag) = args.get("noworse") {
                noworse.extend(margins(&json!(flag), &primary).map_err(|e| Error::usage(format!("--noworse: {e}")))?);
            }
            // A gap's own selectors may name what its target lacks: nothing of it runs.
            let (expect, launch) = match &gap {
                Some(_) => (merged_expect(&targets[target], &c["expect"]), c["launch"].clone()),
                None => (journey::side_expect(&describe["targets"], &c["expect"], target)?, journey::side_launch(&c["launch"], &describe["targets"], target)?),
            };
            for p in &combos {
                out.push(Series {
                    expect: expect.clone(),
                    case_expect: c["expect"].clone(),
                    sides: sides.map(|(b, h)| (b.to_string(), h.to_string())),
                    gap: gap.clone(),
                    adapted: c["adapted"].as_str().map(str::to_string),
                    refused: None,
                    hero: c["hero"] == true,
                    cross_engine: sides.is_some_and(|(b, h)| engine(&targets[b]) != engine(&targets[h])) || (check && engines.len() > 1),
                    case: case.clone(),
                    target: target.clone(),
                    params: p.clone(),
                    primary: primary.clone(),
                    noworse: noworse.clone(),
                    metrics: c["metrics"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                    reset: c["reset"].as_bool().unwrap_or(false),
                    reset_check: c["resetCheck"] != false,
                    launch: launch.clone(),
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

/// One run's inputs strobe sent (host clock, and whether each was a scroll) and the page's own changes
/// (host clock), for input latency from the OS's presents.
type RunInputs = (Vec<probe::present::frames::Sent>, Vec<probe::present::frames::Change>, metrics::Timed);

/// The browser's processes, the cycle clock and the page's counters (with what departed documents
/// carried) at one edge of a run's untimed stretch.
type WebEdge = (super::record::Cycles, Option<u64>, [f64; 6]);

/// The page engine a target runs in (`web`, `url`; `apply_engines` set it): Chromium unless it says.
fn engine_of(target: &Value) -> WebEngine {
    target["engine"].as_str().and_then(|e| WebEngine::parse(e).ok()).unwrap_or_default()
}

/// What a page target emulates (its `emulate`, SPEC 13.1 "Emulated phones and touch screens"): a phone
/// (`mobile`) or a touch screen (`touch`); a desktop without it.
fn emulation_of(target: &Value, name: &str) -> Result<chromium::Emulation> {
    Ok(match target["emulate"].as_str() {
        None if target["emulate"].is_null() => chromium::Emulation::default(),
        Some("mobile") => chromium::Emulation { mobile: true, touch: true, user_agent: None },
        Some("touch") => chromium::Emulation { touch: true, ..Default::default() },
        _ => return Err(Error::usage(format!("target {name}: emulate is \"mobile\" or \"touch\", not {} (a page is a desktop without it)", target["emulate"]))),
    })
}

/// How a block asked a target's page for reduced motion, for the refusal when the page does not match.
fn motion_how(kind: &str, engine: WebEngine) -> &'static str {
    match kind {
        "app" | "exe" => "Chromium's --force-prefers-reduced-motion on the app's command line",
        "webview" => "--force-prefers-reduced-motion in the webview's additional browser arguments",
        "command" => "DevTools' Emulation.setEmulatedMedia on the attached page",
        _ => motion::how(engine),
    }
}

/// `EngineUnsupported` for a pointer input on an emulated phone's page: strobe's driver sends mouse
/// events, and a phone is driven by touch, which strobe does not send yet, so a case that clicks, taps,
/// hovers, drags or scrolls there would measure a mouse on a phone. Keys (`press`, `type` without a
/// selector to click into) and loads work. None for any other call.
fn phone_pointer(call: &str, args: &[Value]) -> Option<Refusal> {
    let has_selector = |v: Option<&Value>| v.is_some_and(|v| !(v.is_null() || v.as_str() == Some("")));
    let pointer = match call {
        "click" | "tap" | "hover" | "move" | "down" | "up" | "drag" | "scroll" | "wheel" => true,
        // Both focus their element with a click unless it already has focus.
        "type" => has_selector(args.first()),
        "press" => has_selector(args.get(1).map(|o| &o["selector"])),
        _ => false,
    };
    pointer.then(|| {
        Refusal::new(ValidatorId::EngineUnsupported, format!("page.{call} on a page that emulates a phone: strobe sends mouse input, and touch input isn't built yet; load cases (and keys with no element to click into) work"))
            .fix("time the phone's load, or its keys with page.press/type and no selector; measure the pointer case on a desktop target")
    })
}

/// The window an emulated phone's page shows when its target declares none: the phone's viewport, at
/// its scale headless (a headed phone is refused before anything runs, `record::phone_refusal`).
fn phone_window(window: &mut Window, target: &Value, emulation: &chromium::Emulation, headless: bool) {
    if !emulation.mobile {
        return;
    }
    if !target["viewport"].is_object() {
        (window.width, window.height) = super::record::PHONE;
    }
    if window.scale.is_none() && headless {
        window.scale = Some(super::record::PHONE_SCALE);
    }
}

/// `390×844 @3x · emulated mobile`: the window a series' pages drew in (its blocks' content area, else the
/// window it asked for) with its scale, emulation and reduced motion, for its result (`window`) and every report; none for
/// a series with no window.
fn series_window(ctx: &Ctx, s: &Series, blocks: &[Block]) -> Value {
    if series_windowless(ctx, s) || s.sides.is_some() {
        return Value::Null;
    }
    let target = &ctx.describe["targets"][&s.target];
    // An emulated phone is its device's size: a page without a viewport tag lays out wider on it.
    let mobile = target["emulate"] == "mobile";
    let size = match blocks.iter().find_map(|b| b.client).filter(|_| !mobile) {
        Some(c) => [c[0], c[1], c[2]],
        None => {
            let mut w = ctx.window.clone();
            if let Some(vp) = target["viewport"].as_object() {
                w.width = vp.get("width").and_then(Value::as_u64).unwrap_or(1400) as u32;
                w.height = vp.get("height").and_then(Value::as_u64).unwrap_or(900) as u32;
            }
            w.scale = target["scale"].as_f64();
            if let Ok(e) = emulation_of(target, &s.target) {
                phone_window(&mut w, target, &e, headless_of(ctx, s));
            }
            [w.width as f64, w.height as f64, w.scale.unwrap_or(1.0)]
        }
    };
    let round = |x: f64| format!("{x:.2}").trim_end_matches('0').trim_end_matches('.').to_string();
    let scale = if (size[2] - 1.0).abs() > 0.005 { format!(" @{}x", round(size[2])) } else { String::new() };
    let emulated = target["emulate"].as_str().map(|e| format!(" · emulated {e}")).unwrap_or_default();
    let reduced = if target["motion"] == "reduce" { format!(" · {}", motion::LABEL) } else { String::new() };
    json!(format!("{}×{}{scale}{emulated}{reduced}", size[0], size[1]))
}

/// What every report reads of a series' own setup, beside its verdicts: the window it drew in
/// (`series_window`), whether it has none, what its page emulated, its `motion`, and a `url()` target's address (the
/// page both sides load whatever the variant, so reports name it, not a build).
fn series_setup(ctx: &Ctx, s: &Series, window: Value, out: &mut Value) {
    let target = &ctx.describe["targets"][&s.target];
    out["windowless"] = json!(series_windowless(ctx, s));
    out["window"] = window;
    out["emulate"] = target["emulate"].clone();
    // Both sides of an implementation comparison ask for the same (`implementation_preflight`).
    let side = s.sides.as_ref().map_or(&s.target, |(b, _)| b);
    out["motion"] = ctx.describe["targets"][side]["motion"].clone();
    if target["kind"] == "url" && s.sides.is_none() {
        out["href"] = target["href"].clone();
    }
}

/// Whether a series' page runs headless: `--headless`, or its target's `headless`.
fn headless_of(ctx: &Ctx, s: &Series) -> bool {
    ctx.headless || ctx.describe["targets"][&s.target]["headless"] == true
}

/// What an app's page (Electron, a webview, a command's browser) sees of its input devices: the
/// machine's, which strobe does not set (it does not launch that browser), recorded as seen. A page that
/// sees no pointer at all warns once (`NoPointer`): the app runs as on a machine without a mouse. With
/// it, whether the page matched `(prefers-reduced-motion: reduce)` (the same read).
fn observed_input(ctx: &mut Ctx, s: &Series, page: &Page) -> Option<(validity::InputClass, bool)> {
    let seen = validity::InputSeen::read(page).ok()?;
    let class = seen.class();
    if class == validity::InputClass::None && !ctx.warnings.iter().any(|w| w.id == "NoPointer") {
        ctx.warnings.push(validity::warn(
            "NoPointer",
            format!("{}: the app's page sees no pointer ((hover: none), neither a fine nor a coarse one), as on a machine without a mouse, and runs its touch or no-hover paths; measure it on a machine with a mouse", s.label()),
        ));
    }
    Some((class, seen.reduced_motion))
}

/// The input devices a series' pages saw (`Block.input`): the one class every block with a page saw
/// (`admit_input` refused sides that disagree), else null: no page, or blocks that saw several. In its
/// result and a page series' tracked fingerprint, so a series from before strobe set a desktop's is
/// never one history with these.
fn input_of(blocks: &[Block]) -> Value {
    let mut seen = blocks.iter().filter_map(|b| b.input);
    match seen.next() {
        Some(first) if seen.all(|c| c == first) => json!(first),
        _ => Value::Null,
    }
}

/// `--engine` and the targets' own `engine` and `headless` (SPEC §9.6): every page target gets its engine
/// (Chromium by default). `--engine e` keeps only the targets of engine `e` when the file has some, and
/// otherwise runs every page target in `e`; either way a run that would include an app, exe, native,
/// command or script target refuses (`check_engines`). WebKit refuses on Windows, and headless on
/// macOS (`EngineUnsupported`).
fn apply_engines(describe: &mut Value, args: &Args) -> Result<()> {
    let flag = args.get("engine").map(WebEngine::parse).transpose()?;
    let page = |t: &Value| matches!(t["kind"].as_str(), Some("web" | "url"));
    let Some(targets) = describe["targets"].as_object_mut() else { return Ok(()) };
    for (name, t) in targets.iter() {
        // What a target writes into its home stays there (checked again before each write).
        for rel in t["files"].as_object().into_iter().flatten().map(|(k, _)| k) {
            isolate::check_home_rel(rel, &format!("target {name}: files"))?;
        }
        if !t["motion"].is_null() {
            if let Some(kind @ ("script" | "cli")) = t["kind"].as_str() {
                return Err(Error::usage(format!("target {name}: motion is for targets that show something (web, url, app, exe, command, native, webview); this is a {kind} target")));
            }
            Motion::of(t, name)?;
        }
        if !t["emulate"].is_null() {
            if !page(t) {
                return Err(Error::usage(format!("target {name}: emulate is for page targets (web, url): an app's page sees the machine's own input; this is a {} target", t["kind"].as_str().unwrap_or("?"))));
            }
            emulation_of(t, name)?;
        }
        if let Some(e) = t["engine"].as_str() {
            if !page(t) {
                return Err(Error::usage(format!("target {name}: engine is for page targets (web, url); this is a {} target", t["kind"].as_str().unwrap_or("?"))));
            }
            WebEngine::parse(e).map_err(|e| Error::usage(format!("target {name}: {}", e.message)))?;
        }
    }
    if let Some(e) = flag {
        if !targets.values().any(page) {
            return Err(Error::usage("--engine picks the browser of page targets (web, url); this bench file has none"));
        }
        let declared = targets.values().any(|t| t["engine"].as_str() == Some(e.name()));
        if declared {
            targets.retain(|_, t| t["engine"].as_str() == Some(e.name()));
        } else {
            for t in targets.values_mut().filter(|t| page(t)) {
                t["engine"] = json!(e);
            }
        }
    }
    for t in targets.values_mut().filter(|t| page(t)) {
        if t["engine"].is_null() {
            t["engine"] = json!(WebEngine::Chromium);
        }
        if args.has("headless") {
            t["headless"] = json!(true);
        }
    }
    Ok(())
}

/// Before anything builds: `--engine` on a series that is no page (the run refuses). A primary the series'
/// engine cannot measure (`EngineUnsupported`, with the closest one it can), a headless page cannot show
/// (`Headless`), or a launch case in an engine without one refuses that series alone (`Series::refused`):
/// it is listed and the rest runs; when that leaves nothing to run, the first refusal is the run's.
/// Declared secondaries are refused per block instead. Two builds that run one fixed target alike
/// (`same_target`) refuse that series too.
fn check_engines(series: &mut [Series], describe: &Value, args: &Args, variants: &[Variant], config: &Value, projects: &BTreeMap<String, (PathBuf, Value)>) -> Result<()> {
    // Every target a series runs on: an implementation comparison's both sides. A parity gap never runs.
    for s in series.iter_mut().filter(|s| s.gap.is_none()) {
        let names: Vec<String> = match &s.sides {
            Some((b, h)) => vec![b.clone(), h.clone()],
            None => vec![s.target.clone()],
        };
        for name in names {
            if s.refused.is_none() {
                s.refused = target_refusal(s, &name, describe, args)?;
            }
            if s.refused.is_none() {
                s.refused = motion_contract(s, &name, describe, variants, projects.get(&name).map_or(config, |(_, c)| c));
            }
        }
        if s.refused.is_none() && s.sides.is_none() && !args.has("aa") {
            s.refused = same_target(&describe["targets"][&s.target], variants);
        }
    }
    let refused: Vec<(String, &Refusal)> = series.iter().filter_map(|s| Some((s.label(), s.refused.as_ref()?))).collect();
    if let Some((label, r)) = refused.first().filter(|_| series.iter().all(|s| s.refused.is_some())) {
        let r = Refusal { summary: format!("{label}: {}", r.summary), ..(*r).clone() };
        return Err(if refused.len() > 1 { r.detail(format!("and every other series too ({} in all)", refused.len())) } else { r }.into());
    }
    Ok(())
}

/// `NotForTarget` for a native target asking for reduced motion whose app profile (its own, or an
/// `app:<id>` variant's) declares no `motion.reduce`, or that has no profile (`native(path)`): strobe knows
/// no setting of a native app's own. The series alone; a profile that does not resolve is `preflight`'s.
fn motion_contract(s: &Series, name: &str, describe: &Value, variants: &[Variant], config: &Value) -> Option<Refusal> {
    let t = &describe["targets"][name];
    if t["kind"] != "native" || t["motion"] != "reduce" {
        return None;
    }
    let apps = variants.iter().filter(|v| v.target_of(s) == name).filter_map(|v| if let Kind::App(id) = &v.kind { Some(id.as_str()) } else { None });
    let ids: Vec<&str> = profile_of(t).into_iter().chain(apps).collect();
    let missing = if ids.is_empty() {
        Some(Path::new(t["path"].as_str().unwrap_or("")).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
    } else {
        ids.into_iter().find(|id| isolate::app(id, config).is_ok_and(|a| !a.declares_motion())).map(str::to_string)
    };
    let r = isolate::no_motion_contract(&missing?);
    let on = if s.sides.is_some() { format!("{}: ", t["name"].as_str().unwrap_or(name)) } else { String::new() };
    Some(Refusal { summary: format!("{on}{}", r.summary), ..r })
}

/// Before the first block (SPEC 9.10 "Declared viewport"): each headed page target's viewport against
/// the largest page its browser's window shows on this display. Known from an earlier window of the same
/// browser on the same work area (`web::known_room`); else, with a bench's `ctx`, measured in one window
/// opened for it; else not known yet. One answer per engine, scale and size. A series whose viewport
/// does not fit refuses `DisplayTooSmall` alone, naming the most it can be; when that leaves nothing,
/// the run refuses.
fn display_fit(ctx: Option<&mut Ctx>, series: &mut [Series], describe: &Value, headless: bool) -> Result<()> {
    let mut ctx = ctx;
    let mut answers: BTreeMap<String, Option<Refusal>> = BTreeMap::new();
    let size = |vp: &serde_json::Map<String, Value>, k: &str, or: u64| vp.get(k).and_then(Value::as_u64).unwrap_or(or) as u32;
    for s in series.iter_mut().filter(|s| s.gap.is_none() && s.refused.is_none()) {
        let names: Vec<String> = match &s.sides {
            Some((b, h)) => vec![b.clone(), h.clone()],
            None => vec![s.target.clone()],
        };
        for name in names {
            let t = &describe["targets"][&name];
            let Some((w, h)) = t["viewport"].as_object().map(|vp| (size(vp, "width", 1400), size(vp, "height", 900))) else { continue };
            if !matches!(t["kind"].as_str(), Some("web" | "url")) || headless || t["headless"] == true {
                continue;
            }
            let (engine, scale) = (engine_of(t), t["scale"].as_f64());
            let key = format!("{engine} {scale:?} {w}x{h}");
            let answer = match answers.get(&key) {
                Some(a) => a.clone(),
                None => {
                    let a = fit_answer(ctx.as_deref_mut(), engine, scale, (w, h))?;
                    answers.insert(key, a.clone());
                    a
                }
            };
            if let Some(r) = answer {
                let on = if s.sides.is_some() { format!("{} ({engine}): ", t["name"].as_str().unwrap_or(&name)) } else { String::new() };
                s.refused = Some(Refusal { summary: format!("{on}{}", r.summary), ..r });
                break;
            }
        }
    }
    let refused: Vec<(String, &Refusal)> = series.iter().filter(|s| s.gap.is_none()).filter_map(|s| Some((s.label(), s.refused.as_ref()?))).collect();
    if let Some((label, r)) = refused.first().filter(|_| series.iter().all(|s| s.gap.is_some() || s.refused.is_some())) {
        let r = Refusal { summary: format!("{label}: {}", r.summary), ..(*r).clone() };
        return Err(if refused.len() > 1 { r.detail(format!("and every other series too ({} in all)", refused.len())) } else { r }.into());
    }
    Ok(())
}

/// Whether a page of `engine` shows `asked` here: from what strobe knows of this browser on this work
/// area, else (with a bench's `ctx`) one window of it opened and fitted for the purpose, in the bench
/// slot alone (no power plan, nothing held after it: builds come next). No window opens on a locked
/// session or while a person uses the Mac (input in the last minute): then, as when the window cannot
/// open, nothing is known here and the block's own fit checks it.
fn fit_answer(ctx: Option<&mut Ctx>, engine: WebEngine, scale: Option<f64>, asked: (u32, u32)) -> Result<Option<Refusal>> {
    if let Some((browser, room)) = web::known_room(engine, scale)
        && (ctx.is_none() || room.too_small(&browser, asked).is_none())
    {
        return Ok(room.too_small(&browser, asked));
    }
    let Some(ctx) = ctx else { return Ok(None) };
    if host::session_locked() || host::person_idle_s().is_some_and(|i| i < 60.0) {
        progress(format!("  {engine}: whether a {}×{} page fits this display is checked in its first block (no window opens now: the session is locked or in use)", asked.0, asked.1));
        return Ok(None);
    }
    let what = format!("fitting a {}×{} page in a {engine} window", asked.0, asked.1);
    progress(format!("  {what} on this display"));
    let held = match ctx.slot {
        Some(_) => None,
        None => {
            let (slot, waited) = ctx.reg.acquire(&what)?;
            ctx.waited_ms += waited.as_secs_f64() * 1e3;
            Some(slot)
        }
    };
    ctx.reg.set("running", &what);
    let mut window = ctx.window.clone();
    (window.width, window.height, window.scale) = (asked.0, asked.1, scale);
    let fitted = web::launch(engine, &window, false, ctx.pin).and_then(|browser| {
        let r = browser.page().and_then(|page| browser.fit_viewport(&page, asked.0, asked.1, false));
        browser.close();
        r
    });
    if held.is_some() {
        drop(held);
        ctx.reg.set("preparing", "before the first block");
    }
    match fitted {
        Ok(()) => Ok(None),
        Err(e) if e.validator() == Some(ValidatorId::DisplayTooSmall) => Ok(e.refusal.map(|r| *r)),
        Err(e) if e.code == 130 => Err(e),
        Err(_) => Ok(None),
    }
}

/// `VariantMismatch` before anything builds: base and head would run the same thing, so the comparison
/// would be an A/A by accident. Builds (a folder, a git ref) of a target that names what it runs itself
/// (a `url()`'s address, an `exe()`'s or a `native(path)`'s executable, an app profile's); two folders
/// of one checkout (each is that checkout's root, `resolve_variant`); or two `url:`, `exe:` or `app:`
/// variants (or one beside such a target) that name the same page, executable or app. Not for `--aa`,
/// an A/A on purpose.
fn same_target(target: &Value, variants: &[Variant]) -> Option<Refusal> {
    let [b, h] = variants else { return None };
    let kind = target["kind"].as_str()?;
    let fixed = |key: &str| target[key].as_str().unwrap_or("?").to_string();
    // What a side runs, when nothing it builds decides it.
    let runs = |v: &Variant| -> Option<String> {
        match (&v.kind, kind) {
            (Kind::Url(href), "url") => Some(format!("the page at {href}")),
            (Kind::Url(href), "web") => Some(format!("the page at {}", join_url(href, target["url"].as_str().unwrap_or("/")))),
            (Kind::Exe(p), _) => Some(format!("the executable {}", p.display())),
            (Kind::App(id), _) => Some(format!("app {id}'s installed executable")),
            (Kind::Dir | Kind::Git(_), "url") => Some(format!("the page at {}", fixed("href"))),
            (Kind::Dir | Kind::Git(_), "exe") => Some(format!("the executable {}", fixed("path"))),
            (Kind::Dir | Kind::Git(_), "native" | "webview") if target["path"].is_string() => Some(format!("the executable {}", fixed("path"))),
            (Kind::Dir | Kind::Git(_), "app") => Some(format!("app {}'s installed executable", fixed("id"))),
            _ => None,
        }
    };
    let what = match (&b.kind, &h.kind, runs(b), runs(h)) {
        (Kind::Dir, Kind::Dir, _, _) if b.root.is_some() && b.root == h.root => {
            let root = b.root.as_deref().map(|r| r.display().to_string()).unwrap_or_default();
            format!("both name the checkout at {}, whose root a folder variant stands for", root.trim_start_matches(r"\\?\"))
        }
        (_, _, Some(x), Some(y)) if x == y => format!("this target runs {x} on both sides, whatever is built"),
        _ => return None,
    };
    Some(
        Refusal::new(ValidatorId::VariantMismatch, format!("base ({}) and head ({}) would run the same thing: {what}", b.label, h.label)).fix(
            "name what each side runs (--base url:<href> --head url:<href>, exe:<path>, app:<id>, two checkouts or git refs), give the target something strobe builds or serves (web() with build and serve, native(appId) with a build), or --aa for an A/A on purpose",
        ),
    )
}

/// `check_engines` for one target of a series: a usage error or whole-run refusal, else the series' own
/// refusal (its summary without the series' label, which every listing prints beside it).
fn target_refusal(s: &Series, name: &str, describe: &Value, args: &Args) -> Result<Option<Refusal>> {
    let t = &describe["targets"][name];
    let page = matches!(t["kind"].as_str(), Some("web" | "url"));
    if args.get("engine").is_some() && !page {
        return Err(Error::usage(format!("{}: --engine picks a page's browser; {} is a {} target (narrow the run with --target)", s.label(), name, t["kind"].as_str().unwrap_or("?"))));
    }
    if args.has("headless") && !page {
        return Err(Error::usage(format!("{}: --headless runs pages in a headless browser; {} is a {} target, which opens its own window", s.label(), name, t["kind"].as_str().unwrap_or("?"))));
    }
    let reduce = Motion::of(t, name)?;
    // A command's app reaches strobe once it printed its endpoint: what it drew before is not reduced.
    if reduce.reduced() && t["kind"] == "command" && s.launch.is_object() {
        let on = if s.sides.is_some() { format!("{}: ", t["name"].as_str().unwrap_or(name)) } else { String::new() };
        return Ok(Some(
            Refusal::new(ValidatorId::NotForTarget, format!("{on}a command's app takes reduced motion only from DevTools once strobe attaches to it, after its launch drew"))
                .fix("time work after the launch in a run case, or have the command start the app with --force-prefers-reduced-motion and drop the target's motion"),
        ));
    }
    if !page {
        return Ok(None);
    }
    if s.launch["profile"] == "warm" {
        return Err(launch::warm_page(&s.case));
    }
    let engine = engine_of(t);
    // On one side of an implementation comparison, the side named.
    let on = if s.sides.is_some() { format!("{} ({engine}): ", t["name"].as_str().unwrap_or(name)) } else { String::new() };
    let headless = t["headless"] == true || args.has("headless");
    // An emulated device: none in WebKit; a phone only headless, narrower than a desktop browser's window.
    let emulation = emulation_of(t, name)?;
    if emulation.touches() {
        let why = web::refusal_as(engine, true, &emulation).or_else(|| super::record::phone_refusal(&emulation, headless));
        if let Some(r) = why.and_then(|e| e.refusal) {
            return Ok(Some(Refusal { summary: format!("{on}{}", r.summary), ..*r }));
        }
    }
    // Reduced motion in a WebKit with no setting of its own for it (Safari, WPE), before WPE's own refusal.
    if let Some(r) = motion::engine_refusal(engine, headless).filter(|_| reduce.reduced()) {
        return Ok(Some(Refusal { summary: format!("{on}{}", r.summary), ..r }));
    }
    if let Some(e) = web::refusal(engine, headless) {
        return Err(e);
    }
    if engine == WebEngine::Webkit && t["scale"].is_number() {
        return Err(Error::refused(
            "EngineUnsupported",
            format!("{}: the target asks for scale {}, and a WebKit page draws at the display's own: WebDriver has no device scale override", s.label(), t["scale"]),
            "drop the target's scale for --engine webkit, or measure the scaled page in Chromium or Firefox",
        ));
    }
    // Firefox attributes with its own profiler and leak-checks with about:memory (`probe::gecko`); a
    // WebKit page has neither in strobe.
    if engine == WebEngine::Webkit && matches!(args.get("probes"), Some("attribution" | "memory")) {
        return Err(Error::refused(
            "EngineUnsupported",
            format!("{}: --probes {}: WebKit pages have no profilers or memory census in strobe", s.label(), args.get("probes").unwrap_or("")),
            "attribute or leak-check the page in Chromium or Firefox (--engine firefox)",
        ));
    }
    // A launch is timed by an observer in every document from its first script: Chromium's DevTools and
    // Firefox's preload scripts put it there; WebDriver Classic has no preload script.
    Ok(if engine == WebEngine::Webkit && s.launch.is_object() {
        Some(
            Refusal::new(ValidatorId::EngineUnsupported, format!("{on}a {engine} page has no preload script over WebDriver Classic, so nothing of strobe's sees a launch's document from its start"))
                .fix("measure the launch in Chromium or Firefox, or time the load in a run case (phase around page.goto)"),
        )
    } else if let Some(r) = validity::engine_refusal(engine, &s.primary) {
        Some(Refusal { summary: format!("{on}{}", r.summary), ..r })
    } else if (t["headless"] == true || args.has("headless")) && (display_metric(&s.primary) || gpu_use(&s.primary)) {
        Some(
            Refusal::new(ValidatorId::Headless, format!("{on}the primary {} needs a display, and the page runs headless: nothing it draws reaches one", s.primary))
                .fix("drop --headless (and the target's headless) to measure it on screen, or choose a CPU primary: cpu.work, cpu.app.total, run.duration or phase.<name> (main.work in Chromium)"),
        )
    } else {
        None
    })
}

/// The page's own changes since the last read (`CHANGES_SCRIPT`), on the host clock, as the earliest and
/// latest each can have happened. The read returns the page's monotonic clock with them, between two
/// host-clock reads; each change goes back from those by its distance from that clock, give or take the
/// 1 ms the page's clock may be coarsened to. The latest says whether it can have followed an input; the
/// earliest is the soonest a frame showing it can have begun. With them, the page's trusted pointer
/// presses on the same terms (`CHANGES_SCRIPT`).
fn page_changes(page: &Page) -> PageChanges {
    let h0 = host::now_ns();
    let read: (f64, Vec<f64>, Vec<(f64, u8, f64)>) = page
        .eval("JSON.stringify([performance.now(), globalThis.__strobeChanges?.splice(0) ?? [], globalThis.__strobePresses?.splice(0) ?? []])")
        .ok()
        .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
        .unwrap_or_default();
    let h1 = host::now_ns().max(h0) as f64;
    let (now, times, presses) = read;
    let back = |h: f64, ms: f64| (h - ms.max(0.0) * 1e6).max(0.0) as u64;
    let at = |t: f64| probe::present::frames::Change(back(h0 as f64, now - t + 1.0), back(h1, now - t - 1.0), false);
    PageChanges { changes: times.iter().map(|&t| at(t)).collect(), presses: presses.iter().map(|&(t, acted, again)| (at(t), acted == 1, (again >= 0.0).then(|| at(again)))).collect() }
}

/// What `page_changes` read: the page's changes, and its trusted presses, each with whether the page
/// changed between it and its release and when it first changed after the release, before the next
/// press, key or wheel turn.
#[derive(Default)]
struct PageChanges {
    changes: Vec<probe::present::frames::Change>,
    presses: Vec<(probe::present::frames::Change, bool, Option<probe::present::frames::Change>)>,
}

impl PageChanges {
    fn extend(&mut self, o: PageChanges) {
        self.changes.extend(o.changes);
        self.presses.extend(o.presses);
    }

    /// Each single click's say on whether the page acted on its press (`Sent::acted`): it changed
    /// between the press and the release, and not again after the release within a second. The
    /// page's press for a click is the first one not yet matched that can have followed strobe's
    /// press and can have come before strobe's next input: a page whose main thread was busy past the
    /// release sees the press late, still in order.
    fn acted(&self, sent: &mut [probe::present::frames::Sent]) {
        let mut from = 0;
        for k in 0..sent.len() {
            let Some(p) = sent[k].press else { continue };
            let next = sent.get(k + 1).map_or(u64::MAX, |n| n.press.unwrap_or(n.t_ns));
            let release = sent[k].t_ns;
            let found = self.presses.iter().enumerate().skip(from).find(|(_, (d, _, _))| d.1 > p && d.0 < next);
            sent[k].acted = found.map(|(i, (_, acted, again))| {
                from = i + 1;
                *acted && !again.is_some_and(|a| a.0 < release + probe::present::frames::HORIZON_NS)
            });
        }
    }
}

/// The page's own changes, for input latency where the browser reports none (Firefox): when the DOM
/// changed (in open and closed shadow roots too), an input or change event fired, or the page scrolled.
/// At most one entry per millisecond: a page that mutates in a tight loop fills nothing. Beside them,
/// each trusted pointer press with whether the page changed between it and its release, and when it
/// first changed after the release before the next press, key or wheel turn, in the page's own order
/// of events (a mutation the press's handlers made, or one queued after the press: never one from
/// before it, such as a hover's), which no clock can tell within a millisecond. Installed in every
/// document of a headed series in such an engine.
const CHANGES_SCRIPT: &str = r#"(() => {
  if (globalThis.__strobeChangesOn) return
  globalThis.__strobeChangesOn = true
  globalThis.__strobeChanges ??= []
  globalThis.__strobePresses ??= []
  let last = -1, pressed = null, released = null
  const at = () => {
    const c = globalThis.__strobeChanges, t = performance.now()
    if (pressed) pressed[1] = 1
    else if (released && released[2] < 0) released[2] = t
    if (c.length < 10000 && Math.floor(t) !== last) { last = Math.floor(t); c.push(t) }
  }
  const press = { capture: true, passive: true }
  addEventListener("pointerdown", (e) => {
    const p = globalThis.__strobePresses
    released = null
    if (e.isTrusted && p.length < 10000) { pressed = [performance.now(), 0, -1]; p.push(pressed) }
  }, press)
  for (const e of ["pointerup", "pointercancel"]) addEventListener(e, (ev) => { if (ev.isTrusted) { released = pressed; pressed = null } }, press)
  for (const e of ["keydown", "wheel"]) addEventListener(e, (ev) => { if (ev.isTrusted) released = null }, press)
  const opts = { subtree: true, childList: true, attributes: true, characterData: true }
  const observer = new MutationObserver(at)
  observer.observe(document, opts)
  const attach = Element.prototype.attachShadow
  Element.prototype.attachShadow = function (init) {
    const root = attach.call(this, init)
    observer.observe(root, opts)
    return root
  }
  for (const e of ["input", "change", "scroll"]) addEventListener(e, at, true)
})()"#;

/// A page's frames where the engine reports none of its own (Firefox, headed): the OS's presents of its
/// processes in the measured runs (§7.2, as for a native app: on macOS and Linux X11 the intervals and
/// drops are refused), and `interaction.*` / `scroll.latency.*` from strobe's inputs to the first frame
/// shown after the page changed (§7.4).
#[allow(clippy::too_many_arguments)]
fn os_frames(
    ctx: &mut Ctx,
    s: &Series,
    v: &Variant,
    presents: Result<probe::present::Presents>,
    pids: &[u32],
    timed: &[metrics::Timed],
    inputs: &[RunInputs],
    values: &mut BTreeMap<String, f64>,
    refused: &mut BTreeMap<String, Refusal>,
) -> Option<probe::present::Presents> {
    use probe::present::frames;
    let spans: &[(u64, u64)] = &timed.iter().map(|r| (r.t0, r.t1)).collect::<Vec<_>>();
    let declared: Vec<String> = declared_metrics(s).into_iter().cloned().collect();
    let frame_metric = |m: &str| ["frames.", "interaction.", "scroll.", "display."].iter().any(|p| m.starts_with(p));
    let line = |why: &str| Refusal::from_line(why).unwrap_or_else(|| Refusal::new(ValidatorId::NoPresentSource, why));
    let p = match presents {
        Ok(p) => p,
        Err(e) => {
            for m in declared.iter().filter(|m| frame_metric(m)) {
                refused.entry(m.clone()).or_insert_with(|| Refusal::new(ValidatorId::NoPresentSource, format!("{m}: OS present events: {}", e.message)));
            }
            return None;
        }
    };
    let display = host::main_window(pids).and_then(|w| host::display_of(w.hwnd));
    let f = frames::native_values(&p, pids, timed, display.as_ref());
    let refresh = f.values.get("display.hz").map(|hz| 1000.0 / hz);
    let span_ms = p.t1_ns.saturating_sub(p.t0_ns) as f64 / 1e6;
    let overhead = p.consumer_cpu_ms.filter(|_| span_ms > 0.0).map(|c| 100.0 * c / span_ms);
    let whole = match overhead {
        Some(o) if o > 2.0 => Some(format!("OverheadBudget: the OS present session's consumer used {o:.2}% of a core (budget 0.5%), so the frames it saw describe a disturbed run")),
        _ => f.refused.clone().or_else(|| refresh.and_then(|ms| frames::unpaced(&p.presents, pids, spans, ms))),
    };
    if let Some(why) = whole {
        for m in declared.iter().filter(|m| frame_metric(m)) {
            refused.insert(m.clone(), line(&why));
        }
        return Some(p);
    }
    values.extend(f.values);
    for m in &declared {
        if let Some(w) = f.partial.get(m) {
            refused.entry(m.clone()).or_insert_with(|| line(w));
        }
    }
    if !inputs.is_empty() {
        let quiet = frames::quiet_ns(refresh.unwrap_or(1000.0 / 60.0));
        let mut reactions = frames::Reactions::default();
        for (sent, changes, _) in inputs {
            reactions.add(frames::reactions(sent, changes, &p.presents, pids, quiet));
        }
        let mut why = BTreeMap::new();
        reactions.values(p.events_lost, values, &mut why);
        for m in &declared {
            if let Some(w) = why.get(m) {
                refused.entry(m.clone()).or_insert_with(|| line(&w.replace("the app's accessibility tree", "the page").replace("app's tree", "page")));
            }
        }
    }
    if p.events_lost > 0 && !ctx.warnings.iter().any(|w| w.id == "PresentsLost") {
        ctx.warnings.push(validity::warn("PresentsLost", format!("{} block: the present session lost {} events; frame counts may be short", side_of(ctx, v), p.events_lost)));
    }
    Some(p)
}

/// The gaps (ms, sorted) between the frames of `pids` shown in the runs while each next frame was handed
/// over before the last one showed plus half a refresh (the app kept drawing, §7.2): a traceless block's
/// frame pacing for reports, where the OS tells when a frame was handed over (Windows).
fn present_intervals(p: &probe::present::Presents, pids: &[u32], runs: &[metrics::Timed], refresh_ms: f64) -> Vec<f64> {
    let mut out = Vec::new();
    for run in runs {
        let mut shown = probe::present::frames::from_timed(&p.presents, pids, run).shown;
        shown.sort_by_key(|s| (s.stream, s.seg, s.screen_ns));
        for w in shown.windows(2).filter(|w| (w[0].stream, w[0].seg) == (w[1].stream, w[1].seg)) {
            if w[1].screen_ns > w[0].screen_ns && (w[1].pending_ns as f64) <= w[0].screen_ns as f64 + refresh_ms * 1e6 / 2.0 {
                out.push((w[1].screen_ns - w[0].screen_ns) as f64 / 1e6);
            }
        }
    }
    out.sort_by(f64::total_cmp);
    out
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

    /// CDP's Network events of the page's session, or BiDi's network events of the page's browsing context
    /// (`session` is the context then): a request's id stays across its redirects in both.
    fn take(&mut self, ev: &Value, current: Option<usize>) {
        let p = &ev["params"];
        if ev["sessionId"].as_str() != Some(self.session.as_str()) && p["context"].as_str() != Some(self.session.as_str()) {
            return;
        }
        let id = p["requestId"].as_str().or(p["request"]["request"].as_str()).unwrap_or("").to_string();
        match ev["method"].as_str().unwrap_or("") {
            // BiDi completes each hop of a redirect under the one id, each with its own bytes: summed.
            "network.responseCompleted" if self.owner.contains_key(&id) => {
                *self.finished.entry(id).or_default() += p["response"]["bytesReceived"].as_f64().unwrap_or(0.0);
            }
            "Network.requestWillBeSent" | "network.beforeRequestSent" => {
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
    let (kind, label, root) = if let Some(line) = spec.strip_prefix("cmd:") {
        if line.trim().is_empty() {
            return Err(Error::usage("cmd:<line> needs a command line"));
        }
        (Kind::Cmd(line.to_string()), line.to_string(), Some(std::env::current_dir()?))
    } else if let Some(href) = spec.strip_prefix("url:") {
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
            // A folder in a checkout is that checkout, as a git ref's worktree is: strobe.json's
            // `build.cwd` and `serve.cwd` resolve against its root, whichever folder of it was named.
            let named = std::fs::canonicalize(&p)?;
            let root = git::toplevel(&named).and_then(|t| std::fs::canonicalize(t).ok()).unwrap_or(named);
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
                target: None,
                name: String::new(),
            };
            v.commit = (!sha.is_empty()).then_some(sha);
            v.name = v.label.clone();
            return Ok(v);
        } else {
            (Kind::Git(spec.to_string()), spec.to_string(), None)
        }
    };
    let mut v = Variant {
        role,
        spec: spec.to_string(),
        kind,
        label,
        root,
        base_url: None,
        server: None,
        exe: None,
        commit: None,
        dirty: false,
        state: None,
        changed: Vec::new(),
        target: None,
        name: String::new(),
    };
    if let Kind::Git(r) = &v.kind {
        let sha = git::out(project, &["rev-parse", "--short=10", r])
            .ok_or_else(|| Error::usage(format!("{spec} is neither a directory nor a git ref in {}", project.display())))?;
        v.label = format!("{r}@{sha}");
        v.commit = Some(sha);
    }
    v.name = v.label.clone();
    Ok(v)
}

/// A side of an implementation comparison in `--base`/`--head`: `target:<name>[=<variant>]`, or
/// `<name>[=<variant>]` where `<name>` is a target of the bench file. A bare name that is also a
/// folder or a git ref is a usage error that asks for `target:<name>`. None: a variant as before.
fn side_spec<'a>(spec: &'a str, describe: &Value, project: &Path) -> Result<Option<(&'a str, Option<&'a str>)>> {
    let targets = &describe["targets"];
    let known = || targets.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
    if let Some(rest) = spec.strip_prefix("target:") {
        let (name, variant) = rest.split_once('=').map_or((rest, None), |(n, v)| (n, Some(v)));
        if !targets[name].is_object() {
            return Err(Error::usage(format!("{spec}: the bench file has no target {name} (its targets: {})", known())));
        }
        return Ok(Some((name, variant)));
    }
    let (name, variant) = spec.split_once('=').map_or((spec, None), |(n, v)| (n, Some(v)));
    if !targets[name].is_object() {
        return Ok(None);
    }
    if variant.is_none() {
        let dir = Path::new(name).is_dir();
        let commit = git::out(project, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")]).is_some();
        if dir || commit {
            return Err(Error::usage(format!(
                "{name} is the bench file's target {name} and also a {}: say target:{name} for the target, or {} for the variant",
                if dir { "folder here" } else { "git ref" },
                if dir { format!("./{name}") } else { format!("git:{name}") }
            )));
        }
    }
    Ok(Some((name, variant)))
}

/// The project a target's builds and app profiles come from: its `cwd`'s strobe.json root (another
/// repository), else none (the bench file's own).
pub(crate) fn target_project(describe: &Value, name: &str) -> Result<Option<(PathBuf, Value)>> {
    let Some(cwd) = describe["targets"][name]["cwd"].as_str().filter(|_| describe["targets"][name]["kind"] != "command") else { return Ok(None) };
    let file = Path::new(describe["file"].as_str().unwrap_or("."));
    let dir = file.parent().unwrap_or(Path::new(".")).join(cwd);
    let dir = std::fs::canonicalize(&dir).map_err(|_| Error::usage(format!("target {name}: cwd {cwd} is no folder ({})", dir.display())))?;
    let project = config::project_root(&dir);
    let config = config::load(&project)?;
    Ok(Some((project, config)))
}

/// Every target's own project, for those with a `cwd`.
fn target_projects(describe: &Value) -> Result<BTreeMap<String, (PathBuf, Value)>> {
    let mut out = BTreeMap::new();
    for name in describe["targets"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        if let Some(p) = target_project(describe, &name)? {
            out.insert(name, p);
        }
    }
    Ok(out)
}

/// A side: the target `name` at `variant` (default: the target's `cwd` folder, else the bench file's
/// project), resolved in the target's own project.
fn resolve_side(role: &'static str, name: &str, variant: Option<&str>, describe: &Value, project: &Path, config: &Value) -> Result<Variant> {
    let own = target_project(describe, name)?;
    let (project, config) = own.as_ref().map_or((project, config), |(p, c)| (p.as_path(), c));
    // The checkout strobe.json's builds run in: the target's own project, or the bench file's.
    let default = project.display().to_string();
    let mut v = resolve_variant(role, variant.unwrap_or(&default), project, config)?;
    v.target = Some(name.to_string());
    v.name = describe["targets"][name]["name"].as_str().unwrap_or(name).to_string();
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

/// Builds and serves the variants for a series: each for the target its side runs, in that target's
/// own project (its `cwd`'s strobe.json, with no harness copied into it) or the bench file's.
fn prepare_all(ctx: &mut Ctx, s: &Series, variants: &mut [Variant], config: &Value) -> Result<()> {
    for v in variants.iter_mut() {
        let name = v.target_of(s).to_string();
        let target = ctx.describe["targets"][&name].clone();
        match ctx.projects.get(&name) {
            Some((project, own)) => prepare_variant(v, &target, own, &Harness { from: project.clone(), paths: Vec::new() }, project)?,
            None => prepare_variant(v, &target, config, &ctx.harness, &ctx.project)?,
        }
    }
    // Builds of one target, or each side on its own target.
    match &s.sides {
        None => unbuilt(&ctx.describe["targets"][&s.target], variants, &s.label()),
        Some(_) => variants.iter().try_for_each(|v| unbuilt(&ctx.describe["targets"][v.target_of(s)], std::slice::from_ref(v), &s.label())),
    }
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
    if matches!(v.kind, Kind::Cmd(_)) && target["kind"] != "cli" {
        return Err(Error::usage(format!("{}: a cmd:<line> variant is a command line, for a cli() target; this target is a {}", v.role, target["kind"].as_str().unwrap_or("?"))));
    }
    if matches!(target["kind"].as_str(), Some("command") | Some("script") | Some("cli")) {
        // A command line runs as given, in the folder strobe started in: nothing to place or build.
        if let Kind::Cmd(_) = &v.kind {
            return Ok(());
        }
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
    if is_native(target) || target["kind"] == "webview" {
        // `native(path)` and `webview(path)` name a fixed executable: no build replaces it.
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
            .ok_or_else(|| Error::usage(format!("a native or webview target's build in strobe.json needs output for {}: the executable it builds", crate::target::isolate::os())))?;
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
        let all = host::topology().all();
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
    let all = topo.all();
    let pick = |g: &host::CacheGroup, i: usize| (Some(g.mask), Some(all & !g.mask), format!("CCD{i} ({} MB L3, {} CPUs) · runner on the other CPUs", g.l3_mb, g.cpus));
    match spec {
        "none" => Ok((None, None, "unpinned".into())),
        // SPEC §9.4 `--pin auto`: within the CPUs strobe may use, by the machine's caches and cores.
        "auto" => {
            let p = topo.auto_pin();
            Ok((p.target, p.reserved, p.reason))
        }
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
        if let Some(why) = sus["refused"].as_str() {
            out += "            (dom: nodes in the document; nodes no document holds show as orphan DOM bytes in query memory)\n";
            out += &format!("  suspects  none named: {why}\n");
            out += &format!("  next      strobe query {} memory --at 0 · strobe query {} memory\n", l["recording"].as_str().unwrap_or(""), l["recording"].as_str().unwrap_or(""));
            continue;
        }
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

/// `self   strobe 1.2% · runner 0.3% … of a core (mean over the blocks' runs)`, when the series measured it,
/// and what `ctx.untimed` left out of its runs.
fn self_line(s: &Value) -> String {
    let mut out = match serde_json::from_value::<Overhead>(s["overhead"].clone()).ok().map(|o| o.text()).filter(|t| !t.is_empty()) {
        Some(t) => format!("self   {t} (mean over the blocks' runs)\n"),
        None => String::new(),
    };
    if let Some(u) = untimed_line(&s["untimed"]) {
        out += &format!("untimed  {u}\n");
    }
    out
}

/// A `--blocks` series' text: the primary in each block, and the blocks' median with a 95% interval
/// (Hodges–Lehmann over the blocks; log scale when every value is positive). Nothing is compared.
fn blocks_text(r: &Value, s: &Value, file: &str) -> String {
    let metric = s["primary"]["metric"].as_str().unwrap_or("");
    let def = model::metric(metric);
    let unit = def.map_or("", |d| d.unit);
    let values: Vec<f64> = s["blocks"].as_array().into_iter().flatten().filter_map(|b| b["values"][metric].as_f64()).filter(|v| v.is_finite()).collect();
    let blocks = s["blocks"].as_array().map_or(0, Vec::len);
    // A `url()` target's address, not the build, whose code it never loads.
    let head = s["href"].as_str().filter(|_| r["variants"][0]["kind"] != "url").or(r["variants"][0]["label"].as_str()).unwrap_or("");
    let mut out = format!("strobe bench {file} · {} · head {head} · {blocks} blocks, nothing compared\n", s["label"].as_str().unwrap_or(""));
    let input = s["input"].as_str().map(|i| format!("{i} input")).unwrap_or_default();
    let window = s["window"].as_str().map(|w| format!("window {w}")).unwrap_or_default();
    let engine = fmt::fields([s["browser"].as_str().unwrap_or(""), &input, &window, motion_note(s, &window), if s["headless"] == true { "headless (no display)" } else { "" }]);
    if !engine.is_empty() {
        out += &format!("env    {engine}\n");
    }
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
    out += &warnings_text(s["warnings"].as_array().into_iter().flatten());
    out + "\n"
}

/// `reduced motion` for a series whose target asked for it, unless its window's label already says so
/// (a series with no window of its own: windowless, or two implementations).
fn motion_note(s: &Value, window: &str) -> &'static str {
    if s["motion"] == "reduce" && !window.contains(motion::LABEL) { motion::LABEL } else { "" }
}

/// `\nwarnings\n! Id  text` lines, or nothing.
fn warnings_text<'a>(warns: impl Iterator<Item = &'a Value>) -> String {
    let lines: String = warns.map(|w| format!("{} {:<24}{}\n", if w["level"] == "warn" { "!" } else { "i" }, w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or(""))).collect();
    if lines.is_empty() { lines } else { format!("\nwarnings\n{lines}") }
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
        // An implementation comparison names its sides; builds name their variants.
        let sides = r["comparison"] == "implementations";
        let (bn, hn) = if sides { (base["name"].as_str().unwrap_or("base"), head["name"].as_str().unwrap_or("head")) } else { ("base", "head") };
        if s["kind"] == "gap" {
            out += &format!("strobe bench {file} · {}   {bn} vs {hn}\n  not compared: {}\n\n", s["label"].as_str().unwrap_or(""), s["primary"]["refusal"]["summary"].as_str().unwrap_or(""));
            continue;
        }
        if s["kind"] == "refused" {
            out += &format!("strobe bench {file} · {}\n  not run: {}\n\n", s["label"].as_str().unwrap_or(""), refused_text(&s["primary"]["refusal"]));
            continue;
        }
        if sides {
            out += &format!("strobe bench {file} · {}   {bn} ({})   vs   {hn} ({})\n", s["label"].as_str().unwrap_or(""), base["label"].as_str().unwrap_or(""), head["label"].as_str().unwrap_or(""));
        } else if let Some(href) = s["href"].as_str().filter(|_| base["kind"] != "url" && head["kind"] != "url") {
            // A `url()` target loads its address whatever the build: the builds are not what ran.
            out += &format!("strobe bench {file} · {}   {href} on both sides\n", s["label"].as_str().unwrap_or(""));
        } else {
            out += &format!("strobe bench {file} · {}   base {}   head {}\n", s["label"].as_str().unwrap_or(""), base["label"].as_str().unwrap_or(""), head["label"].as_str().unwrap_or(""));
        }
        let cpus = env["logical_cpus"].as_u64().map(|n| format!("{n} logical CPUs")).unwrap_or_default();
        out += &format!("env    {}\n", fmt::fields([env["os"].as_str().unwrap_or(""), env["cpu"].as_str().unwrap_or(""), &cpus]));
        let gpu = r["gpu"]["devices"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
        // A script, a command line or a `window: false` app (every side's) has no window and draws to no
        // display (`windowless` in the series; older results by the target kinds).
        let kinds = if sides { vec![&s["target_kinds"]["base"], &s["target_kinds"]["head"]] } else { vec![&s["target_kind"]] };
        let windowless = s["windowless"].as_bool().unwrap_or_else(|| kinds.iter().all(|k| matches!(k.as_str(), Some("script" | "cli"))));
        // Two implementations: the window both were measured in, or none (one side's alone says nothing).
        let size = if sides {
            let c = &r["fairness"]["window"]["client_dip"];
            match (c[0].as_f64(), c[1].as_f64()) {
                (Some(x), Some(y)) => format!("{x:.0}×{y:.0}"),
                _ => String::new(),
            }
        } else {
            // The series' own window (results from before it: the experiment's last one).
            s["window"].as_str().map_or_else(|| fmt::size(&r["window"]), str::to_string)
        };
        let window = Some(size).filter(|s| !s.is_empty() && !windowless).map(|s| format!("window {s}")).unwrap_or_default();
        // A headless page drew to no display: no refresh, no visibility to claim.
        let headless = s["headless"] == true;
        let refresh = if headless {
            "headless (no display)".to_string()
        } else if windowless {
            String::new()
        } else {
            r["refresh_hz"].as_f64().map(|h| format!("display {h:.0} Hz")).unwrap_or_default()
        };
        let browser = s["browser"].as_str().or(r["browser"].as_str()).unwrap_or("");
        let input = s["input"].as_str().map(|i| format!("{i} input")).unwrap_or_default();
        let display = fmt::fields([browser, &input, gpu, &window, motion_note(&s, &window), &refresh]);
        if !display.is_empty() {
            out += &format!("       {display}\n");
        }
        let screen = match s["session"].as_str() {
            _ if headless => "headless (no display)",
            _ if windowless => "no window",
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
        // A seeded workload: both sides ran the same inputs, and `--seed` replays them.
        let seeded = if r["plan"]["used_random"] == true { format!(" · seed {}", r["plan"]["seed"]) } else { String::new() };
        out += &format!("blocks {} pairs ({} fresh launches) · {per_block}{seeded} · stopped: {}\n", s["pairs"], s["launches"], s["stopped"].as_str().unwrap_or(""));
        let steps: Vec<(String, f64)> = serde_json::from_value(s["steps"].clone()).unwrap_or_default();
        out += &format!("time   {} · per block {}\n", fmt::ms_duration(s["elapsed_ms"].as_f64().unwrap_or(0.0) as u64), steps_text(&steps));
        out += &self_line(&s);
        if let Some(line) = shown_line(&s, (bn, hn)) {
            out += &format!("shown  {line}{}\n", if s["shown"]["changed"] == true { "  ! different screens (ScreenChanged)" } else { "" });
        }
        let f = &r["fixture"];
        if f.is_object() {
            let yes = |x: &Value| x.as_bool().map_or("not read", |c| if c { "connected" } else { "not connected" });
            let cpu = |x: &Value| x.as_f64().map_or("—".to_string(), |ms| fmt::value(ms, "ms"));
            out += &format!(
                "fixture {}{} · {bn} {} ({} CPU per run) · {hn} {} ({})\n",
                f["command"].as_str().unwrap_or(""),
                f["seed_sha256"].as_str().map(|h| format!(" · seed {} (sha256 {})", f["seed"].as_str().unwrap_or(""), &h[..12.min(h.len())])).unwrap_or_default(),
                yes(&f["connected"]["base"]),
                cpu(&f["cpu_ms_per_run"]["base"]),
                yes(&f["connected"]["head"]),
                cpu(&f["cpu_ms_per_run"]["head"]),
            );
        }
        out.push('\n');
        // Columns as wide as their widest cell: a long value (`480.3 M instr`) or side name keeps the rest in line.
        let mut table: Vec<Vec<String>> = vec![vec!["metric".into(), bn.to_string(), hn.to_string(), "Δ".into(), "CI".into(), "verdict".into()]];
        let mut rows = vec![(true, s["primary"].clone())];
        rows.extend(s["secondaries"].as_array().cloned().unwrap_or_default().into_iter().map(|c| (false, c)));
        let descriptive = s["descriptive"].as_array().cloned().unwrap_or_default();
        let tested = rows.len();
        rows.extend(descriptive.into_iter().map(|c| (false, c)));
        for (i, (primary, c)) in rows.into_iter().enumerate() {
            if i == tested {
                table.push(vec!["  · descriptive (not tested)".into()]);
            }
            let name = c["metric"].as_str().unwrap_or("");
            // A percentage compared as a difference is in percentage points (`cells`).
            let (b, h, delta, ci) = cells(&c);
            table.push(vec![format!("{} {name}", if primary { "★" } else { " " }), b, h, delta, ci, c["verdict"].as_str().unwrap_or("").to_string()]);
        }
        out += &fmt::table(&table, &[false, true, true, true, false, false], "", 2);
        if let Some(per) = s["per_side"].as_object().filter(|_| sides) {
            let names: std::collections::BTreeSet<&String> = per.values().filter_map(Value::as_object).flat_map(|o| o.keys()).collect();
            if !names.is_empty() {
                let mut per_table: Vec<Vec<String>> = vec![vec![String::new(), bn.to_string(), hn.to_string(), "per implementation, not compared (one engine's own)".into()]];
                for name in names {
                    let cell = |role: &str| {
                        let m = &per[role][name.as_str()];
                        m["median"].as_f64().map_or("-".to_string(), |x| fmt::value(x, m["unit"].as_str().unwrap_or("")))
                    };
                    per_table.push(vec![format!("  {name}"), cell("base"), cell("head"), String::new()]);
                }
                out.push('\n');
                out += &fmt::table(&per_table, &[false, true, true, false], "", 2);
            }
        }
        if sides {
            out += &format!("\n{hn} against {bn}: Δ and verdicts say how {hn} differs from {bn}\n");
            for c in std::iter::once(&s["primary"]).chain(s["secondaries"].as_array().into_iter().flatten()) {
                if let Some(n) = c["care_note"].as_str() {
                    out += &format!("  {}: {n}\n", c["metric"].as_str().unwrap_or(""));
                }
            }
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
        // The series' own warnings; the experiment's are said once, after every series.
        out += &warnings_text(s["warnings"].as_array().into_iter().flatten());
        let attr = &s["attribution"];
        if sides && (attr["base"].is_string() || attr["head"].is_string()) {
            // Each implementation on its own, with its own kind's view: two share no code to diff.
            let kinds = &s["target_kinds"];
            let line = |role: &str, name: &str| attr[role].as_str().map(|rec| format!("       {name}: strobe query {rec} {}\n", why_view(&kinds[role])));
            out += &format!(
                "\nwhy    attribution blocks, each on its own: {bn} {} · {hn} {}\n{}{}       strobe report experiment {} --open (both side by side)\n",
                attr["base"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("—"),
                line("base", bn).unwrap_or_default(),
                line("head", hn).unwrap_or_default(),
                r["id"].as_str().unwrap_or("")
            );
        } else if attr.get("base").is_some() || attr.get("head").is_some() {
            out += &format!(
                "\nwhy    attribution blocks: base {} · head {}\n       strobe query {} {}\n",
                attr["base"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("—"),
                attr["head"].as_str().unwrap_or("rec_…"),
                why_view(&s["target_kind"])
            );
        }
        let at: Vec<String> = [("base", bn), ("head", hn)].iter().filter_map(|(r, name)| Some(format!("{name} {}", sampled(&s["attribution_sampling"][r])?))).collect();
        if !at.is_empty() {
            out += &format!("       JavaScript {}\n", at.join(" · "));
        }
        if s["video"]["base"].is_string() || s["video"]["head"].is_string() {
            out += &format!(
                "\nvideo  one warmed run per side, untimed: base {} · head {}{}\n       strobe report experiment {} --open (both players side by side)\n",
                s["video"]["base"].as_str().unwrap_or("-"),
                s["video"]["head"].as_str().unwrap_or("-"),
                if s["video"]["mixed"] == true { " (different kinds: not comparable side by side)" } else { "" },
                r["id"].as_str().unwrap_or("")
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
    if let Some(w) = r["warnings"].as_array().filter(|w| !w.is_empty()) {
        out += &warnings_text(w.iter()).replacen("\nwarnings\n", "warnings (the experiment's)\n", 1);
        out += "\n";
    }
    let id = r["id"].as_str().unwrap_or("");
    // An implementation comparison's journeys as one family (SPEC §9.3).
    if r["scoreboard"].is_object() {
        let board = super::family::Board::of(r);
        let (head, rest) = board.counts();
        let gaps = board.family.gaps;
        let correction = board.correction();
        out += &format!(
            "family  {}{}{}\n",
            fmt::fields(std::iter::once(format!("{head} compared journeys")).chain(rest).chain((gaps > 0).then(|| format!("{gaps} not comparable")))),
            if correction.is_empty() { "" } else { " · " },
            correction
        );
    }
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
    if r["series"].as_array().is_some_and(|a| a.iter().any(|s| s["kind"] == "blocks") && a.iter().all(|s| s["kind"] == "blocks" || s["kind"] == "refused")) {
        out += &format!("{id} · strobe query {id}\n");
    } else {
        out += &format!("{id} · strobe report experiment {id} --open\n");
    }
    out
}
