//! A target started by a command, such as a dev server that builds and launches the app, and attached
//! through the debugging endpoint the command prints (SPEC §9.6, `command(...)` targets).

use super::cdp::Cdp;
use super::chromium::{Browser, Window};
use super::electron::Started;
use super::isolate::{Launching, epoch_ms, lines};
use crate::host;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One command target, as the bench file declares it.
#[derive(Clone, Debug)]
pub struct Spec {
    /// A shell command line, run in `cwd`; `{home}` and `{port}` are filled in as in `env`.
    pub run: String,
    pub cwd: PathBuf,
    /// Extra environment; `{home}` (a fresh folder per launch) and `{port}` (a free port) are filled in.
    pub env: Vec<(String, String)>,
    /// Environment variables taken away, `PREFIX_*` patterns allowed.
    pub unset: Vec<String>,
    /// The output text right before the endpoint: the first `ws://…` after it is attached to.
    pub attach: String,
    /// How long the command may take to print its endpoint.
    pub timeout: Duration,
}

impl Spec {
    /// The `command(...)` target of a bench file, run in `root` (the variant's folder).
    pub fn from_target(target: &Value, root: &Path) -> Spec {
        let strings = |k: &str| target[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Spec {
            run: target["run"].as_str().unwrap_or("").to_string(),
            cwd: target["cwd"].as_str().map_or_else(|| root.to_path_buf(), |c| root.join(c)),
            env: target["env"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default(),
            unset: strings("unset"),
            attach: target["attach"].as_str().unwrap_or("DevTools listening on").to_string(),
            timeout: Duration::from_millis(target["timeout"].as_u64().unwrap_or(600_000)),
        }
    }
}

/// A command line for the platform's shell. On Windows the line goes to `cmd /s /c "…"` verbatim:
/// cmd strips the outer quotes and keeps the line's own, which Rust's argument quoting would escape.
pub fn shell(line: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.raw_arg(format!("/d /s /c \"{line}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.args(["-c", line]);
        c
    }
}

/// Starts the command with `home` as `{home}`, waits for its endpoint and attaches. The browser's
/// profile is the home, so closing it removes the folder; the command's whole process tree ends with it.
pub fn launch(spec: &Spec, home: &Path, window: &Window, cpus: Option<u64>) -> Result<(Browser, Started)> {
    if spec.run.trim().is_empty() {
        return Err(Error::usage("a command target needs `run`, the command line to start"));
    }
    std::fs::create_dir_all(home)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let fill = |s: &str| s.replace("{home}", &home.display().to_string()).replace("{port}", &port.to_string());
    let mut cmd = shell(&fill(&spec.run));
    for (k, _) in std::env::vars() {
        if super::isolate::env_unset(&spec.unset, &k) {
            cmd.env_remove(&k);
        }
    }
    for (k, v) in &spec.env {
        cmd.env(k, fill(v));
    }
    cmd.envs(super::isolate::launch_env(home));
    cmd.current_dir(&spec.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let epoch = epoch_ms();
    // Its own group, so the dev server, the app and everything they spawn end together.
    let (mut child, group) = host::spawn_owned(&mut cmd, cpus, 0, Some(home)).map_err(|e| Error::msg(format!("start `{}`: {e}", spec.run)))?;
    let pid = child.id();
    let output = lines(&mut child);
    // A start that fails from here ends the command's processes and what runs for its home; the
    // folder is the caller's.
    let launching = Launching { pid, child: Some(child), group: Some(group), home: Some(home.to_path_buf()), fresh: false };
    // The lines read while waiting, replayed to the caller (milestones before the endpoint count too).
    let (tx, rx) = std::sync::mpsc::channel();
    let deadline = Instant::now() + spec.timeout;
    let ws = loop {
        match output.recv_timeout(Duration::from_millis(200)) {
            Ok((at, ns, line)) => {
                let found = line.find(&spec.attach).and_then(|i| line[i..].find("ws://").map(|j| i + j)).map(|start| line[start..].split_whitespace().next().unwrap_or("").to_string());
                let _ = tx.send((at, ns, line));
                if let Some(ws) = found.filter(|w| w.len() > "ws://".len()) {
                    break ws;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::msg(format!("`{}` exited before it printed \"{} ws://…\"", spec.run, spec.attach)));
            }
        }
        if Instant::now() > deadline {
            return Err(Error::unreachable(format!("`{}` printed no \"{} ws://…\" in {} s", spec.run, spec.attach, spec.timeout.as_secs())));
        }
    };
    // Every later line goes to the caller too.
    std::thread::spawn(move || {
        for item in output {
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    let cdp = Cdp::connect(&ws)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    let (child, group, profile) = launching.done();
    // The home is a fresh folder per launch (a caller that keeps it takes `profile`): it goes with the browser.
    let browser = Browser { cdp, ws, child, group, pid, profile, fresh: true, exe: PathBuf::from(&spec.run), version, window: window.clone() };
    Ok((browser, Started { epoch_ms: epoch, output: Some(rx), main: None }))
}
