//! `strobe query`: views over recordings and experiments (SPEC §11.1).

use super::{Output, fmt};
use crate::cli::Args;
use crate::engine;
use crate::metrics;
use crate::model::{Error, Result};
use crate::probe::symbols::{Frame, Symbolizer};
use crate::store;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub fn run(args: &Args) -> Result<Output> {
    let handle = args.pos.first().ok_or_else(|| Error::usage("query needs a handle: strobe query rec_… <view>"))?;
    let view = args.pos.get(1).map(String::as_str).unwrap_or("summary");
    let top = args.num("top")?.unwrap_or(15.0) as usize;
    if let Some(dir) = store::resolve(handle).ok().filter(|d| store::is_experiment(d)) {
        if !dir.join("result.json").exists() {
            let log = std::fs::read_to_string(dir.join("progress.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            let text = format!("{handle} is running\n{}\n", tail.join("\n"));
            return Ok(Output::new(text, json!({ "id": handle, "state": "running", "progress": tail })));
        }
        let result = store::read_json(&dir.join("result.json"))?;
        return match view {
            "summary" | "progress" => Ok(Output::new(result["text"].as_str().unwrap_or("").to_string(), result)),
            "insights" => experiment_insights(&result),
            other => Err(Error::usage(format!("experiments support summary, progress and insights; use a recording handle for {other} (see the result's recordings)"))),
        };
    }
    // `a..b`: two recordings compared (heap snapshots by class).
    if let Some((a, b)) = handle.split_once("..") {
        let (a, b) = (store::resolve(a)?, store::resolve(b)?);
        return match view {
            "heap" => heap_diff(&a, &b, top, args.has("reveal")),
            "memory" => memory_diff(&a, &b, top),
            other => Err(Error::usage(format!("a..b supports heap and memory, not {other}"))),
        };
    }
    let dir = store::resolve(handle)?;
    let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
    let trace = dir.join("trace.pftrace");
    match view {
        "summary" => Ok(Output::new(super::record::summary_text(&meta), meta)),
        "sql" => {
            let sql = args.pos.get(2).ok_or_else(|| Error::usage("query <h> sql \"select …\""))?;
            sql_view(&trace, sql, top.max(50))
        }
        "frames" if dir.join("frames.json").exists() || meta["native"] == true => native_frames(&dir, top),
        "threads" if dir.join("threads.json").exists() => native_threads(&dir, top),
        "frames" => frames(&trace, &meta, top),
        "threads" => threads(&trace, top),
        "gpu" if !trace.exists() => native_gpu(&meta),
        "gpu" => gpu(&trace, top),
        "hotspots" | "alloc" => {
            let by = args.get("by").unwrap_or("function");
            if !matches!(by, "function" | "file" | "package") {
                return Err(Error::usage(format!("--by: function, file or package, not {by}")));
            }
            if view == "alloc" {
                alloc(&dir, by, top)
            } else if args.has("native") || args.has("thread") || args.has("phase") {
                native_hotspots(&dir, by, top, args.get("thread"), args.get("phase"))
            } else {
                hotspots(&dir, by, top)
            }
        }
        "layout" => layout(&dir, top),
        "shifts" => {
            let v =
                store::read_json(&dir.join("shifts.json")).map_err(|_| Error::usage("no layout shifts in this recording: they come from attribution blocks of a series that declares layout.shift"))?;
            Ok(Output::new(crate::probe::shifts::text(&v), v))
        }
        "gc" => gc(&dir),
        "heap" => heap(&dir, top, args.has("reveal")),
        "leaks" => leaks(&dir, top, args.has("reveal")),
        "insights" => insights(&dir),
        "memory" => {
            let by = args.get("by").unwrap_or("process");
            if !matches!(by, "process" | "layer" | "allocator") {
                return Err(Error::usage(format!("--by: process, layer or allocator, not {by}")));
            }
            if dir.join("memory.json").exists() {
                return native_memory(&dir, &meta, by, top, args.get("at"));
            }
            if meta["native"] == true || meta["pid"].is_u64() {
                return Err(Error::usage(format!("this native recording has no memory reading: {}", crate::probe::memory::why_empty())));
            }
            memory(&dir, by, top, args.get("at"))
        }
        other => Err(Error::usage(format!("unknown view {other}: summary frames threads gpu hotspots alloc layout shifts gc heap leaks memory insights sql"))),
    }
}

/// The recording's heap snapshot: live objects by class with retained sizes, detached DOM, the largest
/// owners and their retainer paths. String values stay hidden unless `--reveal` (SPEC §8.6).
pub fn heap(dir: &Path, top: usize, reveal: bool) -> Result<Output> {
    let snap = crate::heapsnap::read(&heap_file(dir)?)?;
    let v = crate::heapsnap::summary(&snap, top, reveal);
    Ok(Output::new(crate::heapsnap::summary_text(&v, top), v))
}

/// Live objects in `b` against `a` by class: count, self and retained size changes, and what only `b` has.
/// Two recordings are two processes, so new objects group by class, never by node id.
pub fn heap_diff(a: &Path, b: &Path, top: usize, reveal: bool) -> Result<Output> {
    let (base, head) = (crate::heapsnap::read(&heap_file(a)?)?, crate::heapsnap::read(&heap_file(b)?)?);
    let v = crate::heapsnap::diff(&base, &head, false, top, reveal);
    Ok(Output::new(crate::heapsnap::diff_text(&v, top), v))
}

/// Insights (SPEC §11.3): rules over one recording's own data, each hit with its numbers and the
/// commands that show the evidence. Facts with thresholds, never a verdict strobe cannot back.
pub fn insights(dir: &Path) -> Result<Output> {
    let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
    let id = meta["id"].as_str().unwrap_or("").to_string();
    let runs = meta["runs"].as_f64().unwrap_or(1.0).max(1.0);
    let v = &meta["values"];
    let work = v["main.work"].as_f64().unwrap_or(f64::NAN);
    let mut hits: Vec<Value> = Vec::new();
    let mut hit =
        |rule: &str, level: &str, title: String, evidence: Value, handles: Vec<String>| hits.push(json!({ "rule": rule, "level": level, "title": title, "evidence": evidence, "handles": handles }));
    let q = |view: &str| format!("strobe query {id} {view}");
    // Forced layout: geometry read after a DOM write makes the browser lay out mid-script.
    if let Ok(l) = layout(dir, 3).map(|o| o.json) {
        let ms = l["forced"]["layout"]["ms_per_run"].as_f64().unwrap_or(0.0) + l["forced"]["style"]["ms_per_run"].as_f64().unwrap_or(0.0);
        let share = if work > 0.0 { ms / work } else { f64::NAN };
        if ms >= 5.0 || share >= 0.05 {
            let top = &l["rows"][0];
            hit(
                "forced-layout",
                if share >= 0.1 { "warn" } else { "info" },
                format!(
                    "forced layout and style: {:.0} layouts and {:.0} style recalcs ({}) per run{}, most from {} at {}",
                    l["forced"]["layout"]["count_per_run"].as_f64().unwrap_or(0.0),
                    l["forced"]["style"]["count_per_run"].as_f64().unwrap_or(0.0),
                    fmt::value(ms, "ms"),
                    if share.is_finite() { format!(", {:.0}% of main-thread work", share * 100.0) } else { String::new() },
                    top["name"].as_str().unwrap_or("?"),
                    top["where"].as_str().unwrap_or("?")
                ),
                json!({ "ms_per_run": ms, "share_of_work": share, "forced": l["forced"], "top": top }),
                vec![q("layout")],
            );
        }
    }
    // GC pressure: collections pause the main thread; allocation sites say who causes them.
    if let Ok(g) = gc(dir).map(|o| o.json) {
        let ms = g["values"]["main_ms_per_run"].as_f64().unwrap_or(0.0);
        let share = if work > 0.0 { ms / work } else { f64::NAN };
        if share >= 0.05 || ms >= 10.0 {
            let sites: Vec<String> = alloc(dir, "function", 3)
                .map(|o| o.json["rows"].as_array().into_iter().flatten().map(|r| format!("{} {}", r["name"].as_str().unwrap_or(""), fmt::bytes(r["weight"].as_f64().unwrap_or(0.0) / runs))).collect())
                .unwrap_or_default();
            hit(
                "gc-pressure",
                if share >= 0.1 { "warn" } else { "info" },
                format!(
                    "garbage collection: {} of main-thread pauses per run{} ({:.0} minor, {:.0} major); top allocation sites per run: {}",
                    fmt::value(ms, "ms"),
                    if share.is_finite() { format!(", {:.0}% of main-thread work", share * 100.0) } else { String::new() },
                    g["values"]["minor_per_run"].as_f64().unwrap_or(0.0),
                    g["values"]["major_per_run"].as_f64().unwrap_or(0.0),
                    if sites.is_empty() { "no allocation profile".into() } else { sites.join(", ") }
                ),
                json!({ "gc": g["values"], "share_of_work": share, "sites": sites }),
                vec![q("gc"), q("alloc")],
            );
        }
    }
    // Wakes while idle: a native app recorded with no input whose threads keep waking (a polling loop,
    // a short timer) costs power and CPU all day; the threads that wake most are named.
    let wakes = v["cpu.wakes"].as_f64().unwrap_or(0.0);
    if meta["native"] == true && meta["kind"] == "record" && wakes >= 50.0 {
        let threads = store::read_json(&dir.join("threads.json")).ok().and_then(|t| t.as_array().cloned()).unwrap_or_default();
        let line = |t: &Value| {
            let module = t["module"].as_str().map(|m| format!(" ({m})")).unwrap_or_default();
            format!("{}{module} {}", t["name"].as_str().unwrap_or("(unnamed)"), fmt::value(t["wakes_per_s"].as_f64().unwrap_or(0.0), "/s"))
        };
        // The app's own threads are what its code can change; a driver's or the OS's threads inside
        // its processes wake for what the app asked of them (a GPU device, an audio stream), so they
        // are named apart.
        let mine = |t: &&Value| t["origin"] != "driver" && t["origin"] != "os";
        let sum = |own: bool| threads.iter().filter(|t| mine(t) == own).map(|t| t["wakes_per_s"].as_f64().unwrap_or(0.0)).sum::<f64>();
        let (app, other) = (sum(true), sum(false));
        let top: Vec<String> = threads.iter().filter(mine).take(3).map(line).collect();
        let theirs: Vec<String> = threads.iter().filter(|t| !mine(t)).take(3).map(line).collect();
        let mut title = format!("the app's threads woke {} with nothing to do; most: {}", fmt::value(app, "/s"), if top.is_empty() { "none".into() } else { top.join(", ") });
        if other > 0.0 {
            title += &format!("; driver and OS threads in its processes woke {} more: {}", fmt::value(other, "/s"), theirs.join(", "));
        }
        hit(
            "wakes-while-idle",
            if app >= 200.0 { "warn" } else { "info" },
            title,
            json!({ "wakes_per_s": wakes, "app_wakes_per_s": app, "driver_os_wakes_per_s": other, "top": top, "driver_os_top": theirs }),
            vec![q("threads")],
        );
    }
    // Long tasks: one task over 50 ms blocks input for that long.
    let trace = dir.join("trace.pftrace");
    if trace.is_file() && !meta["token"].as_str().unwrap_or("").is_empty() {
        let sql = format!(
            "{}
select count(*), max(s.dur), (select s2.ts from slice s2 join thread_track tt2 on s2.track_id = tt2.id join thread t2 using(utid)
                              where s2.name = 'RunTask' and s2.depth = 0 and t2.name = 'CrRendererMain' order by s2.dur desc limit 1)
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
 where s.name = 'RunTask' and s.depth = 0 and t.name = 'CrRendererMain' and s.dur > 50000000",
            windows_sql(meta["token"].as_str().unwrap_or(""))
        );
        if let Ok(t) = engine::query(&trace, &sql) {
            let cell = |i: usize| t.rows.first().and_then(|r| r.get(i).cloned().flatten()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let (n, worst) = (cell(0), cell(1) / 1e6);
            if n > 0.0 {
                hit(
                    "long-tasks",
                    "warn",
                    format!("long tasks: {n:.0} main-thread tasks over 50 ms in the recording, the longest {}: input waits that long", fmt::value(worst, "ms")),
                    json!({ "count": n, "longest_ms": worst }),
                    vec![format!("strobe query {id} sql \"select ts, dur / 1e6 as ms from slice where name = 'RunTask' and depth = 0 and dur > 50e6 order by dur desc limit 20\""), q("hotspots")],
                );
            }
        }
    }
    // Work while idle: a recording with no input that still presents frames or keeps the main thread busy.
    if meta["kind"] == "record" && v["interaction.n"].is_null() && v["scroll.n"].is_null() {
        let seconds = meta["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0;
        let fps = v["frames.n"].as_f64().unwrap_or(0.0) / seconds.max(1e-9);
        let busy = v["main.busy"].as_f64().unwrap_or(0.0);
        if fps >= 1.0 || busy >= 5.0 {
            hit(
                "work-while-idle",
                "warn",
                format!("work while idle: with no input, the page presented {fps:.1} frames per second and kept its main thread {busy:.1}% busy: an animation, a timer or polling"),
                json!({ "frames_per_second": fps, "main_busy_pct": busy }),
                vec![q("frames"), q("hotspots")],
            );
        }
    }
    // Detached DOM: removed from the document but kept alive by script.
    if let Ok(h) = heap(dir, 3, false).map(|o| o.json) {
        let d = &h["detached"];
        if d["count"].as_u64().unwrap_or(0) > 0 {
            let largest = &d["classes"][0];
            hit(
                "detached-dom",
                if d["retained_size"].as_f64().unwrap_or(0.0) > 1e6 { "warn" } else { "info" },
                format!(
                    "detached DOM: {} nodes removed from the document are still alive, retaining {}; the largest, {}, is held by {}",
                    d["count"],
                    fmt::bytes(d["retained_size"].as_f64().unwrap_or(0.0)),
                    largest["class"].as_str().unwrap_or("?"),
                    path_tail(&largest["largest"]["path"])
                ),
                json!({ "detached": d["count"], "retained_size": d["retained_size"], "largest": largest }),
                vec![q("heap")],
            );
        }
    }
    // Background load on the measured CPUs.
    if v["cpu.foreign.max"].as_f64().is_some_and(|f| f >= 25.0) {
        hit(
            "background",
            "warn",
            format!("background load: other processes used up to {:.0}% of a core on the CPUs the target ran on", v["cpu.foreign.max"].as_f64().unwrap_or(0.0)),
            json!({ "cpu_foreign_max_pct": v["cpu.foreign.max"], "busiest": meta["background"] }),
            vec![],
        );
    }
    // Unresolved time: script without source maps, and native time strobe cannot name without deep.
    if let Ok(h) = hotspots(dir, "function", 400).map(|o| o.json) {
        if h["native"] == true && h["resolved_share"].as_f64().is_some_and(|s| s < 0.95) {
            let share = h["resolved_share"].as_f64().unwrap_or(0.0);
            hit(
                "unresolved",
                "info",
                format!("unresolved native: {:.0}% of native samples have no symbol (no matching PDB), mostly in {}", 100.0 * (1.0 - share), h["unsymbolized"][0]["module"].as_str().unwrap_or("?")),
                json!({ "resolved_share": share, "unsymbolized": h["unsymbolized"] }),
                vec![q("hotspots --by package")],
            );
        }
        let (scripts, mapped) = (h["script_samples"].as_f64().unwrap_or(0.0), h["mapped_samples"].as_f64().unwrap_or(0.0));
        if scripts > 0.0 && mapped / scripts < 0.95 {
            hit(
                "unresolved",
                "info",
                format!("unresolved script: {:.0}% of script samples have no source map, so names and files stay minified there", 100.0 * (1.0 - mapped / scripts)),
                json!({ "script_samples": scripts, "mapped_samples": mapped }),
                vec![q("hotspots --by file")],
            );
        }
        let rows = h["rows"].as_array().cloned().unwrap_or_default();
        let busy: f64 = rows.iter().filter(|r| r["name"] != "(idle)").map(|r| r["weight"].as_f64().unwrap_or(0.0)).sum();
        let program = rows.iter().find(|r| r["name"] == "(program)").and_then(|r| r["weight"].as_f64()).unwrap_or(0.0);
        if busy > 0.0 && program / busy >= 0.3 {
            hit(
                "native-time",
                "info",
                format!(
                    "native time: {:.0}% of busy samples are (program), the browser's own work outside JavaScript (rendering, IPC, parsing); naming it needs native stacks",
                    100.0 * program / busy
                ),
                json!({ "program_share": program / busy }),
                vec![q("threads")],
            );
        }
    }
    // Memory recordings and leak runs: what grew and what stayed (facts only).
    if let Some(l) = meta["leak"]["js_heap"].as_object() {
        hit(
            "leak",
            if l["verdict"].as_str().is_some_and(|x| x.starts_with("leaks")) { "warn" } else { "info" },
            format!("js heap: {}", l["verdict"].as_str().unwrap_or("")),
            json!(l),
            vec![q("leaks"), q("memory")],
        );
    } else if meta["probes"] == "memory" && meta["leaks"]["count"].as_u64().unwrap_or(0) > 0 {
        hit(
            "leak",
            "info",
            format!("{} objects from the first half are still alive at the end, retaining {}", meta["leaks"]["count"], fmt::bytes(meta["leaks"]["retained_size"].as_f64().unwrap_or(0.0))),
            meta["leaks"].clone(),
            vec![q("leaks"), q("memory")],
        );
    }
    let mut text = format!("insights · {id} · {} hit(s)\n", hits.len());
    for h in &hits {
        text += &format!("{} {}\n", if h["level"] == "warn" { "!" } else { "i" }, h["title"].as_str().unwrap_or(""));
        for c in h["handles"].as_array().into_iter().flatten() {
            text += &format!("    {}\n", c.as_str().unwrap_or(""));
        }
    }
    if hits.is_empty() {
        text += "nothing stood out: no rule crossed its threshold\n";
    }
    Ok(Output::new(text, json!({ "recording": id, "insights": hits })))
}

/// An experiment's insights: per series, what moved most between the base and head profiled launches
/// (CPU self time, forced layout and allocations by function, per run), then the head recording's
/// own insights. Functions match by name and file name: the two builds live in different folders.
pub fn experiment_insights(result: &Value) -> Result<Output> {
    const VIEWS: [(&str, &str, &str); 3] = [("CPU self time", "ms", "hotspots"), ("forced layout", "ms", "layout"), ("allocations", "bytes", "alloc")];
    let mut text = format!("insights · {}\n", result["id"].as_str().unwrap_or(""));
    let mut series_out = Vec::new();
    for s in result["series"].as_array().into_iter().flatten() {
        let (Some(b), Some(h)) = (s["attribution"]["base"].as_str(), s["attribution"]["head"].as_str()) else { continue };
        let (bd, hd) = (store::resolve(b)?, store::resolve(h)?);
        text += &format!("\n{}\n", s["label"].as_str().unwrap_or(""));
        // Each view on each side is its own profile read or trace_processor run: all at once.
        let (bp, hp) = (bd.as_path(), hd.as_path());
        let (sides, own) = std::thread::scope(|scope| {
            let sides: Vec<_> = VIEWS.iter().map(|&(_, unit, view)| (scope.spawn(move || mover_rows(bp, view, unit)), scope.spawn(move || mover_rows(hp, view, unit)))).collect();
            let own = scope.spawn(move || insights(hp));
            (sides.into_iter().map(|(x, y)| (x.join().unwrap_or_default(), y.join().unwrap_or_default())).collect::<Vec<_>>(), own.join())
        });
        let mut movers = Vec::new();
        for ((what, unit, view), (base, head)) in VIEWS.iter().zip(sides) {
            let mut deltas: Vec<(String, f64, f64)> = base
                .keys()
                .chain(head.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|k| (k.clone(), base.get(k).copied().unwrap_or(0.0), head.get(k).copied().unwrap_or(0.0)))
                .collect();
            deltas.sort_by(|x, y| (y.2 - y.1).abs().total_cmp(&(x.2 - x.1).abs()));
            let floor = if *unit == "ms" { 1.0 } else { 64.0 * 1024.0 };
            for (k, x, y) in deltas.into_iter().filter(|(_, x, y)| (y - x).abs() >= floor).take(3) {
                let show = |v: f64| if *unit == "ms" { fmt::value(v, "ms") } else { fmt::bytes(v) };
                let line = format!("{what}: {k} {} → {} per run ({}{})", show(x), show(y), if y > x { "+" } else { "−" }, show((y - x).abs()));
                text += &format!("i {line}\n");
                movers.push(json!({ "kind": what, "function": k, "base_per_run": x, "head_per_run": y, "unit": unit, "title": line, "handles": [format!("strobe query {h} {view}"), format!("strobe query {b} {view}")] }));
            }
        }
        let own = own.map_err(|_| Error::msg("insights panicked"))??;
        for line in own.text.lines().skip(1) {
            text += &format!("{line}\n");
        }
        series_out.push(json!({ "series": s["label"], "movers": movers, "head": own.json["insights"] }));
    }
    Ok(Output::new(text, json!({ "experiment": result["id"], "series": series_out })))
}

/// One profiled launch's rows of a view as `function file-name` → value per run (ms for time, bytes
/// for allocations); the `(idle)`-style pseudo-frames of a CPU profile are left out.
fn mover_rows(dir: &Path, view: &str, unit: &str) -> BTreeMap<String, f64> {
    let out = match view {
        "hotspots" => hotspots(dir, "function", 200),
        "layout" => layout(dir, 200),
        _ => alloc(dir, "function", 200),
    };
    let runs = store::read_json(&dir.join("meta.json")).ok().and_then(|m| m["runs"].as_f64()).unwrap_or(1.0).max(1.0);
    let scale = if unit == "ms" { 1000.0 } else { 1.0 };
    out.map(|o| {
        o.json["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| !(view == "hotspots" && r["where"] == "(native)" && r["name"].as_str().unwrap_or("").starts_with('(')))
            .map(|r| {
                let file = r["where"].as_str().unwrap_or("").replace('\\', "/");
                let file = file.rsplit('/').next().unwrap_or("").split(':').next().unwrap_or("").to_string();
                (format!("{} {file}", r["name"].as_str().unwrap_or("")), r["weight"].as_f64().unwrap_or(0.0) / scale / runs)
            })
            .collect()
    })
    .unwrap_or_default()
}
/// The last two steps of a retainer path, as `… › parent › object`.
fn path_tail(path: &Value) -> String {
    let steps: Vec<String> =
        path.as_array().into_iter().flatten().map(|s| format!("{} {}", s["edge"].as_str().map(str::to_string).unwrap_or_else(|| s["edge"].to_string()), s["name"].as_str().unwrap_or(""))).collect();
    match steps.len() {
        0 => "an unknown retainer".into(),
        n => format!("… › {}", steps[n.saturating_sub(2)..].join(" › ")),
    }
}

/// Memory by process, layer or allocator at one census (`--at` index, default the last): OS private
/// bytes and working set per process, the page's JS heap and DOM, and Chromium's allocators by
/// effective size (SPEC §8.6). `unexplained` is OS private bytes no allocator accounts for: an
/// estimate, since shared memory and the OS account differently.
pub fn memory(dir: &Path, by: &str, top: usize, at: Option<&str>) -> Result<Output> {
    let (c, label) = census_at(dir, at)?;
    let procs = c["processes"].as_array().cloned().unwrap_or_default();
    let alloc = &c["allocators"];
    let mb = |b: f64| fmt::bytes(b);
    // The trace that took the dump allocates its own buffers: strobe's memory, not the target's.
    let layers = |pid: &str| -> BTreeMap<String, f64> {
        alloc[pid]["layers"].as_object().map(|o| o.iter().filter(|(k, _)| k.as_str() != "tracing").map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0))).collect()).unwrap_or_default()
    };
    let inflated = c["os_after_snapshot"] == true;
    // The page's renderer: the one whose V8 heap matches the page's (other renderers serve workers or
    // extensions). Only it was snapshotted, so only its OS readings are inflated after a snapshot.
    let js_total = c["js_heap_total_bytes"].as_f64().unwrap_or(f64::NAN);
    let page_pid = procs
        .iter()
        .filter(|p| p["type"] == "renderer")
        .filter_map(|p| Some((p["pid"].to_string(), (layers(&p["pid"].to_string()).get("v8")? - js_total).abs())))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|x| x.0);
    // A utility process by its service (`network.mojom.NetworkService` → network); the tracing service
    // runs because strobe traces, so it is strobe's, not the target's (rule 2).
    let kind = |p: &Value| -> String {
        let ty = p["type"].as_str().unwrap_or("other");
        match ty.split_once(".mojom.") {
            Some(("tracing", _)) => "tracing (strobe's)".into(),
            Some((service, _)) => service.to_string(),
            None if page_pid.as_deref() == Some(p["pid"].to_string().as_str()) => "renderer (page)".into(),
            None => ty.to_string(),
        }
    };
    let procs: Vec<Value> = procs
        .into_iter()
        .map(|mut p| {
            p["kind"] = json!(kind(&p));
            p
        })
        .collect();
    let target_procs: Vec<&Value> = procs.iter().filter(|p| p["kind"] != "tracing (strobe's)").collect();
    let mut text = format!("memory by {by} · {label}{}\n", if alloc.is_object() && alloc.get("error").is_none() { "" } else { " · no allocator dump" });
    let json = match by {
        "layer" => {
            let mut sums: BTreeMap<String, f64> = BTreeMap::new();
            for p in &target_procs {
                for (k, v) in layers(&p["pid"].to_string()) {
                    *sums.entry(k).or_default() += v;
                }
            }
            let mut rows: Vec<(String, f64)> = sums.into_iter().collect();
            rows.sort_by(|a, b| b.1.total_cmp(&a.1));
            let private: f64 = target_procs.iter().filter_map(|p| p["private_bytes"].as_f64()).sum();
            text += &format!("  {:>10}  OS private, all {} target processes (strobe's tracing service left out)\n", mb(private), target_procs.len());
            text += &format!("  {:>10}  JS heap of the page (used, after GC)\n", mb(c["js_heap_bytes"].as_f64().unwrap_or(f64::NAN)));
            for (k, v) in rows.iter().take(top) {
                text += &format!("  {:>10}  {k}\n", mb(*v));
            }
            json!({ "census": label, "os_private_bytes": private, "js_heap_bytes": c["js_heap_bytes"], "layers": rows.iter().map(|(k, v)| json!({ "layer": k, "effective_bytes": v })).collect::<Vec<_>>() })
        }
        "allocator" => {
            let mut rows: Vec<(String, String, f64, f64)> = target_procs
                .iter()
                .flat_map(|p| {
                    let ty = p["kind"].as_str().unwrap_or("").to_string();
                    alloc[p["pid"].to_string()]["allocators"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |a| (ty.clone(), a["path"].as_str().unwrap_or("").to_string(), a["effective"].as_f64().unwrap_or(0.0), a["size"].as_f64().unwrap_or(0.0)))
                })
                .filter(|r| !r.1.starts_with("tracing/"))
                .collect();
            rows.sort_by(|a, b| b.2.total_cmp(&a.2));
            text += "  effective        size  process           allocator\n";
            for (ty, path, e, s) in rows.iter().take(top) {
                text += &format!("  {:>9}  {:>10}  {:<16}  {path}\n", mb(*e), mb(*s), ty);
            }
            json!({ "census": label, "allocators": rows.iter().take(top).map(|(t, p, e, s)| json!({ "process": t, "path": p, "effective_bytes": e, "size_bytes": s })).collect::<Vec<_>>() })
        }
        _ => {
            text += "  process               pid      private  working set    allocators  unexplained\n";
            let mut rows = Vec::new();
            for p in &procs {
                let pid = p["pid"].to_string();
                let private = p["private_bytes"].as_f64().unwrap_or(f64::NAN);
                let tracked: f64 = layers(&pid).iter().filter(|(k, _)| k.as_str() != "shared_memory").map(|(_, v)| v).sum();
                let has = alloc[&pid].is_object();
                let ty = p["kind"].as_str().unwrap_or("other");
                let flag = if inflated && page_pid.as_deref() == Some(pid.as_str()) { " *" } else { "" };
                text += &format!(
                    "  {:<18} {:>6}  {:>11}  {:>11}  {:>12}  {:>11}{flag}\n",
                    ty,
                    p["pid"],
                    mb(private),
                    mb(p["working_set_bytes"].as_f64().unwrap_or(f64::NAN)),
                    if has { mb(tracked) } else { "—".into() },
                    if has { mb(private - tracked) } else { "—".into() }
                );
                rows.push(json!({ "pid": p["pid"], "type": ty, "strobe_own": ty == "tracing (strobe's)", "page": page_pid.as_deref() == Some(pid.as_str()), "name": p["name"], "private_bytes": private, "working_set_bytes": p["working_set_bytes"], "allocator_bytes": if has { json!(tracked) } else { Value::Null }, "unexplained_bytes": if has { json!(private - tracked) } else { Value::Null }, "layers": layers(&pid) }));
            }
            text += &format!(
                "page      JS heap {} used of {} · {} DOM nodes · {} documents · {} listeners\n",
                mb(c["js_heap_bytes"].as_f64().unwrap_or(f64::NAN)),
                mb(c["js_heap_total_bytes"].as_f64().unwrap_or(f64::NAN)),
                c["dom"]["nodes"],
                c["dom"]["documents"],
                c["dom"]["listeners"]
            );
            if inflated && page_pid.is_some() {
                text += "* read after a heap snapshot of this process, which raises its memory (CensusContaminated)\n";
            }
            json!({ "census": label, "processes": rows, "page": { "js_heap_bytes": c["js_heap_bytes"], "js_heap_total_bytes": c["js_heap_total_bytes"], "dom": c["dom"] }, "os_after_snapshot": inflated })
        }
    };
    Ok(Output::new(text, json))
}

/// A native recording's memory by what holds it (`memory.json`): one reading (`--at`, default the last).
fn native_memory(dir: &Path, meta: &Value, by: &str, top: usize, at: Option<&str>) -> Result<Output> {
    let (r, label) = reading_at(dir, at)?;
    let (text, json) = crate::probe::memory::view(&r, &label, if by == "process" { "process" } else { "layer" }, top, &meta["values"]);
    Ok(Output::new(text, json))
}

/// A native recording's memory reading by index (`--at 0`, negative from the end) or its last.
fn reading_at(dir: &Path, at: Option<&str>) -> Result<(Value, String)> {
    let all = store::read_json(&dir.join("memory.json"))?["readings"].as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return Err(Error::usage(format!("this recording has no memory reading: {}", crate::probe::memory::why_empty())));
    }
    let i = match at.map(|a| a.parse::<i64>()) {
        None => all.len() - 1,
        Some(Ok(i)) if i < 0 => all.len().saturating_sub(i.unsigned_abs() as usize),
        Some(Ok(i)) => (i as usize).min(all.len() - 1),
        Some(Err(_)) => return Err(Error::usage("--at: a reading index (0 is the first, -1 the last)")),
    };
    Ok((all[i].clone(), format!("reading {} of {}", i + 1, all.len())))
}

