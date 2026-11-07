//! The bench runner link: spawns `bun` with strobe's embedded SDK and speaks the runner protocol (SPEC §9.6).

use crate::host::{self, paths};
use crate::model::{Error, Refusal, Result};
use crate::store::actions::{ActionLog, Hook, Outcome};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const INDEX: &str = include_str!("../sdk/bench/src/index.ts");
const RUNNER: &str = include_str!("../sdk/bench/src/runner.ts");
const PRELOAD: &str = include_str!("../sdk/bench/src/preload.ts");
const HOST: &str = include_str!("../sdk/bench/src/host.mjs");

/// An SDK folder no strobe build has used for this long is removed (`sdk_dir`).
const SDK_UNUSED: Duration = Duration::from_secs(30 * 24 * 3600);

/// Writes the embedded SDK under the state dir (once per strobe build) and returns its folder. Each
/// use touches its `.used` file; the folders of other builds unused for 30 days go (one per build, they
/// would pile up with every update).
pub fn sdk_dir() -> Result<PathBuf> {
    let root = paths::sdk();
    let dir = root.join(env!("STROBE_BUILD_ID"));
    std::fs::create_dir_all(&dir)?;
    // Marked used before anything else, so another strobe's prune never takes a folder being filled.
    let now = std::time::SystemTime::now();
    if let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".used")) {
        let _ = f.set_modified(now);
    }
    for (name, body) in [("index.ts", INDEX), ("runner.ts", RUNNER), ("preload.ts", PRELOAD), ("host.mjs", HOST)] {
        let p = dir.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(body) {
            paths::write_atomic(&p, body.as_bytes())?;
        }
    }
    for other in std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && *p != dir) {
        // A folder from before `.used` counts from when it was written.
        let used = std::fs::metadata(other.join(".used")).or_else(|_| std::fs::metadata(&other)).and_then(|m| m.modified());
        if used.is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > SDK_UNUSED)) {
            let _ = std::fs::remove_dir_all(&other);
        }
    }
    Ok(dir)
}

pub fn find_bun() -> Option<PathBuf> {
    let home = host::home();
    let exe = if cfg!(windows) { "bun.exe" } else { "bun" };
    host::find_exe("bun", &[home.join(".bun").join("bin").join(exe)])
}

/// A bench hook that failed, as `RunIncomplete`: `what` names it once (the runner's own `<op>: `
/// prefix dropped), `extra` follows the message (a screenshot), and a fix the driver gave (a missing
/// permission and where to grant it) replaces the generic `fix`. A hook that failed because a
/// validator refused one of its driver calls (`Runner::call`) is that validator's refusal, named
/// for the hook: a covered window is `WindowHidden`, which a bench waits out, not a broken case.
pub fn incomplete(what: &str, op: &str, e: Error, extra: &str, fix: &str) -> Error {
    let prefix = format!("{op}: ");
    let msg = e.message.strip_prefix(&prefix).unwrap_or(&e.message);
    match e.refusal {
        Some(r) => {
            let why = msg.strip_prefix(&format!("{}: ", r.validator)).unwrap_or(msg);
            Refusal { summary: format!("{what}: {why}{extra}"), detail: None, ..*r }.into()
        }
        None => Error::refused("RunIncomplete", format!("{what}: {msg}{extra}"), e.hint.as_deref().unwrap_or(fix)),
    }
}

/// The runner process, in a group of its own with everything its hooks start: dropped (after `exit`
/// or without it), the group ends.
pub struct Runner {
    /// Taken when it ends.
    child: Option<host::spawn::Owned>,
    pid: u32,
    conn: TcpStream,
    lines: Receiver<String>,
    next: u64,
    pub log: PathBuf,
    /// The experiment's seed, once given (`seed`).
    seed: Option<u32>,
    /// Some hook read `ctx.seed` or called `ctx.random()` (SPEC §14.2: only then is `--seed` the workload).
    used_random: bool,
    /// What the current block's hooks did to the target (`store::actions`), since its `block` call,
    /// until the block's recording takes it (`take_actions`).
    actions: RefCell<ActionLog>,
    /// Whether blocks from now log their actions (`collect_actions`): only those that keep them.
    collect: bool,
    /// The block's `run` calls so far, the current hook's open `ctx.untimed` stretch and `ctx.phase`s.
    runs: u32,
    untimed: bool,
    phases: Vec<String>,
}

