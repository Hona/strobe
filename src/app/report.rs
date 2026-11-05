//! `strobe report`: self-contained artifacts from views (SPEC §11.2).

use super::Output;
use super::query::{self, Rec};
use crate::cli::Args;
use crate::model::{Error, Result};
use crate::store;
use serde_json::{Value, json};

pub fn run(args: &Args) -> Result<Output> {
    let view = args.pos.first().ok_or_else(|| Error::usage("report <view> <handle…>: views experiment, recording"))?;
    if view == "history" {
        return super::history::report(args);
    }
    let handle = args.pos.get(1).ok_or_else(|| Error::usage("report needs a handle"))?;
    let dir = store::resolve(handle)?;
    let experiment = store::is_experiment(&dir);
    let mut data: Value = if experiment { store::read_json(&dir.join("result.json"))? } else { store::read_json(&dir.join("meta.json"))? };
    // `bench --blocks`: one build's values, nothing compared, so nothing for a PR, a card or a badge to say.
    let compares = data["series"].as_array().is_none_or(|a| a.iter().any(|s| s["kind"] != "blocks"));
    if let Some(format @ ("pr" | "card" | "badge")) = args.get("format").filter(|_| experiment && !compares) {
        return Err(Error::usage(format!("{handle} measured one build (bench --blocks): it compares nothing, so there is no {format} for it")).hint(format!("strobe report experiment {handle} draws its blocks (html); strobe history shows a tracked point")));
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
    let series = data["series"].as_array_mut()?;
    let mut insights = Vec::new();
    for s in series {
        let recs: Vec<(&str, Rec)> = ["base", "head"].into_iter().filter_map(|role| Some((role, Rec::resolve(s["attribution"][role].as_str()?).ok()?))).collect();
        let sides: Vec<(&str, Option<Value>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = recs.iter().map(|(role, rec)| (*role, scope.spawn(move || attribution(rec, plan_runs)))).collect();
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
