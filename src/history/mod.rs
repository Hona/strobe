//! Tracked history (SPEC §25): per device, one append-only file per series, under `<state>/history/`.
//! The only writer of `history/`. A series is one case × target × params × primary metric of one bench
//! file in one repository, on one device and one environment fingerprint: a change to the OS, the
//! GPU driver, the power plan, the session's lock state or strobe's version starts a new series.

pub mod detect;

use crate::host::{self, paths};
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

/// `<state>/history/`: schedules and every device's series.
pub fn root() -> PathBuf {
    paths::sub("history")
}

/// This device's series. Never another device's: results are not pooled across machines.
pub fn dir() -> PathBuf {
    let d = root().join(host::device_name());
    let _ = std::fs::create_dir_all(&d);
    d
}

fn hash8(v: &Value) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(v.to_string().as_bytes()).iter().take(4).map(|x| format!("{x:02x}")).collect()
}

/// The series a family (repository, bench file, series label, metric) has on this device and fingerprint.
pub fn series_id(family: &Value, fingerprint: &Value) -> String {
    format!("ser_{}", hash8(&json!({ "family": family, "fingerprint": fingerprint })))
}

pub fn family_id(family: &Value) -> String {
    format!("fam_{}", hash8(family))
}

pub fn change_id(series: &str, good: &str, bad: &str) -> String {
    format!("chg_{}", &hash8(&json!([series, good, bad]))[..6])
}

fn path(id: &str) -> PathBuf {
    dir().join(format!("{id}.jsonl"))
}

/// Appends one event (`t`: `series`, `point` or `change`) to a series' file.
pub fn append(id: &str, event: &Value) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path(id))?;
    writeln!(f, "{event}")?;
    Ok(())
}

/// Appends a line to `history/<device>/runs.jsonl`: every track run, with its outcome.
pub fn log_run(event: &Value) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("runs.jsonl"))?;
    writeln!(f, "{event}")?;
    Ok(())
}

pub fn runs() -> Vec<Value> {
    read_lines(&dir().join("runs.jsonl"))
}

fn read_lines(p: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect()
}

/// One series folded from its events: points in commit order, changes at their latest state.
#[derive(Clone, Debug)]
pub struct Series {
    pub id: String,
    pub meta: Value,
    pub points: Vec<Value>,
    pub changes: Vec<Value>,
}

impl Series {
    /// Points that carry a value the detector may use.
    pub fn valid(&self) -> Vec<&Value> {
        self.points.iter().filter(|p| p["valid"] == true).collect()
    }

    pub fn has_commit(&self, commit: &str) -> bool {
        self.points.iter().any(|p| p["commit"] == commit && p["valid"] == true)
    }
}

pub fn exists(id: &str) -> bool {
    path(id).is_file()
}

pub fn load(id: &str) -> Option<Series> {
    let events = read_lines(&path(id));
    let meta = events.iter().find(|e| e["t"] == "series")?.clone();
    let mut points: Vec<Value> = events.iter().filter(|e| e["t"] == "point").cloned().collect();
    // Commit order: commits reachable from each (grows along the first-parent line), committer
    // time, then the order they were tracked in.
    let key = |p: &Value| (p["depth"].as_u64(), p["commit_time"].as_u64());
    points.sort_by(|a, b| key(a).cmp(&key(b)).then(a["at"].as_str().cmp(&b["at"].as_str())));
    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, Value> = BTreeMap::new();
    for e in events.iter().filter(|e| e["t"] == "change") {
        let cid = e["id"].as_str().unwrap_or("").to_string();
        let merged = match latest.remove(&cid) {
            // Each event says what changed; later fields win, earlier ones stay.
            Some(mut prev) => {
                for (k, v) in e.as_object().into_iter().flatten() {
                    prev[k] = v.clone();
                }
                prev
            }
            None => {
                order.push(cid.clone());
                e.clone()
            }
        };
        latest.insert(cid, merged);
    }
    let changes = order.into_iter().filter_map(|c| latest.remove(&c)).collect();
    Some(Series { id: id.to_string(), meta, points, changes })
}

/// Every series of this device, newest point first.
pub fn list() -> Vec<Series> {
    let mut out: Vec<Series> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_prefix("ser_").and_then(|n| n.strip_suffix(".jsonl")).map(|n| format!("ser_{n}"))
        })
        .filter_map(|id| load(&id))
        .collect();
    out.sort_by(|a, b| b.points.iter().filter_map(|p| p["at"].as_str()).max().cmp(&a.points.iter().filter_map(|p| p["at"].as_str()).max()));
    out
}

/// A series id or a unique prefix of one.
pub fn resolve(handle: &str) -> Result<String> {
    if exists(handle) {
        return Ok(handle.to_string());
    }
    let ids: Vec<String> = list().into_iter().map(|s| s.id).filter(|id| id.starts_with(handle)).collect();
    match ids.len() {
        1 => Ok(ids[0].clone()),
        0 => Err(Error::usage(format!("no tracked series {handle} on this device (strobe history lists them)"))),
        n => Err(Error::usage(format!("{handle} is ambiguous ({n} series)"))),
    }
}

/// The daemon's schedules (`strobe track --schedule`).
pub fn schedules() -> Vec<Value> {
    std::fs::read_to_string(root().join("schedules.json")).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok()).unwrap_or_default()
}

pub fn save_schedules(s: &[Value]) -> Result<()> {
    let p = root().join("schedules.json");
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// Where a confirmed change's alert payload is kept (and handed to the alert command).
pub fn alert_path(change: &str) -> PathBuf {
    let d = dir().join("alerts");
    let _ = std::fs::create_dir_all(&d);
    d.join(format!("{change}.json"))
}
