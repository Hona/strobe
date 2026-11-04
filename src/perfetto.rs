//! A recording as one Perfetto trace (SPEC §11.4): the Chrome trace as it is, with strobe's own data
//! appended on the same clock: CPU profiles and native stacks as stack samples (the Perfetto UI's
//! flame graphs), runs and phases, marks, inputs, OS presents, threads' CPU and wakes, memory, and
//! the recording's validity and context. Protobuf by hand: only the fields written here.

use crate::model::Result;
use crate::probe::symbols::{Frame, Symbolizer};
use crate::store;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

/// strobe's own times are the host's monotonic clock (QPC on Windows, where Chrome's MONOTONIC and
/// BOOTTIME are the same QPC); Chrome's traces map it in their clock snapshots.
const MONOTONIC: u64 = 3;
/// The trace's own clock: times read back from the Chrome trace by SQL.
const BOOTTIME: u64 = 6;
const SEQ_CLEARED: u64 = 1;
const SEQ_NEEDS: u64 = 2;
/// Sequence ids for strobe's packets, far from the small ids a tracing service hands out.
const SEQ_TRACKS: u32 = 0x7374_0001;
const SEQ_JS: u32 = 0x7374_0002;
const SEQ_ALLOC: u32 = 0x7374_0003;
const SEQ_NATIVE: u32 = 0x7374_0004;
const UUID_BASE: u64 = 0x5354_524F_0000_0000;

/// A protobuf message being written.
#[derive(Default)]
struct Pb(Vec<u8>);

impl Pb {
    fn raw(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn key(&mut self, field: u32, wire: u8) {
        self.raw((u64::from(field) << 3) | u64::from(wire));
    }
    fn u(&mut self, field: u32, v: u64) -> &mut Pb {
        self.key(field, 0);
        self.raw(v);
        self
    }
    fn i(&mut self, field: u32, v: i64) -> &mut Pb {
        self.u(field, v as u64)
    }
    fn d(&mut self, field: u32, v: f64) -> &mut Pb {
        self.key(field, 1);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, field: u32, b: &[u8]) -> &mut Pb {
        self.key(field, 2);
        self.raw(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }
    fn s(&mut self, field: u32, s: &str) -> &mut Pb {
        self.bytes(field, s.as_bytes())
    }
    fn m(&mut self, field: u32, build: impl FnOnce(&mut Pb)) -> &mut Pb {
        let mut inner = Pb::default();
        build(&mut inner);
        self.bytes(field, &inner.0)
    }
}

/// What a built trace holds, for the command's summary and the check against trace_processor.
#[derive(Debug, Default, serde::Serialize)]
pub struct Built {
    pub bytes: u64,
    /// The Chrome trace went in as it is.
    pub chrome: bool,
    pub js_samples: u64,
    pub alloc_samples: u64,
    pub native_samples: u64,
    /// Distinct frames in the samples, and how many carry a source file and line.
    pub frames: u64,
    pub located_frames: u64,
    pub sampled_threads: usize,
    /// strobe's tracks by kind, with their event counts.
    pub tracks: BTreeMap<String, u64>,
    pub notes: Vec<String>,
}

/// Writes `out` from the recording in `dir` and says what went in.
pub fn build(dir: &Path, out: &Path) -> Result<Built> {
    let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
    let mut b = Builder::default();
    let chrome = dir.join("trace.pftrace");
    let mut file = Vec::new();
    if chrome.is_file() {
        // A trace is a sequence of packets: appending strobe's keeps both whole.
        let raw = std::fs::read(&chrome)?;
        if raw.starts_with(&[0x1f, 0x8b]) {
            flate2::read::GzDecoder::new(raw.as_slice()).read_to_end(&mut file)?;
        } else {
            file = raw;
        }
        b.built.chrome = true;
    }
    let page = if b.built.chrome { page_marks(&chrome) } else { PageMarks::default() };
    b.root(&meta);
    let threads = Threads::read(dir, &meta);
    if !b.built.chrome {
        b.describe_threads(&threads);
    }
    b.web_runs(&page);
    b.native_runs(dir);
    b.timeline(dir, &threads);
    b.presents(dir, &meta);
    b.memory(dir);
    b.js(dir, &page, &meta);
    b.native(dir);
    b.summary(&meta);
    // A trace without Chrome's clock snapshots gets one: strobe's clock is the trace's.
    if !b.built.chrome {
        let t = b.lo.unwrap_or(1);
        let mut p = Pb::default();
        p.m(6, |c| {
            c.m(1, |k| {
                k.u(1, BOOTTIME).u(2, t);
            });
            c.m(1, |k| {
                k.u(1, MONOTONIC).u(2, t);
            });
        });
        packet(&mut file, &p);
    }
    file.extend_from_slice(&b.out);
    let tmp = out.with_extension("pftrace.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&file)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, out)?;
    b.built.bytes = file.len() as u64;
    Ok(b.built)
}

fn packet(out: &mut Vec<u8>, p: &Pb) {
    let mut head = Pb::default();
    head.key(1, 2);
    head.raw(p.0.len() as u64);
    out.extend_from_slice(&head.0);
    out.extend_from_slice(&p.0);
}

fn read_gz<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).ok()?).read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

/// What the Chrome trace says about the page: strobe's marks (run and phase edges) and the threads
/// that ran them, in the trace's own time.
#[derive(Default)]
struct PageMarks {
    marks: Vec<(String, u64)>,
    /// The page's main thread (pid, tid): where strobe's marks ran.
    page: Option<(u32, u32)>,
    /// The browser process's main thread: an Electron app's main-process profile.
    browser: Option<(u32, u32)>,
}

fn page_marks(trace: &Path) -> PageMarks {
    let sql = "select s.name as name, s.ts as ts, t.tid as tid, p.pid as pid, '' as thread
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid) left join process p using(upid)
  where s.name glob 'strobe:*'
union all
select null, null, t.tid, p.pid, t.name from thread t join process p using(upid) where t.name = 'CrBrowserMain'";
    let mut out = PageMarks::default();
    let Ok(table) = crate::engine::query(trace, sql) else { return out };
    let num = |c: &Option<String>| c.as_deref().and_then(|s| s.parse::<f64>().ok()).map(|v| v as u64);
    for row in &table.rows {
        let ids = num(&row[3]).zip(num(&row[2])).map(|(p, t)| (p as u32, t as u32));
        match (&row[0], num(&row[1])) {
            (Some(name), Some(ts)) => {
                out.marks.push((name.clone(), ts));
                out.page = out.page.or(ids);
            }
            _ => out.browser = out.browser.or(ids),
        }
    }
    out.marks.sort_by_key(|m| m.1);
    out
}