/// Memory of `b` against `a` by process type and layer, at each recording's last census (native
/// recordings: by what holds it, at each one's last reading).
pub fn memory_diff(a: &Path, b: &Path, top: usize) -> Result<Output> {
    let totals = |dir: &Path| -> Result<BTreeMap<String, f64>> {
        if dir.join("memory.json").exists() {
            return Ok(crate::probe::memory::totals(&reading_at(dir, None)?.0).into_iter().collect());
        }
        let (c, _) = census_at(dir, None)?;
        let mut m: BTreeMap<String, f64> = BTreeMap::new();
        for p in c["processes"].as_array().into_iter().flatten() {
            let ty = p["type"].as_str().unwrap_or("other");
            let ty = match ty.split_once(".mojom.") {
                // strobe's own tracing service (rule 2).
                Some(("tracing", _)) => continue,
                Some((service, _)) => service,
                None => ty,
            };
            *m.entry(format!("OS private · {ty}")).or_default() += p["private_bytes"].as_f64().unwrap_or(0.0);
            for (k, v) in c["allocators"][p["pid"].to_string()]["layers"].as_object().into_iter().flatten().filter(|(k, _)| k.as_str() != "tracing") {
                *m.entry(format!("{k} · {ty}")).or_default() += v.as_f64().unwrap_or(0.0);
            }
        }
        m.insert("JS heap · page".into(), c["js_heap_bytes"].as_f64().unwrap_or(0.0));
        Ok(m)
    };
    let (base, head) = (totals(a)?, totals(b)?);
    let mut rows: Vec<(String, f64, f64)> =
        base.keys().chain(head.keys()).collect::<std::collections::BTreeSet<_>>().into_iter().map(|k| (k.clone(), base.get(k).copied().unwrap_or(0.0), head.get(k).copied().unwrap_or(0.0))).collect();
    rows.sort_by(|x, y| (y.2 - y.1).abs().total_cmp(&(x.2 - x.1).abs()));
    let mut text = String::from("memory diff · last census of each · by layer and process type\n        base        head        Δ  layer · process\n");
    for (k, x, y) in rows.iter().take(top) {
        text += &format!("  {:>10}  {:>10}  {:>9}  {k}\n", fmt::bytes(*x), fmt::bytes(*y), format!("{}{}", if y < x { "−" } else { "+" }, fmt::bytes((y - x).abs())));
    }
    Ok(Output::new(text, json!({ "rows": rows.iter().map(|(k, x, y)| json!({ "layer": k, "base_bytes": x, "head_bytes": y, "delta_bytes": y - x })).collect::<Vec<_>>() })))
}

