//! Plain data shared by every module: errors, metrics, verdicts.

use serde::Serialize;
use std::fmt;

/// An error with the exit code it maps to (SPEC §5.2) and an optional fix. A refusal (exit 4) also
/// carries what refused it, typed: `refusal` is set exactly when `code` is 4 and it came from
/// `Error::refused` or a `Refusal`.
#[derive(Debug)]
pub struct Error {
    pub code: i32,
    pub message: String,
    pub hint: Option<String>,
    pub refusal: Option<Box<Refusal>>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Error { code, message: message.into(), hint: None, refusal: None }
    }
    pub fn msg(message: impl Into<String>) -> Self {
        Error::new(1, message)
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Error::new(2, message)
    }
    /// Something did not happen in time (exit 124).
    pub fn timeout(message: impl Into<String>) -> Self {
        Error::new(124, message)
    }
    /// A refusal by the validator named `validator` (its SPEC §10.1 id). A name that is no validator
    /// is strobe's own bug: it fails as an internal error instead of refusing under a made-up id.
    pub fn refused(validator: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        match ValidatorId::from_name(validator) {
            Some(id) => Refusal::new(id, message).fix(hint).into(),
            None => Error::msg(format!("internal: {validator} is not a validator: {}", message.into())),
        }
    }
    pub fn unreachable(message: impl Into<String>) -> Self {
        Error::new(255, message)
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    /// The validator that refused, for an exit-4 error.
    pub fn validator(&self) -> Option<ValidatorId> {
        self.refusal.as_ref().map(|r| r.validator)
    }
    /// How the window was hidden, for a `WindowHidden` refusal whose producer said.
    pub fn hidden(&self) -> Option<Hidden> {
        self.refusal.as_ref().and_then(|r| r.hidden)
    }
}

macro_rules! validators {
    ($($id:ident),* $(,)?) => {
        /// A validator (SPEC §10.1): what refuses a run, or one metric of it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
        pub enum ValidatorId { $($id),* }

        impl ValidatorId {
            pub const ALL: &[ValidatorId] = &[$(ValidatorId::$id),*];

            /// The id as SPEC, messages and result.json write it (`WindowHidden`).
            pub fn name(self) -> &'static str {
                match self { $(ValidatorId::$id => stringify!($id)),* }
            }
        }
    };
}

validators!(
    SoftwareGpu,
    DevBuild,
    DebuggerAttached,
    WindowHidden,
    WrongScreen,
    UnpacedSpin,
    NoPresentSource,
    AmbiguousInteraction,
    PatternInput,
    SessionLocked,
    ConcurrentBench,
    VariantMismatch,
    RunIncomplete,
    Throttled,
    Background,
    BackgroundPriority,
    PresentsUnmatched,
    OverheadBudget,
    NoDisplayForBudget,
    TailTooShort,
    ClockUnsynced,
    LiveChannel,
    NoPhase,
    NoInput,
);

impl ValidatorId {
    pub fn from_name(name: &str) -> Option<ValidatorId> {
        ValidatorId::ALL.iter().copied().find(|v| v.name() == name)
    }

    /// The machine's state at the moment, not the target or the bench: a locked session, a hidden
    /// window (a display off on idle, another app in front), other work on the CPUs. It passes, so a
    /// bench waits it out or runs the block again rather than give up.
    pub fn is_transient_environment(self) -> bool {
        use ValidatorId::*;
        match self {
            SessionLocked | WindowHidden | Background => true,
            SoftwareGpu | DevBuild | DebuggerAttached | WrongScreen | UnpacedSpin | NoPresentSource | AmbiguousInteraction | PatternInput | ConcurrentBench | VariantMismatch | RunIncomplete
            | Throttled | BackgroundPriority | PresentsUnmatched | OverheadBudget | NoDisplayForBudget | TailTooShort | ClockUnsynced | LiveChannel | NoPhase | NoInput => false,
        }
    }
}

