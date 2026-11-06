//! Firefox's own profiler and memory reporters in attribution and memory blocks (SPEC §8.3, §8.6,
//! §13.1). The Gecko profiler starts and stops over WebDriver BiDi (`moz:profiler`, else the chrome
//! scope's `Services.profiler`), and its profile becomes strobe's own sources: a CPU profile per page
//! thread and measured run (`query hotspots`, source-mapped as Chromium's are), the page's style,
//! layout and GC markers per run (`query layout`, `query gc`, the summary's phases), and the profile
//! itself (`gecko-profile.json.gz`, which the Firefox Profiler opens and `strobe open` draws). Every
//! time is moved onto strobe's clock through the start each process records on the host's monotonic
//! clock, so samples count only inside a run's own span. about:memory (`nsIMemoryInfoDumper`) gives
//! the census.

use crate::host;
use crate::model::{Error, Refusal, Result, ValidatorId};
use crate::target::firefox::Browser;
use crate::target::web::WebPage;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Each process's main thread, the compositor and WebRender's renderer, and the page's workers.
pub const THREADS: [&str; 4] = ["GeckoMain", "Compositor", "Renderer", "DOM Worker"];
/// JavaScript frames, native stacks (whose labels name the browser's own work: a reflow, a style
/// flush, a DOM call) and each thread's CPU.
const FEATURES: [&str; 3] = ["js", "stackwalk", "cpu"];
/// Buffer entries per process (8 bytes each, taken as they fill). A full buffer drops its oldest
/// samples, which `convert` refuses rather than counting a run short.
const ENTRIES: u64 = 64 << 20;
/// The most one sample weighs. The sampler is late at times (1 ms at the 99th percentile on a busy
/// adais, up to 30 ms), and that time is the thread's, as V8 counts it; past this the thread was not
/// sampled at all (its last sample, a process that went away), and the time is no sample's.
const MAX_GAP_NS: i64 = 50_000_000;
/// The profile as Firefox wrote it, kept in the recording.
pub const PROFILE: &str = "gecko-profile.json.gz";
/// What strobe read from it: the run windows, page threads and the views' rows.
pub const READ: &str = "gecko.json";

#[derive(Clone, Copy)]
enum Control {
    /// WebDriver BiDi's `moz:profiler` module (Firefox 159 has it).
    Bidi,
    /// `Services.profiler` in the browser's chrome scope.
    Chrome,
}

/// A running Gecko profiler. Dropped without `stop`, it stops and discards what it recorded.
pub struct Profiler<'a> {
    browser: &'a Browser,
    control: Control,
    running: bool,
}

/// The first Firefox whose Windows sampler waits on a high-resolution timer (bug 2082760).
const WINDOWS_TIMER_FROM: u32 = 161;

/// The sampling interval (µs) for a Firefox (`version`: `Firefox 159.0.1`). macOS and Linux: strobe's
/// 200 µs (their sampler sleeps in `nanosleep` or `usleep`). Windows, through Firefox 160: 1 ms, as
/// `SamplerThread::SleepMicro` (`platform-win32.cpp`) busy-waits through any interval under 1 ms on
/// purpose (bug 1349801; bug 1574815), one core per profiled process (10.2 cores for a page's Firefox
/// 159 on adais at 0.5 ms or 0.2 ms, 0.4 at 1 ms), and its 1 ms is a `Sleep(1)`, every 2 ms in
/// practice. From 161 it waits on a high-resolution waitable timer: 500 µs, Windows' finest timer
/// (asked for 0.2 ms, Nightly 162 sampled every 0.54 ms at 0.5-0.8 cores). A version strobe cannot
/// read is taken as the older.
pub fn interval_us(version: &str) -> u64 {
    if !cfg!(windows) {
        return super::cpu::INTERVAL_US;
    }
    let major = version.trim_start_matches("Firefox").trim().split('.').next().and_then(|m| m.parse::<u32>().ok()).unwrap_or(0);
    if major >= WINDOWS_TIMER_FROM { 500 } else { 1000 }
}

/// Starts the Gecko profiler at `interval_us` in every process of `browser`.
pub fn start(browser: &Browser) -> Result<Profiler<'_>> {
    let interval = interval_us(&browser.version) as f64 / 1000.0;
    let bidi = browser.bidi.send("moz:profiler.start", json!({ "entries": ENTRIES, "interval": interval, "features": FEATURES, "threads": THREADS, "activeContext": browser.context() }));
    let control = match bidi {
        Ok(_) => Control::Bidi,
        // A Firefox before the module: the same call in the chrome scope (which the profile allows).
        Err(first) => {
            let call = format!("Services.profiler.StartProfiler({ENTRIES}, {interval}, {}, {}, 0, 0).then(() => true)", json!(FEATURES), json!(THREADS));
            browser.chrome_eval(&call).map_err(|e| Error::msg(format!("Firefox's profiler did not start: {}; in the chrome scope: {}", first.message, e.message)))?;
            Control::Chrome
        }
    };
    Ok(Profiler { browser, control, running: true })
}

impl Profiler<'_> {
    /// Stops the profiler and returns the profile Firefox wrote into its own profile folder.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.running = false;
        let wait = Duration::from_secs(180);
        match self.control {
            Control::Bidi => {
                let r = self.browser.bidi.send_timeout("moz:profiler.stop", json!({}), wait)?;
                r["path"].as_str().map(PathBuf::from).ok_or_else(|| Error::msg("Firefox's profiler stopped without saving a profile"))
            }
            Control::Chrome => {
                let path = self.browser.profile.clone().ok_or_else(|| Error::msg("Firefox has no profile folder"))?.join("gecko-profile.json");
                let call = format!(
                    "(async () => {{ Services.profiler.Pause(); try {{ await Services.profiler.dumpProfileToFileAsync({}) }} finally {{ await Services.profiler.StopProfiler() }} return true }})()",
                    json!(path)
                );
                self.browser.chrome_eval_timeout(&call, wait)?;
                Ok(path)
            }
        }
    }
}

impl Drop for Profiler<'_> {
    fn drop(&mut self) {
        if !self.running {
            return;
        }
        let _ = match self.control {
            Control::Bidi => self.browser.bidi.send_timeout("moz:profiler.stop", json!({ "discard": true }), Duration::from_secs(10)).map(|_| ()),
            Control::Chrome => self.browser.chrome_eval("Services.profiler.StopProfiler().then(() => true)").map(|_| ()),
        };
    }
}