/// A recording's census by index (`--at 0`, negative from the end) or its last, with a label.
fn census_at(dir: &Path, at: Option<&str>) -> Result<(Value, String)> {
    let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
    let all = meta["census"].as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return Err(Error::usage("this recording has no census; record --probes memory, a bench leak run (--probes memory) or an attribution block takes one"));
    }
    let i = match at.map(|a| a.parse::<i64>()) {
        None => all.len() - 1,
        Some(Ok(i)) if i < 0 => all.len().saturating_sub(i.unsigned_abs() as usize),
        Some(Ok(i)) => (i as usize).min(all.len() - 1),
        Some(Err(_)) => return Err(Error::usage("--at: a census index (0 is the first, -1 the last)")),
    };
    Ok((all[i].clone(), format!("census {} of {}", i + 1, all.len())))
}

/// Leak suspects of a memory recording: objects allocated between its first and middle snapshots that
/// are still alive in the last, by class, with retainer paths (SPEC §8.6).
pub fn leaks(dir: &Path, top: usize, reveal: bool) -> Result<Output> {
    let s = |name: &str| dir.join(format!("{name}.heapsnapshot.gz"));
    if !s("heap-s0").exists() || !s("heap-s1").exists() {
        return Err(Error::usage("this recording has no leak snapshots; record with --probes memory"));
    }
    let v = crate::heapsnap::leaks(&crate::heapsnap::ids(&s("heap-s0"))?, &crate::heapsnap::ids(&s("heap-s1"))?, &crate::heapsnap::read(&s("heap-0"))?, top, reveal);
    Ok(Output::new(crate::heapsnap::leaks_text(&v, top), v))
}