/// Names of the recording's processes and threads, from what strobe kept of them.
#[derive(Default)]
struct Threads {
    names: BTreeMap<(u32, u32), String>,
    processes: BTreeMap<u32, String>,
    /// Thread ids to their process, for samples and timelines.
    pid_of: HashMap<u32, u32>,
}

impl Threads {
    fn read(dir: &Path, meta: &Value) -> Threads {
        let mut t = Threads::default();
        let exe = meta["target"].as_str().map(|s| s.rsplit(['\\', '/']).next().unwrap_or(s).to_string());
        if let Ok(f) = store::read_json(&dir.join("frames.json")) {
            for p in f["pids"].as_array().into_iter().flatten().filter_map(Value::as_u64) {
                if let Some(e) = &exe {
                    t.processes.insert(p as u32, e.clone());
                }
            }
        }
        if let Ok(v) = store::read_json(&dir.join("threads.json")) {
            for th in v.as_array().into_iter().flatten() {
                let (Some(pid), Some(tid)) = (th["pid"].as_u64(), th["tid"].as_u64()) else { continue };
                t.pid_of.insert(tid as u32, pid as u32);
                if let Some(n) = th["name"].as_str() {
                    t.names.insert((pid as u32, tid as u32), n.to_string());
                }
            }
        }
        if let Some(tl) = read_gz::<crate::probe::wakes::TimelineData>(&dir.join("timeline.json.gz")) {
            for th in tl.threads {
                t.pid_of.insert(th.tid, th.pid);
                if let Some(n) = th.name {
                    t.names.entry((th.pid, th.tid)).or_insert(n);
                }
            }
        }
        if let Ok(Some(d)) = crate::symbol::read_deep(dir) {
            for s in &d.samples {
                t.pid_of.entry(s.2).or_insert(s.1);
            }
            for (tid, n) in &d.threads {
                if let Some(pid) = t.pid_of.get(tid) {
                    t.names.entry((*pid, *tid)).or_insert(n.clone());
                }
            }
            if let (Some(e), Some(p)) = (&exe, d.pids.first()) {
                t.processes.entry(*p).or_insert(e.clone());
            }
        }
        t
    }
}

#[derive(Default)]
struct Builder {
    out: Vec<u8>,
    built: Built,
    next_uuid: u64,
    started: bool,
    root: u64,
    /// Child tracks of the root by name.
    tracks: HashMap<String, u64>,
    thread_tracks: HashMap<(u32, u32), u64>,
    lo: Option<u64>,
    hi: Option<u64>,
    /// The page's runs in the Chrome trace's own time, when it has them.
    trace_span: Option<(u64, u64)>,
    /// Measured runs in host time: anchors for samples that carry no time.
    runs: Vec<(u64, u64)>,
}

/// One debug annotation's value.
fn annotate(e: &mut Pb, args: &[(String, Value)]) {
    for (k, v) in args {
        e.m(4, |a| {
            a.s(10, k);
            match v {
                Value::Bool(x) => {
                    a.u(2, u64::from(*x));
                }
                Value::Number(n) if n.is_i64() => {
                    a.i(4, n.as_i64().unwrap_or(0));
                }
                Value::Number(n) if n.is_u64() => {
                    a.u(3, n.as_u64().unwrap_or(0));
                }
                Value::Number(n) => {
                    a.d(5, n.as_f64().unwrap_or(0.0));
                }
                Value::String(s) => {
                    a.s(6, s);
                }
                other => {
                    a.s(6, &other.to_string());
                }
            }
        });
    }
}

/// A JSON value as flat `a.b.c` arguments; arrays of plain values joined, the rest bounded.
fn flatten(prefix: &str, v: &Value, skip: &[&str], out: &mut Vec<(String, Value)>) {
    if out.len() >= 400 || skip.contains(&prefix) {
        return;
    }
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                flatten(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, x, skip, out);
            }
        }
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            if !a.is_empty() {
                out.push((prefix.to_string(), Value::String(a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", "))));
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate().take(16) {
                flatten(&format!("{prefix}[{i}]"), x, skip, out);
            }
        }
        Value::Null => {}
        _ => out.push((prefix.to_string(), v.clone())),
    }
}

impl Builder {
    fn uuid(&mut self) -> u64 {
        self.next_uuid += 1;
        UUID_BASE | self.next_uuid
    }

    fn seen(&mut self, ts: u64) {
        self.lo = Some(self.lo.map_or(ts, |l| l.min(ts)));
        self.hi = Some(self.hi.map_or(ts, |h| h.max(ts)));
    }

    /// A packet on the tracks' sequence.
    fn emit(&mut self, ts: u64, clock: u64, build: impl FnOnce(&mut Pb)) {
        let mut p = Pb::default();
        p.u(8, ts).u(58, clock).u(10, u64::from(SEQ_TRACKS)).u(13, if std::mem::replace(&mut self.started, true) { SEQ_NEEDS } else { SEQ_CLEARED | SEQ_NEEDS });
        build(&mut p);
        packet(&mut self.out, &p);
    }

    fn descriptor(&mut self, build: impl FnOnce(&mut Pb)) {
        let mut p = Pb::default();
        p.u(10, u64::from(SEQ_TRACKS)).u(13, if std::mem::replace(&mut self.started, true) { SEQ_NEEDS } else { SEQ_CLEARED | SEQ_NEEDS });
        p.m(60, build);
        packet(&mut self.out, &p);
    }

