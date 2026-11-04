//! `strobe bench <file> --bisect <good>..<bad>`: the commit that moved a case's primary (SPEC §9.7).

use super::{Output, fmt, progress};
use crate::cli::Args;
use crate::config;
use crate::model::{Error, Result};
use crate::store;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

/// Walks `git rev-list --first-parent good..bad`. First benches `bad` against `good` to learn which
/// way the primary moved; then each step benches the middle commit against `good` with the same
/// settings. A step that moves the primary the way `bad` did becomes `bad`, any other outcome becomes
/// `good`, until one commit is left: the culprit. A step without a verdict can hide the culprit
/// behind it, so the result names those steps and is `unclear` instead of found.
pub fn run(args: &Args, range: &str) -> Result<Output> {
    run_from(args, range, None)
}

/// A bisect whose step 0 (`bad` against `good`) already ran: a tracked change's paired confirmation
/// (SPEC §25.4) is that bench, so the bisect does not run it again.
pub(crate) fn run_from(args: &Args, range: &str, first: Option<&Output>) -> Result<Output> {
    let file = args.pos.first().ok_or_else(|| Error::usage("bench needs a bench file"))?;
    let file = std::fs::canonicalize(file).map_err(|_| Error::usage(format!("no such bench file: {file}")))?;
    let root = config::project_root(file.parent().unwrap_or(Path::new(".")));
    let (good, bad) = range.split_once("..").ok_or_else(|| Error::usage("--bisect <good>..<bad>, for example --bisect v1.2..main"))?;
    let (mut good, mut bad) = (rev_parse(&root, good)?, rev_parse(&root, bad)?);
    let id = args.get("id").map(str::to_string).unwrap_or_else(|| store::new_id("exp"));
    let dir = store::experiment_dir(&id)?;
    let log = dir.join("progress.log");
    super::progress_to(Some(log.clone()));
    let mut candidates = git(&root, &["rev-list", "--first-parent", "--reverse", &format!("{good}..{bad}")])?.lines().map(str::to_string).collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(Error::usage(format!("{range}: no commits from good to bad on the first-parent line")));
    }
    progress(format!("strobe bisect {range} · {} commits on the first-parent line · {id}", candidates.len()));
    let line: Vec<Value> = candidates.iter().map(|c| json!({ "commit": c, "short": short(c), "subject": subject(&root, c) })).collect();
    let first_good = good.clone();
    let first = match first {
        Some(out) => step_json(out, &root, 0, &good, &bad)?,
        None => step(args, &id, &log, &root, 0, &good, &bad)?,
    };
    let direction = moved(&first);
    let mut steps = vec![first];
    // Steps without a verdict, with the `good` each was benched against: the culprit may hide there.
    let mut undecided: Vec<(String, String)> = Vec::new();
    if let Some(sign) = direction {
        while candidates.len() > 1 {
            // The middle commit, never `bad` itself: `bad` is already known to have moved.
            let at = (candidates.len() - 1) / 2;
            let mid = candidates[at].clone();
            let s = step(args, &id, &log, &root, steps.len(), &good, &mid)?;
            if moved(&s) == Some(sign) {
                bad = mid;
                candidates.truncate(at + 1);
            } else {
                if s["decided"] != true {
                    undecided.push((mid.clone(), good.clone()));
                }
                good = mid;
                candidates.drain(..=at);
            }
            steps.push(s);
        }
    }
    let culprit = direction.map(|_| json!({ "commit": bad, "short": short(&bad), "subject": subject(&root, &bad) }));
    let verdict = match (&culprit, undecided.is_empty()) {
        (None, _) => format!("no change: {} did not move between good and bad ({})", steps[0]["metric"].as_str().unwrap_or(""), steps[0]["verdict"].as_str().unwrap_or("")),
        (Some(_), true) => format!("found: {} {}", short(&bad), subject(&root, &bad)),
        (Some(_), false) => format!(
            "unclear: {} {} is the likeliest culprit (it moved, and no step before it did), but {} step(s) had no verdict ({}), so the culprit may be anywhere in {}..{}; more pairs (--pairs auto) settle it",
            short(&bad),
            subject(&root, &bad),
            undecided.len(),
            undecided.iter().map(|c| short(&c.0)).collect::<Vec<_>>().join(", "),
            short(&undecided[0].1),
            short(&bad)
        ),
    };
    // Where the culprit may still be: from the first undecided step's `good` (or the last `good`) to `bad`.
    let remaining = culprit.as_ref().map(|_| json!({ "good": undecided.first().map_or(&good, |u| &u.1), "bad": bad }));
    let result = json!({
        "id": id, "kind": "bisect", "file": file, "range": range, "created": super::record::now_iso(),
        "metric": steps[0]["metric"], "series": steps[0]["series"], "steps": steps, "culprit": culprit, "verdict": verdict,
        "good": first_good, "commits": line, "remaining": remaining,
    });
    let text = text(&result);
    let mut result = result;
    result["text"] = json!(text);
    store::write_json(&dir.join("result.json"), &result)?;
    let mut out = Output::new(text, result);
    if args.has("require-verdict") && !verdict_found(&out.json) {
        out.code = 5;
    }
    Ok(out)
}

