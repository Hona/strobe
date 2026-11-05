//! One recording opened for views: each source read once, however many views ask (SPEC §11.2).

use super::Forced;
use crate::engine;
use crate::model::{Error, Result};
use crate::probe::symbols::{Frame, Symbolizer};
use crate::store;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// One recording opened for views. Each source (the CPU and allocation profiles, native samples, the
/// heap snapshot, the trace's views) is read once, however many views ask for it: a report asks for
/// the same sources through a dozen views, and reading them once per view loaded one trace into
/// trace_processor up to seven times at once.
pub struct Rec {
    pub dir: PathBuf,
    pub id: String,
    pub meta: Value,
    /// Source maps decode once per script for every view (a `Symbolizer` caches per instance).
    pub(super) sym: Mutex<Symbolizer>,
    cpu: OnceLock<Result<Stacks>>,
    alloc: OnceLock<Result<Stacks>>,
    native: OnceLock<Result<Option<NativeStacks>>>,
    heap: OnceLock<Result<crate::heapsnap::Snapshot>>,
    pub(super) forced: OnceLock<Result<Forced>>,
    /// Trace views by part: those asked for together run in one trace_processor pass.
    trace: Mutex<BTreeMap<Part, Result<Arc<Rows>>>>,
}

pub(super) type Rows = Vec<Vec<Option<String>>>;

/// One distinct stack and what it weighs: µs of time or bytes allocated, the samples behind it, and
/// the sampling variance of the weight (µs² or bytes²), which depends on the sampler: a CPU sample
/// weighs the time to the next sample, so Σ Δt²; an allocation sample is a Horvitz-Thompson estimate
/// (size s seen with probability 1 − e^(−s/B)), so Σ (s/p)²(1 − p), near zero for one large
/// allocation; native samples weigh one interval each, so n·interval²; traced events have none.
pub struct Weight {
    pub stack: Vec<u32>,
    pub w: f64,
    pub n: u64,
    pub var: f64,
}

/// Weighted stacks with each distinct frame stored once (stacks are root first, indices into `frames`).
#[derive(Default)]
pub struct Stacks {
    pub frames: Vec<Frame>,
    pub weights: Vec<Weight>,
    at: BTreeMap<Frame, u32>,
}

impl Stacks {
    pub(super) fn intern(&mut self, f: Frame) -> u32 {
        if let Some(&i) = self.at.get(&f) {
            return i;
        }
        let i = self.frames.len() as u32;
        self.frames.push(f.clone());
        self.at.insert(f, i);
        i
    }

    /// Native samples: each weighs one sampling interval, so a stack's variance is n·interval².
    fn from_native(weights: Vec<(Vec<Frame>, f64, u64)>) -> Stacks {
        let mut s = Stacks::default();
        for (stack, w, n) in weights {
            let stack = stack.into_iter().map(|f| s.intern(f)).collect();
            let var = if n > 0 { w * w / n as f64 } else { 0.0 };
            s.weights.push(Weight { stack, w, n, var });
        }
        s
    }
}

/// A recording's native samples (`deep`) as stacks, with what resolved. `untimed`: the samples carry
/// no time and cover the whole capture (macOS `sample`: launch, warmup runs and resets included), so
/// they cannot be divided into measured runs.
pub(super) struct NativeStacks {
    pub(super) stacks: Stacks,
    pub(super) interval_us: f64,
    pub(super) samples: u64,
    pub(super) resolved: u64,
    pub(super) untimed: bool,
    pub(super) facts: Value,
}

impl NativeStacks {
    /// A recording's native samples, all of them, or one thread's or one phase's (uncached).
    pub(super) fn read(dir: &Path, thread: Option<&str>, spans: Option<&[(u64, u64)]>) -> Result<Option<NativeStacks>> {
        Ok(crate::symbol::native(dir, thread, spans)?.map(|n| {
            let facts = crate::symbol::summary(&n);
            NativeStacks { interval_us: n.interval_us, samples: n.samples, resolved: n.resolved, untimed: n.untimed, facts, stacks: Stacks::from_native(n.weights) }
        }))
    }
}

