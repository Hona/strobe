//! `query a..b <view>`: one view of two recordings side by side, per measured run (SPEC §11.1), and
//! what an experiment's report and insights take from it: the biggest movers and the diff flame.
//!
//! Two recordings are two launches, often of two builds in two folders: rows match by name and a
//! portable location (the last steps of a file's path, a script URL without its origin, query and
//! bundler hash), never by line, which moves between builds.

use super::{NativeStacks, Output, Part, Rec, Stacks, fmt};
use crate::cli::Args;
use crate::model::{Error, Result};
use crate::probe::symbols::Frame;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

/// The views `a..b` answers, and `query <exp> <view>` for each series' base and head.
pub const VIEWS: [&str; 9] = ["hotspots", "alloc", "layout", "style", "threads", "frames", "summary", "heap", "memory"];

/// The trace views an attribution recording's report and insights read: one trace pass.
pub const ATTRIBUTION_PARTS: [Part; 3] = [Part::Layout, Part::Gc, Part::LongTasks];

pub fn run(a: &Rec, b: &Rec, view: &str, args: &Args, top: usize) -> Result<Output> {
    if let (Some(x), Some(y)) = (implementation(a), implementation(b))
        && x.0 != y.0
    {
        return Err(not_comparable(&format!("{} ran {} and {} ran {}", a.id, x.1, b.id, y.1), view, &[&a.id, &b.id]));
    }
    run_with(a, b, view, args, top, None)
}

/// The side of an implementation comparison a recording ran (SPEC §9.10): its target and the name
/// texts call it. None for any other recording.
fn implementation(rec: &Rec) -> Option<(String, String)> {
    let exp = crate::store::resolve(rec.meta["experiment"].as_str()?).ok()?;
    let r = crate::store::read_json(&exp.join("result.json")).ok()?;
    if r["comparison"] != "implementations" {
        return None;
    }
    let v = r["variants"].as_array()?.iter().find(|v| v["role"] == rec.meta["variant"])?;
    Some((v["target"].as_str()?.to_string(), v["name"].as_str().or(v["target"].as_str())?.to_string()))
}

/// `NotComparable`: two implementations share no code, so no view of one is a diff of the other's.
/// The hint names each recording's own view.
fn not_comparable(what: &str, view: &str, recs: &[&str]) -> Error {
    Error::usage(format!("NotComparable: {what}, two implementations that share no code: a..b {view} would match functions, threads or heaps that are not the same thing"))
        .hint(recs.iter().map(|r| format!("strobe query {r} {view}")).collect::<Vec<_>>().join(" · "))
}

fn run_with(a: &Rec, b: &Rec, view: &str, args: &Args, top: usize, plan_runs: Option<u64>) -> Result<Output> {
    match view {
        "hotspots" | "alloc" | "layout" => {
            let by = if view == "layout" { "function" } else { super::by_arg(args)? };
            let (thread, phase) = (args.get("thread"), args.get("phase"));
            if view != "hotspots" && (thread.is_some() || phase.is_some()) {
                return Err(Error::usage("--thread and --phase select native samples: a..b hotspots takes them"));
            }
            let kind = match view {
                "alloc" => Kind::Alloc,
                "layout" => Kind::Layout,
                _ => Kind::hotspots(a, b, args.has("native") || thread.is_some() || phase.is_some()),
            };
            // A thread or a phase: each side's native samples for that question alone.
            let filtered = if thread.is_some() || phase.is_some() {
                let one = |r: &Rec| super::native_filtered(&r.dir, thread, phase)?.ok_or_else(|| Error::usage(format!("{} has no native samples; record with --probes deep", r.id)));
                Some([one(a)?, one(b)?])
            } else {
                None
            };
            let over = |i: usize| filtered.as_ref().map(|f| &f[i]);
            // Both sides' sources load at once.
            std::thread::scope(|s| {
                s.spawn(|| side(a, kind, plan_runs, over(0)).is_ok());
                let _ = side(b, kind, plan_runs, over(1));
            });
            let sides = [side(a, kind, plan_runs, over(0))?, side(b, kind, plan_runs, over(1))?];
            let mut d = weighted(sides, kind, by);
            d.selection = [thread.map(|t| format!("thread {t}")), phase.map(|p| format!("phase {p}"))].into_iter().flatten().collect::<Vec<_>>().join(", ");
            Ok(d.output(a, b, top))
        }
        "style" => style(a, b, top),
        "threads" => threads(a, b, top),
        "frames" => frames(a, b),
        "summary" => summary(a, b),
        "heap" => super::heap_diff(a, b, top, args.has("reveal")),
        "memory" => super::memory_diff(&a.dir, &b.dir, top),
        other => Err(Error::usage(format!("a..b supports {}, not {other}", VIEWS.join(" ")))),
    }
}