/// What `convert` made of a profile.
pub struct Converted {
    /// Page scripts its samples and marker stacks name, for their source maps.
    pub scripts: BTreeSet<String>,
    /// Samples of the page's threads inside the measured runs.
    pub samples: u64,
    /// The interval asked and the one the page's main threads were sampled at.
    pub sampling: Option<super::cpu::Sampling>,
}

/// One column-named table of the raw profile (`{ schema, data }`).
struct Table<'a> {
    schema: HashMap<&'a str, usize>,
    data: &'a [Value],
}

impl<'a> Table<'a> {
    fn of(v: &'a Value) -> Table<'a> {
        let schema = v["schema"].as_object().map(|o| o.iter().filter_map(|(k, i)| Some((k.as_str(), i.as_u64()? as usize))).collect()).unwrap_or_default();
        Table { schema, data: v["data"].as_array().map(Vec::as_slice).unwrap_or(&[]) }
    }

    fn get(&self, row: usize, col: &str) -> &'a Value {
        static NULL: Value = Value::Null;
        match (self.data.get(row), self.schema.get(col)) {
            (Some(r), Some(&i)) => r.get(i).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
}

/// A frame of the profile, classified once.
#[derive(Clone)]
enum Kind {
    Root,
    /// A native return address: kept by `strobe open`, folded into the frame above it elsewhere.
    Native(u64),
    /// Page JavaScript: the function's name, script and start (1-based), as V8 profiles place it.
    Js {
        name: String,
        url: String,
        line: i64,
        column: i64,
    },
    /// A label the browser put on its own work (`Reflow`, `get HTMLElement.offsetWidth`, a GC), or the
    /// browser's own JavaScript (chrome, resource and self-hosted scripts), named with its file.
    Label(String),
    /// The profiler's own work (capturing a marker's stack): `(profiler)`.
    Profiler,
}

const PROFILER_LABELS: [&str; 4] = ["profiler_capture_backtrace", "profiler_add_marker", "AddMarkerToBuffer", "TextMarker"];

/// `name (url:line:col)[source]`, `url:line:col[source]`, `url[source]`, or a label.
fn classify(location: &str) -> Kind {
    if location == "(root)" {
        return Kind::Root;
    }
    if let Some(hex) = location.strip_prefix("0x") {
        return Kind::Native(u64::from_str_radix(hex, 16).unwrap_or(0));
    }
    if PROFILER_LABELS.contains(&location) {
        return Kind::Profiler;
    }
    // A source index suffix (`[8]`) marks a JavaScript frame; older profiles have none.
    let (rest, sourced) = match location.strip_suffix(']').and_then(|r| r.rsplit_once('[')).filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit())) {
        Some((r, _)) => (r, true),
        None => (location, false),
    };
    let (name, inner) = match rest.strip_suffix(')').and_then(|r| r.rsplit_once(" (")) {
        Some((n, i)) => (n, i),
        None => ("", rest),
    };
    let mut parts = inner.rsplitn(3, ':');
    let (col, line, url) = (parts.next(), parts.next(), parts.next());
    let (url, line, column) = match (url, line.and_then(|l| l.parse::<i64>().ok()), col.and_then(|c| c.parse::<i64>().ok())) {
        (Some(u), Some(l), Some(c)) => (u, l, c),
        _ => (inner, -1, -1),
    };
    let scheme = url.split_once(':').map(|s| s.0).unwrap_or("");
    // A label may hold a URL (`Reflow http://host/`): a script's URL has no spaces, and without the
    // source index a frame needs its line and column too.
    let address = !url.contains(char::is_whitespace) && (url.contains("://") || url == "self-hosted");
    let js = address && (sourced || line >= 0);
    if !js {
        return Kind::Label(without_origins(location));
    }
    let page = matches!(scheme, "http" | "https" | "file" | "blob" | "data") || (url.contains("://") && !matches!(scheme, "chrome" | "resource" | "moz-src" | "moz-extension" | "about" | "jar"));
    if !page {
        let short = url.rsplit('/').next().unwrap_or(url);
        let name = if name.is_empty() { "(anonymous)" } else { name };
        return Kind::Label(if line > 0 { format!("{name} ({short}:{line})") } else { format!("{name} ({short})") });
    }
    // A script without a position is code strobe or the page evaluated (`script.evaluate`).
    if line < 0 {
        return Kind::Label("(evaluated script)".into());
    }
    Kind::Js { name: name.to_string(), url: url.to_string(), line, column }
}

/// A label with every URL in it cut to its path (`Reflow http://127.0.0.1:51234/app?x` → `Reflow
/// /app`): the origin, port and query change from one run of a page to the next (a served port), and
/// a row of `a..b` or an insight must name the same work on both sides.
fn without_origins(label: &str) -> String {
    let mut out = String::new();
    let mut rest = label;
    while let Some(i) = rest.find("://") {
        let scheme = rest[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))).map_or(0, |j| j + 1);
        out += &rest[..scheme];
        let after = &rest[i + 3..];
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let path = after[..end].find('/').map_or("/", |p| &after[p..end]);
        out += path.split(['?', '#']).next().unwrap_or("/");
        rest = &after[end..];
    }
    out + rest
}

/// One thread of the profile, its times on strobe's clock.
struct Thread<'a> {
    name: &'a str,
    pid: u32,
    tid: u32,
    /// Host-clock ns of the profile's time 0 for this thread's process.
    start: i64,
    strings: &'a [Value],
    stacks: Table<'a>,
    frames: Table<'a>,
    samples: Table<'a>,
    markers: Table<'a>,
    kinds: Vec<Kind>,
    /// Category per frame (None where the frame has none).
    categories: Vec<Option<u64>>,
}

fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