/// What the runner reports about a bench file.
pub type Describe = Value;

impl Runner {
    /// Spawns the runner for `file`; `cpus` pins it (and only it) to a CPU mask.
    pub fn spawn(file: &Path, log: PathBuf, cpus: Option<u64>) -> Result<Runner> {
        let bun = find_bun().ok_or_else(|| Error::msg("bench files need Bun").hint("install it from https://bun.sh"))?;
        let sdk = sdk_dir()?;
        // The protocol runs over a loopback socket, never the runner's stdio: a bench file's hooks spawn
        // processes, and Bun's `$` hands stdin on to them, which stalled on a protocol pipe that never
        // closes. stdin is empty; stdout and stderr are the experiment's runner.log.
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let mut cmd = Command::new(bun);
        cmd.arg("--preload")
            .arg(sdk.join("preload.ts"))
            .arg(sdk.join("runner.ts"))
            .arg(file)
            .env("STROBE_SDK_INDEX", sdk.join("index.ts"))
            .env("STROBE_RUNNER_PORT", listener.local_addr()?.port().to_string())
            .current_dir(file.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        // An early return from here drops the child, which ends the runner and what it started.
        let mut child = host::spawn_owned(&mut cmd, cpus, 0, None).map_err(|e| Error::msg(format!("spawn bun: {e}")))?;
        listener.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let conn = loop {
            match listener.accept() {
                Ok((conn, _)) => break conn,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(Error::msg(format!("runner connection: {e}"))),
            }
            if std::time::Instant::now() > deadline || child.exited().is_some() {
                drop(child);
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(Error::msg(format!("the bench runner did not start\n{}", tail.join("\n"))));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        conn.set_nonblocking(false)?;
        conn.set_nodelay(true)?;
        let reader = conn.try_clone()?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let pid = child.pid();
        let mut runner = Runner { child: Some(child), pid, conn, lines: rx, next: 1, log, seed: None, used_random: false, actions: RefCell::default(), collect: false, runs: 0, untimed: false, phases: Vec::new() };
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
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        Error::msg(format!("the bench runner {what}\n{}", tail.join("\n")))
    }

    fn write(&mut self, v: &Value) -> Result<()> {
        writeln!(self.conn, "{v}").map_err(|e| Error::msg(format!("runner: {e}")))?;
        self.conn.flush().ok();
        Ok(())
    }

    /// Calls `op` and serves the runner's requests (driver calls, phases) until it answers. The hooks'
    /// actions on the target go into the block's log (`actions`) as they are served: a host clock read
    /// at each end, no call to the target.
    pub fn call(&mut self, op: &str, args: Value, timeout: Duration, mut serve: impl FnMut(&str, &Value) -> Result<Value>) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        if op == "block" {
            self.actions.borrow_mut().clear(self.seed);
            self.runs = 0;
        }
        let hook = Hook::of_op(op);
        if hook == Some(Hook::Run) {
            self.runs += 1;
        }
        // A stretch or phase a failed hook left open is none of the next one's.
        if hook.is_some() {
            self.untimed = false;
            self.phases.clear();
        }
        // A reset belongs to the run before it.
        let run = matches!(hook, Some(Hook::Run | Hook::Reset)).then(|| self.runs.checked_sub(1)).flatten();
        self.write(&json!({ "id": id, "op": op, "args": args }))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(Error::timeout(format!("{op} took longer than {} s", timeout.as_secs())));
            }
            let line = match self.lines.recv_timeout(left) {
                Ok(l) => l,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Err(self.died("exited")),
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                self.used_random |= msg["used_random"] == true;
                if let Some(err) = msg.get("error").and_then(Value::as_str) {
                    let stack = msg.get("stack").and_then(Value::as_str).unwrap_or("");
                    let first = stack.lines().find(|l| l.contains(".bench.")).unwrap_or("").trim();
                    let message = format!("{op}: {err}{}", if first.is_empty() { String::new() } else { format!(" ({first})") });
                    let hint = msg.get("hint").and_then(Value::as_str).map(str::to_string);
                    // A driver call a validator refused (a click on a covered window), rethrown by the
                    // hook: the refusal rides on the error frame both ways, so it keeps its validator.
                    let refusal = msg.get("refusal").and_then(|r| serde_json::from_value::<crate::model::Refusal>(r.clone()).ok());
                    return Err(match refusal {
                        Some(r) => Error { message, ..Error::from(r) },
                        None => Error { hint, ..Error::msg(message) },
                    });
                }
                return Ok(msg.get("ok").cloned().unwrap_or(Value::Null));
            }
            if let Some(rid) = msg.get("id").and_then(Value::as_str) {
                let req_op = msg.get("op").and_then(Value::as_str).unwrap_or("").to_string();
                let args = msg.get("args").cloned().unwrap_or(Value::Null);
                let action = hook.filter(|_| self.collect).and_then(|h| ActionLog::of_request(h, run, self.untimed, self.phases.last().cloned(), &req_op, &args, host::now_ns()));
                crate::driver::take_sending();
                let served = serve(&req_op, &args);
                if let Some(mut a) = action {
                    // What the driver said it sent: its send time, a click's press, what it aimed at.
                    let sending = crate::driver::take_sending();
                    a.t_ns = sending.t_ns.unwrap_or(a.t_ns);
                    (a.press_ns, a.hit, a.measured) = (sending.press_ns, sending.hit, sending.measured);
                    a.end_ns = host::now_ns();
                    a.outcome = Outcome::of(&served);
                    self.actions.borrow_mut().push(a);
                }
                if served.is_ok() {
                    let start = args["edge"] == "start";
                    match req_op.as_str() {
                        "untimed" => self.untimed = start,
                        "phase" if start => self.phases.push(args["name"].as_str().unwrap_or("").to_string()),
                        "phase" => {
                            self.phases.pop();
                        }
                        _ => {}
                    }
                }
                let reply = match served {
                    Ok(v) => json!({ "id": rid, "ok": v }),
                    Err(e) => {
                        let mut reply = json!({ "id": rid, "error": e.message });
                        if let Some(h) = &e.hint {
                            reply["hint"] = json!(h);
                        }
                        if let Some(r) = &e.refusal {
                            reply["refusal"] = json!(r);
                        }
                        reply
                    }
                };
                self.write(&reply)?;
            }
        }
    }