    /// The root track every strobe track hangs under, described by the recording's note and warnings.
    fn root(&mut self, meta: &Value) {
        self.root = self.uuid();
        let mut about: Vec<String> = Vec::new();
        if let Some(n) = meta["note"].as_str() {
            about.push(n.to_string());
        }
        for w in meta["warnings"].as_array().into_iter().flatten() {
            about.push(format!("{}: {}", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or("")));
        }
        let name = format!("strobe {}", meta["id"].as_str().unwrap_or(""));
        let root = self.root;
        self.descriptor(|d| {
            d.u(1, root).s(2, name.trim()).u(11, 3);
            if !about.is_empty() {
                d.s(14, &about.join("\n"));
            }
        });
    }

    /// A child track of the root (or of `parent`), made once per name.
    fn track(&mut self, name: &str, rank: i32, counter: Option<&str>) -> u64 {
        self.track_under(None, name, rank, counter)
    }

    fn track_under(&mut self, parent: Option<u64>, name: &str, rank: i32, counter: Option<&str>) -> u64 {
        let key = format!("{}/{name}", parent.unwrap_or(0));
        if let Some(u) = self.tracks.get(&key) {
            return *u;
        }
        let uuid = self.uuid();
        let parent = parent.unwrap_or(self.root);
        self.descriptor(|d| {
            d.u(1, uuid).u(5, parent).s(2, name).i(12, i64::from(rank));
            if let Some(unit) = counter {
                d.m(8, |c| {
                    c.s(6, unit);
                });
            }
        });
        self.tracks.insert(key, uuid);
        uuid
    }

    fn count(&mut self, kind: &str) {
        *self.built.tracks.entry(kind.to_string()).or_default() += 1;
    }

    fn slice(&mut self, track: u64, clock: u64, t0: u64, t1: u64, name: &str, args: &[(String, Value)]) {
        self.emit(t0, clock, |p| {
            p.m(11, |e| {
                e.u(9, 1).u(11, track).s(23, name);
                annotate(e, args);
            });
        });
        self.emit(t1.max(t0), clock, |p| {
            p.m(11, |e| {
                e.u(9, 2).u(11, track);
            });
        });
        if clock == MONOTONIC {
            self.seen(t0);
            self.seen(t1);
        } else {
            self.trace_span = Some(self.trace_span.map_or((t0, t1), |(a, b)| (a.min(t0), b.max(t1))));
        }
    }

    fn instant(&mut self, track: u64, clock: u64, ts: u64, name: &str, args: &[(String, Value)]) {
        self.emit(ts, clock, |p| {
            p.m(11, |e| {
                e.u(9, 3).u(11, track).s(23, name);
                annotate(e, args);
            });
        });
        if clock == MONOTONIC {
            self.seen(ts);
        }
    }

    fn counter(&mut self, track: u64, clock: u64, ts: u64, value: f64) {
        self.emit(ts, clock, |p| {
            p.m(11, |e| {
                e.u(9, 4).u(11, track).d(44, value);
            });
        });
    }

    /// Process and thread tracks with their names (a recording without a Chrome trace, which
    /// names its own).
    fn describe_threads(&mut self, t: &Threads) {
        let mut pids: HashSet<u32> = t.processes.keys().copied().collect();
        pids.extend(t.names.keys().map(|k| k.0));
        let mut pids: Vec<u32> = pids.into_iter().collect();
        pids.sort_unstable();
        for pid in pids {
            let uuid = self.uuid();
            let name = t.processes.get(&pid).cloned();
            self.descriptor(|d| {
                d.u(1, uuid).m(3, |p| {
                    p.u(1, u64::from(pid));
                    if let Some(n) = &name {
                        p.s(6, n);
                    }
                });
            });
        }
        let mut ids: Vec<(u32, u32)> = t.names.keys().copied().collect();
        ids.extend(t.pid_of.iter().map(|(tid, pid)| (*pid, *tid)));
        ids.sort_unstable();
        ids.dedup();
        for (pid, tid) in ids {
            self.thread_track(pid, tid, t.names.get(&(pid, tid)).map(String::as_str));
        }
    }

    fn thread_track(&mut self, pid: u32, tid: u32, name: Option<&str>) -> u64 {
        if let Some(u) = self.thread_tracks.get(&(pid, tid)) {
            return *u;
        }
        let uuid = self.uuid();
        self.descriptor(|d| {
            d.u(1, uuid).m(4, |t| {
                t.u(1, u64::from(pid)).u(2, u64::from(tid));
                if let Some(n) = name {
                    t.s(5, n);
                }
            });
        });
        self.thread_tracks.insert((pid, tid), uuid);
        uuid
    }

    /// A page's runs and phases from strobe's marks in the Chrome trace (its own time).
    fn web_runs(&mut self, page: &PageMarks) {
        let starts: HashMap<&str, u64> = page.marks.iter().filter_map(|(n, t)| Some((n.strip_suffix(":start")?, *t))).collect();
        let ends: HashMap<&str, u64> = page.marks.iter().filter_map(|(n, t)| Some((n.strip_suffix(":end")?, *t))).collect();
        let mut spans: Vec<(&str, u64, u64)> = starts.iter().filter_map(|(k, a)| Some((*k, *a, *ends.get(k)?))).collect();
        spans.sort_by_key(|s| (s.1, std::cmp::Reverse(s.2)));
        if !spans.is_empty() {
            let track = self.track("runs and phases", 0, None);
            for (key, a, b) in spans {
                self.slice(track, BOOTTIME, a, b, &span_name(key), &[("mark".into(), Value::String(key.to_string()))]);
                self.count("runs and phases");
            }
        }
        let others: Vec<&(String, u64)> = page.marks.iter().filter(|(n, _)| !n.ends_with(":start") && !n.ends_with(":end")).collect();
        if !others.is_empty() {
            let track = self.track("marks", 1, None);
            for (n, t) in others {
                self.instant(track, BOOTTIME, *t, &span_name(n), &[]);
                self.count("marks");
            }
        }
    }