impl<'a> Thread<'a> {
    fn new(v: &'a Value, start: i64) -> Thread<'a> {
        let strings = v["stringTable"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let frames = Table::of(&v["frameTable"]);
        let text = |i: &Value| i.as_u64().and_then(|i| strings.get(i as usize)).and_then(Value::as_str).unwrap_or("");
        let kinds = (0..frames.data.len()).map(|i| classify(text(frames.get(i, "location")))).collect();
        let categories = (0..frames.data.len()).map(|i| frames.get(i, "category").as_u64()).collect();
        Thread {
            name: v["name"].as_str().unwrap_or(""),
            pid: number(&v["pid"]).unwrap_or(0) as u32,
            tid: number(&v["tid"]).unwrap_or(0) as u32,
            start,
            strings,
            stacks: Table::of(&v["stackTable"]),
            frames,
            samples: Table::of(&v["samples"]),
            markers: Table::of(&v["markers"]),
            kinds,
            categories,
        }
    }

    fn string(&self, i: &'a Value) -> &'a str {
        match i {
            Value::String(s) => s,
            _ => i.as_u64().and_then(|i| self.strings.get(i as usize)).and_then(Value::as_str).unwrap_or(""),
        }
    }

    /// A profile time (ms since the process's start, small enough for an f64 to hold it to the ns) on
    /// strobe's clock (ns).
    fn host(&self, ms: f64) -> i64 {
        self.start + (ms * 1e6).round() as i64
    }

    /// The host-clock time of each sample (None where it has none).
    fn sample_times(&self) -> Vec<Option<i64>> {
        (0..self.samples.data.len()).map(|i| self.samples.get(i, "time").as_f64().map(|ms| self.host(ms))).collect()
    }

    /// The frames of a stack, innermost first.
    fn chain(&self, stack: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = Some(stack);
        while let Some(s) = at {
            let Some(f) = self.stacks.get(s, "frame").as_u64() else { break };
            out.push(f as usize);
            at = self.stacks.get(s, "prefix").as_u64().map(|p| p as usize);
            if out.len() > 4096 {
                break;
            }
        }
        out
    }

    /// The page's JavaScript frames of a stack, innermost first: (url, line, column, name, the
    /// function's line and column), 1-based, where the frame says which line runs.
    fn js_frames(&self, stack: usize) -> Vec<(String, i64, i64, String, i64, i64)> {
        self.chain(stack)
            .into_iter()
            .filter_map(|f| match &self.kinds[f] {
                Kind::Js { name, url, line, column } => {
                    let at = |c: &str, d: i64| self.frames.get(f, c).as_i64().filter(|v| *v > 0).unwrap_or(d);
                    Some((url.clone(), at("line", *line), at("column", *column), name.clone(), *line, *column))
                }
                _ => None,
            })
            .collect()
    }
}

/// Host-clock ns of a process's profile time 0, from the start it records on the host's monotonic
/// clock (QueryPerformanceCounter, CLOCK_MONOTONIC, mach_absolute_time).
fn host_start(meta: &Value) -> Option<i64> {
    // Integers in the profile (an f64 would round a QPC count or ns since boot).
    let int = |k: &str| meta[k].as_i64().or_else(|| meta[k].as_u64().and_then(|v| i64::try_from(v).ok()));
    #[cfg(windows)]
    {
        int("startTimeAsQueryPerformanceCounterValue").map(|t| host::win::qpc_ticks_ns(t) as i64)
    }
    #[cfg(target_os = "linux")]
    {
        int("startTimeAsClockMonotonicNanosecondsSinceBoot")
    }
    #[cfg(target_os = "macos")]
    {
        int("startTimeAsMachAbsoluteTimeNanoseconds")
    }
}

/// Every thread of the profile and its subprocesses with its process's start on strobe's clock.
fn threads(profile: &Value) -> Result<Vec<Thread<'_>>> {
    let mut out = Vec::new();
    let mut todo = vec![profile];
    while let Some(p) = todo.pop() {
        let start = host_start(&p["meta"]).ok_or_else(|| {
            Refusal::new(ValidatorId::ClockUnsynced, "Firefox's profile gives no process start on the host's monotonic clock, so its samples cannot be placed in strobe's runs")
                .fix("use a current Firefox release (159 records it on Windows, Linux and macOS)")
        })?;
        out.extend(p["threads"].as_array().into_iter().flatten().map(|t| Thread::new(t, start)));
        todo.extend(p["processes"].as_array().into_iter().flatten());
    }
    Ok(out)
}

fn read_profile(raw: &[u8]) -> Result<Value> {
    let mut de = serde_json::Deserializer::from_slice(raw);
    de.disable_recursion_limit();
    let v: Value = serde::Deserialize::deserialize(&mut de).map_err(|e| Error::msg(format!("Firefox's profile: {e}")))?;
    Ok(v)
}

/// The interval the profiler was asked for (µs), from the profile's meta: Firefox writes the request
/// there, not what its sampler kept.
fn asked_us(profile: &Value) -> f64 {
    profile["meta"]["interval"].as_f64().map_or(super::cpu::INTERVAL_US as f64, |ms| ms * 1000.0)
}

/// A marker that spans time on a thread (intervals, and start and end pairs), on strobe's clock.
struct Span<'a> {
    name: &'a str,
    t0: i64,
    t1: i64,
    data: &'a Value,
}

fn spans<'a>(t: &Thread<'a>) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut open: HashMap<&str, Vec<(i64, &Value)>> = HashMap::new();
    for i in 0..t.markers.data.len() {
        let name = t.string(t.markers.get(i, "name"));
        let (a, b) = (t.markers.get(i, "startTime").as_f64(), t.markers.get(i, "endTime").as_f64());
        let data = t.markers.get(i, "data");
        match t.markers.get(i, "phase").as_u64() {
            Some(1) => {
                if let (Some(a), Some(b)) = (a, b) {
                    out.push(Span { name, t0: t.host(a), t1: t.host(b), data });
                }
            }
            Some(0) => {
                if let Some(a) = a {
                    out.push(Span { name, t0: t.host(a), t1: t.host(a), data });
                }
            }
            Some(2) => {
                if let Some(a) = a {
                    open.entry(name).or_default().push((t.host(a), data));
                }
            }
            // An end marker's time is its end, or its start where a profile writes it there.
            Some(3) => {
                if let (Some(b), Some((a, data))) = (b.or(a), open.get_mut(name).and_then(Vec::pop)) {
                    out.push(Span { name, t0: a, t1: t.host(b), data });
                }
            }
            _ => {}
        }
    }
    // Spans that start together: the longer (outer) one first.
    out.sort_by_key(|s| (s.t0, std::cmp::Reverse(s.t1)));
    out
}