/// The recording's heap snapshot: `heap-0.heapsnapshot.gz` (the last one), or an imported `.heapsnapshot`.
fn heap_file(dir: &Path) -> Result<std::path::PathBuf> {
    if dir.join("heap-0.heapsnapshot.gz").is_file() {
        return Ok(dir.join("heap-0.heapsnapshot.gz"));
    }
    [dir.to_path_buf(), dir.join("import")]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(".heapsnapshot")))
        .ok_or_else(|| Error::usage("this recording has no heap snapshot; bench attribution blocks and record --probes attribution|memory take one"))
}

fn sql_view(trace: &Path, sql: &str, limit: usize) -> Result<Output> {
    let t = engine::query(trace, sql)?;
    let shown = t.rows.len().min(limit);
    let mut text = t.columns.join(" | ") + "\n";
    for r in &t.rows[..shown] {
        text += &r.iter().map(|c| c.clone().unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join(" | ");
        text.push('\n');
    }
    if t.rows.len() > shown {
        text += &format!("… {} more rows (narrow the query or raise --top)\n", t.rows.len() - shown);
    }
    let rows: Vec<Value> = t.rows[..shown].iter().map(|r| json!(r)).collect();
    Ok(Output::new(text, json!({ "columns": t.columns, "rows": rows, "total": t.rows.len() })))
}

fn frames(trace: &Path, meta: &Value, top: usize) -> Result<Output> {
    let token = meta["token"].as_str().unwrap_or("");
    let data = metrics::read(trace, token)?;
    let values = metrics::block_values(&data, "run", &BTreeMap::new());
    let g = |k: &str| values.get(k).copied().unwrap_or(f64::NAN);
    let mut all: Vec<metrics::Vsync> = data
        .frames
        .iter()
        .filter(|(k, _)| k.contains(":run:") && !k.contains(":phase:"))
        .flat_map(|(_, f)| metrics::vsyncs(f))
        .filter(|s| matches!(s.outcome, metrics::Outcome::Presented | metrics::Outcome::Partial))
        .collect();
    let ms = metrics::run_frames(&data);
    let dropped = g("frames.dropped.pct");
    let q = |p: f64| metrics::percentile(&ms, p);
    let mut text = format!(
        "frames    {} presented · {} of vsyncs dropped · display {}\nlatency   p50 {} · p90 {} · p95 {} · p99 {} · max {}\ninterval  p50 {} · p95 {} · p99 {} · max {}\n",
        ms.len(),
        fmt::value(dropped, "%"),
        if g("display.hz").is_finite() { format!("{:.0} Hz", g("display.hz")) } else { "?".into() },
        fmt::value(q(0.5), "ms"),
        fmt::value(q(0.9), "ms"),
        fmt::value(q(0.95), "ms"),
        fmt::value(q(0.99), "ms"),
        fmt::value(*ms.last().unwrap_or(&f64::NAN), "ms"),
        fmt::value(g("frames.interval.p50"), "ms"),
        fmt::value(g("frames.interval.p95"), "ms"),
        fmt::value(g("frames.interval.p99"), "ms"),
        fmt::value(g("frames.interval.max"), "ms"),
    );
    let phases = metrics::phase_frames(&data);
    if !phases.is_empty() {
        text += "phases\n";
        for (name, v) in &phases {
            let mut v = v.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            text += &format!("  {:<14} {:>5} frames · p50 {} · p95 {}\n", name, v.len(), fmt::value(metrics::percentile(&v, 0.5), "ms"), fmt::value(metrics::percentile(&v, 0.95), "ms"));
        }
    }
    all.sort_by_key(|f| std::cmp::Reverse(f.cost));
    let t0 = data.windows.first().map(|w| w.t0).unwrap_or(0);
    text += "worst\n";
    let worst: Vec<Value> = all.iter().take(top.min(10)).map(|f| json!({ "at_ms": (f.ts - t0) as f64 / 1e6, "ms": f.cost as f64 / 1e6 })).collect();
    for w in &worst {
        text += &format!("  {:>9} at {:>8.1} ms\n", fmt::value(w["ms"].as_f64().unwrap_or(0.0), "ms"), w["at_ms"].as_f64().unwrap_or(0.0));
    }
    let phase_json: BTreeMap<&String, usize> = phases.iter().map(|(k, v)| (k, v.len())).collect();
    Ok(Output::new(
        text,
        json!({ "presented": ms.len(), "dropped_pct": dropped, "latency": { "p50": q(0.5), "p95": q(0.95), "p99": q(0.99) }, "values": values, "worst": worst, "phases": phase_json }),
    ))
}

/// The GPU process's work by name: its named tasks below the task loop on every thread (WebGPU command
/// decoding, shader and pipeline compiles from attribution traces, compositing), heaviest first. A shader
/// compile that froze a game's first frame for seconds showed here and nowhere in the page's own threads.
fn gpu(trace: &Path, top: usize) -> Result<Output> {
    let sql = format!(
        "select t.name as thread, s.name, count(*) as n, round(sum(s.dur) / 1e6, 1) as total_ms, round(max(s.dur) / 1e6, 1) as max_ms
         from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid) join process p using(upid)
         where p.name glob 'GPU*' and s.dur > 0 and s.depth between 1 and 4
           and s.name not in ('RunTask', 'ThreadControllerImpl::RunTask', 'ThreadPool_RunTask', 'SequenceManager::DoIdleWork')
         group by 1, 2 order by total_ms desc limit {top}"
    );
    sql_view(trace, &sql, top)
}

