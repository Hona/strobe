//! `strobe report`: self-contained artifacts from views (SPEC §11.2).

use super::Output;
use super::query::{self, Rec};
use crate::cli::Args;
use crate::model::{Error, Result};
use crate::store;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn run(args: &Args) -> Result<Output> {
    let view = args.pos.first().ok_or_else(|| Error::usage("report <view> <handle…>: views experiment, recording"))?;
    if view == "history" {
        return super::history::report(args);
    }
    let handle = args.pos.get(1).ok_or_else(|| Error::usage("report needs a handle"))?;
    let dir = store::resolve(handle)?;
    let experiment = store::is_experiment(&dir);
    let mut data: Value = if experiment { store::read_json(&dir.join("result.json"))? } else { store::read_json(&dir.join("meta.json"))? };
    // `bench --blocks`: one build's values, nothing compared: the experiment report, its PR block, card
    // and badge are all about a comparison, so none of them is drawn for it.
    let compares = data["series"].as_array().is_none_or(|a| a.iter().any(|s| s["kind"] != "blocks"));
    if experiment && !compares {
        let format = args.get("format").unwrap_or("html");
        return Err(Error::usage(format!("{handle} measured one build (bench --blocks): it compares nothing, so there is no {format} report of it"))
            .hint(format!("strobe query {handle} shows its blocks and their median with an interval ({}); a tracked point shows in strobe history", dir.join("result.json").display())));
    }
    // A card or badge needs only the result: no views, no insights.
    if let Some(format @ ("card" | "badge")) = args.get("format") {
        if !experiment {
            return Err(Error::usage(format!("--format {format} draws an experiment's result: give an exp_… handle")));
        }
        return super::card::report(args, &dir, &data, format);
    }
    if experiment {
        // The rerun ledger as of now: experiments on the same variants since this one ran count too.
        if data["identity"].is_object() {
            data["ledger"] = store::ledger::for_experiment(data["id"].as_str().unwrap_or(""), data["created"].as_str().unwrap_or(""), &data["identity"]);
        }
        if let Some(i) = attach_series(&mut data) {
            data["insights"] = i;
        }
        // Two implementations: the journeys' scoreboard and each one's repeated p and Holm standing,
        // as the PR block and card read them (`family`), for the viewer to show the same.
        if super::family::implementations(&data) {
            let board = super::family::Board::of(&data);
            board.write(&mut data);
            if let Some(series) = data["series"].as_array_mut() {
                for (i, s) in series.iter_mut().enumerate() {
                    s["headline"] = json!(board.hero == Some(i));
                }
            }
        }
    } else {
        let rec = Rec::open(&dir);
        if let Some(a) = attribution(&rec, None) {
            data["attribution"] = a;
            if let Ok(i) = query::insights(&rec) {
                data["insights"] = i.json;
            }
        } else if data["probes"] == "memory" {
            if let Ok(i) = query::insights(&rec) {
                data["insights"] = i.json;
            }
            if let Ok(m) = query::memory(&dir, "process", 30, None) {
                data["memory"] = m.json;
            }
            if let Ok(m) = query::memory(&dir, "layer", 30, None) {
                data["memory_layers"] = m.json;
            }
            // A memory recording: its live heap at the end and the objects that outlived their half.
            if let Ok(o) = query::heap(&rec, 15, false) {
                data["heap"] = o.json;
            }
            if let Ok(o) = query::leaks(&rec, 15, false) {
                data["leaks"] = o.json;
            }
        }
    }
    let format = args.get("format").unwrap_or("html");
    if format == "pr" && !experiment {
        return Err(Error::usage("--format pr writes an experiment's results for a pull request: give an exp_… handle"));
    }
    let out = match args.get("out") {
        Some(p) => std::path::PathBuf::from(p),
        None => dir.join(format!("report-{view}{}", if format == "pr" { ".pr.md".to_string() } else { format!(".{format}") })),
    };
    if args.get("update-pr").is_some() && format != "pr" {
        return Err(Error::usage("--update-pr replaces an experiment's block in a pull request body: add --format pr"));
    }
    // A pull request gets the result card and per-series badges as images gh attaches; the block's
    // headings say the same in words, so a body without them (an older gh, a failed upload) still reads.
    let mut images = Vec::new();
    let mut plain = None;
    if args.get("update-pr").is_some() && !args.has("no-card") {
        plain = Some(super::pr::block(&data));
        let (card, files) = super::card::for_pr(&data, &dir)?;
        data["card"] = card;
        images = files;
    }
    let body = match format {
        "md" => markdown(&data),
        "pr" => super::pr::block(&data),
        _ => html(view, &data),
    };
    write_whole(&out, body.as_bytes())?;
    // The block between its markers in a pull request's body, the author's text around it untouched.
    if let Some(n) = args.get("update-pr") {
        // Only images the block names: gh appends any other attachment to the end of the body.
        let attach: Vec<std::path::PathBuf> = images.into_iter().filter(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| body.contains(&format!("./{f}")))).collect();
        let id = data["id"].as_str().unwrap_or(handle);
        let done = super::pr::update_pr(n, args.get("repo"), id, &body, args.has("force"), &attach, plain.as_deref())?;
        let path = out.display().to_string();
        let how = match done.images {
            0 => "as text".to_string(),
            k => format!("with {k} image{}", if k == 1 { "" } else { "s" }),
        };
        let text = if done.unchanged {
            format!("left {} as it was: it already holds this report of {id}; the report is also in {path}\n", done.url)
        } else {
            format!("updated {} with {id} ({how}, {}); the report is also in {path}\n", done.url, super::fmt::bytes(body.len() as f64))
        };
        return Ok(Output::new(text, json!({ "path": path, "bytes": body.len(), "pr": done.url, "images": done.images, "unchanged": done.unchanged, "seen": done.seen })));
    }
    if args.has("open") {
        open(&out.display().to_string());
    }
    let path = out.display().to_string();
    let size = super::fmt::bytes(body.len() as f64);
    // Markdown is for pasting: it goes to stdout, and the file's path to stderr.
    if format == "pr" || format == "md" {
        if !args.json() {
            eprintln!("strobe: wrote {path} · {size}");
        }
        return Ok(Output::new(body.clone(), json!({ "path": path, "bytes": body.len(), "markdown": body })));
    }
    Ok(Output::new(format!("{path} · {size}\n"), json!({ "path": path, "bytes": body.len() })))
}