/// The stack a marker captured (its cause), as an index into its thread's stack table.
fn marker_stack(data: &Value) -> Option<usize> {
    let s = &data["stack"];
    let table = Table::of(&s["samples"]);
    table.get(0, "stack").as_u64().map(|v| v as usize)
}

/// A JavaScript stack as trace stacks encode it (`stackTrace[i].field` and its value, `\x1f` between
/// them, `\x1e` between pairs, innermost first), with the function's own start beside each call.
fn encode(frames: &[(String, i64, i64, String, i64, i64)]) -> Option<String> {
    if frames.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, (url, line, col, name, fl, fc)) in frames.iter().enumerate() {
        for (k, v) in [
            ("url", url.clone()),
            ("functionName", name.clone()),
            ("lineNumber", line.to_string()),
            ("columnNumber", col.to_string()),
            ("functionLine", fl.to_string()),
            ("functionColumn", fc.to_string()),
        ] {
            out += &format!("stackTrace[{i}].{k}\u{1f}{v}\u{1e}");
        }
    }
    Some(out)
}

type Row = Vec<Option<String>>;

fn cell(v: impl ToString) -> Option<String> {
    Some(v.to_string())
}

/// One V8-format CPU profile being built from one thread's samples in one run.
#[derive(Default)]
struct CpuProfile {
    nodes: Vec<Value>,
    children: Vec<Vec<u64>>,
    index: HashMap<(usize, String), usize>,
}

impl CpuProfile {
    fn new() -> CpuProfile {
        let mut p = CpuProfile::default();
        p.node(None, "(root)", "", -1, -1);
        p
    }

    /// The node for a frame under `parent`, made once.
    fn node(&mut self, parent: Option<usize>, name: &str, url: &str, line: i64, column: i64) -> usize {
        let key = (parent.unwrap_or(usize::MAX), format!("{name}\u{1f}{url}\u{1f}{line}\u{1f}{column}"));
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        let i = self.nodes.len();
        self.nodes.push(json!({ "id": i + 1, "callFrame": { "functionName": name, "url": url, "lineNumber": line, "columnNumber": column, "scriptId": "0" } }));
        self.children.push(Vec::new());
        if let Some(p) = parent {
            self.children[p].push(i as u64 + 1);
        }
        self.index.insert(key, i);
        i
    }

    fn finish(mut self, start_us: i64, end_us: i64, samples: Vec<u64>, deltas: Vec<i64>) -> Value {
        for (n, c) in self.nodes.iter_mut().zip(self.children) {
            n["children"] = json!(c);
        }
        json!({ "nodes": self.nodes, "startTime": start_us, "endTime": end_us, "samples": samples, "timeDeltas": deltas })
    }
}

/// The profile node of one sample's stack: the page's JavaScript and the browser's labels, root
/// first, native frames folded into the frame above them; `(idle)` where the thread waited, `(program)`
/// where nothing but native code ran, `(profiler)` where the profiler captured a stack.
fn sample_node(t: &Thread, stack: Option<usize>, idle: Option<u64>, p: &mut CpuProfile) -> usize {
    let Some(stack) = stack else { return p.node(Some(0), "(program)", "", -1, -1) };
    let chain = t.chain(stack);
    if idle.is_some() && chain.iter().find_map(|&f| t.categories[f]) == idle {
        return p.node(Some(0), "(idle)", "", -1, -1);
    }
    let mut at = None;
    for &f in chain.iter().rev() {
        let up = Some(at.unwrap_or(0));
        match &t.kinds[f] {
            Kind::Root | Kind::Native(_) => {}
            // 1-based in the profile; a column 0 (some frames have none) still names the line.
            Kind::Js { name, url, line, column } => at = Some(p.node(up, name, url, (line - 1).max(0), (column - 1).max(0))),
            Kind::Label(l) => at = Some(p.node(up, l, "", -1, -1)),
            // What the profiler did under a frame is its own cost, whatever it called.
            Kind::Profiler => return p.node(up, "(profiler)", "", -1, -1),
        }
    }
    at.unwrap_or_else(|| p.node(Some(0), "(program)", "", -1, -1))
}