    /// A native recording's runs, phases, marks, strobe's inputs and the accessibility tree's changes.
    fn native_runs(&mut self, dir: &Path) {
        type Args = Vec<(String, Value)>;
        let runs_info: HashMap<String, Value> =
            store::read_json(&dir.join("runs.json")).ok().and_then(|v| v.as_array().cloned()).into_iter().flatten().filter_map(|r| Some((r["run"].as_str()?.to_string(), r))).collect();
        let mut runs: Vec<(String, Value)> = store::read_json(&dir.join("frames.json")).ok().and_then(|f| f["runs"].as_object().cloned()).into_iter().flatten().collect();
        runs.sort_by_key(|(_, r)| r["t0_ns"].as_u64().unwrap_or(0));
        let mut spans: Vec<(String, u64, u64, Args)> = Vec::new();
        let mut marks: Vec<(String, u64)> = Vec::new();
        let mut inputs: Vec<(String, u64, Args)> = Vec::new();
        let mut changes: Vec<u64> = Vec::new();
        for (name, r) in &runs {
            let (Some(a), Some(b)) = (r["t0_ns"].as_u64(), r["t1_ns"].as_u64()) else { continue };
            self.runs.push((a, b));
            let mut args = Vec::new();
            if let Some(info) = runs_info.get(name) {
                flatten("", info, &["run"], &mut args);
            }
            spans.push((name.clone(), a, b, args));
            let edges: Vec<(String, u64)> = r["marks"].as_array().into_iter().flatten().filter_map(|m| Some((m[0].as_str()?.to_string(), m[1].as_u64()?))).collect();
            for (n, t) in &edges {
                match n.strip_suffix(":start") {
                    Some(k) => match edges.iter().find(|(e, _)| e.strip_suffix(":end") == Some(k)) {
                        Some((_, t1)) => spans.push((k.to_string(), *t, *t1, Vec::new())),
                        None => marks.push((n.clone(), *t)),
                    },
                    None if n.ends_with(":end") && edges.iter().any(|(e, _)| e.strip_suffix(":start") == n.strip_suffix(":end")) => {}
                    None => marks.push((n.clone(), *t)),
                }
            }
            for i in r["inputs"].as_array().into_iter().flatten() {
                let Some(t) = i["t_ns"].as_u64() else { continue };
                let mut args = Vec::new();
                flatten("", i, &["t_ns"], &mut args);
                inputs.push((i["kind"].as_str().unwrap_or("input").to_string(), t, args));
            }
            changes.extend(r["changes"].as_array().into_iter().flatten().filter_map(Value::as_u64));
        }
        // A recording without runs of its own keeps its span on the thread timeline.
        if spans.is_empty()
            && let Some(t) = read_gz::<crate::probe::wakes::TimelineData>(&dir.join("timeline.json.gz"))
        {
            for (n, a, b) in t.marks {
                self.runs.push((a, b));
                spans.push((n, a, b, Vec::new()));
            }
        }
        if !spans.is_empty() {
            spans.sort_by_key(|s| (s.1, std::cmp::Reverse(s.2)));
            let track = self.track("runs and phases", 0, None);
            for (n, a, b, args) in spans {
                self.slice(track, MONOTONIC, a, b, &span_name(&n), &args);
                self.count("runs and phases");
            }
        }
        if !marks.is_empty() {
            let track = self.track("marks", 1, None);
            for (n, t) in marks {
                self.instant(track, MONOTONIC, t, &span_name(&n), &[]);
                self.count("marks");
            }
        }
        if !inputs.is_empty() {
            let track = self.track("inputs strobe sent", 2, None);
            for (n, t, args) in inputs {
                self.instant(track, MONOTONIC, t, &n, &args);
                self.count("inputs");
            }
        }
        if !changes.is_empty() {
            let track = self.track("accessibility tree changes", 3, None);
            for t in changes {
                self.instant(track, MONOTONIC, t, "changed", &[]);
                self.count("accessibility changes");
            }
        }
    }

    /// Each thread's CPU (% of one core) and wakes per second over the recording, under its thread.
    fn timeline(&mut self, dir: &Path, threads: &Threads) {
        let Some(t) = read_gz::<crate::probe::wakes::TimelineData>(&dir.join("timeline.json.gz")) else { return };
        if t.ticks.is_empty() {
            return;
        }
        let start = |i: usize| if i == 0 { t.t0_ns } else { t.ticks[i - 1] };
        let mut cells: HashMap<u32, BTreeMap<usize, (f32, u32)>> = HashMap::new();
        for &(tick, th, ms, w) in &t.cells {
            cells.entry(th).or_default().insert(tick as usize, (ms, w));
        }
        let wakes_per_thread = !cfg!(target_os = "macos");
        for (i, th) in t.threads.iter().enumerate() {
            let Some(c) = cells.get(&(i as u32)) else { continue };
            let (Some(&first), Some(&last)) = (c.keys().next(), c.keys().next_back()) else { continue };
            // An unnamed thread goes by the module it started in (a GPU driver's worker, the OS pool).
            let name = th
                .name
                .clone()
                .or_else(|| threads.names.get(&(th.pid, th.tid)).cloned())
                .or_else(|| th.module.as_deref().map(|m| format!("{} thread", m.rsplit(['\\', '/']).next().unwrap_or(m))))
                .unwrap_or_else(|| "thread".into());
            // With a Chrome trace its own thread tracks stay its own: these go under strobe's.
            let parent = if self.built.chrome { None } else { Some(self.thread_track(th.pid, th.tid, th.name.as_deref())) };
            let group = self.built.chrome.then(|| self.track("threads (CPU and wakes)", 10, None));
            let mut make = |what: String, unit: &str| match parent {
                Some(parent) => {
                    let uuid = self.uuid();
                    self.descriptor(|d| {
                        d.u(1, uuid).u(5, parent).s(2, &what).m(8, |c| {
                            c.s(6, unit).s(7, unit);
                        });
                    });
                    uuid
                }
                None => self.track_under(group, &format!("{what} ({})", th.tid), 0, Some(unit)),
            };
            let cpu = make(format!("{name} · CPU % of a core"), "%");
            let wakes = wakes_per_thread.then(|| make(format!("{name} · wakes/s"), "/s"));
            let last = last.min(t.ticks.len() - 1);
            for k in first..=last {
                let (ms, w) = c.get(&k).copied().unwrap_or((0.0, 0));
                let span = t.ticks[k].saturating_sub(start(k)).max(1) as f64;
                self.counter(cpu, MONOTONIC, start(k), 100.0 * f64::from(ms) * 1e6 / span);
                if let Some(wk) = wakes {
                    self.counter(wk, MONOTONIC, start(k), f64::from(w) * 1e9 / span);
                }
                self.seen(start(k));
            }
            self.counter(cpu, MONOTONIC, t.ticks[last], 0.0);
            if let Some(wk) = wakes {
                self.counter(wk, MONOTONIC, t.ticks[last], 0.0);
            }
            self.count("thread CPU and wakes");
        }
    }