/// `query <exp> <view>`: the view of each series' base and head attribution recordings.
pub fn experiment(result: &Value, view: &str, args: &Args, top: usize) -> Result<Output> {
    if result["comparison"] == "implementations" {
        let recs: Vec<&str> = result["series"].as_array().into_iter().flatten().flat_map(|s| ["base", "head"].map(|r| s["attribution"][r].as_str())).flatten().collect();
        let name = |i: usize| result["variants"][i]["name"].as_str().unwrap_or(if i == 0 { "base" } else { "head" }).to_string();
        let what = format!("{} compares {} with {}", result["id"].as_str().unwrap_or("this experiment"), name(1), name(0));
        let mut e = not_comparable(&what, view, &recs);
        if recs.is_empty() {
            e = e.hint(format!("this experiment kept no attribution recordings (bench --why always keeps one per side); its per-side numbers: strobe query {} summary", result["id"].as_str().unwrap_or("")));
        }
        return Err(e);
    }
    let plan_runs = result["plan"]["runs_per_block"].as_u64();
    let (mut text, mut out) = (String::new(), Vec::new());
    for s in result["series"].as_array().into_iter().flatten() {
        let (Some(b), Some(h)) = (s["attribution"]["base"].as_str(), s["attribution"]["head"].as_str()) else { continue };
        let label = s["label"].as_str().unwrap_or("");
        text += &format!("{label} · {b}..{h}\n");
        match Rec::resolve(b).and_then(|base| Ok((base, Rec::resolve(h)?))).and_then(|(base, head)| run_with(&base, &head, view, args, top, plan_runs)) {
            Ok(o) => {
                text += &o.text;
                out.push(json!({ "series": label, "base": b, "head": h, "diff": o.json }));
            }
            Err(e) => {
                text += &format!("  {}\n", e.message);
                out.push(json!({ "series": label, "base": b, "head": h, "error": e.message }));
            }
        }
        text.push('\n');
    }
    if out.is_empty() {
        return Err(Error::usage(format!(
            "{view} compares a series' base and head attribution recordings, and {} kept none (bench keeps them with --why always, or when a series differs)",
            result["id"].as_str().unwrap_or("this experiment")
        )));
    }
    Ok(Output::new(text, json!({ "experiment": result["id"], "view": view, "series": out })))
}

/// What a weighted view measures, and how one stack's weight becomes the view's unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Cpu,
    Native,
    Alloc,
    Layout,
}

impl Kind {
    /// CPU self time from the JavaScript profiles when both sides have them, else native samples.
    fn hotspots(a: &Rec, b: &Rec, native: bool) -> Kind {
        let deep = |r: &Rec| r.dir.join("deep.json.gz").is_file();
        if native || (deep(a) && deep(b) && (a.cpu().is_err() || b.cpu().is_err())) { Kind::Native } else { Kind::Cpu }
    }

    fn what(self) -> &'static str {
        match self {
            Kind::Cpu => "CPU self time",
            Kind::Native => "native self time",
            Kind::Alloc => "allocations",
            Kind::Layout => "forced layout",
        }
    }

    fn view(self) -> &'static str {
        match self {
            Kind::Cpu => "hotspots",
            Kind::Native => "hotspots --native",
            Kind::Alloc => "alloc",
            Kind::Layout => "layout",
        }
    }

    fn unit(self) -> &'static str {
        if self == Kind::Alloc { "bytes" } else { "ms" }
    }

    /// Stack weights are µs of time or bytes.
    fn scale(self) -> f64 {
        if self == Kind::Alloc { 1.0 } else { 1e-3 }
    }

    /// Sampled profiles have a sampling interval; forced layouts are traced, each one counted.
    fn sampled(self) -> bool {
        self != Kind::Layout
    }

    fn show(self, v: f64) -> String {
        if self == Kind::Alloc { fmt::bytes(v) } else { fmt::value(v, "ms") }
    }

    fn signed(self, v: f64) -> String {
        signed(v, &|x| self.show(x))
    }
}

/// One side of a weighted view: its stacks and the measured runs they cover. `untimed`: native samples
/// without times (macOS `sample`) that cover the whole capture, so no run count divides them.
struct Side<'a> {
    stacks: &'a Stacks,
    runs: Option<u64>,
    untimed: bool,
}

/// A side's stacks; `over` stands in for its native samples (one thread's or one phase's).
fn side<'a>(rec: &'a Rec, kind: Kind, plan_runs: Option<u64>, over: Option<&'a NativeStacks>) -> Result<Side<'a>> {
    let missing = || Error::usage(format!("{} has no native samples; record with --probes deep", rec.id));
    let runs = rec.runs().or(plan_runs);
    match kind {
        Kind::Cpu => Ok(Side { stacks: rec.cpu()?, runs, untimed: false }),
        Kind::Native => {
            let n = match over {
                Some(n) => n,
                None => rec.native()?.ok_or_else(missing)?,
            };
            Ok(Side { stacks: &n.stacks, runs: (!n.untimed).then_some(runs).flatten(), untimed: n.untimed })
        }
        Kind::Alloc => Ok(Side { stacks: rec.alloc()?, runs, untimed: false }),
        Kind::Layout => {
            let f = super::forced(rec)?;
            Ok(Side { stacks: &f.stacks, runs: (f.runs > 0).then_some(f.runs as u64), untimed: false })
        }
    }
}

/// Where a frame is, as two checkouts and two builds share it: its path from the first step a checkout
/// does not name (a monorepo's `packages/`, `crates/`, `apps/`, a toolchain's `library/`, else the
/// last `src/` or `lib/`), a dependency's from its package (`node_modules/…`, a cargo crate without
/// its version or hash), or a script URL without origin, query and bundler hash. The checkout's own
/// folder (a worktree named after a branch or a commit) never takes part.
fn portable_file(f: &Frame) -> String {
    if f.file.is_empty() && f.url.is_empty() {
        return "(native)".into();
    }
    if !f.mapped && !f.url.is_empty() {
        return portable_path(&crate::heapsnap::portable_url(&f.url));
    }
    portable_path(&f.file.replace('\\', "/"))
}

