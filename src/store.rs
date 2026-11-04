//! Recordings and experiments on disk: the only writer (SPEC §14).

use crate::host::{self, paths};
use crate::model::{Error, Result};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

pub mod ledger;

/// A short random-looking id: `<prefix>_` + 6 hex.
pub fn new_id(prefix: &str) -> String {
    let seed = host::now_ns() ^ (std::process::id() as u64).rotate_left(32);
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15);
    x ^= x >> 29;
    format!("{prefix}_{:06x}", x & 0xFFFFFF)
}

pub fn recording_dir(id: &str) -> Result<PathBuf> {
    let dir = paths::recordings().join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn experiment_dir(id: &str) -> Result<PathBuf> {
    let dir = paths::experiments().join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn write_json(path: &std::path::Path, value: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn append_index(line: &Value) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(paths::experiments().join("index.jsonl"))?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Resolves a handle or a unique prefix (`rec_3f`, `exp_8c`) to its directory. `bench --id` names an
/// experiment freely, so a handle without either prefix may be a recording or an experiment.
pub fn resolve(handle: &str) -> Result<PathBuf> {
    let roots = if handle.starts_with("exp_") {
        vec![paths::experiments()]
    } else if handle.starts_with("rec_") {
        vec![paths::recordings()]
    } else {
        vec![paths::recordings(), paths::experiments()]
    };
    if let Some(exact) = roots.iter().map(|root| root.join(handle)).find(|p| p.is_dir()) {
        return Ok(exact);
    }
    let matches: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|e| e.file_name().to_string_lossy().starts_with(handle))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(Error::usage(format!("no recording or experiment {handle}"))),
        _ => Err(Error::usage(format!("{handle} is ambiguous ({} matches)", matches.len()))),
    }
}

/// Whether a resolved handle is an experiment (bench or bisect) rather than a recording.
pub fn is_experiment(dir: &std::path::Path) -> bool {
    dir.parent().is_some_and(|parent| parent == paths::experiments())
}

/// Every line of `experiments/index.jsonl`, oldest first; lines that do not parse are skipped.
pub fn index() -> Vec<Value> {
    std::fs::read_to_string(paths::experiments().join("index.jsonl")).map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()).unwrap_or_default()
}

pub fn read_json(path: &std::path::Path) -> Result<Value> {
    use serde::Deserialize;
    let bytes = std::fs::read(path)?;
    // Profiles nest one level per stack frame; serde_json stops at 128 by default.
    let mut de = serde_json::Deserializer::from_slice(&bytes);
    de.disable_recursion_limit();
    Ok(Value::deserialize(&mut de)?)
}

/// The newest recordings or experiments, newest first.
pub fn list(prefix: &str, limit: usize) -> Vec<(String, std::time::SystemTime)> {
    let root = if prefix == "exp" { paths::experiments() } else { paths::recordings() };
    let mut v: Vec<(String, std::time::SystemTime)> = std::fs::read_dir(root)
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).filter_map(|e| Some((e.file_name().to_string_lossy().to_string(), e.metadata().ok()?.modified().ok()?))).collect())
        .unwrap_or_default();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v.truncate(limit);
    v
}