/// A failure cached with its source, handed again to each view that asks.
pub(super) fn cached<T>(r: &Result<T>) -> Result<&T> {
    r.as_ref().map_err(Error::clone)
}

impl Rec {
    pub fn open(dir: &Path) -> Rec {
        let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
        let id = meta["id"].as_str().map(str::to_string).unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        Rec {
            dir: dir.to_path_buf(),
            id,
            meta,
            sym: Mutex::new(Symbolizer::open(dir)),
            cpu: OnceLock::new(),
            alloc: OnceLock::new(),
            native: OnceLock::new(),
            heap: OnceLock::new(),
            forced: OnceLock::new(),
            trace: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn resolve(handle: &str) -> Result<Rec> {
        Ok(Rec::open(&store::resolve(handle)?))
    }

    /// The measured runs the recording's profiles cover: its `runs`, else its duration over one run's.
    pub fn runs(&self) -> Option<u64> {
        self.meta["runs"].as_u64().or_else(|| {
            let dur = self.meta["duration_ms"].as_f64()?;
            let one = self.meta["values"]["run.duration"].as_f64()?;
            (one > 0.0).then(|| (dur / one).round().max(1.0) as u64)
        })
    }

    pub(super) fn cpu(&self) -> Result<&Stacks> {
        cached(self.cpu.get_or_init(|| self.load_cpu()))
    }

    pub(super) fn alloc(&self) -> Result<&Stacks> {
        cached(self.alloc.get_or_init(|| self.load_alloc()))
    }

    pub(super) fn native(&self) -> Result<Option<&NativeStacks>> {
        cached(self.native.get_or_init(|| NativeStacks::read(&self.dir, None, None))).map(Option::as_ref)
    }

    pub fn snapshot(&self) -> Result<&crate::heapsnap::Snapshot> {
        cached(self.heap.get_or_init(|| crate::heapsnap::read(&super::heap_file(&self.dir)?)))
    }

    /// Loads the sources the given views read, all at once (each its own file read or trace pass).
    pub fn preload(&self, parts: &[Part], cpu: bool, alloc: bool, heap: bool) {
        std::thread::scope(|s| {
            s.spawn(|| self.prefetch(parts));
            if alloc {
                s.spawn(|| self.alloc().is_ok());
            }
            if heap {
                s.spawn(|| self.snapshot().is_ok());
            }
            if cpu {
                let _ = self.cpu();
            }
        });
    }

    /// Whether this recording's trace can answer a part: run marks, and an attribution trace for
    /// stacks, GC phases and invalidation tracking.
    pub(super) fn has(&self, p: Part) -> bool {
        let token = self.meta["token"].as_str().unwrap_or("");
        !token.is_empty() && self.dir.join("trace.pftrace").is_file() && (!p.attribution_only() || self.meta["probes"] == "attribution")
    }

    /// Runs the parts not yet read in one trace_processor pass: one load of the trace for all of them.
    pub fn prefetch(&self, parts: &[Part]) {
        let mut cache = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        let mut todo: Vec<Part> = parts.iter().copied().filter(|p| !cache.contains_key(p) && self.has(*p)).collect();
        todo.sort();
        todo.dedup();
        if todo.is_empty() {
            return;
        }
        match engine::query(&self.dir.join("trace.pftrace"), &batch_sql(self.meta["token"].as_str().unwrap_or(""), &todo)) {
            Ok(table) => {
                let mut split: BTreeMap<Part, Rows> = todo.iter().map(|p| (*p, Vec::new())).collect();
                for row in table.rows {
                    let Some(p) = row.first().cloned().flatten().and_then(|t| t.parse::<usize>().ok()).and_then(|i| PARTS.get(i).copied()) else { continue };
                    if let Some(rows) = split.get_mut(&p) {
                        rows.push(row.into_iter().skip(1).take(p.width()).collect());
                    }
                }
                for (p, mut rows) in split {
                    if let Some((a, b)) = p.order() {
                        rows.sort_by(|x, y| cell_cmp(x, y, a).then_with(|| cell_cmp(x, y, b)).then_with(|| cell_cmp(y, x, 3)));
                    }
                    cache.insert(p, Ok(Arc::new(rows)));
                }
            }
            Err(e) => {
                for p in todo {
                    cache.insert(p, Err(e.clone()));
                }
            }
        }
    }

    pub(super) fn part(&self, p: Part) -> Result<Arc<Rows>> {
        if !self.has(p) {
            return Err(Error::usage("this recording has no trace with strobe's run marks"));
        }
        self.prefetch(&[p]);
        let cache = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(&p).map(|r| r.as_ref().map(Arc::clone).map_err(Error::clone)).unwrap_or_else(|| Err(Error::msg("trace part missing")))
    }

    fn load_cpu(&self) -> Result<Stacks> {
        let profiles = files(&self.dir, "cpuprofile")?;
        if profiles.is_empty() {
            return Err(Error::usage("this recording has no CPU profile; record with --probes attribution"));
        }
        let mut out = Stacks::default();
        for p in profiles {
            let bytes = std::fs::read(&p)?;
            let prof: CpuProfile = parse(&bytes)?;
            // Parsing runs beside other loads; symbolizing shares the recording's source maps.
            let sym = self.sym.lock().unwrap_or_else(|e| e.into_inner());
            let index: HashMap<u64, usize> = prof.nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
            let mut parent = vec![usize::MAX; prof.nodes.len()];
            for (i, n) in prof.nodes.iter().enumerate() {
                for c in &n.children {
                    if let Some(&j) = index.get(c) {
                        parent[j] = i;
                    }
                }
            }
            // Per profile node: µs, samples and Σ Δt² (a sample's time is the delta to the next sample).
            let mut at: BTreeMap<usize, (f64, u64, f64)> = BTreeMap::new();
            for (i, s) in prof.samples.iter().enumerate() {
                let Some(&k) = index.get(s) else { continue };
                let dt = prof.time_deltas.get(i + 1).copied().unwrap_or(0.0).max(0.0);
                let e = at.entry(k).or_default();
                e.0 += dt;
                e.1 += 1;
                e.2 += dt * dt;
            }
            let mut frames = Symbolized::new(&sym, prof.nodes.len());
            for (k, (w, n, var)) in at {
                let stack = frames.chain(k, &parent, |i| &prof.nodes[i].call_frame, &mut out);
                out.weights.push(Weight { stack, w, n, var });
            }
        }
        Ok(out)
    }

    fn load_alloc(&self) -> Result<Stacks> {
        let profiles = files(&self.dir, "heapprofile")?;
        if profiles.is_empty() {
            return Err(Error::usage("this recording has no allocation profile; record with --probes attribution"));
        }
        let mut out = Stacks::default();
        for p in profiles {
            let bytes = std::fs::read(&p)?;
            let prof: AllocProfile = parse(&bytes)?;
            let sym = self.sym.lock().unwrap_or_else(|e| e.into_inner());
            // Per node: samples and the variance of the bytes V8 scaled them up to. V8 samples an
            // allocation of s bytes with probability p = 1 − e^(−s/B) and counts it as s/p; its
            // variance is (s/p)²(1 − p): about B² for a small object, near zero for a large one.
            let b = crate::probe::alloc::INTERVAL_BYTES as f64;
            let mut hits: HashMap<u64, (u64, f64)> = HashMap::new();
            for s in &prof.samples {
                let e = hits.entry(s.node_id).or_default();
                e.0 += 1;
                if s.size > 0.0 {
                    let p = 1.0 - (-s.size / b).exp();
                    e.1 += (s.size / p).powi(2) * (1.0 - p);
                }
            }
            // Flatten the call tree once (each node and its parent); a stack is built only for nodes
            // where bytes were sampled (`selfSize`), not copied down every branch.
            let mut nodes: Vec<&AllocNode> = Vec::new();
            let mut parent: Vec<usize> = Vec::new();
            let mut todo: Vec<(&AllocNode, usize)> = prof.head.iter().map(|h| (h, usize::MAX)).collect();
            while let Some((n, up)) = todo.pop() {
                let i = nodes.len();
                nodes.push(n);
                parent.push(up);
                todo.extend(n.children.iter().map(|c| (c, i)));
            }
            let mut frames = Symbolized::new(&sym, nodes.len());
            for i in 0..nodes.len() {
                let w = nodes[i].self_size;
                if w > 0.0 {
                    let stack = frames.chain(i, &parent, |j| &nodes[j].call_frame, &mut out);
                    // A profile without its sample list: the bytes as samples of B bytes each.
                    let (n, var) = hits.get(&nodes[i].id).copied().unwrap_or(((w / b).round().max(1.0) as u64, w * b));
                    out.weights.push(Weight { stack, w, n, var });
                }
            }
        }
        Ok(out)
    }
}

/// The files of one kind in a recording's folder, in name order.
fn files(dir: &Path, ext: &str) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == ext)).collect();
    v.sort();
    Ok(v)
}