fn portable_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let from = |i: usize| parts[i..].join("/");
    // A JavaScript dependency: its package path (bun's `.bun/<pkg>@<version>/node_modules/` too).
    if let Some(i) = parts.iter().rposition(|s| *s == "node_modules") {
        return from(i + 1);
    }
    // A cargo dependency: `registry/src/<index>/<crate>-<version>/…`, `git/checkouts/<repo>-<hash>/<rev>/…`.
    let unversioned = |s: &str| s.rsplit_once('-').filter(|(_, v)| v.starts_with(|c: char| c.is_ascii_digit())).map_or(s.to_string(), |(n, _)| n.to_string());
    if let Some(i) = parts.windows(2).rposition(|w| w == ["registry", "src"]).filter(|i| parts.len() > i + 3) {
        return format!("{}/{}", unversioned(parts[i + 3]), parts[i + 4..].join("/"));
    }
    let rest: &[&str] = match parts.windows(2).rposition(|w| w == ["git", "checkouts"]) {
        Some(i) if parts.len() > i + 4 => &parts[i + 4..],
        _ => &parts,
    };
    // A build script's output: `<crate>-<16 hex>/out/…` keeps the crate.
    let hashless = |s: &str| match s.rsplit_once('-') {
        Some((n, h)) if h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()) => n.to_string(),
        _ => s.to_string(),
    };
    let tail = |i: usize| rest[i..].iter().map(|s| hashless(s)).collect::<Vec<_>>().join("/");
    if let Some(i) = rest.iter().rposition(|s| matches!(*s, "packages" | "crates" | "apps" | "library")) {
        return tail(i);
    }
    if let Some(i) = rest.iter().rposition(|s| matches!(*s, "src" | "lib")) {
        return tail(i);
    }
    tail(rest.len().saturating_sub(2))
}

/// Frames of both sides as one table of portable (name, file) pairs: each side's frame index → id.
#[derive(Default)]
struct Portable {
    frames: Vec<(String, String)>,
    at: HashMap<(String, String), u32>,
}

impl Portable {
    fn ids(&mut self, stacks: &Stacks) -> Vec<u32> {
        stacks
            .frames
            .iter()
            .map(|f| {
                let key = (f.name.clone(), portable_file(f));
                *self.at.entry(key.clone()).or_insert_with(|| {
                    self.frames.push(key);
                    (self.frames.len() - 1) as u32
                })
            })
            .collect()
    }
}

/// Pseudo-frames that are no work of the target's: a CPU profile's idle time and its root.
fn skipped(name: &str) -> bool {
    name == "(idle)" || name == "(root)"
}

struct Row {
    name: String,
    at: String,
    /// Per side: the value per run in the view's unit, the samples, and the sampling variance of the
    /// raw weight (µs² or bytes², before scaling and dividing by runs).
    v: [f64; 2],
    n: [u64; 2],
    var: [f64; 2],
    /// Per stack (portable ids, root first): per-run value on each side.
    stacks: HashMap<Vec<u32>, [f64; 2]>,
}

impl Row {
    fn delta(&self) -> f64 {
        self.v[1] - self.v[0]
    }
}

/// A weighted view of two recordings: rows by function, file or package, per run on each side when
/// both sides know their runs, else per recording on both.
pub struct Weighted {
    kind: Kind,
    by: String,
    rows: Vec<Row>,
    frames: Vec<(String, String)>,
    total: [f64; 2],
    samples: [u64; 2],
    /// What divides each side (1 when not per run).
    runs: [f64; 2],
    per_run: bool,
    untimed: bool,
    /// A thread or phase the native samples were narrowed to (said in the header).
    selection: String,
}

fn weighted(sides: [Side; 2], kind: Kind, by: &str) -> Weighted {
    // Per run only when both sides know their runs: dividing one side alone compares unlike things.
    let per_run = sides.iter().all(|s| s.runs.is_some());
    let runs = sides.each_ref().map(|s| if per_run { s.runs.unwrap_or(1).max(1) as f64 } else { 1.0 });
    let mut portable = Portable::default();
    let mut rows: BTreeMap<(String, String), Row> = BTreeMap::new();
    let (mut total, mut samples) = ([0.0; 2], [0u64; 2]);
    for (s, side) in sides.iter().enumerate() {
        let ids = portable.ids(side.stacks);
        let mut packages = BTreeMap::new();
        for wt in &side.stacks.weights {
            let Some(f) = wt.stack.last().map(|&i| &side.stacks.frames[i as usize]) else { continue };
            if skipped(&f.name) {
                continue;
            }
            let per = wt.w * kind.scale() / runs[s];
            let (name, at) = match by {
                "function" => (f.name.clone(), portable_file(f)),
                "file" if !f.file.is_empty() || !f.url.is_empty() => (portable_file(f), String::new()),
                _ => (super::row_key(f, by, &mut packages), String::new()),
            };
            let row = rows.entry((name.clone(), at.clone())).or_insert_with(|| Row { name, at, v: [0.0; 2], n: [0; 2], var: [0.0; 2], stacks: HashMap::new() });
            row.v[s] += per;
            row.n[s] += wt.n;
            row.var[s] += wt.var;
            if by == "function" {
                row.stacks.entry(wt.stack.iter().map(|&i| ids[i as usize]).collect()).or_insert([0.0; 2])[s] += per;
            }
            total[s] += per;
            samples[s] += wt.n;
        }
    }
    let mut rows: Vec<Row> = rows.into_values().collect();
    rows.sort_by(|x, y| y.delta().abs().total_cmp(&x.delta().abs()));
    Weighted { kind, by: by.to_string(), rows, frames: portable.frames, total, samples, runs, per_run, untimed: sides.iter().any(|s| s.untimed), selection: String::new() }
}

impl Weighted {
    /// The 95% interval of a row's change from sampling alone: each side's variance from its own
    /// sampler (`Weight`), in the view's unit per run. A side with no sample is not certain to be
    /// zero: it counts two samples' variance of the other side, which puts the line where an exact
    /// binomial test does for equal samples (0 against 5 samples, p ≈ 0.06, reads as noise; 0 against
    /// 6, p ≈ 0.03, does not). One launch per side: launch-to-launch variation is not in it.
    fn ci(&self, row: &Row) -> Option<f64> {
        if !self.kind.sampled() {
            return None;
        }
        let scale = self.kind.scale();
        let var = |s: usize, o: usize| {
            let raw = if row.n[s] > 0 { row.var[s] } else if row.n[o] > 0 { 2.0 * row.var[o] / row.n[o] as f64 } else { 0.0 };
            raw * scale * scale / (self.runs[s] * self.runs[s])
        };
        Some(1.96 * (var(0, 1) + var(1, 0)).sqrt())
    }