    /// OS present events: every present by process, the app's frames on screen and their
    /// intervals, and each display's vertical blanks.
    fn presents(&mut self, dir: &Path, meta: &Value) {
        let Some(p) = read_gz::<crate::etw::Presents>(&dir.join("presents.json.gz")) else { return };
        let app: HashSet<u32> = store::read_json(&dir.join("frames.json"))
            .ok()
            .and_then(|f| f["pids"].as_array().cloned())
            .or_else(|| meta["os_frames"]["pids"].as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_u64().map(|x| x as u32))
            .collect();
        let group = self.track("OS presents", 20, None);
        let mut shown: Vec<u64> = Vec::new();
        let mut by_pid: BTreeMap<u32, Vec<&crate::etw::Present>> = BTreeMap::new();
        for x in &p.presents {
            by_pid.entry(x.pid).or_default().push(x);
        }
        for (pid, list) in by_pid {
            let track = self.track_under(Some(group), &format!("presents · pid {pid}{}", if app.contains(&pid) { " (the app)" } else { "" }), if app.contains(&pid) { 0 } else { 1 }, None);
            for x in list {
                let mut args: Vec<(String, Value)> = vec![("mode".into(), x.mode.clone().into()), ("tid".into(), x.tid.into()), ("swapchain".into(), format!("{:#x}", x.swapchain).into()), ("sync_interval".into(), x.sync_interval.into())];
                if x.screen_ns > 0 {
                    args.push(("on screen after (ms)".into(), (x.screen_ns.saturating_sub(x.start_ns) as f64 / 1e6).into()));
                }
                if x.ready_ns > 0 {
                    args.push(("GPU done after (ms)".into(), (x.ready_ns.saturating_sub(x.start_ns) as f64 / 1e6).into()));
                }
                if p.no_begin || x.stop_ns <= x.start_ns {
                    self.instant(track, MONOTONIC, x.start_ns, &x.state, &args);
                } else {
                    self.slice(track, MONOTONIC, x.start_ns, x.stop_ns, &x.state, &args);
                }
                self.count("presents");
                if x.screen_ns > 0 && (app.is_empty() || app.contains(&x.pid)) {
                    shown.push(x.screen_ns);
                }
            }
        }
        shown.sort_unstable();
        if !shown.is_empty() {
            let on = self.track_under(Some(group), "the app's frames on screen", -2, None);
            let gap = self.track_under(Some(group), "the app's frame interval (ms)", -1, Some("ms"));
            for (i, &t) in shown.iter().enumerate() {
                self.instant(on, MONOTONIC, t, "frame", &[]);
                if i > 0 {
                    self.counter(gap, MONOTONIC, t, (t - shown[i - 1]) as f64 / 1e6);
                }
                self.count("frames on screen");
            }
        }
        let mut displays: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
        for (t, d) in &p.vsyncs {
            displays.entry(*d).or_default().push(*t);
        }
        for (d, list) in displays {
            let track = self.track_under(Some(group), &format!("vertical blanks · display path {d}"), 5, None);
            for t in list {
                self.instant(track, MONOTONIC, t, "vblank", &[]);
                self.count("vertical blanks");
            }
        }
    }

    /// The memory readings a native recording took, per process, in MB.
    fn memory(&mut self, dir: &Path) {
        let Ok(m) = store::read_json(&dir.join("memory.json")) else { return };
        for r in m["readings"].as_array().into_iter().flatten() {
            let Some(at) = r["at_ns"].as_u64() else { continue };
            for p in r["processes"].as_array().into_iter().flatten() {
                let who = format!("{} {}", p["name"].as_str().unwrap_or("process"), p["pid"]);
                let g = self.track(&format!("memory · {who}"), 30, None);
                for part in ["private", "host", "gpu_mapped", "unmapped", "shared", "files", "image", "working_set"] {
                    if let Some(bytes) = p[part].as_f64() {
                        let t = self.track_under(Some(g), &format!("{part} (MB)"), 0, Some("MB"));
                        self.counter(t, MONOTONIC, at, bytes / (1024.0 * 1024.0));
                    }
                }
                self.count("memory readings");
            }
        }
    }

    /// The recording as one slice over everything it holds: its validity, timing, probes, note,
    /// warnings, environment, values and OS averages as arguments.
    fn summary(&mut self, meta: &Value) {
        // A page's runs span the recording in the trace's own clock; else what strobe's clock saw.
        let (clock, (lo, hi)) = match (self.trace_span, self.lo.zip(self.hi)) {
            (Some(span), _) => (BOOTTIME, span),
            (None, Some(span)) => (MONOTONIC, span),
            (None, None) => return,
        };
        let mut args = Vec::new();
        flatten("", meta, &["os.cores", "census", "token", "background"], &mut args);
        let track = self.track("recording", -1, None);
        let name = format!("{} · {}{}", meta["id"].as_str().unwrap_or("recording"), if meta["valid"] == false { "invalid" } else { "valid" }, meta["timing"].as_str().map(|t| format!(" · timing {t}")).unwrap_or_default());
        self.slice(track, clock, lo, hi, &name, &args);
    }