/// Writes a report whole or not at all (`store::write_atomic`), so a reader never sees half a report.
pub(super) fn write_whole(path: &std::path::Path, body: &[u8]) -> Result<()> {
    store::write_atomic(path, body)
}

/// The "why" of each series, one series at a time: its attribution recordings (base and head) as the
/// report shows them, how the two sides' live heaps and memory differ, the diff flame, and the series'
/// insights. Each recording's sources load once for all of these, both sides at once; a series'
/// recordings are dropped before the next one loads. Returns the experiment's insights.
fn attach_series(data: &mut Value) -> Option<Value> {
    let plan_runs = data["plan"]["runs_per_block"].as_u64();
    let id = data["id"].clone();
    // Two implementations share no code: each side is read on its own, never diffed (SPEC §11.2).
    let implementations = super::family::implementations(data);
    let series = data["series"].as_array_mut()?;
    let mut insights = Vec::new();
    for s in series {
        let recs: Vec<(&str, Rec)> = ["base", "head"].into_iter().filter_map(|role| Some((role, Rec::resolve(s["attribution"][role].as_str()?).ok()?))).collect();
        let sides: Vec<(&str, Option<Value>)> = std::thread::scope(|scope| {
            let handles: Vec<_> =
                recs.iter().map(|(role, rec)| (*role, scope.spawn(move || if implementations { side_attribution(rec, plan_runs) } else { attribution(rec, plan_runs) }))).collect();
            handles.into_iter().map(|(role, h)| (role, h.join().ok().flatten())).collect()
        });
        let mut out = json!({});
        for (role, h) in sides {
            if let Some(h) = h {
                out[role] = h;
            }
        }
        if out.as_object().is_some_and(|o| !o.is_empty()) {
            s["hotspots"] = out;
        }
        if implementations {
            // The headline's words for this journey, as the PR block says them (`uses 62% less CPU time`).
            if s["primary"]["estimate"].is_number() {
                s["claim"] = json!(super::pr::claim(&s["primary"]).pr(false));
            }
            continue;
        }
        let [(_, a), (_, b)] = recs.as_slice() else { continue };
        if let Ok(d) = query::heap_diff(a, b, 15, false) {
            s["heap_diff"] = d.json;
        }
        // A native app's two launches by what holds their memory (host, GPU mapped, private, …).
        if a.dir.join("memory.json").exists()
            && b.dir.join("memory.json").exists()
            && let Ok(d) = query::memory_diff(&a.dir, &b.dir, 10)
        {
            s["memory_diff"] = d.json;
        }
        let tree = query::diff_tree(a, b, plan_runs);
        if tree.as_object().is_some_and(|o| !o.is_empty()) {
            s["diff_tree"] = tree;
        }
        if let Ok(i) = query::series_insights(a, b, plan_runs) {
            let mut j = i.json;
            j["series"] = s["label"].clone();
            insights.push(j);
        }
    }
    (!insights.is_empty()).then(|| json!({ "experiment": id, "series": insights }))
}