/// Turns the profile Firefox wrote (`raw`) into the recording's sources in `dir`: `cpu-g<thread>-<run>.cpuprofile`
/// per page thread and measured run (`runs`: run key and host-clock span), `gecko.json` with the
/// views' rows, and the profile itself, gzipped. The page's processes are those holding strobe's run
/// marks (`token`). Samples in the runs' untimed stretches (`untimed`, host clock) are none of them.
/// Refuses a profile that lost a run's samples.
pub fn convert(raw: &Path, token: &str, runs: &[(String, u64, u64)], untimed: &[(u64, u64)], dir: &Path) -> Result<Converted> {
    let bytes = std::fs::read(raw).map_err(|e| Error::msg(format!("Firefox's profile {}: {e}", raw.display())))?;
    let profile = read_profile(&bytes)?;
    let all = threads(&profile)?;
    let idle = profile["meta"]["categories"].as_array().and_then(|c| c.iter().position(|c| c["name"] == "Idle")).map(|i| i as u64);
    let prefix = format!("strobe:{token}:");
    // strobe's run marks (`performance.mark` around each run, outside its span), by name, on strobe's
    // clock, and the processes that hold them: the page's (a run that loads another site moves it).
    let mut marks: HashMap<&str, i64> = HashMap::new();
    let mut page: BTreeSet<u32> = BTreeSet::new();
    for t in &all {
        for i in 0..t.markers.data.len() {
            let data = t.markers.get(i, "data");
            let (Some(name), Some(ms)) = (data["name"].as_str().filter(|n| n.starts_with(&prefix)), t.markers.get(i, "startTime").as_f64()) else { continue };
            if t.string(t.markers.get(i, "name")) == "UserTiming" {
                marks.insert(name, t.host(ms));
                page.insert(t.pid);
            }
        }
    }
    let windows: Vec<(String, i64, i64)> = runs.iter().map(|(k, a, b)| (format!("{prefix}{k}"), *a as i64, *b as i64)).collect();
    // Every measured run needs both its marks: a full buffer drops its oldest data first, the start
    // marks with it. And the marks must bracket the run on strobe's clock (they were written just
    // before and after it), which checks the profile's clock against strobe's.
    const SLACK_NS: i64 = 1_000_000;
    for (k, t0, t1) in &windows {
        let (Some(&a), Some(&b)) = (marks.get(format!("{k}:start").as_str()), marks.get(format!("{k}:end").as_str())) else {
            let summary = format!("Firefox's profile lacks strobe's marks of {}: its buffer dropped them, or the page ran in a process strobe did not see", k.trim_start_matches(&prefix));
            return Err(Refusal::new(ValidatorId::RunIncomplete, summary).fix("profile fewer or shorter runs (--runs), so the profiler's buffer holds them all").into());
        };
        if a > t0 + SLACK_NS || b < t1 - SLACK_NS {
            let summary = format!(
                "Firefox's profile puts strobe's marks of {} at {:+.2} and {:+.2} ms from the run's start and end, outside it: its clock does not match strobe's",
                k.trim_start_matches(&prefix),
                (a - t0) as f64 / 1e6,
                (b - t1) as f64 / 1e6
            );
            return Err(Refusal::new(ValidatorId::ClockUnsynced, summary).into());
        }
    }
    let window_of = |ts: i64| windows.iter().position(|w| ts >= w.1 && ts < w.2).filter(|_| !untimed.iter().any(|&(a, b)| ts >= a as i64 && ts < b as i64));
    // Each process's name, from its threads.
    let mut process_names: HashMap<u64, String> = HashMap::new();
    let mut todo = vec![&profile];
    while let Some(p) = todo.pop() {
        for th in p["threads"].as_array().into_iter().flatten() {
            if let Some(pid) = number(&th["pid"]) {
                process_names.entry(pid).or_insert_with(|| th["processName"].as_str().or(th["processType"].as_str()).unwrap_or("").to_string());
            }
        }
        todo.extend(p["processes"].as_array().into_iter().flatten());
    }
    // Every sampled thread's CPU in the runs: each sample carries the thread's CPU since the one before.
    let cpu_scale = match profile["meta"]["sampleUnits"]["threadCPUDelta"].as_str() {
        Some("ns") => Some(1e-6),
        Some("µs") => Some(1e-3),
        // Other units (`variable CPU cycles`) are no time.
        _ => None,
    };
    let mut thread_cpu: Vec<Value> = Vec::new();
    for t in all.iter().filter(|_| cpu_scale.is_some()) {
        let scale = cpu_scale.unwrap_or(0.0);
        let mut ms = 0.0;
        for i in 0..t.samples.data.len() {
            let (Some(at), Some(d)) = (t.samples.get(i, "time").as_f64(), t.samples.get(i, "threadCPUDelta").as_f64()) else { continue };
            if window_of(t.host(at)).is_some() {
                ms += d * scale;
            }
        }
        if ms > 0.0 {
            let process = process_names.get(&u64::from(t.pid)).cloned().unwrap_or_default();
            thread_cpu.push(json!({ "pid": t.pid, "tid": t.tid, "thread": t.name, "process": process, "page": page.contains(&t.pid), "cpu_ms": ms }));
        }
    }
    thread_cpu.sort_by(|a, b| b["cpu_ms"].as_f64().unwrap_or(0.0).total_cmp(&a["cpu_ms"].as_f64().unwrap_or(0.0)));
    // What the sampler kept: the median gap between consecutive samples of the page's main threads in
    // the runs (in the whole profile where the runs hold none), not the interval asked.
    let gaps = |in_runs: bool| -> Vec<f64> {
        let mut out = Vec::new();
        for t in all.iter().filter(|t| page.contains(&t.pid) && t.name == "GeckoMain") {
            let times: Vec<i64> = t.sample_times().into_iter().flatten().collect();
            for p in times.windows(2).filter(|p| p[1] - p[0] <= MAX_GAP_NS && (!in_runs || windows.iter().any(|w| p[0] >= w.1 && p[1] <= w.2))) {
                out.push((p[1] - p[0]) as f64 / 1e3);
            }
        }
        out
    };
    let asked = asked_us(&profile);
    let sampling = super::cpu::Sampling::of(asked, gaps(true)).or_else(|| super::cpu::Sampling::of(asked, gaps(false)));
    let interval_ns = (sampling.map_or(asked, |s| s.median_us) * 1e3).round() as i64;
    let mut scripts = BTreeSet::new();
    let mut total = 0u64;
    let mut covered = vec![false; windows.len()];
    let mut thread_rows = Vec::new();
    for (g, t) in all.iter().enumerate().filter(|(_, t)| page.contains(&t.pid) && matches!(t.name, "GeckoMain" | "DOM Worker")) {
        let times = t.sample_times();
        let mut in_runs = 0u64;
        for (w, (_, t0, t1)) in windows.iter().enumerate() {
            // Each sample weighs the time to the next one, cut to the run's span (the last sample before
            // the run counts for the run's first moments) and to MAX_GAP: a thread's last sample, or one
            // before the sampler lost the thread, never stands for the rest of the run.
            let first = times.partition_point(|x| x.is_none_or(|x| x <= *t0)).saturating_sub(1);
            let mut p = CpuProfile::new();
            let (mut ids, mut weights, mut begin) = (Vec::new(), Vec::new(), None);
            for i in first..times.len() {
                let Some(a) = times[i] else { continue };
                if a >= *t1 {
                    break;
                }
                let next = times[i + 1..].iter().flatten().next().copied().unwrap_or(a + interval_ns);
                let w = next.min(a + MAX_GAP_NS).min(*t1) - a.max(*t0);
                if w <= 0 {
                    continue;
                }
                let stack = t.samples.get(i, "stack").as_u64().map(|s| s as usize);
                ids.push(sample_node(t, stack, idle, &mut p) as u64 + 1);
                weights.push(w);
                begin = begin.or(Some(a.max(*t0)));
                if a >= *t0 {
                    in_runs += 1;
                }
            }
            if ids.is_empty() {
                continue;
            }
            if t.name == "GeckoMain" {
                covered[w] = true;
            }
            // Whole µs on a rounded timeline, so rounding never accumulates: each sample's weight is the
            // next delta, the last one's a trailing delta. Where MAX_GAP cut a weight, the timeline closes
            // the gap: the profile's times then run behind the run's own clock.
            let us = |ns: i64| (ns as f64 / 1e3).round() as i64;
            let mut at = begin.unwrap_or(*t0);
            let mut deltas = vec![us(at) - us(*t0)];
            let mut last = us(at);
            for w in &weights {
                at += w;
                deltas.push(us(at) - last);
                last = us(at);
            }
            for n in &p.nodes {
                if let Some(u) = n["callFrame"]["url"].as_str().filter(|u| !u.is_empty()) {
                    scripts.insert(u.to_string());
                }
            }
            let prof = p.finish(us(*t0), us(*t1), ids, deltas);
            crate::store::write_json(&dir.join(format!("cpu-g{g}-r{w}.cpuprofile")), &prof)?;
        }
        total += in_runs;
        thread_rows.push(json!({ "pid": t.pid, "tid": t.tid, "name": t.name, "samples_in_runs": in_runs }));
    }
    if let Some(w) = covered.iter().position(|c| !c) {
        let summary = format!(
            "Firefox's profile holds no samples of the page's main thread in {}: its buffer dropped them, or the page ran in a process strobe did not see",
            runs.get(w).map_or("a run", |r| r.0.as_str())
        );
        return Err(Refusal::new(ValidatorId::RunIncomplete, summary).fix("profile fewer or shorter runs (--runs), so the profiler's buffer holds them all").into());
    }
    // The views' rows, as a trace's SQL gives them (`query::Part`), from the page's main threads.
    let window_rows = |width: usize| -> Vec<Row> { windows.iter().map(|w| std::iter::once(cell(&w.0)).chain(std::iter::repeat_n(None, width - 1)).collect()).collect() };
    let (mut layout, mut gc, mut phases) = (window_rows(6), window_rows(7), window_rows(5));
    let (mut long_tasks, mut long_max) = (0u64, 0i64);
    for t in all.iter().filter(|t| page.contains(&t.pid) && t.name == "GeckoMain") {
        let spans = spans(t);
        let ns = |x: i64| x.to_string();
        // A sync flush's caller is the script that forced the style and layout work inside it.
        let flushes: Vec<(i64, i64, Option<String>)> = spans
            .iter()
            .filter(|s| s.name == "DoFlushPendingNotifications")
            .map(|s| {
                let frames = marker_stack(s.data).map(|st| t.js_frames(st)).unwrap_or_default();
                for f in &frames {
                    scripts.insert(f.0.clone());
                }
                (s.t0, s.t1, encode(&frames))
            })
            .collect();
        for s in &spans {
            let Some(w) = window_of(s.t0) else { continue };
            let k = &windows[w].0;
            let dur = (s.t1 - s.t0).max(0);
            let tid = t.tid.to_string();
            match s.name {
                "Styles" => {
                    let forced = flushes.iter().filter(|f| f.0 <= s.t0 && s.t0 < f.1).find_map(|f| f.2.clone());
                    layout.push(vec![cell(k), cell("UpdateLayoutTree"), cell(ns(s.t0)), cell(ns(dur)), cell(&tid), forced]);
                    phases.push(vec![cell(k), cell("UpdateLayoutTree"), cell(ns(s.t0)), cell(ns(dur)), cell(s.data["elementsStyled"].as_u64().unwrap_or(0))]);
                }
                n if n.starts_with("Reflow") => {
                    let frames = marker_stack(s.data).map(|st| t.js_frames(st)).unwrap_or_default();
                    for f in &frames {
                        scripts.insert(f.0.clone());
                    }
                    layout.push(vec![cell(k), cell("Layout"), cell(ns(s.t0)), cell(ns(dur)), cell(&tid), encode(&frames)]);
                    phases.push(vec![cell(k), cell("Layout"), cell(ns(s.t0)), cell(ns(dur)), cell(0)]);
                }
                "DisplayList" => phases.push(vec![cell(k), cell("Paint"), cell(ns(s.t0)), cell(ns(dur)), cell(0)]),
                "GCMinor" => {
                    let n = &s.data["nursery"];
                    let freed = n["bytes_used"].as_f64().zip(n["bytes_tenured"].as_f64()).map(|(u, k)| (u - k).max(0.0));
                    gc.push(vec![cell(k), cell("MinorGC"), cell(ns(s.t0)), cell(ns(dur)), cell(&tid), cell(1), freed.map(|f| f.to_string())]);
                }
                // A major collection runs in slices with the page between them: the slices are its pauses,
                // the collection itself is counted once with no time of its own.
                "GCMajor" => {
                    let tm = &s.data["timings"];
                    let freed = tm["allocated_bytes"].as_f64().zip(tm["post_heap_size"].as_f64()).map(|(a, p)| (a - p).max(0.0));
                    gc.push(vec![cell(k), cell("MajorGC"), cell(ns(s.t0)), cell(0), cell(&tid), cell(1), freed.map(|f| f.to_string())]);
                }
                "GCSlice" => gc.push(vec![cell(k), cell("GCSlice"), cell(ns(s.t0)), cell(ns(dur)), cell(&tid), cell(1), None]),
                "LongTask" => {
                    long_tasks += 1;
                    long_max = long_max.max(dur);
                }
                _ => {}
            }
        }
    }
    let read = json!({
        "token": token,
        "interval_us": asked.round() as u64,
        "sampling": sampling,
        "format": profile["meta"]["version"],
        "windows": windows.iter().map(|w| json!([w.0, w.1, w.2])).collect::<Vec<_>>(),
        "page_pids": page,
        "threads": thread_rows,
        "thread_cpu": thread_cpu,
        "parts": {
            "Layout": layout, "Gc": gc, "Phases": phases,
            "LongTasks": [[cell(long_tasks), (long_tasks > 0).then(|| long_max.to_string())]],
        },
    });
    crate::store::write_json(&dir.join(READ), &read)?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&bytes)?;
    // Owner-only: a profile holds the page's URLs, function names and marker texts (rule 9).
    host::paths::write_owner_only(&dir.join(PROFILE), &gz.finish()?)?;
    let _ = std::fs::remove_file(raw);
    Ok(Converted { scripts, samples: total, sampling })
}

