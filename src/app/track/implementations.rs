//! `strobe track <file> --base <target>[=<variant>] --head <target>=git:<ref>` (SPEC §25.7): a port
//! tracked over the head's commits. Each commit gets a short implementation comparison (a few
//! alternating pairs of the base and the head at that commit), and each journey a point: the pairs'
//! `ln(head / base)` with an interval, whether the journey was compared or a parity gap, and both
//! sides' identities. The base's identity is not in the series' fingerprint: a comparator that moves
//! stays in one timeline as a marked epoch, never a regression.

use super::{finite, majority, rel, repo_id};
use crate::app::{family, fmt, pr, progress};
use crate::cli::Args;
use crate::history::{self, detect};
use crate::host::{self, git, git::commit_time, git::short, git::subject};
use crate::model::{self, Error, Result};
use crate::runner::Runner;
use crate::stats;
use crate::store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Pairs per point unless `--pairs`: enough for a ratio with an interval, few enough to run on every
/// commit. A point is evidence for where to look; the paired A/B of the head decides.
pub(super) const PAIRS: usize = 3;

/// A point with fewer pairs than this carries no value: below 3 pairs per point, 3 points on a side
/// of a change cannot reach the detector's p < 0.001 (§25.3), and the interval is only 50%.
const MIN_PAIRS: usize = 3;

/// The two sides of a tracked implementation comparison.
pub(super) struct Sides {
    /// `--base` as given: the comparator, at a variant that does not move with the head.
    pub base: String,
    pub base_target: String,
    /// The base target's project (its `cwd`'s, else the bench file's) and its strobe.json: where its
    /// app profiles come from.
    pub base_config: Value,
    pub head_target: String,
    /// The ref `--head` names (`git:<ref>`): what `--ref` is to a single target's track.
    pub head_ref: String,
    /// The repository the head's commits are in: its target's `cwd` project, else the bench file's.
    pub head_root: PathBuf,
}

/// Whether `rev` names a commit that cannot move in `root`: a tag or a commit's hash, never a branch
/// or `HEAD`.
fn pinned_ref(root: &Path, rev: &str) -> bool {
    let Some(commit) = git::commit(root, rev) else { return false };
    let hex = rev.len() >= 7 && rev.chars().all(|c| c.is_ascii_hexdigit()) && commit.starts_with(&rev.to_ascii_lowercase());
    hex || git::ok(root, &["show-ref", "--verify", "--quiet", &format!("refs/tags/{rev}")])
}

