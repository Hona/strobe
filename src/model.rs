//! Plain data shared by every module: errors, metrics, verdicts.

use serde::Serialize;
use std::fmt;

/// An error with the exit code it maps to (SPEC §5.2) and an optional fix.
#[derive(Debug)]
pub struct Error {
    pub code: i32,
    pub message: String,
    pub hint: Option<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Error { code: 1, message: message.into(), hint: None }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Error { code: 2, message: message.into(), hint: None }
    }
    pub fn refused(validator: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Error { code: 4, message: format!("{validator}: {}", message.into()), hint: Some(hint.into()) }
    }
    pub fn unreachable(message: impl Into<String>) -> Self {
        Error { code: 255, message: message.into(), hint: None }
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
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
    MetricDef { name: "frames.presented.p50", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 20, about: "median frame latency (begin of frame to presentation) of presented frames" },
    MetricDef { name: "frames.presented.p90", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 100, about: "90th percentile frame cost" },
    MetricDef { name: "frames.presented.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 200, about: "95th percentile frame cost" },
    MetricDef { name: "frames.presented.p99", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1000, about: "99th percentile frame cost" },
    MetricDef { name: "frames.presented.max", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1, about: "worst presented frame cost" },
    MetricDef { name: "frames.presented.count", unit: "count", better: Better::Higher, kind: Kind::Timed, words: ("more", "fewer"), min_n: 0, about: "presented frames per run" },
    MetricDef { name: "frames.interval.p50", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 20, about: "median gap between presented frames while the page wanted updates" },
    MetricDef { name: "frames.interval.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 200, about: "95th percentile gap between presented frames (stutter)" },
    MetricDef { name: "frames.interval.p99", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 1000, about: "99th percentile gap between presented frames" },
    MetricDef { name: "frames.interval.max", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 1, about: "longest gap between presented frames" },
    MetricDef { name: "frames.dropped.pct", unit: "%", better: Better::Lower, kind: Kind::Timed, words: SMOOTH, min_n: 0, about: "share of vsyncs with pending work that presented nothing" },
    MetricDef { name: "frames.dropped", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "vsyncs Chromium reported as dropped, per run" },
    MetricDef { name: "frames.partial", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "frames presented without all updates, per run" },
    MetricDef { name: "main.busy", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "renderer main thread busy share during runs" },
    MetricDef { name: "main.work", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "renderer main-thread task time per run: the work an interaction costs, not quantized to frames like its duration" },
    MetricDef { name: "main.script", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "JavaScript time on the renderer main thread per run" },
    MetricDef { name: "main.layout", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "layout time on the renderer main thread per run" },
    MetricDef { name: "main.style", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "style recalculation time on the renderer main thread per run" },
    MetricDef { name: "layout.count", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "layouts per run" },
    MetricDef { name: "style.count", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "style recalculations per run" },
    MetricDef { name: "main.tasks.long", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "main-thread tasks over 50 ms, per run" },
    MetricDef { name: "interaction.p50", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1, about: "input to presentation, median of discrete inputs" },
    MetricDef { name: "interaction.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 1, about: "input to presentation, 95th percentile of discrete inputs" },
    MetricDef { name: "scroll.latency.p95", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 20, about: "scroll update input to presentation, 95th percentile" },
    MetricDef { name: "run.duration", unit: "ms", better: Better::Lower, kind: Kind::Timed, words: TIME, min_n: 0, about: "wall time of one run" },
    MetricDef { name: "cpu.app.total", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "CPU of every process of the target, % of one core, during runs" },
    MetricDef { name: "cpu.process.total", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "alias of cpu.app.total" },
    MetricDef { name: "memory.app.private", unit: "MB", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "private bytes of every target process after the measured runs" },
    MetricDef { name: "gpu.process.3d", unit: "%", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "GPU 3D engine use by the target's processes during runs" },
    MetricDef { name: "gpu.vram", unit: "MB", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "dedicated VRAM of the target's processes after the measured runs" },
    MetricDef { name: "js.heap.used", unit: "MB", better: Better::Lower, kind: Kind::Timed, words: AMOUNT, min_n: 0, about: "renderer JS heap in use after the measured runs" },
    MetricDef { name: "dom.nodes", unit: "count", better: Better::Lower, kind: Kind::Timed, words: COUNT, min_n: 0, about: "DOM nodes after the measured runs" },
    MetricDef { name: "js.heap.growth", unit: "MB/min", better: Better::Lower, kind: Kind::Sampled, words: AMOUNT, min_n: 0, about: "post-GC JS heap growth per minute over a memory recording" },
    MetricDef { name: "dom.nodes.growth", unit: "count/min", better: Better::Lower, kind: Kind::Sampled, words: COUNT, min_n: 0, about: "DOM node growth per minute over a memory recording" },
    MetricDef { name: "leak.suspects.retained", unit: "MB", better: Better::Lower, kind: Kind::Sampled, words: AMOUNT, min_n: 0, about: "memory retained by objects allocated in a memory recording's first half and still alive at its end" },
];

pub fn metric(name: &str) -> Option<&'static MetricDef> {
    METRICS.iter().find(|m| m.name == name)
}