/// A recording without a trace (a native app, a process from outside): per adapter the target's
/// engine use and GPU memory from the OS's GPU statistics, the device's own counters, and what
/// reached the screen.
fn native_gpu(meta: &Value) -> Result<Output> {
    let os = if meta["os"].is_object() { &meta["os"] } else { &meta["gpu"] };
    if !os.is_object() {
        return Err(Error::usage("this recording has no GPU readings: they come from timing recordings (record, bench blocks kept with --keep or attribution)"));
    }
    let id = meta["id"].as_str().unwrap_or("");
    let mut text = String::from("gpu      the target's processes, from the OS's per-process GPU statistics, over the recording\n");
    if os["gpu_3d_pct"].is_null() && os["gpu_memory"].is_null() && os["vram_dedicated_mb"].is_null() {
        text += "         unavailable: no per-process GPU statistics opened on this machine (Windows D3DKMT, Linux DRM client statistics, macOS IORegistry)\n";
    }
    let engines = os["gpu_adapters"].as_object().cloned().unwrap_or_default();
    let memory = os["gpu_memory"].as_object().cloned().unwrap_or_default();
    let mut adapters: Vec<&String> = engines.keys().chain(memory.keys()).collect();
    adapters.sort();
    adapters.dedup();
    for a in &adapters {
        text += &format!("  {a}\n");
        if let Some(e) = engines.get(*a).and_then(Value::as_object) {
            let mut e: Vec<(&String, f64)> = e.iter().map(|(k, v)| (k, v.as_f64().unwrap_or(0.0))).collect();
            e.sort_by(|x, y| y.1.total_cmp(&x.1));
            text += &format!("    engines  {}   (% of one engine)\n", e.iter().map(|(k, v)| format!("{k} {}", fmt::value(*v, "%"))).collect::<Vec<_>>().join(" · "));
        }
        if let Some(m) = memory.get(*a) {
            let mb = |i: usize| fmt::bytes(m[i].as_f64().unwrap_or(f64::NAN) * 1048576.0);
            text += &format!("    memory   {} dedicated · {} shared (system memory)\n", mb(0), mb(1));
        }
    }
    if adapters.is_empty() && !os["gpu_3d_pct"].is_null() {
        text += &format!(
            "  no engine time or GPU memory on any adapter · VRAM {} · shared {}\n",
            fmt::value(os["vram_dedicated_mb"].as_f64().unwrap_or(f64::NAN), "MB"),
            fmt::value(os["vram_shared_mb"].as_f64().unwrap_or(f64::NAN), "MB")
        );
    }
    if let Some(u) = os["gpu_unreadable_pids"].as_array().filter(|u| !u.is_empty()) {
        text += &format!("  unreadable: pids {} (left out of the sums)\n", u.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
    }
    let d = &os["device"];
    if d.is_object() {
        let f = |k: &str, unit: &str| d[k].as_f64().map(|v| format!("{} {}", k.split('_').next().unwrap_or(k), fmt::value(v, unit)));
        let parts: Vec<String> = [f("gpu_pct", "%"), f("mem_pct", "%"), f("gfx_mhz", "MHz"), f("power_w", "W"), f("temp_c", "°C"), f("vram_used_mb", "MB")].into_iter().flatten().collect();
        text += &format!("device   {}: {}   (the whole device, every process on it)\n", d["name"].as_str().unwrap_or("?"), parts.join(" · "));
    }
    let v = &meta["values"];
    match (v["frames.n"].as_f64(), meta["presents_refused"].as_str().or(meta["os_frames"]["refused"].as_str())) {
        (Some(n), _) => text += &format!("frames   {n:.0} on screen (OS present events): strobe query {id} frames\n"),
        (None, Some(why)) => text += &format!("frames   refused: {why}\n"),
        _ => {}
    }
    if meta["session_locked"] == true {
        text += "session  locked: the compositor drew nothing, so engine use describes no screen (GPU memory does not depend on it)\n";
    }
    text += "no trace: a browser GPU process's tasks by name need a Chromium attribution recording\n";
    Ok(Output::new(text, json!({ "gpu": os, "frames": v["frames.n"], "session_locked": meta["session_locked"] })))
}

/// A native recording's threads, busiest waker first: context switches per second over the measured
/// runs, CPU per run from each thread's cycle counter, and its name.
fn native_threads(dir: &Path, top: usize) -> Result<Output> {
    let rows = store::read_json(&dir.join("threads.json"))?;
    let all = rows.as_array().cloned().unwrap_or_default();
    let mut text = format!("{:>10}  {:>12}  {:>7}  {:>7}  {:<6}  thread (started in)\n", "wakes/s", "cpu ms/run", "pid", "tid", "whose");
    for r in all.iter().take(top) {
        let started = r["module"].as_str().map(|m| format!(" ({m})")).unwrap_or_default();
        text += &format!(
            "{:>10}  {:>12}  {:>7}  {:>7}  {:<6}  {}{started}\n",
            fmt::value(r["wakes_per_s"].as_f64().unwrap_or(f64::NAN), "count"),
            fmt::value(r["cpu_ms_per_run"].as_f64().unwrap_or(f64::NAN), "count"),
            r["pid"].as_u64().unwrap_or(0),
            r["tid"].as_u64().unwrap_or(0),
            r["origin"].as_str().unwrap_or("?"),
            r["name"].as_str().unwrap_or("(unnamed)")
        );
    }
    if all.len() > top {
        text += &format!("… {} more (--top)\n", all.len() - top);
    }
    let sum = |o: &str| all.iter().filter(|r| r["origin"] == o).map(|r| r["wakes_per_s"].as_f64().unwrap_or(0.0)).sum::<f64>();
    if all.iter().any(|r| r["origin"].is_string()) {
        text += &format!("by whose threads: app {} · driver {} · os {} (wakes/s)\n", fmt::value(sum("app"), "count"), fmt::value(sum("driver"), "count"), fmt::value(sum("os"), "count"));
    }
    text += "wakes: context switches of the thread, each a switch in to run after a wait or a preemption; whose: app (started in the app's own code), driver (a device driver's user-mode part, such as GPU driver workers), os (the OS's thread pool and services; a pool thread may run the app's callbacks). A thread that exited during a run is not listed\n";
    Ok(Output::new(text, json!({ "threads": all.into_iter().take(top).collect::<Vec<_>>() })))
}

/// A native recording's frames: per measured run, what reached the screen (OS present events), the
/// inputs strobe gave the app, and each input's latency to the frame that showed it (SPEC §7.4).
fn native_frames(dir: &Path, top: usize) -> Result<Output> {
    let f = store::read_json(&dir.join("frames.json")).map_err(|_| Error::usage("this native recording has no frames: they come from bench blocks of a native target (--keep, or attribution)"))?;
    let presents: crate::etw::Presents = {
        let path = dir.join("presents.json.gz");
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(std::fs::File::open(&path).map_err(|_| Error::usage("this recording has no OS present events (no access, or a block without a window)"))?),
            &mut text,
        )?;
        serde_json::from_str(&text)?
    };
    let pids: Vec<u32> = f["pids"].as_array().into_iter().flatten().filter_map(|p| p.as_u64().map(|p| p as u32)).collect();
    let refresh = crate::etw::frames::refresh_ms(&presents.vsyncs, presents.t0_ns, presents.t1_ns, None);
    let quiet = crate::etw::frames::quiet_ns(refresh.unwrap_or(1000.0 / 60.0));
    let mut text = String::new();
    let mut runs_json = Vec::new();
    let mut slow: Vec<(f64, String, &'static str)> = Vec::new();
    for (run, r) in f["runs"].as_object().into_iter().flatten() {
        let (t0, t1) = (r["t0_ns"].as_u64().unwrap_or(0), r["t1_ns"].as_u64().unwrap_or(0));
        let shown = crate::etw::frames::from_presents(&presents.presents, &pids, t0, t1);
        let mut screens: Vec<u64> = shown.shown.iter().map(|s| s.screen_ns).collect();
        screens.sort_unstable();
        let mut gaps: Vec<f64> = screens.windows(2).map(|w| (w[1] - w[0]) as f64 / 1e6).collect();
        gaps.sort_by(f64::total_cmp);
        let inputs = r["inputs"].as_array().cloned().unwrap_or_default();
        let kind = |k: &str| inputs.iter().filter(|i| i["kind"] == k).count();
        let sent: Vec<crate::etw::frames::Sent> = inputs
            .iter()
            .filter(|i| i["kind"] == "discrete" || (i["kind"] == "scroll" && i["first"] == true))
            .map(|i| crate::etw::frames::Sent { t_ns: i["t_ns"].as_u64().unwrap_or(0), scroll: i["kind"] == "scroll" })
            .collect();
        let changes: Vec<u64> = r["changes"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
        let got = crate::etw::frames::reactions(&sent, &changes, &presents.presents, &pids, quiet);
        slow.extend(got.interaction.iter().map(|ms| (*ms, run.clone(), "input")));
        slow.extend(got.scroll.iter().map(|ms| (*ms, run.clone(), "scroll")));
        text += &format!(
            "{run:<8} {:>5} frames on screen · {} discarded · interval p50 {} · p95 {} · inputs {} ({} pointer, {} action) · latency p50 {}{}\n",
            shown.shown.len(),
            shown.discarded,
            fmt::value(metrics::percentile(&gaps, 0.5), "ms"),
            fmt::value(metrics::percentile(&gaps, 0.95), "ms"),
            kind("discrete") + kind("scroll"),
            kind("pointer"),
            kind("pattern"),
            fmt::value(
                {
                    let mut v = got.interaction.clone();
                    v.sort_by(f64::total_cmp);
                    metrics::percentile(&v, 0.5)
                },
                "ms"
            ),
            if got.unchanged + got.busy.0 + got.unshown.0 > 0 {
                format!(" · not counted: {} changed no tree, {} came mid-drawing, {} showed no frame", got.unchanged, got.busy.0, got.unshown.0)
            } else {
                String::new()
            },
        );
        runs_json.push(json!({ "run": run, "frames": shown.shown.len(), "discarded": shown.discarded, "interval_p95": metrics::percentile(&gaps, 0.95), "latencies_ms": got.interaction, "scroll_latencies_ms": got.scroll, "unchanged": got.unchanged, "busy": got.busy, "unshown": got.unshown }));
    }
    slow.sort_by(|a, b| b.0.total_cmp(&a.0));
    text += "slowest inputs (posted to the screen time of the frame that showed them)\n";
    for (ms, run, k) in slow.iter().take(top.min(10)) {
        text += &format!("  {:>9}  {run} {k}\n", fmt::value(*ms, "ms"));
    }
    text += "an input counts when the app changed its accessibility tree after it and was not drawing already (SPEC §7.4)\n";
    Ok(Output::new(text, json!({ "runs": runs_json, "refresh_ms": refresh })))
}
fn threads(trace: &Path, top: usize) -> Result<Output> {
    let sql = format!(
        "select p.name as process, p.pid, t.name as thread, count(*) as tasks, round(sum(s.dur) / 1e6, 1) as busy_ms, round(max(s.dur) / 1e6, 1) as max_ms
         from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid) left join process p using(upid)
         where s.depth = 0 and s.dur > 0 group by 1, 2, 3 order by busy_ms desc limit {top}"
    );
    sql_view(trace, &sql, top)
}

/// Self time per function, file or package from the recording's CPU profiles (attribution only).
pub fn hotspots(dir: &Path, by: &str, top: usize) -> Result<Output> {
    let profiles: Vec<_> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "cpuprofile")).collect();
    if profiles.is_empty() && dir.join("deep.json.gz").is_file() {
        return native_hotspots(dir, by, top, None, None);
    }
    if profiles.is_empty() {
        return Err(Error::usage("this recording has no CPU profile; record with --probes attribution"));
    }
    let sym = Symbolizer::open(dir);
    let mut weights: Vec<(Vec<Frame>, f64, u64)> = Vec::new();
    for p in profiles {
        let prof = store::read_json(&p)?;
        let nodes = prof["nodes"].as_array().cloned().unwrap_or_default();
        let by_id: BTreeMap<u64, &Value> = nodes.iter().filter_map(|n| Some((n["id"].as_u64()?, n))).collect();
        let parent: BTreeMap<u64, u64> = nodes
            .iter()
            .flat_map(|n| {
                let id = n["id"].as_u64().unwrap_or(0);
                n["children"].as_array().into_iter().flatten().filter_map(move |c| Some((c.as_u64()?, id)))
            })
            .collect();
        let frames: BTreeMap<u64, Frame> = by_id.iter().map(|(id, n)| (*id, sym.call_frame(&n["callFrame"]))).collect();
        // Per profile node: µs and samples (a sample's time is the delta to the next sample).
        let mut at: BTreeMap<u64, (f64, u64)> = BTreeMap::new();
        let deltas = prof["timeDeltas"].as_array().cloned().unwrap_or_default();
        for (i, s) in prof["samples"].as_array().into_iter().flatten().enumerate() {
            let Some(id) = s.as_u64() else { continue };
            let e = at.entry(id).or_default();
            e.0 += deltas.get(i + 1).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            e.1 += 1;
        }
        weights.extend(at.into_iter().map(|(id, (us, n))| (chain(id, &frames, &parent), us, n)));
    }
    let header = format!("hotspots by {by} · self time · {{samples}} samples at {} µs{{mapped}}", crate::probe::cpu::INTERVAL_US);
    let mut out = rank(weights, by, top, &header, |us| fmt::value(us / 1000.0, "ms"));
    out.json["interval_us"] = json!(crate::probe::cpu::INTERVAL_US);
    out.json["total_ms"] = json!(out.json["total"].as_f64().unwrap_or(0.0) / 1000.0);
    for r in out.json["rows"].as_array_mut().into_iter().flatten() {
        r["self_ms"] = json!(r["weight"].as_f64().unwrap_or(0.0) / 1000.0);
    }
    Ok(out)
}

