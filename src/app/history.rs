//! Views of tracked history (SPEC §25.5): `strobe history`, `query history|changes`, `report history`.

use super::{Output, fmt};
use crate::cli::Args;
use crate::history::{self, Series};
use crate::model::{Error, Result};
use serde_json::{Value, json};

/// `strobe history [<ser_…|file>]`.
pub fn run(args: &Args) -> Result<Output> {
    view(args.pos.first().map(String::as_str), "history", args)
}

/// `strobe query history [<ser_…|file>]`, `query changes [<ser_…>]`, `query <ser_…> history|changes`.
pub fn query(args: &Args) -> Result<Output> {
    let a = args.pos.first().map(String::as_str).unwrap_or("history");
    // MCP's query names a view always (summary by default): over history it means the default view.
    let b = args.pos.get(1).map(String::as_str).filter(|v| *v != "summary");
    match (a, b) {
        ("history" | "changes", which) => view(which, a, args),
        (series, None) => view(Some(series), "history", args),
        (series, Some(v @ ("history" | "changes"))) => view(Some(series), v, args),
        (_, Some(other)) => Err(Error::usage(format!("a tracked series has the views history and changes, not {other}"))),
    }
}

/// The series a handle names: one id or prefix, a bench file (every series of it), or all of them.
fn select(which: Option<&str>) -> Result<Vec<Series>> {
    let Some(w) = which else { return Ok(history::list()) };
    if w.starts_with("ser_") {
        return Ok(history::load(&history::resolve(w)?).into_iter().collect());
    }
    let abs = std::fs::canonicalize(w).ok().map(|p| p.display().to_string().trim_start_matches(r"\\?\").to_string());
    let norm = w.replace('\\', "/");
    let found: Vec<Series> =
        history::list().into_iter().filter(|s| abs.as_deref().is_some_and(|a| s.meta["abs_file"] == a) || s.meta["file"].as_str().is_some_and(|f| f == norm || f.ends_with(&format!("/{norm}")))).collect();
    if found.is_empty() {
        return Err(Error::usage(format!("no tracked series for {w} on this device (strobe track {w} --ref <branch> starts one)")));
    }
    Ok(found)
}

fn view(which: Option<&str>, kind: &str, args: &Args) -> Result<Output> {
    let series = select(which)?;
    let one = which.is_some_and(|w| w.starts_with("ser_")) || series.len() == 1;
    if kind == "changes" {
        let mut text = String::new();
        let mut all = Vec::new();
        for s in &series {
            for c in &s.changes {
                text += &format!("{}\n", change_text(s, c));
                all.push(json!({ "series": s.id, "label": s.meta["label"], "metric": s.meta["metric"], "change": c }));
            }
        }
        if text.is_empty() {
            text = "no changes detected\n".into();
        }
        return Ok(Output::new(text, json!({ "changes": all })));
    }
    if !one || which.is_none() {
        let mut text = String::new();
        for s in &series {
            text += &list_line(s);
        }
        if text.is_empty() {
            text = "nothing tracked on this device yet: strobe track <file.bench.ts> --ref <branch>\n".into();
        } else if series.len() <= 4 && which.is_some() {
            text.clear();
            for s in &series {
                text += &detail(s, args.num("top").ok().flatten().map(|t| t as usize));
                text += "\n";
            }
        }
        return Ok(Output::new(text, json!({ "series": series.iter().map(summary).collect::<Vec<_>>() })));
    }
    let s = &series[0];
    Ok(Output::new(detail(s, args.num("top").ok().flatten().map(|t| t as usize)), full(s)))
}

fn summary(s: &Series) -> Value {
    let last = s.valid().last().map(|p| json!({ "commit": p["commit"], "median": p["median"], "lo": p["lo"], "hi": p["hi"], "at": p["at"] }));
    json!({
        "id": s.id, "label": s.meta["label"], "metric": s.meta["metric"], "unit": s.meta["unit"], "file": s.meta["file"], "fingerprint": s.meta["fingerprint"],
        "points": s.points.len(), "valid": s.valid().len(), "last": last, "changes": s.changes.iter().map(|c| json!({ "id": c["id"], "state": c["state"], "direction": c["direction"] })).collect::<Vec<_>>(),
    })
}

fn full(s: &Series) -> Value {
    json!({ "id": s.id, "meta": s.meta, "points": s.points, "changes": s.changes })
}

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline(s: &Series) -> String {
    let v: Vec<f64> = s.valid().iter().filter_map(|p| p["median"].as_f64()).collect();
    let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(*x), b.max(*x)));
    v.iter().map(|x| if hi > lo { BARS[(((x - lo) / (hi - lo)) * 7.0).round() as usize] } else { BARS[3] }).collect()
}

fn fingerprint_text(s: &Series) -> String {
    let f = &s.meta["fingerprint"];
    fmt::fields([
        f["machine"]["os"].as_str().unwrap_or("").to_string(),
        format!("strobe {}", f["machine"]["strobe"].as_str().unwrap_or("")),
        format!("power {}", f["power"].as_str().unwrap_or("")),
        format!("session {}", f["session"].as_str().unwrap_or("")),
        format!("display {}", f["display"].as_str().unwrap_or("")),
        f["pin"].as_str().unwrap_or("").to_string(),
    ])
}