/// A recording's Gecko profile for `strobe open`: every sampled thread's samples (waits left out)
/// with their frames, and its markers, on strobe's clock.
pub struct Timeline {
    pub threads: Vec<TimelineThread>,
    /// Native libraries by load address: (start, end, name, path).
    pub libs: Vec<(u64, u64, String, String)>,
    /// The measured runs' marks and spans the profile was read for.
    pub runs: Vec<(String, u64, u64)>,
}

pub struct TimelineThread {
    pub pid: u32,
    pub tid: u32,
    pub name: String,
    pub process: String,
    /// Distinct stacks, root first.
    pub stacks: Vec<Vec<TimelineFrame>>,
    /// (host ns, weight ns, index into `stacks`).
    pub samples: Vec<(u64, u64, usize)>,
    /// (name, host ns start, end, arguments).
    pub markers: Vec<Marker>,
}

/// A marker: name, host-clock ns start and end, arguments.
pub type Marker = (String, u64, u64, Vec<(String, Value)>);

#[derive(Clone)]
pub enum TimelineFrame {
    /// Page JavaScript: name, script, the function's start (0-based) as V8 places it.
    Js {
        name: String,
        url: String,
        line0: i64,
        column0: i64,
    },
    Label(String),
    Native(u64),
}

/// Markers the timeline leaves out: bookkeeping that would drown the page's own work.
const NOISE: [&str; 9] = [
    "Awake",
    "TaskController::AddTask",
    "EventQueueInternal::PutEvent",
    "SetNeedStyleFlush",
    "TimingDistribution::accumulate",
    "TimingDistribution::start",
    "TimingDistribution::stop",
    "Counter::add",
    "CustomDistribution::accumulate",
];