/// Self time per function, file or package from the recording's native samples (the kernel's
/// sampled profile, symbolized from PDBs), in the shape of `hotspots`. `thread`: a thread name or id.
/// `phase`: only samples inside that phase of each measured run (`phase(name, fn)` in the bench file).
pub fn native_hotspots(dir: &Path, by: &str, top: usize, thread: Option<&str>, phase: Option<&str>) -> Result<Output> {
    let spans = phase.map(|p| phase_spans(dir, p)).transpose()?;
    let n = crate::symbol::native(dir, thread, spans.as_deref())?.ok_or_else(|| Error::usage("this recording has no native samples; record with --probes deep"))?;
    let share = if n.samples > 0 { 100.0 * n.resolved as f64 / n.samples as f64 } else { 0.0 };
    let header = format!("native hotspots by {by} · self time · {{samples}} samples at {} µs · {share:.1}% resolved{{mapped}}", n.interval_us);
    let interval = n.interval_us;
    let facts = crate::symbol::summary(&n);
    let mut out = rank(n.weights, by, top, &header, |us| fmt::value(us / 1000.0, "ms"));
    out.json["native"] = json!(true);
    out.json["interval_us"] = json!(interval);
    out.json["total_ms"] = json!(out.json["total"].as_f64().unwrap_or(0.0) / 1000.0);
    out.json["resolved_samples"] = facts["resolved_samples"].clone();
    out.json["resolved_share"] = facts["resolved_share"].clone();
    out.json["unsymbolized"] = facts["unsymbolized"].clone();
    for r in out.json["rows"].as_array_mut().into_iter().flatten() {
        r["self_ms"] = json!(r["weight"].as_f64().unwrap_or(0.0) / 1000.0);
    }
    if share < 95.0 {
        let missing: Vec<String> = facts["unsymbolized"].as_array().into_iter().flatten().take(3).map(|m| format!("{} ({})", m["module"].as_str().unwrap_or(""), m["samples"])).collect();
        out.text += &format!("! Unresolved: {:.1}% of samples have no symbol (no PDB found): {}\n", 100.0 - share, missing.join(", "));
    }
    Ok(out)
}

/// The host-clock spans of a phase in a native recording's measured runs, from its marks.
fn phase_spans(dir: &Path, phase: &str) -> Result<Vec<(u64, u64)>> {
    let f = store::read_json(&dir.join("frames.json")).map_err(|_| Error::usage("--phase needs a native block's recording (its frames.json holds the phase marks)"))?;
    let mut out = Vec::new();
    for r in f["runs"].as_object().into_iter().flatten().map(|(_, r)| r) {
        let marks: Vec<(String, u64)> = r["marks"].as_array().into_iter().flatten().filter_map(|m| Some((m[0].as_str()?.to_string(), m[1].as_u64()?))).collect();
        let at = |edge: &str| marks.iter().find(|(n, _)| n.ends_with(&format!(":phase:{phase}:{edge}"))).map(|m| m.1);
        if let (Some(a), Some(b)) = (at("start"), at("end")) {
            out.push((a, b));
        }
    }
    if out.is_empty() {
        return Err(Error::usage(format!("no phase {phase:?} in this recording's runs")));
    }
    Ok(out)
}

/// Bytes allocated per function, file or package from the recording's sampling heap profiles
/// (attribution only): who allocates, with full stacks, including objects already collected.
pub fn alloc(dir: &Path, by: &str, top: usize) -> Result<Output> {
    let profiles: Vec<_> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "heapprofile")).collect();
    if profiles.is_empty() {
        return Err(Error::usage("this recording has no allocation profile; record with --probes attribution"));
    }
    let sym = Symbolizer::open(dir);
    let mut weights: Vec<(Vec<Frame>, f64, u64)> = Vec::new();
    for p in profiles {
        let prof = store::read_json(&p)?;
        let mut hits: BTreeMap<u64, u64> = BTreeMap::new();
        for s in prof["samples"].as_array().into_iter().flatten() {
            *hits.entry(s["nodeId"].as_u64().unwrap_or(0)).or_default() += 1;
        }
        // Flatten the call tree once (each node's frame and parent); a stack is built only for nodes
        // where bytes were sampled (`selfSize`), not copied down every branch.
        let (mut frames, mut parent, mut sampled) = (BTreeMap::new(), BTreeMap::new(), Vec::new());
        let mut todo: Vec<(&Value, Option<u64>)> = prof.get("head").map(|h| (h, None)).into_iter().collect();
        while let Some((n, up)) = todo.pop() {
            let id = n["id"].as_u64().unwrap_or(0);
            frames.insert(id, sym.call_frame(&n["callFrame"]));
            if let Some(u) = up {
                parent.insert(id, u);
            }
            let bytes = n["selfSize"].as_f64().unwrap_or(0.0);
            if bytes > 0.0 {
                sampled.push((id, bytes));
            }
            todo.extend(n["children"].as_array().into_iter().flatten().map(|c| (c, Some(id))));
        }
        weights.extend(sampled.into_iter().map(|(id, bytes)| (chain(id, &frames, &parent), bytes, hits.get(&id).copied().unwrap_or(1))));
    }
    let header = format!("allocations by {by} · sampled every {} KB on average · {{samples}} samples{{mapped}}", crate::probe::alloc::INTERVAL_BYTES / 1024);
    let mut out = rank(weights, by, top, &header, fmt::bytes);
    out.json["interval_bytes"] = json!(crate::probe::alloc::INTERVAL_BYTES);
    Ok(out)
}