fn list_line(s: &Series) -> String {
    let unit = s.meta["unit"].as_str().unwrap_or("");
    let last = s.valid().last().and_then(|p| p["median"].as_f64()).map(|v| fmt::value(v, unit)).unwrap_or_else(|| "—".into());
    let open: Vec<String> = s.changes.iter().filter(|c| c["state"] != "rejected").map(|c| format!("{} {}", c["state"].as_str().unwrap_or(""), c["direction"].as_str().unwrap_or(""))).collect();
    format!(
        "{}  {} · {}  {} points  last {last}  {}{}\n      {}\n",
        s.id,
        s.meta["label"].as_str().unwrap_or(""),
        s.meta["metric"].as_str().unwrap_or(""),
        s.valid().len(),
        sparkline(s),
        if open.is_empty() { String::new() } else { format!("  {}", open.join(", ")) },
        fingerprint_text(s)
    )
}

fn detail(s: &Series, top: Option<usize>) -> String {
    let unit = s.meta["unit"].as_str().unwrap_or("");
    let mut t = format!(
        "{} · {} · {}\n{}\n{} · {}\n\n",
        s.id,
        s.meta["label"].as_str().unwrap_or(""),
        s.meta["metric"].as_str().unwrap_or(""),
        s.meta["file"].as_str().unwrap_or(""),
        s.meta["device"].as_str().unwrap_or(""),
        fingerprint_text(s)
    );
    let shown = top.unwrap_or(40);
    let skip = s.points.len().saturating_sub(shown);
    if skip > 0 {
        t += &format!("  ({skip} earlier points; --top N shows more)\n");
    }
    for p in &s.points[skip..] {
        let commit = p["commit"].as_str().unwrap_or("");
        // A change's first point after it gets its marker.
        let mark = s.changes.iter().find(|c| c["bad"] == commit).map(|c| format!("  ◆ {} {} ({})", c["direction"].as_str().unwrap_or(""), c["id"].as_str().unwrap_or(""), c["state"].as_str().unwrap_or(""))).unwrap_or_default();
        let date = p["at"].as_str().unwrap_or("").get(..10).unwrap_or("");
        let value = if p["valid"] == true {
            format!(
                "{:>10}  [{} … {}]  n={}",
                fmt::value(p["median"].as_f64().unwrap_or(f64::NAN), unit),
                fmt::value(p["lo"].as_f64().unwrap_or(f64::NAN), unit),
                fmt::value(p["hi"].as_f64().unwrap_or(f64::NAN), unit),
                p["n"]
            )
        } else {
            format!("no value: {}", p["why"].as_str().unwrap_or(""))
        };
        let subj: String = p["subject"].as_str().unwrap_or("").chars().take(40).collect();
        t += &format!("  {date}  {}  {}  {}{mark}\n", p["short"].as_str().unwrap_or(""), fmt::pad(&subj, 41), value);
    }
    t += &format!("\n  {}\n", sparkline(s));
    if !s.changes.is_empty() {
        t += "\nchanges\n";
        for c in &s.changes {
            t += &format!("  {}\n", change_text(s, c));
        }
    }
    t
}

fn change_text(s: &Series, c: &Value) -> String {
    let unit = s.meta["unit"].as_str().unwrap_or("");
    let rel = c["relative"] == true;
    let f = |v: &Value| if rel { fmt::pct(v.as_f64().unwrap_or(f64::NAN)) } else { fmt::signed(v.as_f64().unwrap_or(f64::NAN), unit) };
    let mut t = format!(
        "{} {} {}  {}..{}  history {} (p {:.1e})",
        c["id"].as_str().unwrap_or(""),
        c["state"].as_str().unwrap_or(""),
        c["direction"].as_str().unwrap_or(""),
        c["good_short"].as_str().unwrap_or(""),
        c["bad_short"].as_str().unwrap_or(""),
        f(&c["estimate"]),
        c["p"].as_f64().unwrap_or(f64::NAN)
    );
    let cf = &c["confirm"];
    if cf["verdict"].is_string() {
        let crel = cf["relative"] == true;
        let g = |v: &Value| if crel { fmt::pct(v.as_f64().unwrap_or(f64::NAN)) } else { fmt::signed(v.as_f64().unwrap_or(f64::NAN), unit) };
        t += &format!(" · paired {} [{} … {}] {} {}", g(&cf["estimate"]), g(&cf["lo"]), g(&cf["hi"]), cf["verdict"].as_str().unwrap_or(""), cf["exp"].as_str().unwrap_or(""));
    }
    if let Some(sh) = c["culprit"]["short"].as_str() {
        t += &format!(" · culprit {sh} {}", c["culprit"]["subject"].as_str().unwrap_or(""));
    } else if let Some(v) = c["bisect"]["verdict"].as_str() {
        t += &format!(" · bisect {v}");
    }
    if let Some(a) = c["alerted"]["text"].as_str() {
        t += &format!(" · alerted ({a})");
    }
    t
}

const TEMPLATE: &str = include_str!("../../viewer/history.html");

/// `strobe report history <ser_…|file>`: the series as a self-contained HTML page.
pub fn report(args: &Args) -> Result<Output> {
    let which = args.pos.get(1).map(String::as_str);
    let series = select(which)?;
    let data: Vec<Value> = series.iter().map(full).collect();
    let payload = serde_json::to_string(&json!({ "series": data, "device": crate::host::device_name(), "created": super::record::now_iso() })).unwrap_or_default().replace("</", "<\\/");
    let html = TEMPLATE.replace("/*STROBE_DATA*/null", &payload);
    let out = match args.get("out") {
        Some(p) => std::path::PathBuf::from(p),
        None => history::dir().join(format!("report-{}.html", if series.len() == 1 { series[0].id.clone() } else { "history".into() })),
    };
    std::fs::write(&out, &html)?;
    let path = out.display().to_string();
    Ok(Output::new(format!("{path} · {}\n", fmt::bytes(html.len() as f64)), json!({ "path": path, "bytes": html.len() })))
}
