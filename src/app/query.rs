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
    // Per row (key, where): self µs, samples, and its call stacks (root → leaf) with µs and samples.
    type Stacks = BTreeMap<Vec<Frame>, (f64, u64)>;
    let mut self_us: BTreeMap<(String, String), (f64, u64, Stacks)> = BTreeMap::new();
    let mut total = 0.0;
    let mut samples_total = 0u64;
    let mut mapped = 0u64;
    for p in profiles {
        let prof = store::read_json(&p)?;
        // `cpu-<n>.symbols.json` (source maps, SPEC §8.3): original names and locations by frame.
        let symbols = store::read_json(&p.with_extension("symbols.json")).unwrap_or(Value::Null);
        let nodes = prof["nodes"].as_array().cloned().unwrap_or_default();
        let by_id: BTreeMap<u64, &Value> = nodes.iter().filter_map(|n| Some((n["id"].as_u64()?, n))).collect();
        let parent: BTreeMap<u64, u64> = nodes.iter().flat_map(|n| {
            let id = n["id"].as_u64().unwrap_or(0);
            n["children"].as_array().into_iter().flatten().filter_map(move |c| Some((c.as_u64()?, id)))
        }).collect();
        let frames: BTreeMap<u64, Frame> = by_id.iter().map(|(id, n)| (*id, frame(&n["callFrame"], &symbols))).collect();
        let mut stacks: BTreeMap<u64, Vec<Frame>> = BTreeMap::new();
        let samples = prof["samples"].as_array().cloned().unwrap_or_default();
        let deltas = prof["timeDeltas"].as_array().cloned().unwrap_or_default();
        samples_total += samples.len() as u64;
        for (i, s) in samples.iter().enumerate() {
            let dt = deltas.get(i + 1).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            let Some(id) = s.as_u64() else { continue };
            let Some(f) = frames.get(&id) else { continue };
            if f.mapped {
                mapped += 1;
            }
            let key = match by {
                "file" | "package" if f.url.is_empty() => f.name.clone(),
                "file" => f.file.clone(),
                "package" if f.mapped => package(&f.file),
                "package" => package(&f.url),
                _ => f.name.clone(),
            };
            let detail = if by == "function" { f.location() } else { String::new() };
            let e = self_us.entry((key, detail)).or_insert((0.0, 0, Stacks::new()));
            e.0 += dt;
            e.1 += 1;
            if by == "function" {
                let stack = stacks.entry(id).or_insert_with(|| {
                    let mut chain = Vec::new();
                    let mut at = Some(id);
                    while let Some(n) = at {
                        if let Some(fr) = frames.get(&n).filter(|fr| fr.name != "(root)") {
                            chain.push(fr.clone());
                        }
                        at = parent.get(&n).copied();
                    }
                    chain.reverse();
                    chain
                });
                let st = e.2.entry(stack.clone()).or_insert((0.0, 0));
                st.0 += dt;
                st.1 += 1;
            }
            total += dt;
        }
    }
    let mut rows: Vec<((String, String), (f64, u64, Stacks))> = self_us.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let mut text = format!(
        "hotspots by {by} · self time · {samples_total} samples at {} µs{}\n",
        crate::probe::cpu::INTERVAL_US,
        if mapped > 0 { format!(" · {:.0}% of samples source-mapped", 100.0 * mapped as f64 / samples_total.max(1) as f64) } else { String::new() }
    );
    let mut json_rows = Vec::new();
    let n = (samples_total as f64).max(1.0);
    for ((k, detail), (us, hits, row_stacks)) in rows.iter().take(top) {
        let share = if total > 0.0 { us / total } else { 0.0 };
        let ci = 1.96 * (share * (1.0 - share) / n).sqrt();
        text += &format!("  {:>9}  {:>5.1}% ±{:>4.2}  {}  {}\n", fmt::value(us / 1000.0, "ms"), share * 100.0, ci * 100.0, k, detail);
        // The heaviest full call stacks into this function, leaf last; the text shows the first in full.
        let mut heavy: Vec<(&Vec<Frame>, &(f64, u64))> = row_stacks.iter().collect();
        heavy.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
        if let Some((stack, (sus, _))) = heavy.first().filter(|(s, _)| s.len() > 1) {
            text += &format!("             heaviest stack ({:.0}% of this row):\n", 100.0 * sus / us.max(1e-9));
            for fr in stack.iter().rev().skip(1) {
                text += &format!("               ← {}  {}\n", fr.name, fr.location());
            }
        }
        let stacks_json: Vec<Value> = heavy
            .iter()
            .take(3)
            .map(|(stack, (sus, sn))| json!({ "ms": sus / 1000.0, "samples": sn, "frames": stack.iter().map(Frame::json).collect::<Vec<_>>() }))
            .collect();
        json_rows.push(json!({ "name": k, "self_ms": us / 1000.0, "samples": hits, "share": share, "share_ci": ci, "where": detail, "stacks": stacks_json }));
    }
    Ok(Output::new(text, json!({ "by": by, "samples": samples_total, "interval_us": crate::probe::cpu::INTERVAL_US, "mapped_samples": mapped, "total_ms": total / 1000.0, "rows": json_rows })))
}

/// One profile frame, located as completely as strobe can: the original name, the absolute file on
/// disk when known (else the source path the map records, else the script URL), 1-based line and column.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Frame {
    name: String,
    file: String,
    line: i64,
    column: i64,
    url: String,
    mapped: bool,
    /// Where `name` came from: "map", "source" (the declaration on disk), or None (the profile's own).
    name_from: Option<String>,
}

impl Frame {
    /// `file:line:column`, or `(native)` for frames without a script.
    fn location(&self) -> String {
        if self.url.is_empty() {
            return "(native)".into();
        }
        match (self.line, self.column) {
            (l, c) if l > 0 && c > 0 => format!("{}:{l}:{c}", self.file),
            (l, _) if l > 0 => format!("{}:{l}", self.file),
            _ => self.file.clone(),
        }
    }

    fn json(&self) -> Value {
        json!({ "name": self.name, "name_from": self.name_from, "file": self.file, "line": self.line, "column": self.column, "url": self.url, "source_mapped": self.mapped })
    }
}

fn frame(f: &Value, symbols: &Value) -> Frame {
    let url = f["url"].as_str().unwrap_or("").to_string();
    let (line0, col0) = (f["lineNumber"].as_i64().unwrap_or(-1), f["columnNumber"].as_i64().unwrap_or(-1));
    let minified = f["functionName"].as_str().filter(|n| !n.is_empty()).unwrap_or("(anonymous)");
    let sym = &symbols[format!("{url}:{line0}:{col0}")];
    if sym.is_object() {
        return Frame {
            name: sym["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(minified).to_string(),
            file: sym["file"].as_str().or(sym["source"].as_str()).unwrap_or(&url).to_string(),
            line: sym["line"].as_i64().unwrap_or(0),
            column: sym["column"].as_i64().unwrap_or(0),
            url,
            mapped: true,
            name_from: sym["name_from"].as_str().map(str::to_string),
        };
    }
    Frame { name: minified.to_string(), file: url.clone(), line: line0 + 1, column: col0 + 1, url, mapped: false, name_from: None }
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