    /// V8 CPU profiles (source-mapped frames, `(idle)` left out) and sampled allocations as stack samples.
    fn js(&mut self, dir: &Path, page: &PageMarks, meta: &Value) {
        let mut profiles: Vec<std::path::PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "cpuprofile")).collect();
        profiles.sort();
        let mut allocs: Vec<std::path::PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "heapprofile")).collect();
        allocs.sort();
        if profiles.is_empty() && allocs.is_empty() {
            return;
        }
        let sym = Symbolizer::open(dir);
        // Without a Chrome trace (a script target) the samples need a thread of their own.
        let own = (!self.built.chrome).then(|| {
            let name = meta["target"].as_str().map(|s| s.rsplit(['\\', '/']).next().unwrap_or(s).to_string()).unwrap_or_else(|| "script".into());
            let uuid = self.uuid();
            self.descriptor(|d| {
                d.u(1, uuid).m(3, |p| {
                    p.u(1, 1).s(6, &name);
                });
            });
            self.thread_track(1, 1, Some("main"));
            (1u32, 1u32)
        });
        let page_thread = page.page.or(own).unwrap_or((0, 0));
        let mut js = Stacks::new(SEQ_JS, "javascript", "time", 1);
        let mut first_ts: Option<u64> = None;
        let (mut scripts, mut mapped) = (HashSet::new(), HashSet::new());
        for path in &profiles {
            let Ok(prof) = store::read_json(path) else { continue };
            let main = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("cpu-main"));
            let (pid, tid) = if main { page.browser.unwrap_or(page_thread) } else { page_thread };
            let nodes = prof["nodes"].as_array().cloned().unwrap_or_default();
            let parent: HashMap<u64, u64> = nodes
                .iter()
                .flat_map(|n| {
                    let id = n["id"].as_u64().unwrap_or(0);
                    n["children"].as_array().into_iter().flatten().filter_map(move |c| Some((c.as_u64()?, id)))
                })
                .collect();
            let frames: HashMap<u64, Frame> = nodes.iter().filter_map(|n| Some((n["id"].as_u64()?, sym.call_frame(&n["callFrame"])))).collect();
            for f in frames.values().filter(|f| !f.url.is_empty()) {
                let key = (f.url.clone(), f.line, f.column, f.name.clone());
                if f.mapped {
                    mapped.insert(key.clone());
                }
                scripts.insert(key);
            }
            let mut stacks: HashMap<u64, Option<u64>> = HashMap::new();
            let samples = prof["samples"].as_array().cloned().unwrap_or_default();
            let deltas: Vec<i64> = prof["timeDeltas"].as_array().into_iter().flatten().map(|d| d.as_i64().unwrap_or(0)).collect();
            let mut t = prof["startTime"].as_i64().unwrap_or(0);
            let task = js.task(pid, tid);
            for (i, s) in samples.iter().enumerate() {
                t += deltas.get(i).copied().unwrap_or(0);
                let Some(id) = s.as_u64() else { continue };
                if frames.get(&id).is_some_and(|f| f.name == "(idle)") || t <= 0 {
                    continue;
                }
                let stack = *stacks.entry(id).or_insert_with(|| {
                    let chain = js_chain(id, &frames, &parent);
                    (!chain.is_empty()).then(|| js.callstack_of(&chain))
                });
                let Some(stack) = stack else { continue };
                // A sample stands for the time to the next one (the profile's own weighting).
                let us = deltas.get(i + 1).copied().filter(|d| *d > 0).unwrap_or(crate::probe::cpu::INTERVAL_US as i64);
                let ts = t as u64 * 1000;
                first_ts = Some(first_ts.map_or(ts, |f| f.min(ts)));
                js.sample(ts, task, None, stack, us as u64 * 1000);
                self.seen(ts);
                self.built.js_samples += 1;
            }
        }
        self.built.frames += js.frames.len() as u64;
        self.built.located_frames += js.located;
        self.out.extend_from_slice(&js.out);
        // Sampled allocations carry no time: they stand at the first measured run (or the profile's start).
        let anchor = self.runs.first().map(|r| r.0).or(first_ts).or(self.lo);
        if let Some(at) = anchor.filter(|_| !allocs.is_empty()) {
            let mut al = Stacks::new(SEQ_ALLOC, "javascript.allocations", "bytes allocated (sampled; untimed, all at one instant)", 4);
            let task = al.task(page_thread.0, page_thread.1);
            for path in &allocs {
                let Ok(prof) = store::read_json(path) else { continue };
                let (mut frames, mut parent) = (HashMap::new(), HashMap::new());
                let mut todo: Vec<(&Value, Option<u64>)> = prof.get("head").map(|h| (h, None)).into_iter().collect();
                while let Some((n, up)) = todo.pop() {
                    let id = n["id"].as_u64().unwrap_or(0);
                    frames.insert(id, sym.call_frame(&n["callFrame"]));
                    if let Some(u) = up {
                        parent.insert(id, u);
                    }
                    for c in n["children"].as_array().into_iter().flatten() {
                        todo.push((c, Some(id)));
                    }
                }
                let mut stacks: HashMap<u64, Option<u64>> = HashMap::new();
                for s in prof["samples"].as_array().into_iter().flatten() {
                    let (Some(id), Some(size)) = (s["nodeId"].as_u64(), s["size"].as_u64()) else { continue };
                    let stack = *stacks.entry(id).or_insert_with(|| {
                        let chain = js_chain(id, &frames, &parent);
                        (!chain.is_empty()).then(|| al.callstack_of(&chain))
                    });
                    if let Some(stack) = stack {
                        al.sample(at, task, None, stack, size);
                        self.built.alloc_samples += 1;
                    }
                }
            }
            self.built.frames += al.frames.len() as u64;
            self.built.located_frames += al.located;
            self.out.extend_from_slice(&al.out);
        }
        if self.built.js_samples > 0 {
            self.built.sampled_threads += 1;
        }
        if !scripts.is_empty() {
            self.built.notes.push(format!("{} of {} distinct script frames source-mapped to original files", mapped.len(), scripts.len()));
        }
    }

    /// Native stacks (ETW, perf, macOS sample, or strobe's own sampler) with their PDB or DWARF
    /// symbols, inline frames and file:line, per thread.
    fn native(&mut self, dir: &Path) {
        let Ok(Some(deep)) = crate::symbol::read_deep(dir) else { return };
        let resolved = match crate::symbol::resolve_all(dir, &deep) {
            Ok(r) => r,
            Err(e) => {
                self.built.notes.push(format!("native symbols: {}", e.message));
                return;
            }
        };
        let mut modules: HashMap<u32, Vec<&crate::etw::Module>> = HashMap::new();
        for m in &deep.modules {
            modules.entry(m.pid).or_default().push(m);
        }
        for v in modules.values_mut() {
            v.sort_by_key(|m| m.base);
        }
        let kernel = crate::symbol::KERNEL;
        let find = |pid: u32, a: u64| -> Option<&crate::etw::Module> {
            let list = modules.get(&if a >= kernel { 0 } else { pid })?;
            let i = list.partition_point(|m| m.base <= a).checked_sub(1)?;
            let m = list[i];
            (a < m.base + m.size.max(1)).then_some(m)
        };
        let (timebase, unit) = if deep.untimed { ("samples (untimed: the sampler gives no times)", 9) } else { ("CPU time", 1) };
        let mut st = Stacks::new(SEQ_NATIVE, "native", timebase, unit);
        let weight = if deep.untimed { 1 } else { (deep.interval_us * 1000.0).round().max(1.0) as u64 };
        let mut samples = deep.samples.clone();
        samples.sort_by_key(|s| s.0);
        let mut stacks: HashMap<(u32, u32), Option<u64>> = HashMap::new();
        let mut sampled: HashSet<(u32, u32)> = HashSet::new();
        for &(ts, pid, tid, cpu, idx) in &samples {
            let stack = *stacks.entry((idx, pid)).or_insert_with(|| {
                let addrs = deep.stacks.get(idx as usize)?;
                let mut frames: Vec<StackFrame> = Vec::new();
                // Outermost first: the stack is innermost first, and so is each address's inline list.
                for (i, &a) in addrs.iter().enumerate().rev() {
                    let Some(r) = resolved.get(&crate::symbol::addr_key(pid, a, i == 0)) else { continue };
                    let m = find(pid, a);
                    let mapping = m.map(|m| m.path.clone()).or_else(|| (a >= kernel).then(|| "kernel".to_string()));
                    let rel = m.map_or(a, |m| a - m.base);
                    for (name, file, line, _package, ok) in r.iter().rev() {
                        // Consecutive frames of one unsymbolized module are one frame to a reader.
                        if !*ok && frames.last().is_some_and(|f| !f.ok && f.name == *name) {
                            continue;
                        }
                        frames.push(StackFrame { name: name.clone(), file: file.clone(), line: *line, column: 0, mapping: mapping.clone(), rel_pc: Some(rel), kind: Some(if a >= kernel { 2 } else { 1 }), kind_str: None, ok: *ok });
                    }
                }
                (!frames.is_empty()).then(|| st.callstack(&frames))
            });
            let Some(stack) = stack else { continue };
            let leaf_kernel = deep.stacks.get(idx as usize).and_then(|s| s.first()).is_some_and(|a| *a >= kernel);
            let task = st.task(pid, tid);
            let exec = st.exec(u32::from(cpu), if leaf_kernel { 2 } else { 1 });
            st.sample(ts, task, Some(exec), stack, weight);
            sampled.insert((pid, tid));
            self.seen(ts);
            self.built.native_samples += 1;
        }
        self.built.frames += st.frames.len() as u64;
        self.built.located_frames += st.located;
        self.built.sampled_threads += sampled.len();
        if deep.untimed {
            self.built.notes.push("native samples carry no times (macOS sample): they all stand at the capture's start; the flame graph is whole, the timeline is not".into());
        }
        if deep.suspend.is_some() {
            self.built.notes.push(format!("native stacks{}", crate::etw::stacks_how(&deep)));
        }
        self.out.extend_from_slice(&st.out);
    }
}