    /// The runner's process, for its CPU in a block's overhead.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Gives the runner the experiment's seed (`--seed`, or drawn), before any block: hooks get each
    /// case's seed from it (`ctx.seed`, `ctx.random()`).
    pub fn seed(&mut self, seed: u32) -> Result<()> {
        self.seed = Some(seed);
        self.call("seed", json!({ "seed": seed }), Duration::from_secs(60), |_, _| Err(Error::msg("no driver while seeding")))?;
        Ok(())
    }

    /// Whether some hook so far read `ctx.seed` or called `ctx.random()`.
    pub fn used_random(&self) -> bool {
        self.used_random
    }

    /// Whether the blocks from now log their actions: only one whose recording (or `--check` result)
    /// keeps them does, so an ordinary timed block reads no clock and copies nothing for them.
    pub fn collect_actions(&mut self, on: bool) {
        self.collect = on;
    }

    /// The block's per-input latency outcomes (by send time), joined to its run actions once the runs
    /// are measured (`ActionLog::join`); `runs`: the measured runs' host-clock spans.
    pub fn join_samples(&self, each: &[(u64, crate::store::actions::Sample)], runs: &[(u64, u64)]) {
        self.actions.borrow_mut().join(each, runs);
    }

    /// The current (or last) block's actions since its `block` call, taken: written once.
    pub fn take_actions(&self) -> ActionLog {
        let mut log = self.actions.borrow_mut();
        let seed = log.seed;
        std::mem::replace(&mut *log, ActionLog { seed, ..ActionLog::default() })
    }

    pub fn describe(&mut self) -> Result<Describe> {
        self.call("describe", json!({}), Duration::from_secs(60), |_, _| Err(Error::msg("no driver during describe")))
    }

    /// Asks the runner to exit (its `close` hooks have run), up to 3 s, then ends its group.
    pub fn exit(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        let _ = self.write(&json!({ "id": 0, "op": "exit" }));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && child.exited().is_none() {
            std::thread::sleep(Duration::from_millis(30));
        }
        child.end();
    }
}