    /// A row's stacks where its change concentrated: the largest changes in the row's direction, each
    /// at least 5% of the row's change.
    fn concentrated<'r>(&self, row: &'r Row, n: usize) -> Vec<(f64, [f64; 2], &'r [u32])> {
        let sign = row.delta().signum();
        let floor = 0.05 * row.delta().abs();
        let mut v: Vec<(f64, [f64; 2], &[u32])> = row.stacks.iter().map(|(k, x)| (x[1] - x[0], *x, k.as_slice())).filter(|(d, _, _)| d * sign > 0.0 && d.abs() >= floor).collect();
        v.sort_by(|x, y| (y.0 * sign).total_cmp(&(x.0 * sign)).then_with(|| x.2.cmp(y.2)));
        v.truncate(n);
        v
    }

    fn noise(&self, row: &Row) -> bool {
        self.ci(row).is_some_and(|ci| row.delta().abs() <= ci)
    }

    fn output(&self, a: &Rec, b: &Rec, top: usize) -> Output {
        let k = self.kind;
        let runs = |s: usize| format!("{:.0}", self.runs[s]);
        let per = if self.untimed {
            "per recording: these native samples carry no times and cover the whole capture (launch, warmup runs and resets included), so no run count divides them".to_string()
        } else if self.per_run {
            format!("per run ({} and {} measured runs)", runs(0), runs(1))
        } else {
            "per recording (one side's run count is unknown, so neither is divided)".to_string()
        };
        let mut text = format!("{} by {} · base {} → head {}", k.what(), self.by, a.id, b.id);
        if !self.selection.is_empty() {
            text += &format!(" · {}", self.selection);
        }
        if k.sampled() {
            text += &format!(" · {} and {} samples", self.samples[0], self.samples[1]);
        }
        text += &format!(" · {per}\n  {:>10}  {:>10}  {:>11}  {:>9}  {}\n", "base", "head", "Δ", if k.sampled() { "±95%" } else { "" }, self.by);
        let mut rows_json = Vec::new();
        for row in self.rows.iter().take(top) {
            let ci = self.ci(row);
            let noise = self.noise(row);
            text += &format!(
                "{} {:>10}  {:>10}  {:>11}  {:>9}  {}{}\n",
                if noise { "~" } else { " " },
                k.show(row.v[0]),
                k.show(row.v[1]),
                k.signed(row.delta()),
                ci.map(|c| k.show(c)).unwrap_or_default(),
                row.name,
                if row.at.is_empty() { String::new() } else { format!("  {}", row.at) }
            );
            let mut stacks_json = Vec::new();
            for (d, v, stack) in self.concentrated(row, 3) {
                let share = if row.delta() != 0.0 { d / row.delta() } else { 0.0 };
                // The callers, innermost first: the row's own frame is the leaf.
                let callers: Vec<String> = stack
                    .iter()
                    .rev()
                    .skip(1)
                    .take(4)
                    .map(|&i| match &self.frames[i as usize] {
                        (name, file) if file == "(native)" => name.clone(),
                        (name, file) => format!("{name} ({file})"),
                    })
                    .collect();
                if !callers.is_empty() {
                    text += &format!("               {} ({:.0}%) ← {}{}\n", k.signed(d), 100.0 * share, callers.join(" ← "), if stack.len() > 5 { " ← …" } else { "" });
                }
                stacks_json.push(json!({
                    "delta": d, "share": share, "base": v[0], "head": v[1],
                    "frames": stack.iter().map(|&i| json!({ "name": self.frames[i as usize].0, "file": self.frames[i as usize].1 })).collect::<Vec<_>>(),
                }));
            }
            rows_json.push(json!({
                "name": row.name, "where": row.at, "base": row.v[0], "head": row.v[1], "delta": row.delta(), "ci95": ci, "within_noise": noise,
                "base_samples": row.n[0], "head_samples": row.n[1], "stacks": stacks_json,
            }));
        }
        if self.rows.len() > top {
            text += &format!("… {} more rows (--top)\n", self.rows.len() - top);
        }
        text += &format!("total     {} → {} ({}){}\n", k.show(self.total[0]), k.show(self.total[1]), k.signed(self.total[1] - self.total[0]), if k == Kind::Cpu { ", idle left out" } else { "" });
        text += match k {
            Kind::Cpu => "~ within sampling noise: the change is inside its 95% interval from sampling (each sample weighs the time to the next). One profiled launch per side: the interval covers sampling only, not launch-to-launch variation\n",
            Kind::Native => "~ within sampling noise: the change is inside its 95% interval from sampling (one interval per sample). One profiled launch per side: the interval covers sampling only, not launch-to-launch variation\n",
            Kind::Alloc => "~ within sampling noise: the change is inside its 95% interval from V8's allocation sampling (a large allocation is always sampled, a small one stands for the sampling interval). One profiled launch per side: the interval covers sampling only, not launch-to-launch variation\n",
            Kind::Layout => "traced: every forced layout and recalc in the measured runs counted, one launch per side (no interval across launches)\n",
        };
        let json = json!({
            "view": k.view(), "what": k.what(), "by": self.by, "unit": k.unit(), "sampled": k.sampled(), "per_run": self.per_run, "untimed": self.untimed, "selection": self.selection,
            "base": { "recording": a.id, "runs": self.runs[0], "samples": self.samples[0], "total": self.total[0] },
            "head": { "recording": b.id, "runs": self.runs[1], "samples": self.samples[1], "total": self.total[1] },
            "total_delta": self.total[1] - self.total[0], "rows_total": self.rows.len(), "rows": rows_json,
        });
        Output::new(text, json)
    }
}