/// A mark's key as a slice name: `strobe:<token>:run:3:phase:down` → `run 3 · phase down`.
fn span_name(key: &str) -> String {
    let rest = key.strip_prefix("strobe:").map(|r| r.split_once(':').map_or(r, |x| x.1)).unwrap_or(key);
    let parts: Vec<&str> = rest.split(':').collect();
    parts.chunks(2).map(|c| c.join(" ")).collect::<Vec<_>>().join(" · ")
}

/// The stack of profile node `id`, root first, without the `(root)` frame.
fn js_chain(id: u64, frames: &HashMap<u64, Frame>, parent: &HashMap<u64, u64>) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut at = Some(id);
    while let Some(n) = at {
        if let Some(f) = frames.get(&n).filter(|f| f.name != "(root)") {
            out.push(f.clone());
        }
        at = parent.get(&n).copied();
    }
    out.reverse();
    out
}

/// One frame as Perfetto interns it: a function in a mapping, with its source file and line.
#[derive(Clone, PartialEq, Eq, Hash)]
struct StackFrame {
    name: String,
    file: String,
    line: u32,
    /// Only tells two functions on one line apart (Perfetto keeps no column).
    column: i64,
    mapping: Option<String>,
    /// The module-relative address; JavaScript frames get one per distinct location.
    rel_pc: Option<u64>,
    kind: Option<u64>,
    kind_str: Option<&'static str>,
    ok: bool,
}

/// One stack-sample sequence: its source, timebase, interning state and packets.
struct Stacks {
    seq: u32,
    out: Vec<u8>,
    pending: Pb,
    started: bool,
    strings: [HashMap<String, u64>; 3],
    mappings: HashMap<String, u64>,
    /// Per mapping, the next made-up address for frames without one.
    next_pc: HashMap<u64, u64>,
    frames: HashMap<StackFrame, u64>,
    callstacks: HashMap<Vec<u64>, u64>,
    tasks: HashMap<(u32, u32), u64>,
    execs: HashMap<(u32, u64), u64>,
    located: u64,
}

