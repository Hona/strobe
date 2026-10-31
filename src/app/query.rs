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
    if handle.starts_with("exp_") {
        let dir = store::resolve(handle)?;
        if !dir.join("result.json").exists() {
            let log = std::fs::read_to_string(dir.join("progress.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            let text = format!("{handle} is running\n{}\n", tail.join("\n"));
            return Ok(Output::new(text, json!({ "id": handle, "state": "running", "progress": tail })));
        }
        let result = store::read_json(&dir.join("result.json"))?;
        return match view {
            "summary" | "progress" => Ok(Output::new(result["text"].as_str().unwrap_or("").to_string(), result)),
            other => Err(Error::usage(format!("experiments support summary and progress; use a recording handle for {other} (see the result's recordings)"))),
        };
    }
    // `a..b`: two recordings compared (heap snapshots by class).
    if let Some((a, b)) = handle.split_once("..") {
        let (a, b) = (store::resolve(a)?, store::resolve(b)?);
        return match view {
            "heap" => heap_diff(&a, &b, top, args.has("reveal")),
            other => Err(Error::usage(format!("a..b supports heap, not {other}"))),
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
        "frames" => frames(&trace, &meta, top),
        "threads" => threads(&trace, top),
        "hotspots" => hotspots(&dir, args.get("by").unwrap_or("function"), top),
        "alloc" => alloc(&dir, args.get("by").unwrap_or("function"), top),
        "layout" => layout(&dir, top),
        "gc" => gc(&dir),
        "heap" => heap(&dir, top, args.has("reveal")),
        other => Err(Error::usage(format!("unknown view {other}: summary frames threads hotspots alloc layout gc heap sql"))),
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

/// The recording's heap snapshot: `heap-0.heapsnapshot.gz`, or an imported `.heapsnapshot`.
fn heap_file(dir: &Path) -> Result<std::path::PathBuf> {
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
    all.sort_by(|a, b| b.cost.cmp(&a.cost));
    let t0 = data.windows.first().map(|w| w.t0).unwrap_or(0);
    text += "worst\n";
    let worst: Vec<Value> = all.iter().take(top.min(10)).map(|f| json!({ "at_ms": (f.ts - t0) as f64 / 1e6, "ms": f.cost as f64 / 1e6 })).collect();
    for w in &worst {
        text += &format!("  {:>9} at {:>8.1} ms\n", fmt::value(w["ms"].as_f64().unwrap_or(0.0), "ms"), w["at_ms"].as_f64().unwrap_or(0.0));
    }
    let phase_json: BTreeMap<&String, usize> = phases.iter().map(|(k, v)| (k, v.len())).collect();
    Ok(Output::new(text, json!({ "presented": ms.len(), "dropped_pct": dropped, "latency": { "p50": q(0.5), "p95": q(0.95), "p99": q(0.99) }, "values": values, "worst": worst, "phases": phase_json })))
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
        // Depth-first over the call tree, carrying each node's stack; a node's selfSize is bytes sampled there.
        let mut todo: Vec<(&Value, Vec<Frame>)> = prof.get("head").map(|h| (h, Vec::new())).into_iter().collect();
        while let Some((n, mut stack)) = todo.pop() {
            let f = sym.call_frame(&n["callFrame"]);
            if f.name != "(root)" {
                stack.push(f);
            }
            let bytes = n["selfSize"].as_f64().unwrap_or(0.0);
            if bytes > 0.0 {
                weights.push((stack.clone(), bytes, hits.get(&n["id"].as_u64().unwrap_or(0)).copied().unwrap_or(1)));
            }
            for c in n["children"].as_array().into_iter().flatten() {
                todo.push((c, stack.clone()));
            }
        }
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
        let frames: Vec<Frame> = stack
            .into_iter()
            .rev()
            .map(|f| sites.entry(f.clone()).or_insert_with(|| sym.call_site(&f.0, f.1 - 1, f.2 - 1, &f.3)).clone())
            .collect();
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
    let mut keys: Vec<(usize, String)> = rows
        .iter()
        .filter(|r| r.get(1).is_none_or(Option::is_none))
        .filter_map(|r| r.first().cloned().flatten())
        .filter_map(|k| Some((k.rsplit(':').next()?.parse().ok()?, k)))
        .collect();
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
    let mut rows: BTreeMap<(String, String), (f64, u64, Stacks)> = BTreeMap::new();
    let (mut total, mut mapped) = (0.0, 0u64);
    let samples: u64 = weights.iter().map(|w| w.2).sum();
    let mut packages: BTreeMap<String, String> = BTreeMap::new();
    for (stack, w, hits) in weights {
        let Some(f) = stack.last() else { continue };
        if f.mapped {
            mapped += hits;
        }
        let key = match by {
            "file" | "package" if f.url.is_empty() => f.name.clone(),
            "file" => f.file.clone(),
            "package" => {
                let at = if f.mapped { &f.file } else { &f.url };
                packages.entry(at.clone()).or_insert_with(|| package(at)).clone()
            }
            _ => f.name.clone(),
        };
        let detail = if by == "function" { f.location() } else { String::new() };
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
    let mut rows: Vec<((String, String), (f64, u64, Stacks))> = rows.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let mapped_text = if mapped > 0 { format!(" · {:.0}% source-mapped", 100.0 * mapped as f64 / samples.max(1) as f64) } else { String::new() };
    let mut text = header.replace("{samples}", &samples.to_string()).replace("{mapped}", &mapped_text) + "\n";
    let mut json_rows = Vec::new();
    let n = (samples as f64).max(1.0);
    for ((k, detail), (w, hits, row_stacks)) in rows.iter().take(top) {
        let share = if total > 0.0 { w / total } else { 0.0 };
        let ci = 1.96 * (share * (1.0 - share) / n).sqrt();
        text += &format!("  {:>9}  {:>5.1}% ±{:>4.2}  {}  {}\n", show(*w), share * 100.0, ci * 100.0, k, detail);
        let mut heavy: Vec<(&Vec<Frame>, &(f64, u64))> = row_stacks.iter().collect();
        heavy.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
        if let Some((stack, (sw, _))) = heavy.first().filter(|(s, _)| s.len() > 1) {
            text += &format!("             heaviest stack ({:.0}% of this row):\n", 100.0 * sw / w.max(1e-9));
            for fr in stack.iter().rev().skip(1) {
                text += &format!("               ← {}  {}\n", fr.name, fr.location());
            }
        }
        let stacks_json: Vec<Value> = heavy.iter().take(3).map(|(stack, (sw, sn))| json!({ "weight": sw, "samples": sn, "frames": stack.iter().map(Frame::json).collect::<Vec<_>>() })).collect();
        json_rows.push(json!({ "name": k, "weight": w, "samples": hits, "share": share, "share_ci": ci, "where": detail, "stacks": stacks_json }));
    }
    Output::new(text, json!({ "by": by, "samples": samples, "mapped_samples": mapped, "total": total, "rows": json_rows }))
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
    let text = format!("{id} · imported {} · valid: unknown (nothing proves how it was taken)\nnext     strobe query {id} sql \"select name, count(*) from slice group by 1 order by 2 desc limit 20\"\n", path);
    Ok(Output::new(text, meta))
}