pub fn timeline(dir: &Path) -> Option<Timeline> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(dir.join(PROFILE)).ok()?).read_to_end(&mut raw).ok()?;
    let profile = read_profile(&raw).ok()?;
    let all = threads(&profile).ok()?;
    let idle = profile["meta"]["categories"].as_array().and_then(|c| c.iter().position(|c| c["name"] == "Idle")).map(|i| i as u64);
    // A thread's last sample weighs the gap its sampler kept (what `convert` read), else the one asked.
    let read = crate::store::read_json(&dir.join(READ)).ok();
    let kept = read.as_ref().and_then(|g| g["sampling"]["median_us"].as_f64());
    let interval_ns = (kept.unwrap_or_else(|| asked_us(&profile)) * 1e3).round() as i64;
    let mut libs = Vec::new();
    let mut todo = vec![&profile];
    while let Some(p) = todo.pop() {
        for l in p["libs"].as_array().into_iter().flatten() {
            if let (Some(a), Some(b)) = (l["start"].as_u64(), l["end"].as_u64()) {
                libs.push((a, b, l["name"].as_str().unwrap_or("").to_string(), l["path"].as_str().unwrap_or("").to_string()));
            }
        }
        todo.extend(p["processes"].as_array().into_iter().flatten());
    }
    libs.sort();
    libs.dedup();
    let processes: HashMap<u32, String> = {
        let mut m = HashMap::new();
        let mut todo = vec![&profile];
        while let Some(p) = todo.pop() {
            for t in p["threads"].as_array().into_iter().flatten() {
                if let Some(pid) = number(&t["pid"]) {
                    m.entry(pid as u32).or_insert_with(|| t["processName"].as_str().or(t["processType"].as_str()).unwrap_or("Firefox").to_string());
                }
            }
            todo.extend(p["processes"].as_array().into_iter().flatten());
        }
        m
    };
    let mut out = Vec::new();
    for t in &all {
        let mut samples = Vec::new();
        let mut stacks: Vec<Vec<TimelineFrame>> = Vec::new();
        // Profile stack → index into `stacks`, None where the thread waited.
        let mut index: HashMap<usize, Option<usize>> = HashMap::new();
        let n = t.samples.data.len();
        for i in 0..n {
            let (Some(ms), Some(stack)) = (t.samples.get(i, "time").as_f64(), t.samples.get(i, "stack").as_u64()) else { continue };
            let stack = stack as usize;
            let at = *index.entry(stack).or_insert_with(|| {
                let chain = t.chain(stack);
                if idle.is_some() && chain.iter().find_map(|&f| t.categories[f]) == idle {
                    return None;
                }
                let frames: Vec<TimelineFrame> = chain
                    .iter()
                    .rev()
                    .filter_map(|&f| match &t.kinds[f] {
                        Kind::Root => None,
                        Kind::Native(a) => Some(TimelineFrame::Native(*a)),
                        Kind::Js { name, url, line, column } => Some(TimelineFrame::Js { name: name.clone(), url: url.clone(), line0: (line - 1).max(0), column0: (column - 1).max(0) }),
                        Kind::Label(l) => Some(TimelineFrame::Label(l.clone())),
                        Kind::Profiler => Some(TimelineFrame::Label("(profiler)".into())),
                    })
                    .collect();
                (!frames.is_empty()).then(|| {
                    stacks.push(frames);
                    stacks.len() - 1
                })
            });
            let Some(at) = at else { continue };
            let ts = t.host(ms);
            let next = (i + 1 < n).then(|| t.samples.get(i + 1, "time").as_f64()).flatten().map(|m| t.host(m));
            let weight = next.map_or(interval_ns, |x| (x - ts).clamp(0, MAX_GAP_NS)) as u64;
            samples.push((ts.max(0) as u64, weight, at));
        }
        let markers: Vec<Marker> = spans(t)
            .into_iter()
            .filter(|s| !NOISE.contains(&s.name))
            .map(|s| {
                let mut args: Vec<(String, Value)> = Vec::new();
                if let Some(o) = s.data.as_object() {
                    for (k, v) in o.iter().filter(|(k, _)| !matches!(k.as_str(), "stack" | "type" | "innerWindowID" | "cause")) {
                        match v {
                            Value::Object(_) | Value::Array(_) => {}
                            Value::Number(_) if k == "name" => args.push((k.clone(), json!(t.string(v)))),
                            _ => args.push((k.clone(), v.clone())),
                        }
                    }
                }
                let name = if s.name == "UserTiming" { s.data["name"].as_str().unwrap_or("UserTiming").to_string() } else { s.name.to_string() };
                (name, s.t0.max(0) as u64, s.t1.max(s.t0).max(0) as u64, args)
            })
            .collect();
        if samples.is_empty() && markers.is_empty() {
            continue;
        }
        out.push(TimelineThread { pid: t.pid, tid: t.tid, name: t.name.to_string(), process: processes.get(&t.pid).cloned().unwrap_or_default(), stacks, samples, markers });
    }
    let runs = read
        .and_then(|g| g["windows"].as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|w| Some((w[0].as_str()?.to_string(), w[1].as_u64()?, w[2].as_u64()?)))
        .collect();
    Some(Timeline { threads: out, libs, runs })
}

