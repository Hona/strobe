//! Recordings and experiments on disk: the only writer (SPEC §14).

use crate::host::{self, paths};
use crate::model::{Error, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod actions;
pub mod ledger;

/// A short random id, `<prefix>_` + 6 hex from the OS's random source, that names no recording or
/// experiment yet and that this process never handed out before (two ids in one clock tick were
/// once the same, and the second recording wrote into the first's folder).
pub fn new_id(prefix: &str) -> String {
    static ISSUED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut issued = ISSUED.lock().unwrap_or_else(|e| e.into_inner());
    let issued = issued.get_or_insert_with(HashSet::new);
    let roots = [paths::recordings(), paths::experiments()];
    loop {
        // A counter over the clock stands in should the OS's random source ever fail: each try draws
        // another id, even within one clock tick.
        static FALLBACK: AtomicU64 = AtomicU64::new(0);
        let hex = paths::csprng_hex(3).unwrap_or_else(|_| format!("{:06x}", (host::now_ns() ^ FALLBACK.fetch_add(0x9E37_79B9, Ordering::Relaxed)) & 0xFF_FFFF));
        let id = format!("{prefix}_{hex}");
        if !roots.iter().any(|r| r.join(&id).exists()) && issued.insert(id.clone()) {
            return id;
        }
    }
}

/// A new recording's id and its folder, made by this call alone (a folder that already exists:
/// another process took the id, draws another), held as a `Draft` until its meta.json is written.
pub fn new_recording() -> Result<(String, Draft)> {
    let (id, dir) = claim("rec", &paths::recordings())?;
    Ok((id, Draft(Some(dir))))
}

/// Whether `id` may name a new recording: `rec_` and letters, digits, `_` or `-`, and no folder yet
/// (checked again when it is claimed). A command checks it before it launches anything.
pub fn free_recording_id(id: &str) -> Result<()> {
    let ok = id.strip_prefix("rec_").is_some_and(|r| !r.is_empty() && r.len() <= 64 && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    if !ok {
        return Err(Error::usage(format!("--id {id}: a recording id is rec_ and letters, digits, _ or -")));
    }
    if paths::recordings().join(id).exists() {
        return Err(Error::usage(format!("--id {id}: that recording exists already; pick another id")));
    }
    Ok(())
}

/// The folder of a recording whose id the caller chose (`--id`, so an MCP call that outlives its
/// timeout can be followed), made by this call alone: an id that is not `rec_` and letters, digits,
/// `_` or `-`, or that names a folder already, is a usage error, so no draft ever deletes another
/// recording.
pub fn claim_recording(id: &str) -> Result<Draft> {
    free_recording_id(id)?;
    let dir = paths::recordings().join(id);
    std::fs::create_dir_all(paths::recordings())?;
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok(Draft(Some(dir))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::usage(format!("--id {id}: that recording exists already; pick another id"))),
        Err(e) => Err(Error::msg(format!("create {}: {e}", dir.display()))),
    }
}

/// `new_recording` for an experiment.
pub fn new_experiment() -> Result<(String, PathBuf)> {
    claim("exp", &paths::experiments())
}

fn claim(prefix: &str, root: &Path) -> Result<(String, PathBuf)> {
    for _ in 0..100 {
        let id = new_id(prefix);
        let dir = root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::msg(format!("create {}: {e}", dir.display()))),
        }
    }
    Err(Error::msg(format!("no free {prefix}_ id in {}", root.display())))
}

/// Removes an unfinished recording (`Draft`): none of it is kept.
fn discard(dir: &Path) {
    if dir.parent().is_some_and(|p| p == paths::recordings() || p == paths::experiments()) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

pub fn experiment_dir(id: &str) -> Result<PathBuf> {
    let dir = paths::experiments().join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Writes a file whole or not at all (`host::paths::write_atomic`): a reader never sees half a
/// result.json, and a crash keeps the last complete one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    paths::write_atomic(path, bytes).map_err(|e| Error::msg(format!("write {}: {e}", path.display())))
}

/// A file of a recording written in pieces (a trace or a heap snapshot streamed from the target),
/// whole or not at all: it appears at `path` only on `commit` (`host::paths::Atomic`). What it holds
/// is the target's data verbatim (SPEC rule 9), so only its owner may read it, from its creation.
pub fn create(path: &Path) -> Result<paths::Atomic> {
    paths::Atomic::create(path, 0o600).map_err(|e| Error::msg(format!("create {}: {e}", path.display())))
}

/// Moves a finished file another program wrote (an encoder's video, in the run folder) into a
/// recording's folder as `name`, owner-only like everything of the target's (rule 9). Same volume
/// (the state folder): a rename, so it appears whole.
pub fn adopt(dir: &Path, name: &str, from: &Path) -> Result<PathBuf> {
    let to = dir.join(name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(from, std::fs::Permissions::from_mode(0o600));
    }
    if std::fs::rename(from, &to).is_err() {
        std::fs::copy(from, &to).map_err(|e| Error::msg(format!("keep {}: {e}", to.display())))?;
    }
    Ok(to)
}

/// A recording's folder until it is complete: dropped before `keep` (an error, a refusal), the folder
/// and all it holds go, so no recording is left without its `meta.json`. Only `new_recording` and
/// `claim_recording` make one, over a folder they created, so a draft never deletes a folder it did
/// not make.
pub struct Draft(Option<PathBuf>);

impl Draft {
    pub fn dir(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    pub fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Draft {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            discard(&dir);
        }
    }
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)
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
