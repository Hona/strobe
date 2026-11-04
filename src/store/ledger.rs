//! The rerun ledger (SPEC §14.2): every experiment's identity (its variants by commit and uncommitted
//! changes, the bench file, the harness, its series and the flags that chose what ran) goes into
//! `index.jsonl`, so an experiment can say how many others compared the same two variants and what
//! they found. An agent that reruns until one says "faster" leaves that trail in the PR.

use serde_json::{Value, json};

/// The ledger of an experiment with this `identity` (`bench`'s `identity`): the other experiments on the
/// same variants (base and head, in that order), split into earlier and later ones, what each found, and
/// what changed between them and this one. `Null` without an identity (experiments from before it).
pub fn for_experiment(id: &str, created: &str, identity: &Value) -> Value {
    let key = &identity["variants"];
    if !key.is_array() {
        return Value::Null;
    }
    let (mut earlier, mut later) = (Vec::new(), Vec::new());
    for line in super::index() {
        if line["id"] == id || line["identity"]["variants"] != *key {
            continue;
        }
        let entry = attempt(&line, identity);
        if line["created"].as_str().unwrap_or("") <= created { earlier.push(entry) } else { later.push(entry) }
    }
    let attempt_n = earlier.len() + 1;
    let total = attempt_n + later.len();
    let changed: Vec<String> = earlier.iter().chain(&later).flat_map(|e| e["changed"].as_array().cloned().unwrap_or_default()).filter_map(|c| c.as_str().map(str::to_string)).collect();
    let mut series = json!({});
    for s in identity["series"].as_array().into_iter().flatten() {
        let label = s["label"].as_str().unwrap_or("");
        let found = |list: &Vec<Value>| list.iter().filter_map(|e| e["verdicts"].as_array()?.iter().find(|v| v["series"] == label).map(|v| json!({ "id": e["id"], "verdict": v["verdict"] }))).collect::<Vec<_>>();
        series[label] = json!({ "earlier": found(&earlier), "later": found(&later) });
    }
    let text = (total > 1).then(|| {
        let list = |l: &Vec<Value>| l.iter().map(|e| format!("{} {}{}", e["id"].as_str().unwrap_or(""), e["summary"].as_str().unwrap_or(""), changes(e))).collect::<Vec<_>>().join(", ");
        let mut t = format!("{} experiment on these variants", ordinal(attempt_n));
        if !later.is_empty() {
            t += &format!(" of {total}");
        }
        if !earlier.is_empty() {
            t += &format!("; earlier: {}", list(&earlier));
        }
        if !later.is_empty() {
            t += &format!("; later: {}", list(&later));
        }
        t
    });
    json!({ "attempt": attempt_n, "total": total, "earlier": earlier, "later": later, "series": series, "changed": !changed.is_empty(), "text": text })
}

/// The ledger line a report places (`3rd experiment on these variants; earlier: exp_x unclear, …`), when
/// other experiments compared the same variants.
pub fn line(result: &Value) -> Option<String> {
    result["ledger"]["text"].as_str().map(str::to_string)
}

/// One other experiment on the same variants: what it found and what differed from `now`.
fn attempt(line: &Value, now: &Value) -> Value {
    let then = &line["identity"];
    let mut changed: Vec<String> = Vec::new();
    if then["file"] != now["file"] {
        changed.push(format!("bench file {} → {}", then["file"].as_str().unwrap_or("?"), now["file"].as_str().unwrap_or("?")));
    } else if then["file_hash"] != now["file_hash"] {
        changed.push("the bench file changed".into());
    }
    if then["harness"] != now["harness"] && then["file_hash"] == now["file_hash"] {
        changed.push("the harness changed".into());
    }
    let labels = |id: &Value| id["series"].as_array().into_iter().flatten().filter_map(|s| s["label"].as_str().map(str::to_string)).collect::<Vec<_>>();
    let (a, b) = (labels(then), labels(now));
    let cases = |id: &Value| {
        let mut c: Vec<String> = id["series"].as_array().into_iter().flatten().filter_map(|s| s["case"].as_str().map(str::to_string)).collect();
        c.sort();
        c.dedup();
        c
    };
    if cases(then) != cases(now) {
        changed.push(format!("cases {} → {}", cases(then).join(", "), cases(now).join(", ")));
    }
    for label in b.iter().filter(|l| a.contains(l)) {
        let find = |id: &Value| id["series"].as_array().into_iter().flatten().find(|s| s["label"] == label.as_str()).cloned().unwrap_or(Value::Null);
        let (x, y) = (find(then), find(now));
        if x["primary"] != y["primary"] {
            changed.push(format!("primary {} → {}", x["primary"].as_str().unwrap_or("?"), y["primary"].as_str().unwrap_or("?")));
        }
        if x["noworse"] != y["noworse"] {
            changed.push(format!("no-worse margins {} → {}", margins(&x["noworse"]), margins(&y["noworse"])));
        }
    }
    let flags = |id: &Value| id["flags"].as_object().cloned().unwrap_or_default();
    let (fa, fb) = (flags(then), flags(now));
    let mut keys: Vec<&String> = fa.keys().chain(fb.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        if fa.get(k) != fb.get(k) {
            let show = |v: Option<&Value>| v.and_then(Value::as_str).map(|s| if s == "true" { "on".to_string() } else { s.to_string() }).unwrap_or_else(|| "unset".into());
            changed.push(format!("--{k} {} → {}", show(fa.get(k)), show(fb.get(k))));
        }
    }
    changed.dedup();
    let verdicts = line["verdicts"].clone();
    let short = |v: &Value| v["verdict"].as_str().unwrap_or("").split(": ").next().unwrap_or("").trim_end_matches(", the last planned look").to_string();
    let summary = match verdicts.as_array().map(Vec::as_slice) {
        Some([one]) => short(one),
        Some(many) if !many.is_empty() => many.iter().map(|v| format!("{} {}", v["series"].as_str().unwrap_or(""), short(v))).collect::<Vec<_>>().join(" / "),
        _ => "no result".into(),
    };
    json!({ "id": line["id"], "created": line["created"], "verdicts": verdicts, "summary": summary, "changed": changed })
}

fn changes(e: &Value) -> String {
    let c: Vec<&str> = e["changed"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if c.is_empty() { String::new() } else { format!(" ({})", c.join("; ")) }
}

fn margins(v: &Value) -> String {
    match v.as_object() {
        Some(o) if !o.is_empty() => o.iter().map(|(k, x)| format!("{k} {}", x.as_f64().map(|m| format!("{}%", (m * 1000.0).round() / 10.0)).unwrap_or_default())).collect::<Vec<_>>().join(", "),
        _ => "none".into(),
    }
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}