/// about:memory of every Firefox process, after the GC and CC passes its "Minimize memory usage" runs,
/// as a census (SPEC §8.6): OS memory per process, its explicit allocations by layer (the top of the
/// `explicit/` tree, `heap-unclassified` the heap no reporter claims) and allocator (two levels down),
/// the page's JavaScript memory (its compartments' objects, strings, shapes and their buffers, from
/// SpiderMonkey's reporters) and DOM. The report itself is kept as `report` (gzip) beside the recording.
/// `taken` says when, for the views (`before the profiler started`, `after run 3`).
pub fn census(browser: &Browser, page: &WebPage, report: &Path, taken: &str) -> Result<Value> {
    let tmp = browser.profile.clone().ok_or_else(|| Error::msg("Firefox has no profile folder"))?.join("memory-report.json.gz");
    let _ = std::fs::remove_file(&tmp);
    let call = format!(
        "new Promise((done, fail) => {{ try {{ Cc['@mozilla.org/memory-info-dumper;1'].getService(Ci.nsIMemoryInfoDumper).dumpMemoryReportsToNamedFile({}, () => done(true), null, false, true) }} catch (e) {{ fail(e) }} }})",
        json!(tmp)
    );
    browser.chrome_eval_timeout(&call, Duration::from_secs(120))?;
    let gz = std::fs::read(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    let mut text = String::new();
    flate2::read::GzDecoder::new(gz.as_slice()).read_to_string(&mut text).map_err(|e| Error::msg(format!("about:memory report: {e}")))?;
    let reports: Value = serde_json::from_str(&text).map_err(|e| Error::msg(format!("about:memory report: {e}")))?;
    // Owner-only: about:memory names every page and file the browser holds (rule 9).
    host::paths::write_owner_only(report, &gz)?;
    let at_ns = host::now_ns();
    let page_pid = browser.page_process();
    let pid_of = |p: &str| p.rsplit_once("(pid ").and_then(|(_, r)| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse::<u32>().ok());
    // Per pid: layer → bytes, allocator (`explicit/a/b`) → bytes, heap reported, heap allocated.
    #[derive(Default)]
    struct Acc {
        layers: BTreeMap<String, f64>,
        allocators: BTreeMap<String, f64>,
        heap_reported: f64,
        heap_allocated: Option<f64>,
    }
    let mut by_pid: BTreeMap<u32, Acc> = BTreeMap::new();
    let (mut js_used, mut js_total, mut dom_bytes, mut orphan) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for r in reports["reports"].as_array().into_iter().flatten() {
        let Some(pid) = r["process"].as_str().and_then(pid_of) else { continue };
        let (path, amount) = (r["path"].as_str().unwrap_or(""), r["amount"].as_f64().unwrap_or(0.0));
        // Bytes only (units 0); counts and percentages are not memory.
        if r["units"].as_u64() != Some(0) {
            continue;
        }
        let acc = by_pid.entry(pid).or_default();
        if path == "heap-allocated" {
            acc.heap_allocated = Some(amount);
        }
        let Some(rest) = path.strip_prefix("explicit/") else { continue };
        if r["kind"].as_u64() == Some(1) {
            acc.heap_reported += amount;
        }
        let mut parts = rest.split('/');
        let layer = parts.next().unwrap_or("").to_string();
        // The profiler's own memory (on Linux its unwinder's tables, tens of MB per process) is strobe's.
        if layer == "profiler" {
            continue;
        }
        let second = parts.next().map(|s| format!("{layer}/{s}")).unwrap_or_else(|| layer.clone());
        *acc.layers.entry(layer).or_default() += amount;
        *acc.allocators.entry(second).or_default() += amount;
        if Some(pid) == page_pid && rest.starts_with("window-objects/") {
            if rest.contains("/js-zone(") || rest.contains("/js-realm(") {
                js_total += amount;
                if !rest.ends_with("/unused-gc-things") {
                    js_used += amount;
                }
            }
            if rest.contains("/dom/") {
                dom_bytes += amount;
                if rest.contains("/dom/orphan-nodes") {
                    orphan += amount;
                }
            }
        }
    }
    let types: HashMap<u32, String> = browser.process_types().into_iter().collect();
    let processes: Vec<Value> = browser
        .processes()
        .iter()
        .map(|p| {
            let ty = if Some(p.pid) == page_pid { "renderer" } else { types.get(&p.pid).map_or("other", String::as_str) };
            json!({ "pid": p.pid, "name": p.name, "type": ty, "private_bytes": p.private_bytes, "working_set_bytes": p.working_set })
        })
        .collect();
    let allocators: serde_json::Map<String, Value> = by_pid
        .into_iter()
        .map(|(pid, mut a)| {
            if let Some(h) = a.heap_allocated {
                a.layers.insert("heap-unclassified".into(), (h - a.heap_reported).max(0.0));
            }
            let mut children: Vec<(String, f64)> = a.allocators.into_iter().collect();
            children.sort_by(|x, y| y.1.total_cmp(&x.1));
            children.truncate(40);
            let children: Vec<Value> = children.into_iter().map(|(p, s)| json!({ "path": p, "size": s, "effective": s })).collect();
            (pid.to_string(), json!({ "layers": a.layers, "allocators": children }))
        })
        .collect();
    // Nodes in the document; nodes no document holds any more show as about:memory's orphan-nodes bytes.
    let nodes =
        page.eval("(() => { let n = 1; const w = document.createTreeWalker(document, NodeFilter.SHOW_ALL); while (w.nextNode()) n++; return n })()").ok().and_then(|v| v.as_f64()).map(|n| n as u64);
    Ok(json!({
        "at_ns": at_ns,
        "taken": taken,
        "engine": "firefox",
        "page_pid": page_pid,
        "js_heap_bytes": page_pid.map(|_| js_used), "js_heap_total_bytes": page_pid.map(|_| js_total),
        "dom": { "nodes": nodes, "bytes": dom_bytes, "orphan_bytes": orphan },
        "processes": processes,
        "allocators": allocators,
        "report": report.file_name().map(|n| n.to_string_lossy().to_string()),
    }))
}