/// Forced (synchronous) layouts and style recalculations in the measured runs, by the JavaScript call
/// that forced them (attribution traces carry their stacks): reading geometry after a DOM write makes
/// the browser lay out mid-script instead of once per frame.
pub fn layout(dir: &Path, top: usize) -> Result<Output> {
    let meta = attribution_meta(dir)?;
    let sql = format!(
        "{}
select k, null, null, null, null, null from _w
union all
select w.k, s.name, s.ts, s.dur, t.tid,
  (select group_concat(a.key || char(31) || coalesce(a.display_value, ''), char(30)) from args a
     where a.arg_set_id = s.arg_set_id and a.key glob 'debug.beginData.stackTrace*') as stack
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name in ('Layout', 'UpdateLayoutTree') and t.name = 'CrRendererMain'
  order by 5, 3",
        windows_sql(meta["token"].as_str().unwrap_or(""))
    );
    let table = engine::query(&dir.join("trace.pftrace"), &sql)?;
    let measured = measured_runs(&meta, &table.rows);
    let sym = Symbolizer::open(dir);
    let mut sites: BTreeMap<(String, i64, i64, String), Frame> = BTreeMap::new();
    let mut weights: Vec<(Vec<Frame>, f64, u64)> = Vec::new();
    // [forced, in the frame lifecycle] × [layout, style]: count and ms.
    let mut totals = [[(0u64, 0.0f64); 2]; 2];
    let mut outer_end: BTreeMap<String, i64> = BTreeMap::new();
    for row in table.rows.iter().filter(|r| r.get(1).is_some_and(Option::is_some)) {
        let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
        if !measured.contains(&cell(0)) {
            continue;
        }
        let (ts, dur) = (cell(2).parse::<i64>().unwrap_or(0), cell(3).parse::<i64>().unwrap_or(0));
        // Count only outermost events: nested ones are already inside their parent's time.
        let end = outer_end.entry(cell(4)).or_insert(i64::MIN);
        if ts < *end {
            continue;
        }
        *end = ts + dur;
        let stack = trace_stack(&cell(5));
        let kind = usize::from(cell(1) == "UpdateLayoutTree");
        let lifecycle = usize::from(stack.is_empty());
        totals[lifecycle][kind].0 += 1;
        totals[lifecycle][kind].1 += dur as f64 / 1e6;
        if stack.is_empty() {
            continue;
        }
        // Trace stacks are innermost first with 1-based lines and columns; `rank` wants root first.
        let frames: Vec<Frame> = stack.into_iter().rev().map(|f| sites.entry(f.clone()).or_insert_with(|| sym.call_site(&f.0, f.1 - 1, f.2 - 1, &f.3)).clone()).collect();
        weights.push((frames, dur as f64 / 1e3, 1));
    }
    let runs = measured.len().max(1) as f64;
    let per_run = |(n, ms): (u64, f64)| format!("{:.0} ({}) per run", n as f64 / runs, fmt::value(ms / runs, "ms"));
    let header = format!(
        "forced layout and style by the call that forced them · {} measured runs{{mapped}}\n  forced     layout {} · style {}\n  in frames  layout {} · style {}",
        measured.len(),
        per_run(totals[0][0]),
        per_run(totals[0][1]),
        per_run(totals[1][0]),
        per_run(totals[1][1]),
    );
    let mut out = rank(weights, "function", top, &header, |us| fmt::value(us / 1000.0, "ms"));
    let summary = |(n, ms): (u64, f64)| json!({ "count": n, "ms": ms, "count_per_run": n as f64 / runs, "ms_per_run": ms / runs });
    out.json["runs"] = json!(measured.len());
    out.json["forced"] = json!({ "layout": summary(totals[0][0]), "style": summary(totals[0][1]) });
    out.json["lifecycle"] = json!({ "layout": summary(totals[1][0]), "style": summary(totals[1][1]) });
    for r in out.json["rows"].as_array_mut().into_iter().flatten() {
        r["ms"] = json!(r["weight"].as_f64().unwrap_or(0.0) / 1000.0);
    }
    Ok(out)
}

/// Garbage collection in the measured runs: collections, the main-thread pause they cost (incremental
/// marking steps included) and the work they did on helper threads, from attribution traces.
pub fn gc(dir: &Path) -> Result<Output> {
    let meta = attribution_meta(dir)?;
    let sql = format!(
        "{}
select k, null, null, null, null, null, null from _w
union all
select w.k, s.name, s.ts, s.dur, t.tid, t.name = 'CrRendererMain' as main,
  extract_arg(s.arg_set_id, 'debug.usedHeapSizeBefore') - extract_arg(s.arg_set_id, 'debug.usedHeapSizeAfter') as freed
  from slice s join thread_track tt on s.track_id = tt.id join thread t using(utid)
  join _w w on t.upid = w.upid and s.ts >= w.t0 and s.ts < w.t1
  where s.name in ('MinorGC', 'MajorGC') or s.name glob 'V8.GC*' or s.name glob 'BlinkGC*'
  order by 5, 3",
        windows_sql(meta["token"].as_str().unwrap_or(""))
    );
    let table = engine::query(&dir.join("trace.pftrace"), &sql)?;
    let measured = measured_runs(&meta, &table.rows);
    #[derive(Default, Clone, Copy)]
    struct Run {
        minor: u64,
        major: u64,
        main_ms: f64,
        helper_ms: f64,
        freed: f64,
    }
    let mut runs: BTreeMap<String, Run> = measured.iter().map(|k| (k.clone(), Run::default())).collect();
    // Union of GC intervals per thread: GC phases nest inside their collection.
    let mut covered: BTreeMap<String, i64> = BTreeMap::new();
    for row in table.rows.iter().filter(|r| r.get(1).is_some_and(Option::is_some)) {
        let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
        let Some(run) = runs.get_mut(&cell(0)) else { continue };
        let (ts, dur) = (cell(2).parse::<i64>().unwrap_or(0), cell(3).parse::<i64>().unwrap_or(0));
        match cell(1).as_str() {
            "MinorGC" => run.minor += 1,
            "MajorGC" => run.major += 1,
            _ => {}
        }
        run.freed += cell(6).parse::<f64>().unwrap_or(0.0).max(0.0);
        let end = covered.entry(cell(4)).or_insert(i64::MIN);
        let new = ((ts + dur) - ts.max(*end)).max(0) as f64 / 1e6;
        *end = (*end).max(ts + dur);
        if cell(5) == "1" {
            run.main_ms += new;
        } else {
            run.helper_ms += new;
        }
    }
    let med = |f: fn(&Run) -> f64| metrics::median(&runs.values().map(f).collect::<Vec<_>>());
    let values = json!({
        "minor_per_run": med(|r| r.minor as f64),
        "major_per_run": med(|r| r.major as f64),
        "main_ms_per_run": med(|r| r.main_ms),
        "helper_ms_per_run": med(|r| r.helper_ms),
        "freed_bytes_per_run": med(|r| r.freed),
    });
    let g = |k: &str| values[k].as_f64().unwrap_or(f64::NAN);
    let mut text = format!(
        "garbage collection · median of {} measured runs\n  collections   {:.0} minor · {:.0} major per run\n  main thread   {} per run (pauses and incremental marking)\n  helpers       {} per run (concurrent and parallel work)\n  freed         {} per run\n",
        runs.len(),
        g("minor_per_run"),
        g("major_per_run"),
        fmt::value(g("main_ms_per_run"), "ms"),
        fmt::value(g("helper_ms_per_run"), "ms"),
        fmt::bytes(g("freed_bytes_per_run")),
    );
    text += "  per run       minor major   main ms  helper ms\n";
    let per_run: Vec<Value> = runs
        .iter()
        .map(|(k, r)| {
            text += &format!("  {:<12}  {:>5} {:>5}  {:>8.1}  {:>9.1}\n", k.rsplit(':').next().map(|i| format!("run {i}")).unwrap_or_default(), r.minor, r.major, r.main_ms, r.helper_ms);
            json!({ "run": k, "minor": r.minor, "major": r.major, "main_ms": r.main_ms, "helper_ms": r.helper_ms, "freed_bytes": r.freed })
        })
        .collect();
    Ok(Output::new(text, json!({ "values": values, "runs": per_run })))
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

fn attribution_meta(dir: &Path) -> Result<Value> {
    let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
    if meta["probes"] != "attribution" {
        return Err(Error::usage("this view needs an attribution recording (stacks and GC phases); record with --probes attribution"));
    }
    Ok(meta)
}

/// The window keys of the measured runs, from a result whose window rows (`_w`) have only their key
/// set: the last `runs` of the run windows (a block warms up first; its attribution profilers start
/// at the steady point).
fn measured_runs(meta: &Value, rows: &[Vec<Option<String>>]) -> Vec<String> {
    let mut keys: Vec<(usize, String)> =
        rows.iter().filter(|r| r.get(1).is_none_or(Option::is_none)).filter_map(|r| r.first().cloned().flatten()).filter_map(|k| Some((k.rsplit(':').next()?.parse().ok()?, k))).collect();
    keys.sort();
    keys.dedup();
    let n = meta["runs"].as_u64().map_or(keys.len(), |n| n as usize).min(keys.len());
    keys.split_off(keys.len() - n).into_iter().map(|k| k.1).collect()
}

/// Frames of a trace event's `beginData.stackTrace` from `key \x1f value` pairs joined by `\x1e`:
/// (url, line, column, function name), innermost first.
fn trace_stack(raw: &str) -> Vec<(String, i64, i64, String)> {
    let mut frames: BTreeMap<usize, (String, i64, i64, String)> = BTreeMap::new();
    for pair in raw.split('\u{1e}').filter(|p| !p.is_empty()) {
        let Some((key, value)) = pair.split_once('\u{1f}') else { continue };
        let Some((index, field)) = key.split_once('[').and_then(|(_, r)| r.split_once("].")) else { continue };
        let Ok(i) = index.parse::<usize>() else { continue };
        let f = frames.entry(i).or_default();
        match field {
            "url" => f.0 = value.to_string(),
            "lineNumber" => f.1 = value.parse().unwrap_or(0),
            "columnNumber" => f.2 = value.parse().unwrap_or(0),
            "functionName" => f.3 = value.to_string(),
            _ => {}
        }
    }
    frames.into_values().collect()
}

/// The stack of profile node `id`, root first, without the `(root)` frame.
fn chain(id: u64, frames: &BTreeMap<u64, Frame>, parent: &BTreeMap<u64, u64>) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut at = Some(id);
    while let Some(n) = at {
        if let Some(fr) = frames.get(&n).filter(|fr| fr.name != "(root)") {
            out.push(fr.clone());
        }
        at = parent.get(&n).copied();
    }
    out.reverse();
    out
}