impl fmt::Display for ValidatorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a window counted as hidden (`WindowHidden`), where a bench acts on the difference: on macOS
/// it waits for a person to leave the Mac when the window is covered or the app is not the active
/// one, and otherwise wakes the display once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hidden {
    /// Other apps' windows cover it (macOS).
    Covered,
    /// Its app is not the active app, where strobe measures it active (macOS).
    NotActive,
    /// Closed, hidden, minimized, cloaked, or a page that is not visible.
    Other,
}

/// Why a validator refused: a run (in an `Error`) or one metric (in a `Comparison`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Refusal {
    pub validator: ValidatorId,
    /// What was wrong.
    pub summary: String,
    /// What strobe saw beyond the summary, such as the busiest processes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// How to make the run valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// `WindowHidden` only, when the producer knows: how the window was hidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<Hidden>,
}

impl Refusal {
    pub fn new(validator: ValidatorId, summary: impl Into<String>) -> Self {
        Refusal { validator, summary: summary.into(), detail: None, fix: None, hidden: None }
    }
    /// A `WindowHidden` refusal that says how the window was hidden.
    pub fn window_hidden(hidden: Hidden, summary: impl Into<String>) -> Self {
        Refusal { hidden: Some(hidden), ..Refusal::new(ValidatorId::WindowHidden, summary) }
    }
    /// A metric's refusal as a block's `refused` map writes it, `<validator>: <why>`, read back.
    /// A line whose prefix is no validator is `None`.
    pub fn from_line(line: &str) -> Option<Refusal> {
        let (id, why) = line.split_once(": ").unwrap_or((line, ""));
        Some(Refusal::new(ValidatorId::from_name(id)?, why))
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
    /// `<validator>: <summary>[; <detail>]`, as an error's message says it.
    pub fn message(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}: {}; {d}", self.validator, self.summary),
            None => format!("{}: {}", self.validator, self.summary),
        }
    }
}

impl From<Refusal> for Error {
    fn from(r: Refusal) -> Self {
        Error { code: 4, message: r.message(), hint: r.fix.clone(), refusal: Some(Box::new(r)) }
    }
}

/// `#[serde(with = "refusal_lines")]` for a metric → `Refusal` map that result.json keeps as
/// `"<metric>": "<validator>: <why>"`, as blocks have always written it (SPEC 10.1).
pub mod refusal_lines {
    use super::Refusal;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(map: &BTreeMap<String, Refusal>, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_map(map.iter().map(|(k, r)| (k, r.message())))
    }

    /// Lines whose prefix is no validator (older results) are dropped.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<BTreeMap<String, Refusal>, D::Error> {
        let lines = BTreeMap::<String, String>::deserialize(d)?;
        Ok(lines.into_iter().filter_map(|(k, l)| Some((k, Refusal::from_line(&l)?))).collect())
    }
}

/// What a comparison decided (SPEC §9.3), whatever words its verdict uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    /// Head is better: `faster`, `less`, `fewer`, `smoother`, … (small or exact included).
    Better,
    /// Head is worse: `slower`, `more`, …, or `worse (>X)` against a no-worse margin.
    Worse,
    /// Inside the equivalence bound: `same (±2%)`, `same (exact)`.
    Same,
    /// Inside a no-worse margin: `no worse (≤5%)`.
    NoWorse,
    Unclear,
    /// More pairs would decide it: `underpowered: ~n pairs needed`.
    Underpowered,
    /// Both sides sit at the metric's ceiling, where builds cannot differ.
    Saturated,
    /// The bench was stopped before a verdict.
    Stopped,
    /// A validator refused the metric: the comparison's `refusal` says which.
    Refused,
    /// A secondary the gate did not test: the primary was not decided, or Holm kept it (`—`).
    Untested,
    /// A descriptive metric: shown, never decided (`·`).
    Descriptive,
}

impl VerdictKind {
    pub fn decided(self) -> bool {
        matches!(self, VerdictKind::Better | VerdictKind::Worse | VerdictKind::Same | VerdictKind::NoWorse)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::msg(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::msg(format!("json: {e}"))
    }
}

#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::model::Error::msg(format!($($t)*))) };
}

/// Which direction is better for a metric.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Better {
    Lower,
    Higher,
}

/// How a metric's values behave (SPEC §7.1).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Timed,
    Sampled,
    Exact,
}