/// A profile from JSON: profiles nest one level per stack frame, past serde_json's default limit.
fn parse<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.disable_recursion_limit();
    Ok(T::deserialize(&mut de)?)
}

#[derive(serde::Deserialize)]
struct CallFrame<'a> {
    #[serde(default, borrow, rename = "functionName")]
    name: Cow<'a, str>,
    #[serde(default, borrow)]
    url: Cow<'a, str>,
    #[serde(default = "unknown", rename = "lineNumber")]
    line: i64,
    #[serde(default = "unknown", rename = "columnNumber")]
    column: i64,
}

fn unknown() -> i64 {
    -1
}

#[derive(serde::Deserialize)]
struct CpuNode<'a> {
    #[serde(default)]
    id: u64,
    #[serde(borrow, rename = "callFrame")]
    call_frame: CallFrame<'a>,
    #[serde(default)]
    children: Vec<u64>,
}

#[derive(serde::Deserialize)]
struct CpuProfile<'a> {
    #[serde(default, borrow)]
    nodes: Vec<CpuNode<'a>>,
    #[serde(default)]
    samples: Vec<u64>,
    #[serde(default, rename = "timeDeltas")]
    time_deltas: Vec<f64>,
}

#[derive(serde::Deserialize)]
struct AllocNode<'a> {
    #[serde(default)]
    id: u64,
    #[serde(borrow, rename = "callFrame")]
    call_frame: CallFrame<'a>,
    #[serde(default, rename = "selfSize")]
    self_size: f64,
    #[serde(default, borrow)]
    children: Vec<AllocNode<'a>>,
}