/// An experiment series' insights: what moved most between the base and head profiled launches (CPU
/// self time, forced layout and allocations by function, per run, outside sampling noise), then the
/// head recording's own insights. Each side's sources load once, both sides at once. A view that is
/// not per run on both sides (untimed native samples, an unknown run count) names no mover.
pub fn series_insights(base: &Rec, head: &Rec, plan_runs: Option<u64>) -> Result<Output> {
    std::thread::scope(|s| {
        s.spawn(|| base.preload(&ATTRIBUTION_PARTS, true, true, false));
        head.preload(&ATTRIBUTION_PARTS, true, true, true);
    });
    let mut text = String::new();
    let mut movers = Vec::new();
    for kind in [Kind::hotspots(base, head, false), Kind::Layout, Kind::Alloc] {
        let (Ok(a), Ok(b)) = (side(base, kind, plan_runs, None), side(head, kind, plan_runs, None)) else { continue };
        let d = weighted([a, b], kind, "function");
        if !d.per_run {
            continue;
        }
        let floor = if kind == Kind::Alloc { 64.0 * 1024.0 } else { 1.0 };
        // A CPU profile's pseudo-frames, `(program)` and `(garbage collector)`, name no function to change.
        let pseudo = |r: &Row| kind == Kind::Cpu && r.at == "(native)" && r.name.starts_with('(');
        for row in d.rows.iter().filter(|r| r.delta().abs() >= floor && !d.noise(r) && !pseudo(r)).take(3) {
            let at = row.at.rsplit('/').next().unwrap_or("");
            let line = format!("{}: {} {at} {} → {} per run ({})", kind.what(), row.name, kind.show(row.v[0]), kind.show(row.v[1]), kind.signed(row.delta()));
            text += &format!("i {line}\n");
            movers.push(json!({
                "kind": kind.what(), "function": format!("{} {at}", row.name), "base_per_run": row.v[0], "head_per_run": row.v[1], "unit": kind.unit(), "title": line,
                "ci95": d.ci(row),
                "handles": [format!("strobe query {}..{} {}", base.id, head.id, kind.view()), format!("strobe query {} {}", head.id, kind.view())],
            }));
        }
    }
    let own = super::insights(head)?;
    for line in own.text.lines().skip(1) {
        text += &format!("{line}\n");
    }
    Ok(Output::new(text, json!({ "movers": movers, "head": own.json["insights"] })))
}

/// The diff flame of an experiment series (SPEC §11.2): CPU time and allocations as one call tree of
/// both sides' stacks merged by portable frame, each node with its inclusive value per run on each
/// side. Nodes under 0.2% of the larger side's total are left out, and at most `MAX_NODES` are kept.
pub fn diff_tree(base: &Rec, head: &Rec, plan_runs: Option<u64>) -> Value {
    let mut out = json!({});
    for (name, kind) in [("cpu", Kind::hotspots(base, head, false)), ("alloc", Kind::Alloc)] {
        if let (Ok(a), Ok(b)) = (side(base, kind, plan_runs, None), side(head, kind, plan_runs, None)) {
            out[name] = tree([a, b], kind);
        }
    }
    out
}

/// One implementation's call tree on its own (SPEC §11.2): CPU time (its JavaScript profile, else its
/// native samples) and allocations, each node with its inclusive value per run, in the diff flame's
/// shape with one value per node. Two implementations share no code, so their trees never merge.
pub fn side_tree(rec: &Rec, plan_runs: Option<u64>) -> Value {
    let mut out = json!({});
    let cpu = if rec.cpu().is_err() && rec.dir.join("deep.json.gz").is_file() { Kind::Native } else { Kind::Cpu };
    for (name, kind) in [("cpu", cpu), ("alloc", Kind::Alloc)] {
        if let Ok(s) = side(rec, kind, plan_runs, None) {
            out[name] = tree([s], kind);
        }
    }
    out
}

const MAX_NODES: usize = 3000;

/// The call tree of `N` sides' stacks merged by portable frame: each node `[frame, parent, v…]` with
/// its inclusive value per run on each side.
fn tree<const N: usize>(sides: [Side; N], kind: Kind) -> Value {
    struct Node<const N: usize> {
        frame: u32,
        parent: usize,
        v: [f64; N],
        kids: HashMap<u32, usize>,
    }
    // Per run only when every side knows its runs (untimed native samples never do).
    let per_run = sides.iter().all(|s| s.runs.is_some());
    let runs = sides.each_ref().map(|s| if per_run { s.runs.unwrap_or(1).max(1) as f64 } else { 1.0 });
    let mut portable = Portable::default();
    let mut nodes = vec![Node { frame: u32::MAX, parent: usize::MAX, v: [0.0; N], kids: HashMap::new() }];
    for (s, side) in sides.iter().enumerate() {
        let ids = portable.ids(side.stacks);
        for wt in &side.stacks.weights {
            if wt.stack.last().is_none_or(|&i| skipped(&side.stacks.frames[i as usize].name)) {
                continue;
            }
            let per = wt.w * kind.scale() / runs[s];
            let mut at = 0;
            nodes[0].v[s] += per;
            for &i in &wt.stack {
                let f = ids[i as usize];
                at = match nodes[at].kids.get(&f) {
                    Some(&k) => k,
                    None => {
                        let k = nodes.len();
                        nodes.push(Node { frame: f, parent: at, v: [0.0; N], kids: HashMap::new() });
                        nodes[at].kids.insert(f, k);
                        k
                    }
                };
                nodes[at].v[s] += per;
            }
        }
    }
    // A child never holds more than its parent on either side, so a threshold keeps whole subtrees.
    let size = |n: &Node<N>| n.v.iter().copied().fold(0.0, f64::max);
    let mut sizes: Vec<f64> = nodes.iter().map(size).collect();
    let mut floor = 0.002 * sizes[0];
    if sizes.len() > MAX_NODES {
        sizes.sort_by(|a, b| b.total_cmp(a));
        floor = floor.max(sizes[MAX_NODES - 1]);
    }
    // Parents come before their children: renumber the kept nodes in that order.
    let mut index = vec![usize::MAX; nodes.len()];
    let mut kept: Vec<Value> = Vec::new();
    let mut frames: Vec<u32> = Vec::new();
    let mut frame_at: HashMap<u32, usize> = HashMap::new();
    let round = |v: f64| (v * 1e4).round() / 1e4;
    for i in 0..nodes.len() {
        let n = &nodes[i];
        if size(n) < floor && i != 0 {
            continue;
        }
        if i != 0 && index[n.parent] == usize::MAX {
            continue;
        }
        index[i] = kept.len();
        let f = if i == 0 {
            -1
        } else {
            *frame_at.entry(n.frame).or_insert_with(|| {
                frames.push(n.frame);
                frames.len() - 1
            }) as i64
        };
        let mut node = vec![json!(f), json!(if i == 0 { -1 } else { index[n.parent] as i64 })];
        node.extend(n.v.iter().map(|&v| json!(round(v))));
        kept.push(Value::Array(node));
    }
    json!({
        "what": kind.what(), "unit": kind.unit(), "runs": runs.as_slice(), "per_run": per_run, "untimed": sides.iter().any(|s| s.untimed),
        "floor": floor, "nodes_total": nodes.len(),
        "frames": frames.iter().map(|&f| json!(portable.frames[f as usize])).collect::<Vec<_>>(),
        "nodes": kept,
    })
}