/// Ranks weighted stacks (leaf = the frame that spent the time or allocated the bytes) by function, file
/// or package. Rows keep complete locations; by function, each row keeps its three heaviest full stacks
/// and the text prints the heaviest in full. `header` may use `{samples}` and `{mapped}`.
fn rank(weights: Vec<(Vec<Frame>, f64, u64)>, by: &str, top: usize, header: &str, show: impl Fn(f64) -> String) -> Output {
    type Stacks = BTreeMap<Vec<Frame>, (f64, u64)>;
    /// (name, where) → (weight, samples, stacks).
    type Row = ((String, String), (f64, u64, Stacks));
    let mut rows: BTreeMap<(String, String), (f64, u64, Stacks)> = BTreeMap::new();
    let (mut total, mut mapped, mut scripts) = (0.0, 0u64, 0u64);
    let samples: u64 = weights.iter().map(|w| w.2).sum();
    let mut packages: BTreeMap<String, String> = BTreeMap::new();
    let mut lines: BTreeMap<(String, String), BTreeMap<i64, f64>> = BTreeMap::new();
    for (stack, w, hits) in weights {
        let Some(f) = stack.last() else { continue };
        if f.mapped {
            mapped += hits;
        }
        if !f.url.is_empty() {
            scripts += hits;
        }
        let key = match by {
            "package" if f.package.is_some() => f.package.clone().unwrap_or_default(),
            "file" if f.url.is_empty() && !f.file.is_empty() => f.file.clone(),
            "file" | "package" if f.url.is_empty() => f.name.clone(),
            "file" => f.file.clone(),
            "package" => {
                let at = if f.mapped { &f.file } else { &f.url };
                packages.entry(at.clone()).or_insert_with(|| package(at)).clone()
            }
            _ => f.name.clone(),
        };
        // A native frame is located at the sampled line: its function's row is the function in its
        // file, and the lines it spent the time on are kept beside it.
        let native = f.package.is_some() && f.line > 0;
        let detail = match by {
            "function" if native => f.file.clone(),
            "function" => f.location(),
            _ => String::new(),
        };
        if native && by == "function" {
            *lines.entry((key.clone(), detail.clone())).or_default().entry(f.line).or_default() += w;
        }
        let e = rows.entry((key, detail)).or_insert((0.0, 0, Stacks::new()));
        e.0 += w;
        e.1 += hits;
        if by == "function" {
            let st = e.2.entry(stack).or_insert((0.0, 0));
            st.0 += w;
            st.1 += hits;
        }
        total += w;
    }
    let mut rows: Vec<Row> = rows.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let mapped_text = if mapped > 0 { format!(" · {:.0}% of script samples source-mapped", 100.0 * mapped as f64 / scripts.max(1) as f64) } else { String::new() };
    let mut text = header.replace("{samples}", &samples.to_string()).replace("{mapped}", &mapped_text) + "\n";
    let mut json_rows = Vec::new();
    let n = (samples as f64).max(1.0);
    for ((k, detail), (w, hits, row_stacks)) in rows.iter().take(top) {
        let share = if total > 0.0 { w / total } else { 0.0 };
        let ci = 1.96 * (share * (1.0 - share) / n).sqrt();
        text += &format!("  {:>9}  {:>5.1}% ±{:>4.2}  {}  {}\n", show(*w), share * 100.0, ci * 100.0, k, detail);
        let mut hot: Vec<(i64, f64)> = lines.get(&(k.clone(), detail.clone())).map(|l| l.iter().map(|(a, b)| (*a, *b)).collect()).unwrap_or_default();
        hot.sort_by(|a, b| b.1.total_cmp(&a.1));
        if !hot.is_empty() {
            text += &format!("             lines {}\n", hot.iter().take(4).map(|(l, x)| format!(":{l} {:.0}%", 100.0 * x / w.max(1e-9))).collect::<Vec<_>>().join(" · "));
        }
        let mut heavy: Vec<(&Vec<Frame>, &(f64, u64))> = row_stacks.iter().collect();
        heavy.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
        if let Some((stack, (sw, _))) = heavy.first().filter(|(s, _)| s.len() > 1) {
            text += &format!("             heaviest stack ({:.0}% of this row):\n", 100.0 * sw / w.max(1e-9));
            for fr in stack.iter().rev().skip(1) {
                text += &format!("               ← {}  {}\n", fr.name, fr.location());
            }
        }
        let stacks_json: Vec<Value> = heavy.iter().take(3).map(|(stack, (sw, sn))| json!({ "weight": sw, "samples": sn, "frames": stack.iter().map(Frame::json).collect::<Vec<_>>() })).collect();
        let mut row = json!({ "name": k, "weight": w, "samples": hits, "share": share, "share_ci": ci, "where": detail, "stacks": stacks_json });
        if !hot.is_empty() {
            row["line"] = json!(hot[0].0);
            row["lines"] = json!(hot.iter().take(10).map(|(l, x)| json!({ "line": l, "weight": x })).collect::<Vec<_>>());
        }
        json_rows.push(row);
    }
    Output::new(text, json!({ "by": by, "samples": samples, "script_samples": scripts, "mapped_samples": mapped, "total": total, "rows": json_rows }))
}

fn short_url(url: &str) -> String {
    if url.is_empty() {
        return "(native)".into();
    }
    let path = url.split("://").nth(1).map(|r| r.split_once('/').map(|x| x.1).unwrap_or(r)).unwrap_or(url);
    path.split('?').next().unwrap_or(path).to_string()
}

/// The package a script or file belongs to: the folder after the last `node_modules/` (with its scope),
/// else for a file on disk the `name` in its nearest `package.json`, else the URL's first path segment.
pub fn package(url: &str) -> String {
    if url.is_empty() {
        return "(native)".into();
    }
    let url = &url.replace('\\', "/");
    if let Some(i) = url.rfind("node_modules/") {
        let rest = &url[i + 13..];
        let mut parts = rest.split('/');
        let first = parts.next().unwrap_or("");
        return if first.starts_with('@') { format!("{first}/{}", parts.next().unwrap_or("")) } else { first.to_string() };
    }
    let declared = Path::new(url).ancestors().skip(1).take(12).find_map(|d| store::read_json(&d.join("package.json")).ok()?["name"].as_str().map(str::to_string));
    if let Some(name) = declared {
        return name;
    }
    // A file on disk outside any package: the app's own code.
    if url.starts_with('/') || url.as_bytes().get(1) == Some(&b':') {
        return "(app)".into();
    }
    let path = short_url(url);
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    match parts.next() {
        Some(scope) if scope.starts_with('@') => format!("{scope}/{}", parts.next().unwrap_or("")),
        Some(first) => first.to_string(),
        None => "(app)".into(),
    }
}

/// `strobe record file:<path>`: turns an existing capture into a recording (SPEC §8.7).
pub fn import(path: &str) -> Result<Output> {
    let src = Path::new(path);
    if !src.exists() {
        return Err(Error::usage(format!("{path} does not exist")));
    }
    let id = store::new_id("rec");
    let dir = store::recording_dir(&id)?;
    let mut kinds = Vec::new();
    let copy = |from: &Path, name: &str| -> Result<()> { std::fs::copy(from, dir.join(name)).map(|_| ()).map_err(Error::from) };
    if src.is_dir() {
        let manifest = src.join("manifest.json");
        for e in std::fs::read_dir(src)?.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".heapsnapshot") || name.ends_with(".json") {
                std::fs::create_dir_all(dir.join("import"))?;
                copy(&p, &format!("import/{name}"))?;
                kinds.push(name);
            }
        }
        if !manifest.exists() && kinds.is_empty() {
            return Err(Error::usage("the folder has nothing strobe can read (no manifest.json, heap snapshots or traces)"));
        }
    } else {
        let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.ends_with(".cpuprofile") {
            copy(src, "cpu-0.cpuprofile")?;
        } else if name.ends_with(".heapsnapshot") {
            std::fs::create_dir_all(dir.join("import"))?;
            copy(src, &format!("import/{name}"))?;
        } else {
            copy(src, "trace.pftrace")?;
        }
        kinds.push(name);
    }
    let meta = json!({ "id": id, "kind": "import", "target": format!("file:{path}"), "probes": "import", "token": "", "valid": "unknown", "files": kinds, "values": {}, "duration_ms": 0, "created": super::record::now_iso() });
    store::write_json(&dir.join("meta.json"), &meta)?;
    let text = format!(
        "{id} · imported {} · valid: unknown (nothing proves how it was taken)\nnext     strobe query {id} sql \"select name, count(*) from slice group by 1 order by 2 desc limit 20\"\n",
        path
    );
    Ok(Output::new(text, meta))
}