#[derive(serde::Deserialize)]
struct AllocProfile<'a> {
    #[serde(borrow)]
    head: Option<AllocNode<'a>>,
    #[serde(default)]
    samples: Vec<AllocSample>,
}

#[derive(serde::Deserialize)]
struct AllocSample {
    #[serde(default, rename = "nodeId")]
    node_id: u64,
    #[serde(default)]
    size: f64,
}

/// Profile nodes to interned frames, each node and each distinct call frame resolved once.
struct Symbolized<'s, 'a> {
    sym: &'s Symbolizer,
    node: Vec<Option<u32>>,
    by_frame: HashMap<(&'a str, &'a str, i64, i64), u32>,
}

impl<'s, 'a> Symbolized<'s, 'a> {
    fn new(sym: &'s Symbolizer, nodes: usize) -> Self {
        Symbolized { sym, node: vec![None; nodes], by_frame: HashMap::new() }
    }

    /// The stack of node `i`, root first, without the `(root)` frame.
    fn chain(&mut self, i: usize, parent: &[usize], frame_of: impl Fn(usize) -> &'a CallFrame<'a>, out: &mut Stacks) -> Vec<u32> {
        let mut stack = Vec::new();
        let mut at = i;
        while at != usize::MAX {
            let id = match self.node[at] {
                Some(id) => id,
                None => {
                    let cf = frame_of(at);
                    let key = (cf.url.as_ref(), cf.name.as_ref(), cf.line, cf.column);
                    let id = match self.by_frame.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = out.intern(self.sym.frame(&cf.url, cf.line, cf.column, &cf.name));
                            self.by_frame.insert(key, id);
                            id
                        }
                    };
                    self.node[at] = Some(id);
                    id
                }
            };
            if out.frames[id as usize].name != "(root)" {
                stack.push(id);
            }
            at = parent[at];
        }
        stack.reverse();
        stack
    }
}

