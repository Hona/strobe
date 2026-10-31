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
    let data: Value = if handle.starts_with("exp_") { store::read_json(&dir.join("result.json"))? } else { store::read_json(&dir.join("meta.json"))? };
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

const TEMPLATE: &str = include_str!("../../viewer/report.html");

pub fn html(view: &str, data: &Value) -> String {
    let payload = serde_json::to_string(&json!({ "view": view, "data": data })).unwrap_or_default().replace("</", "<\\/");
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