/// Rows of two keyed maps by the size of their change.
fn keyed(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> Vec<(String, f64, f64)> {
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut rows: Vec<(String, f64, f64)> = keys.into_iter().map(|k| (k.clone(), a.get(k).copied().unwrap_or(0.0), b.get(k).copied().unwrap_or(0.0))).collect();
    rows.sort_by(|x, y| (y.2 - y.1).abs().total_cmp(&(x.2 - x.1).abs()));
    rows
}

fn rows_text(rows: &[(String, f64, f64)], top: usize, show: impl Fn(f64) -> String) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut json_rows = Vec::new();
    for (k, x, y) in rows.iter().take(top) {
        text += &format!("  {:>10}  {:>10}  {:>11}  {k}\n", show(*x), show(*y), signed(y - x, &show));
        json_rows.push(json!({ "name": k, "base": x, "head": y, "delta": y - x }));
    }
    (text, json_rows)
}

/// A change with its sign, none when it shows as zero (`0 MB`, not `−0 MB`).
fn signed(v: f64, show: &impl Fn(f64) -> String) -> String {
    let body = show(v.abs());
    match v {
        _ if body == show(0.0) => body,
        v if v > 0.0 => format!("+{body}"),
        v if v < 0.0 => format!("−{body}"),
        _ => body,
    }
}

/// Style invalidation of two recordings per run: recalcs, what changed, selectors, sources.
fn style(a: &Rec, b: &Rec, top: usize) -> Result<Output> {
    let (x, y) = (super::invalidation(a)?, super::invalidation(b)?);
    let (rx, ry) = (x.runs.max(1) as f64, y.runs.max(1) as f64);
    let count = |v: f64| fmt::value(v, "count");
    let scalars = vec![
        ("recalcs per run".to_string(), x.recalcs as f64 / rx, y.recalcs as f64 / ry),
        ("recalc ms per run".to_string(), x.recalc_ms / rx, y.recalc_ms / ry),
        ("elements restyled per run".to_string(), x.elements / rx, y.elements / ry),
        ("invalidations per run".to_string(), x.tracked as f64 / rx, y.tracked as f64 / ry),
    ];
    let mut text = format!("style invalidation · base {} → head {} · per run ({} and {} measured runs)\n", a.id, b.id, x.runs, y.runs);
    text += &format!("  {:>10}  {:>10}  {:>11}\n", "base", "head", "Δ");
    let (t, scalars_json) = rows_text(&scalars, scalars.len(), count);
    text += &t;
    let per = |m: &BTreeMap<String, u64>, r: f64| -> BTreeMap<String, f64> { m.iter().map(|(k, n)| (k.clone(), *n as f64 / r)).collect() };
    let what = |v: &super::Invalidation, r: f64| -> BTreeMap<String, f64> { v.what.iter().map(|(k, (n, _))| (k.clone(), *n as f64 / r)).collect() };
    let sources = |v: &super::Invalidation, r: f64| -> BTreeMap<String, f64> {
        v.sources.iter().fold(BTreeMap::new(), |mut m, (f, n)| {
            *m.entry(format!("{} ({})", f.name, portable_file(f))).or_default() += *n as f64 / r;
            m
        })
    };
    let mut json = json!({ "base": { "recording": a.id, "runs": x.runs }, "head": { "recording": b.id, "runs": y.runs }, "totals": scalars_json });
    for (title, key, rows) in [
        ("what changed (per run)", "changes", keyed(&what(&x, rx), &what(&y, ry))),
        ("by selector (per run)", "selectors", keyed(&per(&x.selectors, rx), &per(&y.selectors, ry))),
        ("by source (per run)", "sources", keyed(&sources(&x, rx), &sources(&y, ry))),
    ] {
        if rows.is_empty() {
            continue;
        }
        text += &format!("{title}\n");
        let (t, j) = rows_text(&rows, top, count);
        text += &t;
        json[key] = json!(j);
    }
    text += "traced: every invalidation in the measured runs counted, one launch per side (no interval across launches)\n";
    Ok(Output::new(text, json))
}

type ByName = BTreeMap<String, f64>;