impl Sides {
    /// The sides `--base` and `--head` name, or None for a single target's track.
    pub fn of(args: &Args, file: &Path, root: &Path) -> Result<Option<Sides>> {
        let (base, head) = match (args.get("base"), args.get("head")) {
            (None, None) => return Ok(None),
            (Some(b), Some(h)) => (b, h),
            _ => return Err(Error::usage("track compares two implementations with both --base <target>[=<variant>] and --head <target>=git:<ref>")),
        };
        for (k, why) in
            [("ref", "--head names the ref it follows (--head <target>=git:<ref>)"), ("target", "the sides name the targets"), ("blocks", "a point of two implementations is pairs (--pairs N)")]
        {
            if args.has(k) {
                return Err(Error::usage(format!("--{k} with --base/--head: {why}")));
            }
        }
        let split = |spec: &str| {
            let s = spec.strip_prefix("target:").unwrap_or(spec).to_string();
            match s.split_once('=') {
                Some((t, v)) => (t.to_string(), Some(v.to_string())),
                None => (s, None),
            }
        };
        let (head_target, head_variant) = split(head);
        let head_ref = head_variant
            .as_deref()
            .and_then(|v| v.strip_prefix("git:"))
            .filter(|r| !r.is_empty())
            .ok_or_else(|| Error::usage(format!("--head {head}: track follows the head's commits: --head <target>=git:<ref> (git:main)")))?
            .to_string();
        let (base_target, base_variant) = split(base);
        if base_target == head_target {
            return Err(Error::usage(format!("--base {base} --head {head}: one target on both sides is a single target's track (strobe track <file> --ref <ref> --target {head_target})")));
        }
        // The targets and their repositories, from the bench file.
        let log = host::paths::run().join(format!("track-{}.log", std::process::id()));
        let mut runner = Runner::spawn(file, log.clone(), None)?;
        let describe = runner.describe();
        runner.exit();
        let _ = std::fs::remove_file(&log);
        let describe = describe?;
        let known = || describe["targets"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
        for t in [&base_target, &head_target] {
            if !describe["targets"][t.as_str()].is_object() {
                return Err(Error::usage(format!("the bench file has no target {t} (its targets: {})", known())));
            }
        }
        let project = |t: &str| crate::app::bench::target_project(&describe, t).map(|p| p.map_or_else(|| root.to_path_buf(), |(p, _)| p));
        let head_root = project(&head_target)?;
        let base_root = project(&base_target)?;
        // The comparator must not move: a folder in the head's repository moves with every commit
        // tracked, and a branch or HEAD moves whenever someone commits. A tag, a commit, an executable,
        // an app, or a checkout of another repository stays put (its changes show as epochs).
        let top = |p: &Path| git::toplevel(p).and_then(|t| std::fs::canonicalize(t).ok());
        let pin = format!("pin the comparator: --base {base_target}=exe:<path>, =app:<id>, =git:<tag or commit>, or a checkout of another repository");
        let moving_dir = |dir: &Path| top(dir).is_some() && top(dir) == top(&head_root);
        match base_variant.as_deref() {
            None if moving_dir(&base_root) => return Err(Error::usage(format!("--base {base}: its build is the checkout the head's commits are in, so it would move with them; {pin}"))),
            None => {}
            Some(v) if ["exe:", "app:", "url:", "cmd:"].iter().any(|p| v.starts_with(p)) => {}
            Some(v) => {
                let dir = (!v.starts_with("git:")).then(|| if Path::new(v).is_absolute() { PathBuf::from(v) } else { std::env::current_dir().unwrap_or_default().join(v) }).filter(|d| d.is_dir());
                match dir {
                    Some(d) if moving_dir(&d) => return Err(Error::usage(format!("--base {base}: that folder is in the head's repository, so it would move with the head's commits; {pin}"))),
                    Some(_) => {}
                    None => {
                        let rev = v.strip_prefix("git:").unwrap_or(v);
                        if !pinned_ref(&base_root, rev) {
                            return Err(Error::usage(format!("--base {base}: {rev} is a branch or HEAD (or no commit) in {}, which moves; {pin}", base_root.display())));
                        }
                    }
                }
            }
        }
        if git::commit(&head_root, &head_ref).is_none() {
            return Err(Error::usage(format!("--head {head}: {head_ref} is not a commit in {}", head_root.display())));
        }
        let base_config = crate::config::load(&base_root)?;
        Ok(Some(Sides { base: base.to_string(), base_target, base_config, head_target, head_ref, head_root }))
    }

    /// The head side at a commit.
    pub fn head_at(&self, commit: &str) -> String {
        format!("{}=git:{commit}", self.head_target)
    }
}

/// Pairs per point: `--pairs N` (from 3), default 3.
pub(super) fn pairs(args: &Args) -> Result<usize> {
    match args.get("pairs") {
        None => Ok(PAIRS),
        Some(p) => match p.parse::<usize>() {
            Ok(n) if n >= MIN_PAIRS => Ok(n),
            _ => Err(Error::usage(format!("--pairs {p}: a tracked point is a fixed number of pairs, from {MIN_PAIRS} (default {PAIRS})"))),
        },
    }
}

/// What tells two runs of one commit apart: the sides and the pairs, beside a single target's flags.
pub(super) fn key_parts(sides: &Sides, pairs: usize) -> Value {
    json!({ "base": sides.base, "head": sides.head_target, "pairs": pairs })
}

/// The bench a point runs: `pairs` alternating pairs of the base and the head at `commit`, no attribution.
fn bench_args(args: &Args, sides: &Sides, file: &Path, commit: &str, pairs: usize, exp: &str) -> Result<Args> {
    let mut a = Args::parse(vec!["bench".into(), file.display().to_string()])?;
    for k in super::PASS.iter().filter(|k| **k != "target") {
        if let Some(v) = args.get(k) {
            a.set(k, v);
        }
    }
    a.set("base", &sides.base);
    a.set("head", &sides.head_at(commit));
    a.set("pairs", &pairs.to_string());
    a.set("why", "never");
    a.set("id", exp);
    Ok(a)
}

/// What an executable comparator is, by content: the SHA-256 of the executable and, for an app that
/// has them, its `app.asar` (an Electron app's code) and its bundle's version (macOS `Info.plist`).
/// A copy, a touch or a reinstall of the same bytes is the same comparator; a rebuild is another.
fn content_identity(exe: &Path) -> Option<(String, Option<String>)> {
    use sha2::Digest;
    use std::io::Read;
    let mut h = sha2::Sha256::new();
    let dir = exe.parent()?;
    let asar = [dir.join("resources").join("app.asar"), dir.join("../Resources/app.asar")].into_iter().find(|p| p.is_file());
    for f in std::iter::once(exe.to_path_buf()).chain(asar) {
        let mut file = std::fs::File::open(&f).ok()?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    let digest: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    let plist = std::fs::read_to_string(dir.join("../Info.plist")).unwrap_or_default();
    let version = plist.split("<key>CFBundleShortVersionString</key>").nth(1).and_then(|s| s.split("<string>").nth(1)).and_then(|s| s.split("</string>").next()).map(|s| s.trim().to_string());
    Some((digest, version))
}

/// The base side as this experiment ran it: its target, name, identity and that identity as people
/// read it. An executable or app is identified by its content (`content_identity`); anything else by
/// the rerun ledger's identity (§14.2: a commit, a URL); a page's browser version beside it.
fn base_identity(r: &Value, sides: &Sides) -> Value {
    let v = &r["variants"][0];
    let ledger = r["identity"]["variants"][0].as_str().unwrap_or("");
    let mut identity = ledger.split_once('=').map_or(ledger, |(_, i)| i).to_string();
    let spec = v["spec"].as_str().unwrap_or("");
    let exe = match v["kind"].as_str() {
        Some("exe") => Some(PathBuf::from(spec.trim_start_matches("exe:"))),
        Some("app") => crate::target::isolate::app(spec.trim_start_matches("app:"), &sides.base_config).ok().map(|a| a.exe),
        _ => None,
    };
    let mut display = match (v["kind"].as_str(), exe.as_deref().and_then(|e| content_identity(e).map(|c| (e, c)))) {
        (_, Some((e, (digest, version)))) => {
            identity = format!("sha256:{digest}{}", version.as_deref().map(|x| format!(" {x}")).unwrap_or_default());
            let name = e.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            crate::app::fmt::fields([name, version.unwrap_or_default(), format!("sha256 {}", &digest[..8])])
        }
        (Some("git" | "dir"), _) => format!("{}{}", v["commit"].as_str().map(short).unwrap_or_default(), if v["dirty"] == true { " with changes" } else { "" }),
        _ => v["label"].as_str().unwrap_or(&identity).to_string(),
    };
    if matches!(v["target_kind"].as_str(), Some("web" | "url"))
        && let Some(b) = r["browser"].as_str()
    {
        identity += &format!(" · {b}");
        display += &format!(" · {b}");
    }
    json!({ "target": v["target"], "name": v["name"], "kind": v["kind"], "identity": identity, "display": display })
}

/// Each pair's (base, head) values of `metric`, in pair order.
fn pair_values(s: &Value, metric: &str) -> Vec<(f64, f64)> {
    let mut by: std::collections::BTreeMap<u64, (f64, f64)> = std::collections::BTreeMap::new();
    for b in s["blocks"].as_array().into_iter().flatten() {
        let Some(p) = b["pair"].as_u64().filter(|p| *p < 1_000_000) else { continue };
        let v = b["values"][metric].as_f64().unwrap_or(f64::NAN);
        let e = by.entry(p).or_insert((f64::NAN, f64::NAN));
        if b["variant"] == "base" {
            e.0 = v;
        } else if b["variant"] == "head" {
            e.1 = v;
        }
    }
    by.into_values().filter(|(a, b)| a.is_finite() && b.is_finite()).collect()
}

/// The lock and display state most blocks of the experiment ran in, for a journey that ran none (a
/// gap), so its points stay in the series its compared points join; else the machine's now.
fn experiment_state(r: &Value) -> (String, String) {
    let blocks: Vec<&Value> = r["series"].as_array().into_iter().flatten().flat_map(|s| s["blocks"].as_array().into_iter().flatten()).collect();
    if blocks.is_empty() {
        let session = if host::session_locked() { "locked" } else { "unlocked" };
        let display = match host::display_state() {
            Some((0, _)) => "off",
            Some(_) => "on",
            None => "unknown",
        };
        return (session.into(), display.into());
    }
    let session = majority(&blocks.iter().map(|b| if b["locked"] == true { "locked" } else { "unlocked" }).collect::<Vec<_>>());
    let display = majority(&blocks.iter().map(|b| display_of(b)).collect::<Vec<_>>());
    (session, display)
}

fn display_of(b: &Value) -> &'static str {
    match b["display"].as_u64() {
        Some(0) => "off",
        Some(_) => "on",
        None => "unknown",
    }
}

/// Tracks one head commit: one implementation comparison of `pairs` pairs, then one point per journey.
#[allow(clippy::too_many_arguments)]
pub(super) fn point(args: &Args, sides: &Sides, file: &Path, root: &Path, commit: &str, pairs: usize, statics: &Value, key: &Value) -> Result<(Vec<String>, String, Vec<Value>)> {
    let exp = store::new_id("exp");
    let hr = &sides.head_root;
    let (subject, commit_time) = (subject(hr, commit), commit_time(hr, commit));
    let depth: u64 = git::run(hr, &["rev-list", "--count", commit]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    progress(format!("\n{} {subject} · {exp}", short(commit)));
    let a = bench_args(args, sides, file, commit, pairs, &exp)?;
    let out = match crate::app::bench::run(&a).and_then(crate::app::unless_halted) {
        Ok(o) => o,
        Err(e) => {
            history::log_run(&json!({ "at": crate::app::record::now_iso(), "key": key, "commit": commit, "exp": exp, "outcome": "failed", "code": e.code, "error": e.message }))?;
            return Err(e);
        }
    };
    let r = &out.json;
    if !family::implementations(r) {
        return Err(Error::msg(format!("{exp} compared builds, not the two targets {} and {}", sides.base_target, sides.head_target)));
    }
    // A bench that ended early (a build that failed) left journeys out: no point is the commit's yet,
    // and the next track runs it again.
    if let Some(why) = r["stopped"]["why"].as_str() {
        let not_run: Vec<&str> = r["stopped"]["not_run"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let message = format!("{exp} ended early ({why}); not run: {}", not_run.join(", "));
        history::log_run(&json!({ "at": crate::app::record::now_iso(), "key": key, "commit": commit, "exp": exp, "outcome": "failed", "error": message }))?;
        return Err(Error::msg(message));
    }
    let (bn, hn) = pr::side_names(r);
    let base = base_identity(r, sides);
    let head = json!({ "target": sides.head_target, "name": hn, "commit": commit, "short": short(commit), "subject": subject });
    let (exp_session, exp_display) = experiment_state(r);
    let kinds: Vec<Value> = r["variants"].as_array().into_iter().flatten().map(|v| v["target_kind"].clone()).collect();
    let repo = repo_id(root);
    let file_rel = rel(root, file);
    let warns: Vec<Value> = r["warnings"].as_array().into_iter().flatten().map(|w| json!(format!("{}: {}", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or("")))).collect();
    let (mut ids, mut text, mut points) = (Vec::new(), String::new(), Vec::new());
    for s in r["series"].as_array().into_iter().flatten() {
        let metric = s["primary"]["metric"].as_str().unwrap_or("").to_string();
        let label = s["label"].as_str().unwrap_or("").to_string();
        let gap = family::gap(s);
        let pv = pair_values(s, &metric);
        let ln: Vec<f64> = pv.iter().filter(|(b, h)| *b > 0.0 && *h > 0.0).map(|(b, h)| (h / b).ln()).collect();
        let blocks: Vec<&Value> = s["blocks"].as_array().into_iter().flatten().collect();
        let displays: Vec<&str> = blocks.iter().map(|b| display_of(b)).collect();
        let (session, display) = if blocks.is_empty() {
            (exp_session.clone(), exp_display.clone())
        } else {
            (majority(&blocks.iter().map(|b| if b["locked"] == true { "locked" } else { "unlocked" }).collect::<Vec<_>>()), majority(&displays))
        };
        let why: Option<String> = if gap {
            Some(format!("parity gap: {}", family::gap_reason(r, s)))
        } else if s["not_run"] == true {
            Some(format!("not run: {}", s["halt"]["message"].as_str().unwrap_or("")))
        } else if stats::kind_of(&s["primary"]) == model::VerdictKind::Refused {
            Some(s["primary"]["verdict"].as_str().unwrap_or("refused").to_string())
        } else if ln.len() < pv.len() {
            Some(format!("a pair read 0 {metric}: no ratio"))
        } else if ln.len() < MIN_PAIRS {
            Some(blocks.iter().find_map(|b| b["refused"][&metric].as_str()).map(|w| format!("refused ({w})")).unwrap_or_else(|| format!("{} of {pairs} pairs gave {metric}", ln.len())))
        } else if s["session"] == "mixed" || blocks.iter().any(|b| b["locked"] != blocks[0]["locked"]) {
            Some("refused (SessionLocked): pairs ran both locked and unlocked".into())
        } else if displays.iter().any(|d| *d != displays[0]) {
            Some("the display turned off or on between the point's pairs".into())
        } else {
            None
        };
        let def = model::metric(&metric);
        let fingerprint = json!({
            "machine": statics, "power": r["env"]["power_plan"], "battery": r["env"]["on_battery"], "pin": r["plan"]["pin"],
            "runs": r["plan"]["runs_per_block"], "session": session, "display": display, "target_kinds": kinds, "headless": args.has("headless"), "engine": args.get("engine"),
        });
        let fam = json!({ "device": statics["device"], "repo": repo, "file": file_rel, "label": label, "metric": metric, "base_target": sides.base_target, "head_target": sides.head_target });
        let id = history::series_id(&fam, &fingerprint);
        if !history::exists(&id) {
            history::append(
                &id,
                &json!({
                    "t": "series", "id": id, "family": history::family_id(&fam), "comparison": "implementations", "device": statics["device"], "repo": repo, "file": file_rel,
                    "abs_file": file, "root": root, "head_root": hr, "label": label, "case": s["case"], "params": s["params"], "metric": metric,
                    "base_target": sides.base_target, "head_target": sides.head_target, "base_spec": sides.base, "base_name": bn, "head_name": hn,
                    "unit": "ln_ratio", "metric_unit": def.map(|d| d.unit), "better": def.map(|d| d.better), "kind": def.map(|d| d.kind), "words": def.map(|d| [d.words.0, d.words.1]),
                    "fingerprint": fingerprint, "created": crate::app::record::now_iso(),
                }),
            )?;
        }
        let (median, lo, hi, conf) = if ln.is_empty() {
            (f64::NAN, f64::NAN, f64::NAN, 0.0)
        } else {
            let (_, lo, hi, c) = stats::hodges_lehmann(&ln, 0.95);
            (detect::median(&ln), lo, hi, c)
        };
        let side_median = |i: usize| detect::median(&pv.iter().map(|p| if i == 0 { p.0 } else { p.1 }).collect::<Vec<_>>());
        let mut notes = warns.clone();
        notes.extend(s["warnings"].as_array().into_iter().flatten().map(|w| json!(format!("{}: {}", w["id"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or("")))));
        let valid = why.is_none();
        let p = json!({
            "t": "point", "at": crate::app::record::now_iso(), "commit": commit, "short": short(commit), "subject": subject, "commit_time": commit_time, "depth": depth,
            "ref": sides.head_ref, "exp": exp, "valid": valid, "why": why, "parity": if gap { "gap" } else { "compared" }, "gap": if gap { s["gap"].clone() } else { Value::Null },
            "values": ln, "median": finite(median), "lo": finite(lo), "hi": finite(hi), "confidence": conf, "n": ln.len(), "ratio": false, "scale": "ln_ratio",
            "base_median": finite(side_median(0)), "head_median": finite(side_median(1)), "base": base, "head": head, "notes": notes,
        });
        history::append(&id, &p)?;
        text += &match &why {
            None => {
                format!("  {}  {id}  {label} · {metric} {hn}/{bn} {} [{} … {}] ({} pairs)\n", short(commit), fmt::pct(median.exp() - 1.0), fmt::pct(lo.exp() - 1.0), fmt::pct(hi.exp() - 1.0), ln.len())
            }
            Some(w) => format!("  {}  {id}  {label} · no value: {w}\n", short(commit)),
        };
        points.push(json!({ "series": id, "commit": commit, "median": finite(median), "lo": finite(lo), "hi": finite(hi), "valid": valid, "why": why, "parity": p["parity"] }));
        ids.push(id);
    }
    history::log_run(&json!({ "at": crate::app::record::now_iso(), "key": key, "commit": commit, "exp": exp, "outcome": "ok", "series": ids }))?;
    Ok((ids, text, points))
}

/// The marks a series' points call for and its file does not hold yet: `parity` where a journey went
/// from a gap to compared (or back) between two tracked commits, `epoch` where the comparator's
/// identity changed. Appended, returned for the text.
pub(super) fn mark(s: &history::Series) -> Result<Vec<Value>> {
    let mut new = Vec::new();
    let (mut parity, mut base): (Option<&Value>, Option<&Value>) = (None, None);
    for p in &s.points {
        let mut found = Vec::new();
        if p["parity"].is_string() {
            if let Some(q) = parity.filter(|q| q["parity"] != p["parity"]) {
                found.push(json!({
                    "t": "parity", "id": history::mark_id("par", &s.id, p["commit"].as_str().unwrap_or(""), p["parity"].as_str().unwrap_or("")), "series": s.id, "label": s.meta["label"],
                    "from": q["parity"], "to": p["parity"], "commit": p["commit"], "short": p["short"], "subject": p["subject"], "after": q["commit"], "after_short": q["short"],
                    "reason": if p["parity"] == "gap" { p["why"].clone() } else { q["why"].clone() }, "at": crate::app::record::now_iso(),
                }));
            }
            parity = Some(p);
        }
        if p["base"]["identity"].is_string() {
            if let Some(q) = base.filter(|q| q["base"]["identity"] != p["base"]["identity"]) {
                found.push(json!({
                    "t": "epoch", "id": history::mark_id("epo", &s.id, p["commit"].as_str().unwrap_or(""), p["base"]["identity"].as_str().unwrap_or("")), "series": s.id,
                    "name": p["base"]["name"], "from": q["base"]["display"], "to": p["base"]["display"], "from_identity": q["base"]["identity"], "to_identity": p["base"]["identity"],
                    "commit": p["commit"], "short": p["short"], "subject": p["subject"], "after": q["commit"], "after_short": q["short"], "at": crate::app::record::now_iso(),
                }));
            }
            base = Some(p);
        }
        for m in found {
            if !s.marks.iter().chain(new.iter()).any(|k| k["id"] == m["id"]) {
                history::append(&s.id, &m)?;
                new.push(m);
            }
        }
    }
    Ok(new)
}

/// A mark as one line: `parity  Find in session: gap → compared at a1b2c3 (…)`.
pub(crate) fn mark_line(m: &Value) -> String {
    match m["t"].as_str() {
        Some("parity") => format!(
            "parity  {}: {} → {} at {} {}",
            m["label"].as_str().unwrap_or(""),
            m["from"].as_str().unwrap_or(""),
            m["to"].as_str().unwrap_or(""),
            m["short"].as_str().unwrap_or(""),
            m["subject"].as_str().unwrap_or("")
        ),
        _ => format!("epoch   {} at {}", comparator_updated(m), m["short"].as_str().unwrap_or("")),
    }
}

/// `Comparator updated (Electron app.exe (180 MB, 2026-11-01 09:12) → app.exe (181 MB, 2026-11-05 10:40))`.
pub(crate) fn comparator_updated(m: &Value) -> String {
    format!("Comparator updated ({} {} → {})", m["name"].as_str().unwrap_or("base"), m["from"].as_str().unwrap_or("?"), m["to"].as_str().unwrap_or("?"))
}

/// A port's valid points as the detector reads them: a commit tracked again counts once per comparator
/// (its latest point), and the points split into runs of one comparator identity in commit order.
/// Each run is a history of its own: a change is only ever looked for inside one, so a comparator
/// update is an epoch (a mark), never a suspicion to confirm.
pub(super) fn runs(s: &history::Series) -> Vec<Vec<&Value>> {
    let mut latest: Vec<&Value> = Vec::new();
    for p in s.valid() {
        let same = |q: &&Value| q["commit"] == p["commit"] && q["base"]["identity"] == p["base"]["identity"];
        match latest.iter().position(same) {
            // Points are in commit order, then the order they were tracked in: the later one stands.
            Some(i) => latest[i] = p,
            None => latest.push(p),
        }
    }
    let mut out: Vec<Vec<&Value>> = Vec::new();
    for p in latest {
        match out.last_mut() {
            Some(run) if run.last().is_some_and(|q| q["base"]["identity"] == p["base"]["identity"]) => run.push(p),
            _ => out.push(vec![p]),
        }
    }
    out
}