/// A trace view strobe reads with SQL over the run windows (`_w`). Several parts run as one statement
/// (a `union all` tagged by part), so the trace loads once for all of them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Part {
    Layout,
    Gc,
    LongTasks,
    Style,
    Phases,
    Threads,
}

const PARTS: [Part; 6] = [Part::Layout, Part::Gc, Part::LongTasks, Part::Style, Part::Phases, Part::Threads];

impl Part {
    /// Columns of the part's rows. Window rows (`_w`) carry only their key, the rest NULL.
    fn width(self) -> usize {
        match self {
            Part::Layout => 6,
            Part::Gc => 7,
            Part::LongTasks => 2,
            Part::Style => 9,
            Part::Phases => 5,
            Part::Threads => 5,
        }
    }

    fn attribution_only(self) -> bool {
        matches!(self, Part::Layout | Part::Gc | Part::Style)
    }

    /// The two columns its consumers read in order (thread then time, or run then time). Slices that
    /// start together follow longest first (column 3, the duration), so an outer slice is seen before
    /// the one nested at its start and nesting counts it once.
    fn order(self) -> Option<(usize, usize)> {
        match self {
            Part::Layout | Part::Gc => Some((4, 2)),
            Part::Phases => Some((0, 2)),
            Part::LongTasks | Part::Style | Part::Threads => None,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Part::Layout => {
                "select k, null, null, null, null, null from _w
union all
select w.k, s.name, s.ts, s.dur, t.tid,
  (select group_concat(a.key || char(31) || coalesce(a.display_value, ''), char(30)) from args a
     where a.arg_set_id = s.arg_set_id and a.key glob 'debug.beginData.stackTrace*') as stack
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name in ('Layout', 'UpdateLayoutTree') and t.name = 'CrRendererMain'"
            }
            Part::Gc => {
                "select k, null, null, null, null, null, null from _w
union all
select w.k, s.name, s.ts, s.dur, t.tid, t.name = 'CrRendererMain' as main,
  extract_arg(s.arg_set_id, 'debug.usedHeapSizeBefore') - extract_arg(s.arg_set_id, 'debug.usedHeapSizeAfter') as freed
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name in ('MinorGC', 'MajorGC') or s.name glob 'V8.GC*' or s.name glob 'BlinkGC*'"
            }
            Part::LongTasks => {
                "select count(*), max(s.dur)
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
 where s.name = 'RunTask' and s.depth = 0 and t.name = 'CrRendererMain' and s.dur > 50000000"
            }
            Part::Style => {
                "select k, null, null, null, null, null, null, null, null from _w
union all
select w.k, s.name, s.dur,
  coalesce(extract_arg(s.arg_set_id, 'debug.elementCount'), extract_arg(s.arg_set_id, 'debug.data.elementCount')),
  extract_arg(s.arg_set_id, 'debug.data.reason'),
  coalesce('class .' || extract_arg(s.arg_set_id, 'debug.data.changedClass'), 'id #' || extract_arg(s.arg_set_id, 'debug.data.changedId'),
    'attribute [' || extract_arg(s.arg_set_id, 'debug.data.changedAttribute') || ']', 'pseudo-class :' || extract_arg(s.arg_set_id, 'debug.data.changedPseudo')),
  extract_arg(s.arg_set_id, 'debug.data.nodeName'),
  (select group_concat(a.display_value, char(30)) from args a where a.arg_set_id = s.arg_set_id and a.key glob 'debug.data.selectors*.selector'),
  (select group_concat(a.key || char(31) || coalesce(a.display_value, ''), char(30)) from args a where a.arg_set_id = s.arg_set_id and a.key glob 'debug.data.stackTrace*')
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where (s.name = 'UpdateLayoutTree' and t.name = 'CrRendererMain') or s.name glob '*InvalidationTracking'"
            }
            Part::Phases => {
                "select k, null, null, null, null from _w
union all
select w.k, s.name, s.ts, s.dur, coalesce(extract_arg(s.arg_set_id, 'debug.elementCount'), extract_arg(s.arg_set_id, 'debug.data.elementCount'), 0)
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where t.name = 'CrRendererMain' and s.name in ('UpdateLayoutTree', 'Layout', 'PrePaint', 'Paint')"
            }
            // Every process's threads in the run windows: task-loop tasks that started inside a run, by
            // thread. Only the task loop's own slices: a thread's other tracks hold spans that overlap
            // them (animation frames, GC sweeping), which would count time twice.
            Part::Threads => {
                "select k, null, null, null, null from _w
union all
select w.k, coalesce(p.name, '?'), coalesce(t.name, '?'), sum(s.dur), count(*)
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid) left join process p using(upid)
  join (select distinct k, t0, t1 from _w) w on s.ts >= w.t0 and s.ts < w.t1
 where s.depth = 0 and s.dur > 0 and s.name in ('RunTask', 'ThreadControllerImpl::RunTask', 'ThreadPool_RunTask') group by 1, 2, 3"
            }
        }
    }
}