/// Threads of two recordings: CPU per run by thread name. Native recordings from each one's
/// `threads.json` (with wakes); a trace by process and thread, from the tasks that started in each
/// measured run.
fn threads(a: &Rec, b: &Rec, top: usize) -> Result<Output> {
    let native = |r: &Rec| r.dir.join("threads.json").is_file();
    let (mut text, mut json);
    if native(a) && native(b) {
        // Wakes only where the OS counts them per thread (macOS counts them per process: `—`). The
        // CPU of threads that exited during a run is a row of its own, as in `query <rec> threads`.
        const EXITED: &str = "(threads that exited during a run)";
        let read = |r: &Rec| -> Result<(ByName, ByName)> {
            let (mut cpu, mut wakes) = (BTreeMap::new(), BTreeMap::new());
            for t in crate::store::read_json(&r.dir.join("threads.json"))?.as_array().into_iter().flatten() {
                let module = t["module"].as_str().map(|m| format!(" ({})", m.rsplit(['\\', '/']).next().unwrap_or(m))).unwrap_or_default();
                let key = format!("{}{module} [{}]", t["name"].as_str().unwrap_or("(unnamed)"), t["origin"].as_str().unwrap_or("?"));
                *cpu.entry(key.clone()).or_default() += t["cpu_ms_per_run"].as_f64().unwrap_or(0.0);
                if let Some(w) = t["wakes_per_s"].as_f64() {
                    *wakes.entry(key).or_default() += w;
                }
            }
            if let Some(gone) = super::native_threads(&r.dir, usize::MAX)?.json["exited_ms_per_run"].as_f64() {
                cpu.insert(EXITED.into(), gone);
            }
            Ok((cpu, wakes))
        };
        let ((ca, wa), (cb, wb)) = (read(a)?, read(b)?);
        let rows = keyed(&ca, &cb);
        text = format!("threads by name · base {} → head {} · CPU per measured run; wakes per second\n", a.id, b.id);
        text += &format!("  {:>10}  {:>10}  {:>11}  {:>17}  thread (started in) [whose]\n", "base", "head", "Δ", "wakes/s");
        let mut rows_json = Vec::new();
        for (k, x, y) in rows.iter().take(top) {
            let (u, v) = (wa.get(k).copied(), wb.get(k).copied());
            let ms = |v: f64| fmt::value(v, "ms");
            let w = |x: Option<f64>| x.map_or("—".to_string(), |x| fmt::value(x, "count"));
            let wakes = if k == EXITED { String::new() } else { format!("{} → {}", w(u), w(v)) };
            text += &format!("  {:>10}  {:>10}  {:>11}  {:>17}  {k}\n", ms(*x), ms(*y), signed(y - x, &ms), wakes);
            rows_json.push(json!({ "thread": k, "base_cpu_ms_per_run": x, "head_cpu_ms_per_run": y, "delta_cpu_ms": y - x, "base_wakes_per_s": u, "head_wakes_per_s": v }));
        }
        json = json!({ "native": true, "rows": rows_json });
    } else if !native(a) && !native(b) {
        let read = |r: &Rec| -> Result<(ByName, ByName, usize)> {
            let rows = r.part(Part::Threads)?;
            let measured = super::measured_runs(&r.meta, &rows);
            let runs = measured.len().max(1) as f64;
            let (mut busy, mut tasks) = (BTreeMap::new(), BTreeMap::new());
            for row in rows.iter().filter(|r| r.get(1).is_some_and(Option::is_some)) {
                let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
                if !measured.contains(&cell(0)) {
                    continue;
                }
                let key = format!("{} · {}", cell(1), cell(2));
                *busy.entry(key.clone()).or_default() += cell(3).parse::<f64>().unwrap_or(0.0) / 1e6 / runs;
                *tasks.entry(key).or_default() += cell(4).parse::<f64>().unwrap_or(0.0) / runs;
            }
            Ok((busy, tasks, measured.len()))
        };
        let ((ba, ta, ra), (bb, tb, rb)) = (read(a)?, read(b)?);
        let rows = keyed(&ba, &bb);
        text = format!("threads · base {} → head {} · task-loop time per measured run ({ra} and {rb}): the tasks (RunTask) that started in a run, processes of one type summed\n", a.id, b.id);
        text += &format!("  {:>10}  {:>10}  {:>11}  {:>15}  process · thread\n", "base", "head", "Δ", "tasks/run");
        let mut rows_json = Vec::new();
        for (k, x, y) in rows.iter().take(top) {
            let (u, v) = (ta.get(k).copied().unwrap_or(0.0), tb.get(k).copied().unwrap_or(0.0));
            let ms = |v: f64| fmt::value(v, "ms");
            text += &format!("  {:>10}  {:>10}  {:>11}  {:>15}  {k}\n", ms(*x), ms(*y), signed(y - x, &ms), format!("{:.0} → {:.0}", u, v));
            rows_json.push(json!({ "thread": k, "base_busy_ms_per_run": x, "head_busy_ms_per_run": y, "delta_ms": y - x, "base_tasks_per_run": u, "head_tasks_per_run": v }));
        }
        json = json!({ "native": false, "runs": [ra, rb], "rows": rows_json });
    } else {
        return Err(Error::usage("threads compares two native recordings or two traced ones: one of these has a thread list and the other a trace"));
    }
    text += &perturbed_note(a, b);
    text += "one launch per side, no interval: a verdict needs strobe bench\n";
    json["base"] = json!(a.id);
    json["head"] = json!(b.id);
    Ok(Output::new(text, json))
}

