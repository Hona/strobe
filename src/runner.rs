//! The bench runner link: spawns `bun` with strobe's embedded SDK and speaks the runner protocol (SPEC §9.6).

use crate::host::{self, paths};
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const INDEX: &str = include_str!("../sdk/bench/src/index.ts");
const RUNNER: &str = include_str!("../sdk/bench/src/runner.ts");
const PRELOAD: &str = include_str!("../sdk/bench/src/preload.ts");

/// Writes the embedded SDK under the state dir (once per strobe version) and returns its folder.
fn sdk_dir() -> Result<PathBuf> {
    let dir = paths::sdk().join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&dir)?;
    for (name, body) in [("index.ts", INDEX), ("runner.ts", RUNNER), ("preload.ts", PRELOAD)] {
        let p = dir.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(body) {
            std::fs::write(&p, body)?;
        }
    }
    Ok(dir)
}

pub fn find_bun() -> Option<PathBuf> {
    let home = host::home();
    let exe = if cfg!(windows) { "bun.exe" } else { "bun" };
    host::find_exe("bun", &[home.join(".bun").join("bin").join(exe)])
}

pub struct Runner {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next: u64,
    pub log: PathBuf,
}

/// What the runner reports about a bench file.
pub type Describe = Value;

impl Runner {
    /// Spawns the runner for `file`; `cpus` pins it (and only it) to a CPU mask.
    pub fn spawn(file: &Path, log: PathBuf, cpus: Option<u64>) -> Result<Runner> {
        let bun = find_bun().ok_or_else(|| Error::msg("bench files need Bun").hint("install it from https://bun.sh"))?;
        let sdk = sdk_dir()?;
        let previous = cpus.and_then(host::set_affinity);
        let spawned = Command::new(bun)
            .arg("--preload")
            .arg(sdk.join("preload.ts"))
            .arg(sdk.join("runner.ts"))
            .arg(file)
            .env("STROBE_SDK_INDEX", sdk.join("index.ts"))
            .current_dir(file.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::fs::File::create(&log)?))
            .spawn();
        if previous.is_some() {
            host::set_affinity(previous.unwrap_or(0));
        }
        let mut child = spawned.map_err(|e| Error::msg(format!("spawn bun: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| Error::msg("runner stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::msg("runner stdout"))?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut runner = Runner { child, stdin, lines: rx, next: 1, log };
        runner.wait_ready()?;
        Ok(runner)
    }

    fn wait_ready(&mut self) -> Result<()> {
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(60)).map_err(|_| self.died("did not start"))?;
            if serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("ready").cloned()).is_some() {
                return Ok(());
            }
        }
    }

    fn died(&mut self, what: &str) -> Error {
        let _ = self.child.try_wait();
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        Error::msg(format!("the bench runner {what}\n{}", tail.join("\n")))
    }

    fn write(&mut self, v: &Value) -> Result<()> {
        writeln!(self.stdin, "{v}").map_err(|e| Error::msg(format!("runner: {e}")))?;
        self.stdin.flush().ok();
        Ok(())
    }

    /// Calls `op` and serves the runner's requests (driver calls, phases) until it answers.
    pub fn call(&mut self, op: &str, args: Value, timeout: Duration, mut serve: impl FnMut(&str, &Value) -> Result<Value>) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        self.write(&json!({ "id": id, "op": op, "args": args }))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(Error { code: 124, message: format!("{op} took longer than {} s", timeout.as_secs()), hint: None });
            }
            let line = match self.lines.recv_timeout(left) {
                Ok(l) => l,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Err(self.died("exited")),
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error").and_then(Value::as_str) {
                    let stack = msg.get("stack").and_then(Value::as_str).unwrap_or("");
                    let first = stack.lines().find(|l| l.contains(".bench.")).unwrap_or("").trim();
                    return Err(Error::msg(format!("{op}: {err}{}", if first.is_empty() { String::new() } else { format!(" ({first})") })));
                }
                return Ok(msg.get("ok").cloned().unwrap_or(Value::Null));
            }
            if let Some(rid) = msg.get("id").and_then(Value::as_str) {
                let req_op = msg.get("op").and_then(Value::as_str).unwrap_or("").to_string();
                let args = msg.get("args").cloned().unwrap_or(Value::Null);
                let reply = match serve(&req_op, &args) {
                    Ok(v) => json!({ "id": rid, "ok": v }),
                    Err(e) => json!({ "id": rid, "error": e.message }),
                };
                self.write(&reply)?;
            }
        }
    }

    pub fn describe(&mut self) -> Result<Describe> {
        self.call("describe", json!({}), Duration::from_secs(60), |_, _| Err(Error::msg("no driver during describe")))
    }

    pub fn exit(&mut self) {
        let _ = self.write(&json!({ "id": 0, "op": "exit" }));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let _ = self.child.kill();
    }
}