/// One statement for several parts: the run windows once, then each part's rows tagged with its
/// index and padded to the widest part.
fn batch_sql(token: &str, parts: &[Part]) -> String {
    let width = parts.iter().map(|p| p.width()).max().unwrap_or(0);
    let selects: Vec<String> = parts
        .iter()
        .map(|p| format!("select {} as part, *{} from (\n{}\n)", PARTS.iter().position(|x| x == p).unwrap_or(0), ", null".repeat(width - p.width()), p.sql()))
        .collect();
    format!("{}\n{};", windows_sql(token), selects.join("\nunion all\n"))
}

/// Orders two rows by one column: numbers as numbers, NULL first, else as text.
fn cell_cmp(x: &[Option<String>], y: &[Option<String>], i: usize) -> std::cmp::Ordering {
    let (a, b) = (x.get(i).cloned().flatten(), y.get(i).cloned().flatten());
    match (a.as_deref().map(str::parse::<i64>), b.as_deref().map(str::parse::<i64>)) {
        (Some(Ok(p)), Some(Ok(q))) => p.cmp(&q),
        _ => a.cmp(&b),
    }
}

/// `_w`: the trace's run and phase windows between `strobe:<token>:…:start|end` marks.
fn windows_sql(token: &str) -> String {
    format!(
        "create perfetto table _m as
  select s.name, s.ts, t.upid from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  where s.name glob 'strobe:{token}:*';
create perfetto table _w as
  select substr(a.name, 1, length(a.name) - 6) as k, a.ts as t0, b.ts as t1, a.upid as upid
  from _m a join _m b on b.name = substr(a.name, 1, length(a.name) - 6) || ':end'
  where a.name glob '*:start' and a.name not glob '*:phase:*';"
    )
}

