//! `strobe report`: self-contained artifacts from views (SPEC §11.2).

use super::Output;
use crate::cli::Args;
use crate::model::{Error, Result};
use crate::store;
use serde_json::{Value, json};

pub fn run(args: &Args) -> Result<Output> {
    let view = args.pos.first().ok_or_else(|| Error::usage("report <view> <handle…>: views experiment, recording"))?;
    let handle = args.pos.get(1).ok_or_else(|| Error::usage("report needs a handle"))?;
    let dir = store::resolve(handle)?;
    let mut data: Value = if handle.starts_with("exp_") { store::read_json(&dir.join("result.json"))? } else { store::read_json(&dir.join("meta.json"))? };
    if handle.starts_with("exp_") {
        attach_hotspots(&mut data);
    } else if let Some(a) = attribution(&dir, data["id"].as_str().unwrap_or(handle), None) {
        data["attribution"] = a;
    } else if data["probes"] == "memory" {
        // A memory recording: its live heap at the end and the objects that outlived their half.
        if let Ok(o) = super::query::heap(&dir, 15, false) {
            data["heap"] = o.json;
        }
        if let Ok(o) = super::query::leaks(&dir, 15, false) {
            data["leaks"] = o.json;
        }
    }
    let format = args.get("format").unwrap_or("html");
    let out = match args.get("out") {
        Some(p) => std::path::PathBuf::from(p),
        None => dir.join(format!("report-{view}.{format}")),
    };
    let body = match format {
        "md" => markdown(&data),
        _ => html(view, &data),
    };
    std::fs::write(&out, &body)?;
    if args.has("open") {
        open(&out);
    }
    let path = out.display().to_string();
    Ok(Output::new(format!("{path} · {}\n", super::fmt::bytes(body.len() as f64)), json!({ "path": path, "bytes": body.len() })))
}

/// The "why": each series' attribution recordings (base and head) as the report shows them, plus how
/// the two sides' live heaps differ by class.
fn attach_hotspots(data: &mut Value) {
    let plan_runs = data["plan"]["runs_per_block"].as_u64();
    let Some(series) = data["series"].as_array_mut() else { return };
    for s in series {
        let recs: Vec<(&str, String, std::path::PathBuf)> =
            ["base", "head"].into_iter().filter_map(|role| s["attribution"][role].as_str().and_then(|rec| Some((role, rec.to_string(), store::resolve(rec).ok()?)))).collect();
        // Both sides and their heap diff at once: each view is its own trace_processor or file read.
        let (sides, diff) = std::thread::scope(|scope| {
            let sides: Vec<_> = recs.iter().map(|(role, rec, dir)| (*role, scope.spawn(move || attribution(dir, rec, plan_runs)))).collect();
            let diff = match recs.as_slice() {
                [(_, _, a), (_, _, b)] => Some(scope.spawn(move || super::query::heap_diff(a, b, 15, false).ok())),
                _ => None,
            };
            let sides: Vec<(&str, Option<Value>)> = sides.into_iter().map(|(role, h)| (role, h.join().ok().flatten())).collect();
            (sides, diff.and_then(|d| d.join().ok().flatten()))
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
        if let Some(d) = diff {
            s["heap_diff"] = d.json;
        }
    }
}

/// One attribution recording for a report: CPU by function and file, allocations by function and
/// file, forced layouts, GC, the live heap, run count and metric values.
fn attribution(dir: &std::path::Path, rec: &str, plan_runs: Option<u64>) -> Option<Value> {
    // The trace views each run trace_processor; the profile and snapshot views read files meanwhile.
    let mut h = std::thread::scope(|scope| {
        let layout = scope.spawn(|| super::query::layout(dir, 25));
        let gc = scope.spawn(|| super::query::gc(dir));
        let alloc = scope.spawn(|| (super::query::alloc(dir, "function", 25), super::query::alloc(dir, "file", 25)));
        let heap = scope.spawn(|| super::query::heap(dir, 15, false));
        let mut h = super::query::hotspots(dir, "function", 50).ok()?.json;
        if let Ok(o) = super::query::hotspots(dir, "file", 25) {
            h["files"] = o.json["rows"].clone();
        }
        if let Ok((Ok(o), files)) = alloc.join() {
            h["alloc"] = o.json;
            if let Ok(f) = files {
                h["alloc"]["files"] = f.json["rows"].clone();
            }
        }
        if let Ok(Ok(o)) = layout.join() {
            h["layout"] = o.json;
        }
        if let Ok(Ok(o)) = gc.join() {
            h["gc"] = o.json;
        }
        if let Ok(Ok(o)) = heap.join() {
            h["heap"] = o.json;
        }
        Some(h)
    })?;
    let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
    let runs = meta["runs"]
        .as_u64()
        .or_else(|| {
            let dur = meta["duration_ms"].as_f64()?;
            let one = meta["values"]["run.duration"].as_f64()?;
            (one > 0.0).then(|| (dur / one).round().max(1.0) as u64)
        })
        .or(plan_runs);
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
    h["recording"] = json!(rec);
    if let Some(r) = runs {
        h["runs"] = json!(r);
    }
    if meta["values"].is_object() {
        h["values"] = meta["values"].clone();
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

fn open(path: &std::path::Path) {
    let p = path.display().to_string();
    let _ = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/c", "start", "", &p]).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&p).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(&p).spawn()
    };
}