/// One attribution recording for a report: CPU by function and file, allocations by function and
/// file, forced layouts, GC, the live heap, run count and metric values.
fn attribution(rec: &Rec, plan_runs: Option<u64>) -> Option<Value> {
    // Every source this reads, at once: the trace's views in one pass, the profiles and the snapshot.
    rec.preload(&query::ATTRIBUTION_PARTS, true, true, true);
    // A native recording without native stacks still has its threads and memory.
    let native = rec.dir.join("threads.json").exists();
    let mut h = match query::hotspots_of(rec, "function", 50) {
        Ok(o) => o.json,
        Err(_) if native => json!({ "rows": [] }),
        Err(_) => return None,
    };
    if native && let Ok(o) = query::native_threads(&rec.dir, 15) {
        h["threads"] = o.json;
    }
    if let Ok(o) = query::hotspots_of(rec, "file", 25) {
        h["files"] = o.json["rows"].clone();
    }
    if let Ok(o) = query::alloc(rec, "function", 25) {
        h["alloc"] = o.json;
        if let Ok(f) = query::alloc(rec, "file", 25) {
            h["alloc"]["files"] = f.json["rows"].clone();
        }
    }
    if let Ok(o) = query::layout(rec, 25) {
        h["layout"] = o.json;
    }
    if let Ok(o) = query::gc(rec) {
        h["gc"] = o.json;
    }
    if let Ok(o) = query::heap(rec, 15, false) {
        h["heap"] = o.json;
    }
    if let Ok(o) = query::memory(&rec.dir, "process", 30, None) {
        h["memory"] = o.json;
    }
    if let Ok(o) = query::memory(&rec.dir, "layer", 30, None) {
        h["memory_layers"] = o.json;
    }
    let runs = rec.runs().or(plan_runs);
    // A display may abbreviate (the recording and `query` keep everything): three stacks for the top
    // rows, the heaviest one below them, and no `url` beside `file` in frames. Frames repeat across
    // stacks (a framework's are in most of them), so each side keeps one `frame_table` and stacks list
    // indices into it: lossless, and a fifth of the bytes.
    let mut table: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for key in ["rows", "files", "alloc.rows", "alloc.files", "layout.rows"] {
        let rows = key.split('.').try_fold(&mut h, |v, k| v.get_mut(k));
        for (i, row) in rows.and_then(Value::as_array_mut).into_iter().flatten().enumerate() {
            let Some(stacks) = row["stacks"].as_array_mut() else { continue };
            stacks.truncate(if i < 15 { 3 } else { 1 });
            for f in stacks.iter_mut().flat_map(|s| s["frames"].as_array_mut().into_iter().flatten()) {
                if let Some(o) = f.as_object_mut() {
                    o.remove("url");
                }
                let at = *seen.entry(f.to_string()).or_insert_with(|| {
                    table.push(f.clone());
                    table.len() - 1
                });
                *f = json!(at);
            }
        }
    }
    h["frame_table"] = json!(table);
    h["recording"] = json!(rec.id);
    if let Some(r) = runs {
        h["runs"] = json!(r);
    }
    if rec.meta["values"].is_object() {
        h["values"] = rec.meta["values"].clone();
    }
    Some(h)
}

