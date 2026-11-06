//! `query <rec> actions` (SPEC §11.1): what a bench block's hooks sent the target, in order, from the
//! recording's `actions.json.gz`; `query <exp> actions` for an experiment's check cases, a failed one's
//! from the file kept beside its result. Typed text is the user's (rule 9): redacted unless `--reveal`.

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

/// The experiment's actions: each series' recording's, and each failed check case's.
pub fn experiment(result: &Value, args: &Args, top: usize) -> Result<Output> {
    let mut found: Vec<(String, ActionLog)> = Vec::new();
    for s in result["series"].as_array().into_iter().flatten() {
        let label = s["label"].as_str().unwrap_or("");
        let target = s["target"].as_str().filter(|t| !label.contains(*t)).map(|t| format!(" · {t}")).unwrap_or_default();
        if let Some(rec) = s["recording"].as_str()
            && let Ok(dir) = store::resolve(rec)
            && let Some(log) = ActionLog::read(&dir.join(store::actions::FILE))?
        {
            found.push((format!("{label}{target} · {rec}"), log));
        }
        if let Some(path) = s["actions"]["path"].as_str()
            && let Some(log) = ActionLog::read(Path::new(path))?
        {
            found.push((format!("{label}{target} · failed"), log));
        }
    }
    if found.is_empty() {
        return Err(Error::usage(
            "this experiment kept no actions: a check keeps them in each case's recording and beside a failed case's result; a comparison in the recordings it keeps (--keep, attribution blocks): strobe query <rec> actions",
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
        let mut selector = a.selector.clone().unwrap_or_else(|| "-".into());
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
        if a.untimed {
            marks += " untimed";
        }
        if let Some(p) = &a.phase {
            marks += &format!(" phase {p}");
        }
        text += &format!("  {:>5} {:<8} {:>4} {:>9.1} {:>8.1}  {:<8} {:<28} {:<30} {outcome}{marks}\n", a.seq, a.hook.name(), run, at, ms, a.call, clip(&selector, 28), clip(&args_text, 30));
    }
    if rows.len() > top {
        text += &format!("  … {} more (--top {} shows all, --json gives each)\n", rows.len() - top, rows.len());
    }
    if !reveal && rows.iter().any(|a| matches!(a.call.as_str(), "type" | "act" | "call")) {
        text += "  typed text, values and call arguments redacted (--reveal shows them)\n";
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
