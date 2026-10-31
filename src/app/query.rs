//! `strobe query`: views over recordings and experiments (SPEC §11.1).

use super::{Output, fmt};
use crate::cli::Args;
use crate::engine;
use crate::metrics;
use crate::model::{Error, Result};
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
        let result = store::read_json(&dir.join("result.json"))?;
        return match view {
            "summary" | "progress" => Ok(Output::new(result["text"].as_str().unwrap_or("").to_string(), result)),
            other => Err(Error::usage(format!("experiments support summary and progress; use a recording handle for {other} (see the result's recordings)"))),
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
        other => Err(Error::usage(format!("unknown view {other}: summary frames threads hotspots sql"))),
    }
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
    let mut self_us: BTreeMap<String, (f64, String)> = BTreeMap::new();
    let mut total = 0.0;
    let mut samples_total = 0u64;
    for p in profiles {
        let prof = store::read_json(&p)?;
        let nodes = prof["nodes"].as_array().cloned().unwrap_or_default();
        let by_id: BTreeMap<u64, &Value> = nodes.iter().filter_map(|n| Some((n["id"].as_u64()?, n))).collect();
        let samples = prof["samples"].as_array().cloned().unwrap_or_default();
        let deltas = prof["timeDeltas"].as_array().cloned().unwrap_or_default();
        samples_total += samples.len() as u64;
        for (i, s) in samples.iter().enumerate() {
            let dt = deltas.get(i + 1).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            let Some(node) = s.as_u64().and_then(|id| by_id.get(&id)) else { continue };
            let f = &node["callFrame"];
            let name = f["functionName"].as_str().filter(|n| !n.is_empty()).unwrap_or("(anonymous)");
            let url = f["url"].as_str().unwrap_or("");
            let line = f["lineNumber"].as_i64().unwrap_or(-1) + 1;
            let key = match by {
                "file" => short_url(url),
                "package" => package(url),
                _ => name.to_string(),
            };
            let detail = if by == "function" { format!("{}:{line}", short_url(url)) } else { String::new() };
            let e = self_us.entry(key).or_insert((0.0, detail));
            e.0 += dt;
            total += dt;
        }
    }
    let mut rows: Vec<(String, f64, String)> = self_us.into_iter().map(|(k, (v, d))| (k, v, d)).collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut text = format!("hotspots by {by} · self time · {samples_total} samples at 50 µs\n");
    let mut json_rows = Vec::new();
    for (k, us, detail) in rows.iter().take(top) {
        let share = if total > 0.0 { us / total } else { 0.0 };
        let n = (share * samples_total as f64).max(1.0);
        let ci = 1.96 * (share * (1.0 - share) / n.max(1.0)).sqrt();
        text += &format!("  {:>9}  {:>5.1}% ±{:>3.1}  {}  {}\n", fmt::value(us / 1000.0, "ms"), share * 100.0, ci * 100.0, k, detail);
        json_rows.push(json!({ "name": k, "self_ms": us / 1000.0, "share": share, "share_ci": ci, "where": detail }));
    }
    Ok(Output::new(text, json!({ "by": by, "samples": samples_total, "total_ms": total / 1000.0, "rows": json_rows })))
}

fn short_url(url: &str) -> String {
    if url.is_empty() {
        return "(native)".into();
    }
    let path = url.split("://").nth(1).map(|r| r.split_once('/').map(|x| x.1).unwrap_or(r)).unwrap_or(url);
    path.split('?').next().unwrap_or(path).to_string()
}

pub fn package(url: &str) -> String {
    if url.is_empty() {
        return "(native)".into();
    }
    if let Some(i) = url.rfind("node_modules/") {
        let rest = &url[i + 13..];
        let mut parts = rest.split('/');
        let first = parts.next().unwrap_or("");
        return if first.starts_with('@') { format!("{first}/{}", parts.next().unwrap_or("")) } else { first.to_string() };
    }
    let path = short_url(url);
    path.split('/').next().filter(|s| !s.is_empty()).unwrap_or("(app)").to_string()
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