/// A recording's frames as named values: per run counts and the pooled percentiles.
fn frame_values(r: &Rec) -> Result<Vec<(&'static str, f64, &'static str)>> {
    if r.dir.join("frames.json").exists() || r.meta["native"] == true {
        let f = super::native_frames(&r.dir, 10)?.json;
        let runs: Vec<&Value> = f["runs"].as_array().into_iter().flatten().collect();
        let col = |k: &str| -> Vec<f64> { runs.iter().filter_map(|x| x[k].as_f64()).collect() };
        let mut lat: Vec<f64> = runs.iter().flat_map(|x| x["latencies_ms"].as_array().into_iter().flatten().filter_map(Value::as_f64)).collect();
        lat.sort_by(f64::total_cmp);
        return Ok(vec![
            ("frames on screen per run (median)", crate::metrics::median(&col("frames")), "count"),
            ("frame interval p95 (median of runs)", crate::metrics::median(&col("interval_p95")), "ms"),
            ("input latency p50", crate::metrics::percentile(&lat, 0.5), "ms"),
            ("input latency p95", crate::metrics::percentile(&lat, 0.95), "ms"),
            ("inputs counted", lat.len() as f64, "count"),
        ]);
    }
    let f = super::frames(&r.dir.join("trace.pftrace"), &r.meta, 10)?.json;
    let runs = r.runs().unwrap_or(1).max(1) as f64;
    let v = |k: &str| f["values"][k].as_f64().unwrap_or(f64::NAN);
    Ok(vec![
        ("frames presented per run", f["presented"].as_f64().unwrap_or(f64::NAN) / runs, "count"),
        ("vsyncs dropped", f["dropped_pct"].as_f64().unwrap_or(f64::NAN), "%"),
        ("frame latency p50", f["latency"]["p50"].as_f64().unwrap_or(f64::NAN), "ms"),
        ("frame latency p95", f["latency"]["p95"].as_f64().unwrap_or(f64::NAN), "ms"),
        ("frame latency p99", f["latency"]["p99"].as_f64().unwrap_or(f64::NAN), "ms"),
        ("frame interval p50", v("frames.interval.p50"), "ms"),
        ("frame interval p95", v("frames.interval.p95"), "ms"),
        ("frame interval p99", v("frames.interval.p99"), "ms"),
        ("frame interval max", v("frames.interval.max"), "ms"),
    ])
}

fn frames(a: &Rec, b: &Rec) -> Result<Output> {
    let (x, y) = (frame_values(a)?, frame_values(b)?);
    let mut text = format!("frames · base {} → head {}\n", a.id, b.id);
    let mut table: Vec<Vec<String>> = vec![["base", "head", "Δ", ""].map(String::from).to_vec()];
    let mut rows = Vec::new();
    for ((name, u, unit), (_, v, _)) in x.iter().zip(&y) {
        let show = |x: f64| fmt::value(x, unit);
        let change = |x: f64| fmt::value(x, if *unit == "%" { "pp" } else { unit });
        table.push(vec![show(*u), show(*v), if u.is_finite() && v.is_finite() { signed(v - u, &change) } else { "—".into() }, name.to_string()]);
        rows.push(json!({ "name": name, "unit": unit, "base": u, "head": v, "delta": v - u }));
    }
    text += &fmt::table(&table, &[true, true, true, false], "  ", 2);
    text += &perturbed_note(a, b);
    text += "one recording per side, no interval: a verdict needs strobe bench\n";
    Ok(Output::new(text, json!({ "base": a.id, "head": b.id, "rows": rows })))
}

/// Every metric value both recordings hold, side by side (descriptive: one recording per side).
fn summary(a: &Rec, b: &Rec) -> Result<Output> {
    let values = |r: &Rec| -> BTreeMap<String, f64> { r.meta["values"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect() };
    let (x, y) = (values(a), values(b));
    if x.is_empty() || y.is_empty() {
        return Err(Error::usage("summary compares two recordings' metric values, and one of these holds none"));
    }
    let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
    keys.sort();
    keys.dedup();
    let side = |r: &Rec| format!("{} ({}, {} runs)", r.id, r.meta["probes"].as_str().unwrap_or("?"), r.runs().map_or("?".to_string(), |n| n.to_string()));
    let mut text = format!("summary · base {} → head {}\n", side(a), side(b));
    let mut table: Vec<Vec<String>> = vec![["base", "head", "Δ", "Δ%", ""].map(String::from).to_vec()];
    let mut rows = Vec::new();
    for k in keys {
        let unit = crate::model::metric(k).map(|m| m.unit).unwrap_or("");
        let (u, v) = (x.get(k).copied().unwrap_or(f64::NAN), y.get(k).copied().unwrap_or(f64::NAN));
        let show = |x: f64| fmt::value(x, if unit.is_empty() { "count" } else { unit });
        // A percentage changes by percentage points.
        let change = |x: f64| fmt::value(x, if unit == "%" { "pp" } else if unit.is_empty() { "count" } else { unit });
        let rel = if u.is_finite() && v.is_finite() && u != 0.0 { v / u - 1.0 } else { f64::NAN };
        table.push(vec![show(u), show(v), if u.is_finite() && v.is_finite() { signed(v - u, &change) } else { "—".into() }, fmt::pct(rel), k.clone()]);
        rows.push(json!({ "metric": k, "unit": unit, "base": u, "head": v, "delta": v - u, "relative": rel }));
    }
    text += &fmt::table(&table, &[true, true, true, true, false], "  ", 2);
    text += &perturbed_note(a, b);
    text += "one recording per side, no interval: a verdict needs strobe bench\n";
    Ok(Output::new(text, json!({ "base": a.id, "head": b.id, "rows": rows })))
}

/// Timing read from a recording whose profilers ran is perturbed (SPEC §1 rule 3): said beside it.
fn perturbed_note(a: &Rec, b: &Rec) -> String {
    let perturbed = |r: &Rec| matches!(r.meta["probes"].as_str(), Some("attribution" | "mixed" | "memory" | "deep"));
    if perturbed(a) || perturbed(b) {
        "perturbed: profilers ran in these recordings, so their timing is not comparable with timed runs; the experiment's timed blocks hold the verdict\n".into()
    } else {
        String::new()
    }
}
