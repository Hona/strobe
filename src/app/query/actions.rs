//! `query <rec> actions` (SPEC §11.1): what a bench block's hooks sent the target, in order, from the
//! recording's `actions.json.gz`; `query <exp> actions` for an experiment's kept recordings and a check
//! case without one (from the file kept beside its result). Typed text, `act` values,
//! `ctx.call` arguments and `ctx.exec` lines are the user's (rule 9): redacted unless `--reveal`.

use super::Output;
use crate::cli::Args;
use crate::model::{Error, Result};
use crate::store::{self, actions::ActionLog, actions::ActionRecord, actions::Outcome};
use serde_json::{Value, json};
use std::path::Path;

pub fn recording(dir: &Path, args: &Args, top: usize) -> Result<Output> {
    let log = ActionLog::read(&dir.join(store::actions::FILE))?
        .ok_or_else(|| Error::usage("this recording kept no actions: they are kept for bench blocks whose hooks acted on the target (bench recordings from before strobe kept them have none)"))?;
    let id = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (text, json) = view(&id, &log, args, top);
    Ok(Output::new(text, json))
}

/// The experiment's actions: each series' kept recordings' (a check's, attribution blocks', kept
/// timing blocks'), and a check case's without a recording (kept beside the result).
pub fn experiment(result: &Value, args: &Args, top: usize) -> Result<Output> {
    let mut found: Vec<(String, ActionLog)> = Vec::new();
    for s in result["series"].as_array().into_iter().flatten() {
        let label = s["label"].as_str().unwrap_or("");
        let target = s["target"].as_str().filter(|t| !label.contains(*t)).map(|t| format!(" · {t}")).unwrap_or_default();
        let mut recs: Vec<(String, String)> = Vec::new();
        let mut add = |rec: Option<&str>, what: String| {
            if let Some(r) = rec.filter(|r| !recs.iter().any(|x| x.0 == *r)) {
                recs.push((r.to_string(), what));
            }
        };
        add(s["recording"].as_str(), "check".into());
        add(s["attribution"].as_str(), "attribution".into());
        for side in ["base", "head"] {
            add(s["attribution"][side].as_str(), format!("{side} attribution"));
        }
        for t in s["timing_recordings"].as_array().into_iter().flatten() {
            add(t["recording"].as_str(), format!("{} block, pair {}", t["variant"].as_str().unwrap_or(""), t["pair"].as_u64().map_or(0, |p| p + 1)));
        }
        for b in s["blocks"].as_array().into_iter().flatten() {
            add(b["recording"].as_str(), format!("{} block", b["variant"].as_str().unwrap_or("")));
        }
        for (rec, what) in recs {
            if let Ok(dir) = store::resolve(&rec)
                && let Some(log) = ActionLog::read(&dir.join(store::actions::FILE))?
            {
                // A batch's launch shares its recording among its cases (`--check --batch`): each case's
                // own actions, and the first's also the launch's prepare and setup.
                let log = match (s["batch_seq"].as_u64(), s["case"].as_str()) {
                    (Some(seq), Some(case)) if Some(rec.as_str()) == s["recording"].as_str() => log.of_case(case, seq == 1),
                    _ => log,
                };
                found.push((format!("{label}{target} · {rec} ({what})"), log));
            }
        }
        if let Some(path) = s["actions"]["path"].as_str()
            && let Some(log) = ActionLog::read(Path::new(path))?
        {
            found.push((format!("{label}{target} · {}", if s["actions"]["failed"] == false { "no recording" } else { "failed" }), log));
        }
    }
    if found.is_empty() {
        return Err(Error::usage(
            "this experiment kept no actions: a check keeps each case's (in its recording, or beside the result); a comparison keeps them only in the recordings it keeps (the first pair's, --keep, attribution blocks)",
        ));
    }
    let (mut text, mut parts) = (String::new(), Vec::new());
    for (title, log) in &found {
        let (t, j) = view(title, log, args, top);
        text += &t;
        parts.push(j);
    }
    let json = if parts.len() == 1 { parts.remove(0) } else { json!({ "sources": parts }) };
    Ok(Output::new(text, json))
}

