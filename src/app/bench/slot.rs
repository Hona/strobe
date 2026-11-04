//! The device's bench slot (SPEC §10.1 `ConcurrentBench`): one block at a time across every bench on
//! this device. A bench takes it for a block and keeps it for its next one, never while it builds or
//! waits for an unlock, so a bench waiting for a person never blocks one that can run. Waiting benches
//! take turns in the order they asked: a holder hands the slot over after its block once one queues.
//!
//! Every running bench registers itself in `<run>/benches/<pid>.json` with what it is doing, which
//! `strobe status` lists. The slot is an exclusive lock on `<run>/bench.slot`, which the OS releases
//! when its holder dies.

use crate::host::{self, paths};
use crate::model::Result;
use serde_json::{Value, json};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn dir() -> PathBuf {
    let d = paths::run().join("benches");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn now_ns() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Whether `pid` still is a strobe (a registration a killed bench left can name a reused pid).
fn alive(pid: u32) -> bool {
    pid == std::process::id() || host::exe_of(pid).and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())).is_some_and(|n| n.starts_with("strobe"))
}

/// Whether a bench of this device runs inside the daemon (benches started over MCP do).
pub fn daemon_benching() -> bool {
    list().iter().any(|b| b["pid"].as_u64().and_then(|p| host::exe_of(p as u32)).and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())).is_some_and(|n| n.starts_with("strobew")))
}

/// This bench's entry in the registry; dropping the last clone removes it.
#[derive(Clone)]
pub struct Registration(Arc<Inner>);

struct Inner {
    path: PathBuf,
    /// The experiment: benches started over MCP share the daemon's pid.
    exp: String,
    entry: Mutex<Value>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Registration {
    pub fn new(exp: &str, file: &str) -> Registration {
        let pid = std::process::id();
        let entry = json!({ "pid": pid, "exp": exp, "file": file, "state": "starting", "why": "", "since": super::super::record::now_iso() });
        let path = dir().join(format!("{pid}-{exp}.json"));
        let r = Registration(Arc::new(Inner { path, exp: exp.to_string(), entry: Mutex::new(entry) }));
        r.write();
        r
    }

    fn write(&self) {
        let e = self.0.entry.lock().unwrap().clone();
        let _ = std::fs::write(&self.0.path, e.to_string());
    }

    /// What the bench does now: `building`, `waiting` (with why), `queued`, `running`.
    pub fn set(&self, state: &str, why: impl Into<String>) {
        {
            let mut e = self.0.entry.lock().unwrap();
            if e["state"] != state {
                e["since"] = json!(super::super::record::now_iso());
            }
            e["state"] = json!(state);
            e["why"] = json!(why.into());
            if state != "queued" {
                e.as_object_mut().map(|o| o.remove("queued_at"));
            }
        }
        self.write();
    }

    fn queue(&self, why: &str) -> u128 {
        let at = now_ns();
        {
            let mut e = self.0.entry.lock().unwrap();
            e["state"] = json!("queued");
            e["why"] = json!(why);
            e["since"] = json!(super::super::record::now_iso());
            e["queued_at"] = json!(at.to_string());
        }
        self.write();
        at
    }

    /// Waits for the slot: until no live bench asked before this one and the lock is free. Returns
    /// the slot and how long it waited.
    pub fn acquire(&self, what: &str) -> Result<(Slot, Duration)> {
        let started = Instant::now();
        let file = File::options().create(true).truncate(false).write(true).open(paths::run().join("bench.slot"))?;
        let mine = self.queue(what);
        let me = self.0.exp.as_str();
        let mut shown = String::new();
        let mut reminded = Instant::now();
        loop {
            let others = list();
            let ahead: Vec<&Value> = others
                .iter()
                .filter(|b| b["exp"] != me && b["state"] == "queued" && b["queued_at"].as_str().and_then(|t| t.parse::<u128>().ok()).is_some_and(|t| t < mine))
                .collect();
            // Builds before the slot held `bench.lock` for a whole experiment.
            // This build writes `bench.lock` too while it holds the slot: a pid that registered as a bench
            // is a current strobe (named below as the one running a block), not an older one.
            let old = std::fs::read_to_string(legacy())
                .ok()
                .and_then(|t| t.trim().parse::<u32>().ok())
                .filter(|p| *p != std::process::id() && alive(*p) && !others.iter().any(|b| b["pid"].as_u64() == Some(*p as u64)));
            if ahead.is_empty() && old.is_none() && file.try_lock().is_ok() {
                // And they still read it: while a block runs, one starting refuses `ConcurrentBench`.
                let _ = std::fs::write(legacy(), std::process::id().to_string());
                break;
            }
            let running = others.iter().find(|b| b["state"] == "running" && b["exp"] != me);
            let why = match (old, running, ahead.len()) {
                (Some(pid), _, _) => format!("{what}: waiting for the bench of an older strobe (pid {pid}) to finish"),
                (None, Some(r), n) => format!("{what}: waiting for the bench slot (pid {} runs a block of {}{})", r["pid"], r["exp"].as_str().unwrap_or(""), if n > 0 { format!("; {n} queued before this one") } else { String::new() }),
                (None, None, n) if n > 0 => format!("{what}: waiting for the bench slot ({n} queued before this one)"),
                _ => format!("{what}: waiting for the bench slot"),
            };
            if why != shown {
                super::progress(format!("  {why}"));
                let mut e = self.0.entry.lock().unwrap();
                e["why"] = json!(why.clone());
                drop(e);
                self.write();
                shown = why;
                reminded = Instant::now();
            } else if reminded.elapsed() >= Duration::from_secs(60) {
                // A wait has no deadline: say how long it has been and what ends it, once a minute.
                let holder = old.or_else(|| running.and_then(|r| r["pid"].as_u64()).map(|p| p as u32));
                let ends = match holder {
                    Some(pid) => format!("it ends when pid {pid} releases the slot or exits; stop that process if it is stuck"),
                    None => "it ends when the benches ahead finish; strobe status lists them".to_string(),
                };
                super::progress(format!("  still waiting after {} ({ends})", super::super::fmt::ms_duration(started.elapsed().as_millis() as u64)));
                reminded = Instant::now();
            }
            if super::stop_requested() {
                self.set("idle", "");
                return Err(super::stop_error());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.set("running", what);
        Ok((Slot { file: Some(file), reg: self.clone() }, started.elapsed()))
    }
}

/// The held slot; dropping it frees it for the next bench in line.
pub struct Slot {
    file: Option<File>,
    reg: Registration,
}

fn legacy() -> PathBuf {
    paths::run().join("bench.lock")
}

impl Drop for Slot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(legacy());
        if let Some(f) = self.file.take() {
            let _ = f.unlock();
        }
        self.reg.set("idle", "");
    }
}

/// Whether another bench waits for the slot: then the holder hands it over after its block.
pub fn others_queued(exp: &str) -> bool {
    list().iter().any(|b| b["exp"] != exp && b["state"] == "queued")
}

/// Every live bench on this device, oldest first: pid, exp, file, state, why, since.
pub fn list() -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let Some(v) = std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        let pid = v["pid"].as_u64().unwrap_or(0) as u32;
        if pid == 0 || !alive(pid) {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        out.push(v);
    }
    out.sort_by(|a, b| a["since"].as_str().cmp(&b["since"].as_str()));
    out
}