/// One side of an implementation comparison on its own (SPEC §11.2), profiled with its own engine's
/// tools: what `attribution` keeps, its call tree (`tree`, the diff flame's shape with one value per
/// node) and `side`: CPU per run by process (a browser's roles, a native app's programs) and by
/// thread origin (native), self time by function and package, memory by part, and how much of its
/// samples were named. Per run where its samples carry times; untimed native samples (macOS
/// `sample`) cover the whole capture, so they are per recording and say so.
fn side_attribution(rec: &Rec, plan_runs: Option<u64>) -> Option<Value> {
    let mut h = attribution(rec, plan_runs)?;
    let tree = query::side_tree(rec, plan_runs);
    if tree.as_object().is_some_and(|o| !o.is_empty()) {
        h["tree"] = tree;
    }
    let native = h["native"] == true;
    let untimed = native && h["untimed"] == true;
    let runs = rec.runs().or(plan_runs).filter(|_| !untimed);
    let per = |weight_us: f64| weight_us / 1000.0 / runs.unwrap_or(1).max(1) as f64;
    let values = &rec.meta["values"];
    let v = |k: &str| values[k].as_f64().filter(|x| x.is_finite());
    let mut side = json!({
        "engine": if native || rec.dir.join("threads.json").exists() { "native" } else { "chromium" },
        "per_run": runs.is_some(), "runs": runs, "untimed": untimed,
        "cpu_ms_per_run": v("cpu.work"),
    });
    if native {
        side["profiler"] = json!("native stacks");
        side["resolved_share"] = h["resolved_share"].clone();
        side["samples"] = h["samples"].clone();
    } else if h["samples"].as_u64().unwrap_or(0) > 0 {
        side["profiler"] = json!("V8 CPU profile");
        let (scripts, mapped) = (h["script_samples"].as_f64().unwrap_or(0.0), h["mapped_samples"].as_f64().unwrap_or(0.0));
        side["mapped_share"] = if scripts > 0.0 { json!(mapped / scripts) } else { Value::Null };
        side["samples"] = h["samples"].clone();
    }
    // Self time by function, leaving out a profile's idle time and root.
    let functions: Vec<Value> = h["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| !matches!(r["name"].as_str(), Some("(idle)" | "(root)")))
        .take(12)
        .map(|r| json!({ "name": r["name"], "where": r["where"], "ms": per(r["weight"].as_f64().unwrap_or(0.0)) }))
        .collect();
    side["functions"] = json!(functions);
    let idle: f64 = h["rows"].as_array().into_iter().flatten().filter(|r| r["name"] == "(idle)").filter_map(|r| r["weight"].as_f64()).sum();
    if let Some(total) = h["total"].as_f64().filter(|t| *t > idle) {
        side["sampled_ms"] = json!(per(total - idle));
    }
    if let Ok(o) = query::hotspots_of(rec, "package", 12) {
        let rows: Vec<Value> =
            o.json["rows"].as_array().into_iter().flatten().filter(|r| r["name"] != "(idle)").map(|r| json!({ "name": r["name"], "ms": per(r["weight"].as_f64().unwrap_or(0.0)) })).collect();
        side["packages"] = json!(rows);
    }
    let threads: Vec<Value> = store::read_json(&rec.dir.join("threads.json")).ok().and_then(|t| t.as_array().cloned()).unwrap_or_default();
    if threads.is_empty() {
        // A browser's processes by role: each role's share of a core over the runs, as ms per run.
        if let Some(dur) = v("run.duration") {
            let mut rows: Vec<Value> = Vec::new();
            for (metric, name) in [("cpu.renderer", "renderer"), ("cpu.gpu", "GPU process"), ("cpu.browser", "browser")] {
                if let Some(pct) = v(metric) {
                    rows.push(json!({ "name": name, "ms": pct / 100.0 * dur }));
                }
            }
            let named: f64 = rows.iter().filter_map(|r| r["ms"].as_f64()).sum();
            if let Some(total) = v("cpu.app.total").map(|p| p / 100.0 * dur).filter(|t| !rows.is_empty() && t - named > 0.005 * t) {
                rows.push(json!({ "name": "other (utility, network, …)", "ms": total - named }));
            }
            side["processes"] = json!(rows);
        }
    } else {
        // A native app's processes by program, and its threads by whose code they started in.
        // Per pid: CPU per run, the app's threads by start module, and macOS's executable.
        type Process = (f64, BTreeMap<String, usize>, Option<String>);
        let mut by_pid: BTreeMap<u64, Process> = BTreeMap::new();
        let mut origins: BTreeMap<String, f64> = BTreeMap::new();
        for t in &threads {
            let ms = t["cpu_ms_per_run"].as_f64().unwrap_or(0.0);
            let e = by_pid.entry(t["pid"].as_u64().unwrap_or(0)).or_default();
            e.0 += ms;
            if let Some(m) = t["module"].as_str() {
                // macOS's per-process row names its executable; elsewhere the module the app's own
                // threads started in is its program.
                if t["tid"] == 0 {
                    e.2 = Some(m.to_string());
                } else if t["origin"] == "app" {
                    *e.1.entry(m.to_string()).or_default() += 1;
                }
            }
            if ms > 0.0 {
                *origins.entry(t["origin"].as_str().unwrap_or("unknown").to_string()).or_default() += ms;
            }
        }
        let mut procs: Vec<Value> = by_pid
            .into_iter()
            .filter(|(_, (ms, _, _))| *ms > 0.0)
            .map(|(pid, (ms, modules, exe))| {
                let program = exe.or_else(|| modules.into_iter().max_by_key(|(m, n)| (*n, std::cmp::Reverse(m.clone()))).map(|(m, _)| m));
                json!({ "name": program.map(|p| format!("{p} · {pid}")).unwrap_or_else(|| format!("pid {pid}")), "ms": ms })
            })
            .collect();
        procs.sort_by(|a, b| b["ms"].as_f64().unwrap_or(0.0).total_cmp(&a["ms"].as_f64().unwrap_or(0.0)));
        side["processes"] = json!(procs);
        side["origins"] = json!(["app", "driver", "os", "unknown"].iter().filter_map(|o| Some(json!({ "name": o, "ms": origins.get(*o)? }))).collect::<Vec<_>>());
    }
    // Memory by part: a native app's by what holds it (host, GPU mapped, …), a browser's by allocator
    // layer across its processes.
    if rec.dir.join("memory.json").exists() {
        if let Ok(o) = query::native_memory(&rec.dir, &rec.meta, "layer", 10, None) {
            let parts = crate::probe::memory::totals(&o.json["reading"]);
            let total = parts.iter().find(|p| p.0 == "private").map(|p| p.1);
            let rows: Vec<Value> = parts.iter().filter(|p| !matches!(p.0.as_str(), "private" | "working_set")).filter(|p| p.1 > 0.0).map(|(k, b)| json!({ "name": k, "bytes": b })).collect();
            side["memory"] = json!({ "total": { "name": "private", "bytes": total }, "parts": rows });
        }
    } else if h["memory_layers"].is_object() {
        let m = &h["memory_layers"];
        let rows: Vec<Value> = m["layers"].as_array().into_iter().flatten().take(10).map(|l| json!({ "name": l["layer"], "bytes": l["effective_bytes"] })).collect();
        side["memory"] = json!({ "total": { "name": "OS private", "bytes": m["os_private_bytes"] }, "parts": rows });
    }
    h["side"] = side;
    Some(h)
}

const TEMPLATE: &str = include_str!("../../viewer/report.html");

pub fn html(view: &str, data: &Value) -> String {
    let catalog: Vec<Value> = crate::model::METRICS
        .iter()
        .map(|m| json!({ "name": m.name, "unit": m.unit, "better": m.better, "kind": m.kind, "words": [m.words.0, m.words.1], "min_n": m.min_n, "about": m.about }))
        .collect();
    let payload = serde_json::to_string(&json!({ "view": view, "data": data, "metrics": catalog })).unwrap_or_default().replace("</", "<\\/");
    TEMPLATE.replace("/*STROBE_DATA*/null", &payload)
}

pub fn markdown(data: &Value) -> String {
    let text = data["text"].as_str().map(str::to_string).unwrap_or_else(|| super::record::summary_text(data));
    format!("```\n{}\n```\n", text.trim_end())
}

/// Opens a file or a URL (without `&`: cmd's `start` would split it) with the desktop's handler.
pub(super) fn open(p: &str) {
    let _ = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/c", "start", "", p]).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(p).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(p).spawn()
    };
}