/// One entry of the metric catalog.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MetricDef {
    pub name: &'static str,
    pub unit: &'static str,
    pub better: Better,
    pub kind: Kind,
    /// Words for the better and worse directions, e.g. ("faster", "slower").
    #[serde(skip)]
    pub words: (&'static str, &'static str),
    /// Frames needed per block, for percentiles; 0 when not a percentile.
    pub min_n: usize,
    pub about: &'static str,
}

const TIME: (&str, &str) = ("faster", "slower");
const COUNT: (&str, &str) = ("fewer", "more");
const AMOUNT: (&str, &str) = ("less", "more");
const SMOOTH: (&str, &str) = ("smoother", "choppier");

pub const METRICS: &[MetricDef] = &[
    MetricDef {
        name: "frames.presented.p50",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 20,
        about: "median frame latency (begin of frame to presentation) of presented frames",
    },
    MetricDef { name: "frames.presented.p90", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 100, about: "90th percentile frame cost" },
    MetricDef { name: "frames.presented.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 200, about: "95th percentile frame cost" },
    MetricDef { name: "frames.presented.p99", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1000, about: "99th percentile frame cost" },
    MetricDef { name: "frames.presented.max", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1, about: "worst presented frame cost" },
    MetricDef { name: "frames.presented.count", unit: "count", better: Better::Higher, kind: Kind::Timed, words: ("more", "fewer"), min_n: 0, about: "presented frames per run" },
    MetricDef { name: "canvas.frames", unit: "count", better: Better::Higher, kind: Kind::Timed, words: ("more", "fewer"), min_n: 0, about: "frames the page drew into a WebGPU canvas, per run" },
    MetricDef { name: "canvas.interval.p50", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 20, about: "median gap between the page's own canvas frames" },
    MetricDef {
        name: "canvas.interval.p95",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: SMOOTH,
        min_n: 200,
        about: "95th percentile gap between the page's own canvas frames (its stutter, at its own frame rate)",
    },
    MetricDef { name: "canvas.interval.p99", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 1000, about: "99th percentile gap between the page's own canvas frames" },
    MetricDef { name: "canvas.interval.max", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 1, about: "longest gap between the page's own canvas frames" },
    MetricDef {
        name: "frames.interval.p50",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: SMOOTH,
        min_n: 20,
        about: "median gap between presented frames while the page wanted updates",
    },
    MetricDef { name: "frames.interval.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 200, about: "95th percentile gap between presented frames (stutter)" },
    MetricDef { name: "frames.interval.p99", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 1000, about: "99th percentile gap between presented frames" },
    MetricDef { name: "frames.interval.max", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 1, about: "longest gap between presented frames" },
    MetricDef { name: "frames.dropped.pct", unit: "%", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 0, about: "share of vsyncs with pending work that presented nothing" },
    MetricDef {
        name: "frames.dropped",
        unit: "count",
        better: Better::Lower,
        kind: Kind::Timed,
        words: COUNT,
        min_n: 0,
        about: "vsyncs Chromium reported as dropped, per run; native and outside-in: missed refreshes while drawing plus discarded presents (OS present events)",
    },
    MetricDef { name: "frames.discarded", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "presents the OS replaced before they reached the screen, per run" },
    MetricDef { name: "frames.partial", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "frames presented without all updates, per run" },
    MetricDef { name: "main.busy", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "renderer main thread busy share during runs" },
    MetricDef {
        name: "main.work",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "renderer main-thread task time per run: the work an interaction costs, not quantized to frames like its duration",
    },
    MetricDef { name: "main.script", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "JavaScript time on the renderer main thread per run" },
    MetricDef {
        name: "main.gc",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "garbage collection on the renderer main thread per run (V8's top-level GC slices); traced only for series that declare it",
    },
    MetricDef { name: "main.layout", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "layout time on the renderer main thread per run" },
    MetricDef { name: "main.style", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "style recalculation time on the renderer main thread per run" },
    MetricDef { name: "layout.count", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "layouts per run" },
    MetricDef { name: "style.count", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "style recalculations per run" },
    MetricDef { name: "main.tasks.long", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "main-thread tasks over 50 ms, per run" },
    MetricDef {
        name: "interaction.p50",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 1,
        about: "input to presentation, median of discrete inputs (native apps: SPEC §7.4)",
    },
    MetricDef { name: "interaction.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1, about: "input to presentation, 95th percentile of discrete inputs" },
    MetricDef {
        name: "input.rate",
        unit: "/s",
        better: Better::Higher,
        kind: Kind::Timed,
        words: ("more", "fewer"),
        min_n: 0,
        about: "pointer moves strobe sent per second while a button was held (page.drag, moves after page.down(\"Mouse\")): a driver that starved shows here, not as dropped frames",
    },
    MetricDef {
        name: "input.gap.p95",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: SMOOTH,
        min_n: 0,
        about: "95th percentile gap between consecutive pointer moves of a drag, as strobe sent them",
    },
    MetricDef {
        name: "scroll.latency.p50",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 1,
        about: "scroll update input to presentation, median (native apps: the first wheel step of each scroll)",
    },
    MetricDef { name: "scroll.latency.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 20, about: "scroll update input to presentation, 95th percentile" },
    MetricDef { name: "run.duration", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 0, about: "wall time of one run" },
    MetricDef { name: "cpu.app.total", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "CPU of every process of the target, % of one core, during runs" },
    MetricDef {
        name: "cpu.foreign",
        unit: "%",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "other processes' work on the CPUs the target is pinned to during runs, % of one core: a validity signal, not the target's cost",
    },
    MetricDef { name: "cpu.process.total", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "alias of cpu.app.total" },
    MetricDef { name: "cpu.renderer", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "CPU of the browser's renderer processes, % of one core, during runs" },
    MetricDef {
        name: "cpu.gpu",
        unit: "%",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "CPU of the browser's GPU process (compositing, GPU commands), % of one core, during runs",
    },
    MetricDef { name: "cpu.browser", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "CPU of the browser process itself, % of one core, during runs" },
    MetricDef {
        name: "memory.app.private",
        unit: "MB",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "private bytes of every target process after the measured runs (Windows: the commit charge, which includes commit held for the processes' GPU allocations; macOS: the physical footprint)",
    },
    MetricDef {
        name: "memory.app.host",
        unit: "MB",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "native targets: memory the processes allocated in their own address space after the measured runs (heap, stacks, anonymous memory), without GPU memory mapped into them or held for them (Windows, Linux)",
    },
    MetricDef {
        name: "gpu.compile",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 0,
        about: "time the browser's GPU process spent creating pipelines and compiling shaders per run (Dawn), on any of its threads",
    },
    MetricDef {
        name: "gpu.process.3d",
        unit: "%",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "GPU 3D engine use by the target's processes during runs, summed over every adapter they used (% of one engine)",
    },
    MetricDef { name: "gpu.vram", unit: "MB", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "dedicated VRAM of the target's processes after the measured runs" },
    MetricDef {
        name: "gpu.shared",
        unit: "MB",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "GPU memory of the target's processes in system memory after the measured runs: all of an integrated GPU's, and a discrete one's staging",
    },
    MetricDef { name: "js.heap.used", unit: "MB", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "renderer JS heap in use after the measured runs" },
    MetricDef { name: "dom.nodes", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "DOM nodes after the measured runs" },
    MetricDef { name: "js.heap.growth", unit: "MB/min", better: Better::Lower, kind: Kind::Sampled, words: AMOUNT, min_n: 0, about: "post-GC JS heap growth per minute over a memory recording" },
    MetricDef { name: "dom.nodes.growth", unit: "count/min", better: Better::Lower, kind: Kind::Sampled, words: COUNT, min_n: 0, about: "DOM node growth per minute over a memory recording" },
    MetricDef {
        name: "leak.suspects.retained",
        unit: "MB",
        better: Better::Lower,
        kind: Kind::Sampled,
        words: AMOUNT,
        min_n: 0,
        about: "memory retained by objects allocated in a memory recording's first half and still alive at its end",
    },
    MetricDef {
        name: "cpu.work",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "CPU time of the target's processes per run (all threads): a script target's main.work, a launch's total",
    },
    MetricDef {
        name: "ui.cpu",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "native apps: CPU time per run of the thread that owns the main window (the UI thread), from its cycle counter: the UI work an interaction costs, from outside",
    },
    MetricDef {
        name: "ui.wakes",
        unit: "/s",
        better: Better::Lower,
        kind: Kind::Timed,
        words: COUNT,
        min_n: 0,
        about: "native apps: context switches of the UI thread (the main window's) per second during runs",
    },
    MetricDef {
        name: "cpu.wakes",
        unit: "/s",
        better: Better::Lower,
        kind: Kind::Timed,
        words: COUNT,
        min_n: 0,
        about: "context switches of the target's threads per second during runs: each time one of its threads was switched in, after a wait (a timer, an event, IO) or a preemption; most are waits at rest. Driver and OS pool threads inside its processes count too (Windows; `query <rec> threads` names the threads and the module each started in)",
    },
    MetricDef { name: "io.read", unit: "bytes", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "bytes the target's processes read per run (files, devices, pipes)" },
    MetricDef { name: "io.write", unit: "bytes", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "bytes the target's processes wrote per run (files, devices, pipes)" },
    MetricDef {
        name: "js.heap.retained",
        unit: "MB",
        better: Better::Lower,
        kind: Kind::Timed,
        words: AMOUNT,
        min_n: 0,
        about: "script targets: the JS heap a run leaves behind, after a full collection before and after it",
    },
    MetricDef {
        name: "layout.shift",
        unit: "score",
        better: Better::Lower,
        kind: Kind::Timed,
        words: ("steadier", "jumpier"),
        min_n: 0,
        about: "summed layout-shift score per run, shifts right after the run's input included",
    },
    MetricDef {
        name: "net.requests",
        unit: "count",
        better: Better::Lower,
        kind: Kind::Exact,
        words: COUNT,
        min_n: 0,
        about: "requests the page started per run; `net.requests{<url glob>}` counts the matching ones",
    },
    MetricDef {
        name: "net.bytes",
        unit: "bytes",
        better: Better::Lower,
        kind: Kind::Exact,
        words: AMOUNT,
        min_n: 0,
        about: "bytes received over the network per run (as encoded on the wire); `net.bytes{<url glob>}` for matching requests",
    },
    MetricDef {
        name: "count",
        unit: "count",
        better: Better::Lower,
        kind: Kind::Exact,
        words: COUNT,
        min_n: 0,
        about: "`count.<name>`: what the bench file (`ctx.count`) or the page (`strobe.count`) counted per run",
    },
    MetricDef {
        name: "phase",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 0,
        about: "`phase.<name>`: wall time of the run's `phase(name, fn)` spans, from strobe's own marks (summed when a run has several)",
    },
    MetricDef { name: "launch.ready", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 0, about: "a launch case: from process spawn until its `until` selector is visible" },
    MetricDef {
        name: "launch.painted",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 0,
        about: "a launch case: from process spawn until the window's pixels first differ from its first frame (Windows)",
    },
    MetricDef {
        name: "launch.settled",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 0,
        about: "a launch case: from process spawn until the window's pixels last changed before ready (Windows)",
    },
    MetricDef {
        name: "launch.milestone",
        unit: "ms",
        better: Better::Lower,
        kind: Kind::Timed,
        words: TIME,
        min_n: 0,
        about: "`launch.<name>`: from process spawn until a milestone (a log line, Node bootstrap)",
    },
];

/// A metric's definition. Families resolve by their base: `net.requests{<glob>}` → `net.requests`,
/// `count.<name>` → `count`, `phase.<name>` → `phase`, `launch.<milestone>` → `launch.milestone`.
pub fn metric(name: &str) -> Option<&'static MetricDef> {
    let base = name.split_once('{').map_or(name, |(b, _)| b);
    let find = |n: &str| METRICS.iter().find(|m| m.name == n);
    find(base).or_else(|| match base.split_once('.') {
        Some(("count", rest)) if !rest.is_empty() => find("count"),
        Some(("phase", rest)) if !rest.is_empty() => find("phase"),
        Some(("launch", rest)) if !rest.is_empty() => find("launch.milestone"),
        _ => None,
    })
}