impl Stacks {
    /// `unit`: StackSample.Unit (1 ns, 4 bytes, 9 count).
    fn new(seq: u32, source: &str, timebase: &str, unit: u64) -> Stacks {
        let mut s = Stacks {
            seq,
            out: Vec::new(),
            pending: Pb::default(),
            started: false,
            strings: Default::default(),
            mappings: HashMap::new(),
            next_pc: HashMap::new(),
            frames: HashMap::new(),
            callstacks: HashMap::new(),
            tasks: HashMap::new(),
            execs: HashMap::new(),
            located: 0,
        };
        let mut p = Pb::default();
        p.u(10, u64::from(seq)).u(13, SEQ_CLEARED);
        p.m(59, |d| {
            d.u(58, MONOTONIC).m(100, |sd| {
                sd.s(1, source).m(2, |c| {
                    c.s(2, timebase).u(3, unit);
                });
            });
        });
        packet(&mut s.out, &p);
        s.started = true;
        s
    }

    /// An interned string: 0 function names, 1 source paths, 2 mapping paths.
    fn string(&mut self, which: usize, s: &str) -> u64 {
        if let Some(i) = self.strings[which].get(s) {
            return *i;
        }
        let iid = self.strings[which].len() as u64 + 1;
        self.strings[which].insert(s.to_string(), iid);
        let field = [5, 18, 17][which];
        self.pending.m(field, |x| {
            x.u(1, iid).bytes(2, s.as_bytes());
        });
        iid
    }

    fn mapping(&mut self, path: &str) -> u64 {
        if let Some(i) = self.mappings.get(path) {
            return *i;
        }
        // Perfetto joins a mapping's path parts with `/`: a Windows path reads `/C:/Windows/…`.
        let parts: Vec<u64> = if path.contains("://") { vec![self.string(2, path)] } else { path.split(['\\', '/']).filter(|p| !p.is_empty()).map(|p| self.string(2, p)).collect() };
        let iid = self.mappings.len() as u64 + 1;
        self.mappings.insert(path.to_string(), iid);
        self.pending.m(19, |m| {
            m.u(1, iid);
            for p in &parts {
                m.u(7, *p);
            }
        });
        iid
    }

    fn frame(&mut self, f: &StackFrame) -> u64 {
        if let Some(i) = self.frames.get(f) {
            return *i;
        }
        let name = self.string(0, &f.name);
        let mapping = f.mapping.as_deref().map(|m| self.mapping(m));
        let rel_pc = mapping.map(|m| {
            f.rel_pc.unwrap_or_else(|| {
                let n = self.next_pc.entry(m).or_insert(0);
                *n += 1;
                *n
            })
        });
        let file = (!f.file.is_empty()).then(|| self.string(1, &f.file));
        let iid = self.frames.len() as u64 + 1;
        self.frames.insert(f.clone(), iid);
        if file.is_some() && f.line > 0 {
            self.located += 1;
        }
        self.pending.m(6, |x| {
            x.u(1, iid).u(2, name);
            if let (Some(m), Some(pc)) = (mapping, rel_pc) {
                x.u(3, m).u(4, pc);
            }
            if let Some(fi) = file {
                x.u(5, fi);
            }
            if f.line > 0 {
                x.u(6, u64::from(f.line));
            }
            if let Some(k) = f.kind {
                x.u(7, k);
            } else if let Some(k) = f.kind_str {
                x.s(8, k);
            }
        });
        iid
    }

    fn callstack(&mut self, frames: &[StackFrame]) -> u64 {
        let ids: Vec<u64> = frames.iter().map(|f| self.frame(f)).collect();
        if let Some(i) = self.callstacks.get(&ids) {
            return *i;
        }
        let iid = self.callstacks.len() as u64 + 1;
        self.pending.m(7, |c| {
            c.u(1, iid);
            for id in &ids {
                c.u(2, *id);
            }
        });
        self.callstacks.insert(ids, iid);
        iid
    }

    /// A V8 profile's stack (root first) as frames: source-mapped name, file and line.
    fn callstack_of(&mut self, chain: &[Frame]) -> u64 {
        let frames: Vec<StackFrame> = chain
            .iter()
            .map(|f| {
                let (kind, kind_str) = match f.name.as_str() {
                    "(program)" => (Some(6), None),
                    "(garbage collector)" => (Some(5), None),
                    _ => (None, Some("javascript")),
                };
                StackFrame {
                    name: f.name.clone(),
                    file: if f.file.is_empty() { f.url.clone() } else { f.file.clone() },
                    line: u32::try_from(f.line).unwrap_or(0),
                    column: f.column,
                    mapping: (!f.url.is_empty()).then(|| f.url.clone()),
                    rel_pc: None,
                    kind,
                    kind_str,
                    ok: true,
                }
            })
            .collect();
        self.callstack(&frames)
    }

    fn task(&mut self, pid: u32, tid: u32) -> u64 {
        if let Some(i) = self.tasks.get(&(pid, tid)) {
            return *i;
        }
        let iid = self.tasks.len() as u64 + 1;
        self.tasks.insert((pid, tid), iid);
        self.pending.m(48, |t| {
            t.u(1, iid).u(2, u64::from(pid)).u(3, u64::from(tid));
        });
        iid
    }

    /// `mode`: 1 user, 2 kernel.
    fn exec(&mut self, cpu: u32, mode: u64) -> u64 {
        if let Some(i) = self.execs.get(&(cpu, mode)) {
            return *i;
        }
        let iid = self.execs.len() as u64 + 1;
        self.execs.insert((cpu, mode), iid);
        self.pending.m(49, |e| {
            e.u(1, iid).u(2, u64::from(cpu)).u(3, mode);
        });
        iid
    }

    fn sample(&mut self, ts: u64, task: u64, exec: Option<u64>, stack: u64, weight: u64) {
        let mut p = Pb::default();
        p.u(8, ts).u(58, MONOTONIC).u(10, u64::from(self.seq)).u(13, SEQ_NEEDS);
        if !self.pending.0.is_empty() {
            let interned = std::mem::take(&mut self.pending);
            p.bytes(12, &interned.0);
        }
        p.m(135, |s| {
            s.u(2, task);
            if let Some(e) = exec {
                s.u(4, e);
            }
            s.u(6, stack).u(11, weight);
        });
        packet(&mut self.out, &p);
    }
}