/// One bisect step: a full experiment of `head` against `base` with the bisect's own settings.
fn step(args: &Args, id: &str, log: &Path, root: &Path, k: usize, base: &str, head: &str) -> Result<Value> {
    let mut a = args.clone();
    a.remove("bisect");
    a.set("base", &format!("git:{base}"));
    a.set("head", &format!("git:{head}"));
    a.set("id", &format!("{id}_s{k}"));
    if !args.has("why") {
        // Profiled launches explain a move; a bisect only needs to know whether one happened.
        a.set("why", "never");
    }
    progress(format!("\nstep {k}  {} against {}", short(head), short(base)));
    let out = super::bench::run(&a)?;
    super::progress_to(Some(log.to_path_buf()));
    step_json(&out, root, k, base, head)
}

/// A step as the bisect keeps it, from the bench that ran it.
fn step_json(out: &Output, root: &Path, k: usize, base: &str, head: &str) -> Result<Value> {
    let series = out.json["series"].as_array().cloned().unwrap_or_default();
    if series.len() != 1 {
        return Err(Error::usage(format!("--bisect follows one series, and the file has {} here: narrow it with --filter, --target or --params", series.len())));
    }
    let p = &series[0]["primary"];
    let s = json!({
        "step": k, "good": base, "commit": head, "short": short(head), "subject": subject(root, head),
        "experiment": out.json["id"], "series": series[0]["label"], "metric": p["metric"], "relative": p["relative"],
        "estimate": p["estimate"], "lo": p["lo"], "hi": p["hi"], "verdict": p["verdict"], "decided": p["decided"], "pairs": series[0]["pairs"],
    });
    progress(format!("  {}  {}", change(&s), s["verdict"].as_str().unwrap_or("")));
    Ok(s)
}

/// +1 or −1 when the step's primary moved with a verdict (its interval excludes 0), else None.
fn moved(s: &Value) -> Option<i8> {
    let (lo, hi) = (s["lo"].as_f64()?, s["hi"].as_f64()?);
    (s["decided"] == true).then_some(())?;
    if lo > 0.0 {
        Some(1)
    } else if hi < 0.0 {
        Some(-1)
    } else {
        None
    }
}

fn verdict_found(r: &Value) -> bool {
    r["verdict"].as_str().is_some_and(|v| v.starts_with("found") || v.starts_with("no change"))
}

fn change(s: &Value) -> String {
    let f = |k: &str| s[k].as_f64().unwrap_or(f64::NAN);
    if s["relative"] == true {
        format!("{} [{} … {}]", fmt::pct(f("estimate")), fmt::pct(f("lo")), fmt::pct(f("hi")))
    } else {
        format!("{:+.2} [{:+.2} … {:+.2}]", f("estimate"), f("lo"), f("hi"))
    }
}

fn text(r: &Value) -> String {
    let mut t = format!("strobe bisect {} · {} · {}\n", r["range"].as_str().unwrap_or(""), r["series"].as_str().unwrap_or(""), r["metric"].as_str().unwrap_or(""));
    for s in r["steps"].as_array().into_iter().flatten() {
        t += &format!(
            "step {:<2} {} vs {}  {:<28} {:<22} {}  {}\n",
            s["step"],
            s["short"].as_str().unwrap_or(""),
            short(s["good"].as_str().unwrap_or("")),
            change(s),
            s["verdict"].as_str().unwrap_or(""),
            s["experiment"].as_str().unwrap_or(""),
            s["subject"].as_str().unwrap_or("")
        );
    }
    t += &format!("\n{}\n", r["verdict"].as_str().unwrap_or(""));
    t
}

fn rev_parse(root: &Path, rev: &str) -> Result<String> {
    let rev = rev.strip_prefix("git:").unwrap_or(rev);
    git(root, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")]).map(|s| s.trim().to_string()).map_err(|_| Error::usage(format!("--bisect: {rev} is not a commit in {}", root.display())))
}

fn subject(root: &Path, rev: &str) -> String {
    git(root, &["log", "-1", "--format=%s", rev]).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn short(rev: &str) -> String {
    rev.chars().take(10).collect()
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output()?;
    if !out.status.success() {
        return Err(Error::msg(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