/// The rows (`--phase` filters, `--top` bounds the text) and the redacted log as JSON.
fn view(title: &str, log: &ActionLog, args: &Args, top: usize) -> (String, Value) {
    let reveal = args.has("reveal");
    let phase = args.get("phase");
    let rows: Vec<ActionRecord> = log.actions.iter().filter(|a| phase.is_none_or(|p| a.phase.as_deref() == Some(p))).map(|a| if reveal { a.clone() } else { redact(a) }).collect();
    let t0 = log.actions.first().map_or(0, |a| a.t_ns);
    let mut text = format!(
        "{title} · {} actions{}{}{}\n",
        rows.len(),
        log.seed.map(|s| format!(" · seed {s}")).unwrap_or_default(),
        phase.map(|p| format!(" · phase {p}")).unwrap_or_default(),
        if log.truncated { " · cut at 100,000 (the rest were not kept)" } else { "" }
    );
    text += &format!("  {:>5} {:<8} {:>4} {:>9} {:>8}  {:<8} {:<28} {:<30} outcome\n", "seq", "hook", "run", "at ms", "ms", "call", "selector", "args");
    for a in rows.iter().take(top) {
        let run = a.run.map(|r| r.to_string()).unwrap_or_else(|| "-".into());
        let at = (a.t_ns.saturating_sub(t0)) as f64 / 1e6;
        let ms = (a.end_ns.saturating_sub(a.t_ns)) as f64 / 1e6;
        let mut selector = match &a.selector {
            Some(Value::String(s)) => s.clone(),
            Some(o) => o.to_string(),
            None => "-".into(),
        };
        if let Some(h) = &a.hit {
            selector += &format!(" → {} {:?}", h.role, h.name);
        }
        let args_text = if a.args.is_empty() { "-".to_string() } else { a.args.iter().map(Value::to_string).collect::<Vec<_>>().join(", ") };
        let outcome = match &a.outcome {
            Outcome::Ok => "ok".to_string(),
            Outcome::Refusal { validator, .. } => format!("refused {validator}"),
            Outcome::Error { message } => format!("error: {}", clip(message, 60)),
        };
        let mut marks = String::new();
        if let Some(c) = &a.case {
            marks += &format!(" · case {c}");
        }
        if let Some(s) = &a.sample {
            marks += &format!(" · sample {}", s.text());
        }
        if a.untimed {
            marks += " untimed";
        }
        if let Some(p) = &a.phase {
            marks += &format!(" phase {p}");
        }
        if a.args_truncated {
            marks += " args cut";
        }
        text += &format!("  {:>5} {:<8} {:>4} {:>9.1} {:>8.1}  {:<8} {:<28} {:<30} {outcome}{marks}\n", a.seq, a.hook.name(), run, at, ms, a.call, clip(&selector, 28), clip(&args_text, 30));
    }
    if rows.len() > top {
        text += &format!("  … {} more (--top {} shows all, --json gives each)\n", rows.len() - top, rows.len());
    }
    if !reveal && rows.iter().any(|a| matches!(a.call.as_str(), "type" | "act" | "call" | "exec")) {
        text += "  typed text, act values, ctx.call arguments and ctx.exec lines redacted (--reveal shows them)\n";
    }
    if rows.iter().any(|a| a.args_truncated) {
        text += "  args cut: strings kept to 4 KiB and arrays to 32 items\n";
    }
    text += "\n";
    (text, json!({ "title": title, "seed": log.seed, "truncated": log.truncated, "actions": rows }))
}

/// Text the bench typed or handed over, as its length.
fn redact(a: &ActionRecord) -> ActionRecord {
    let mut a = a.clone();
    match a.call.as_str() {
        "type" => a.args.iter_mut().take(1).for_each(hide),
        "act" => a.args.iter_mut().filter_map(|o| o.get_mut("value")).for_each(hide),
        "call" => a.args.iter_mut().skip(1).for_each(hide),
        // A command line can hold a token or a path of the user's.
        "exec" => a.args.iter_mut().for_each(hide),
        _ => {}
    }
    a
}

fn hide(v: &mut Value) {
    match v {
        Value::String(s) => *v = Value::String(format!("«{} chars»", s.chars().count())),
        Value::Array(a) => a.iter_mut().for_each(hide),
        Value::Object(o) => o.values_mut().for_each(hide),
        _ => {}
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}
