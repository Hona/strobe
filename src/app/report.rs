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

/// The "why": self time per function and file from each series' attribution blocks (base and head),
/// plus the attribution block's run count and metric values so the report can show per-run ms.
fn attach_hotspots(data: &mut Value) {
    let plan_runs = data["plan"]["runs_per_block"].as_u64();
    let Some(series) = data["series"].as_array_mut() else { return };
    for s in series {
        let mut out = json!({});
        for role in ["base", "head"] {
            let Some(rec) = s["attribution"][role].as_str() else { continue };
            let Ok(dir) = store::resolve(rec) else { continue };
            let Ok(fn_out) = super::query::hotspots(&dir, "function", 50) else { continue };
            let mut h = fn_out.json;
            if let Ok(file_out) = super::query::hotspots(&dir, "file", 25) {
                h["files"] = file_out.json["rows"].clone();
            }
            let meta = store::read_json(&dir.join("meta.json")).unwrap_or(Value::Null);
            let runs = meta["runs"]
                .as_u64()
                .or_else(|| {
                    let dur = meta["duration_ms"].as_f64()?;
                    let one = meta["values"]["run.duration"].as_f64()?;
                    (one > 0.0).then(|| (dur / one).round().max(1.0) as u64)
                })
                .or(plan_runs);
            h["recording"] = json!(rec);
            if let Some(r) = runs {
                h["runs"] = json!(r);
            }
            if meta["values"].is_object() {
                h["values"] = meta["values"].clone();
            }
            out[role] = h;
        }
        if out.as_object().is_some_and(|o| !o.is_empty()) {
            s["hotspots"] = out;
        }
    }
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
