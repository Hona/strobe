//! Launching an installed Electron app in an isolated home (`isolate.rs`) with a debugging port, and
//! attaching to it over CDP (SPEC §13.2).

use super::cdp::Cdp;
use super::chromium::{Browser, Window};
use super::isolate::{self, App, Guard, Home, Launching};
use crate::host;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Launches `app` in a fresh private home (seeded from `seed`) with a debugging port.
pub fn launch(app: &App, seed: Option<&Path>, window: &Window, cpus: Option<u64>) -> Result<(Browser, Guard)> {
    launch_with(app, seed, window, cpus, &Extra::default()).map(|(b, g, _)| (b, g))
}

/// What a timed launch adds: arguments (such as `--inspect`), the app's output line by line, and a
/// home of the caller's (prepared by a hook, or kept warm across launches) instead of a new one.
#[derive(Clone, Debug, Default)]
pub struct Extra {
    pub args: Vec<String>,
    pub output: bool,
    pub home: Option<PathBuf>,
    /// A home [`isolate::prepare_home`] already made ready (a hook ran against it); `home` and the seed
    /// are then not used.
    pub prepared: Option<Home>,
    /// Starts the main process paused (`--inspect-brk` on this port) and runs it once a CPU profile of
    /// it is recording from its first statement: an attribution launch.
    pub profile_main: Option<u16>,
}

/// When the process was spawned (Unix epoch, ms) and, when asked, its stdout and stderr lines with
/// the time each arrived.
pub struct Started {
    pub epoch_ms: f64,
    pub output: Option<std::sync::mpsc::Receiver<crate::target::isolate::OutputLine>>,
    /// The main process's inspector, profiling, with `extra.profile_main`.
    pub main: Option<Arc<Cdp>>,
}

/// Connects to a paused main process's inspector, starts its CPU profiler at 200 µs and lets it run.
fn profile_from_start(port: u16) -> Result<Arc<Cdp>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let ws = loop {
        let found = super::chromium::http_get(&format!("127.0.0.1:{port}"), "/json/list")
            .ok()
            .and_then(|b| serde_json::from_str::<Value>(&b).ok())
            .and_then(|v| v.as_array().and_then(|a| a.first()).and_then(|t| t["webSocketDebuggerUrl"].as_str().map(str::to_string)));
        if let Some(ws) = found {
            break ws;
        }
        if Instant::now() > deadline {
            return Err(Error::msg("the app's main process opened no inspector in 30 s").hint("an Electron fuse (EnableNodeCliInspectArguments) may turn --inspect off in this build"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let cdp = Cdp::connect(&ws)?;
    cdp.send("Profiler.enable", json!({}), None)?;
    cdp.send("Profiler.setSamplingInterval", json!({ "interval": 200 }), None)?;
    cdp.send("Profiler.start", json!({}), None)?;
    cdp.send("Runtime.runIfWaitingForDebugger", json!({}), None)?;
    Ok(cdp)
}

/// [`launch`], also returning the spawn time and, with `extra.output`, the app's output.
pub fn launch_with(app: &App, seed: Option<&Path>, window: &Window, cpus: Option<u64>, extra: &Extra) -> Result<(Browser, Guard, Started)> {
    if isolate::interpreter(&app.exe) {
        return Err(Error::usage(format!(
            "{} is a script runtime, not a Chromium or Electron app: an exe: or app: target launches with a debugging port (--remote-debugging-port) it does not take",
            app.exe.display()
        ))
        .hint("measure a script with a bench script() target, or a running process from outside with strobe record pid:<pid>"));
    }
    let guard = Guard::take(app);
    isolate::live(app, &guard)?;
    let prepared = match &extra.prepared {
        Some(h) => h.clone(),
        None => isolate::prepare_home(app, seed, extra.home.clone())?,
    };
    let home = prepared.dir.clone();
    let mut args = extra.args.clone();
    if let Some(port) = extra.profile_main {
        args.push(format!("--inspect-brk={port}"));
    }
    args.push("--remote-debugging-port=0".into());
    // A launch that fails from here ends what it started; its home goes when the launch made it.
    let spawned = match isolate::spawn(app, &prepared, &args, &[], extra.output, cpus) {
        Ok(s) => s,
        Err(e) => {
            isolate::release_home(&home);
            if prepared.fresh {
                isolate::remove_home(&home);
            }
            return Err(e);
        }
    };
    let (pid, epoch, output) = (spawned.pid, spawned.epoch_ms, spawned.output);
    let mut launching = Launching::new(spawned.child, Some(home.clone()), prepared.fresh);
    let main = match extra.profile_main {
        Some(port) => Some(profile_from_start(port)?),
        None => None,
    };
    // Electron writes DevToolsActivePort into its userData folder, wherever the app names it.
    let deadline = Instant::now() + Duration::from_secs(60);
    let text = loop {
        guard.check("while the app started")?;
        if let Some(t) = isolate::find(&home, "DevToolsActivePort", 6).into_iter().find_map(|p| std::fs::read_to_string(p).ok().filter(|t| t.lines().count() >= 2)) {
            break t;
        }
        // A program that is not Chromium-based rejects the flag and fails: say so now, not in 60 s. A launcher
        // stub that starts the real app and exits succeeds, so a clean exit keeps waiting for the port.
        if let Some(code) = launching.child.as_mut().and_then(|c| c.exited())
            && code != 0
        {
            return Err(Error::unreachable(format!("{} exited (code {code}) before it opened a debugging port", app.exe.display()))
                .hint("exe: and app: targets are Chromium or Electron apps; for any other program use exe:<path> --native, or record a running process with pid:<pid>"));
        }
        if Instant::now() > deadline {
            return Err(Error::unreachable(format!("{} did not open a debugging port in 60 s", app.exe.display())));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut lines = text.lines();
    let (debug_port, path) = (lines.next().unwrap_or("").trim(), lines.next().unwrap_or("").trim());
    let ws = format!("ws://127.0.0.1:{debug_port}{path}");
    let cdp = Cdp::connect(&ws)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    let fresh = launching.fresh;
    let (child, profile) = launching.done();
    Ok((Browser { cdp, ws, child, pid, profile, fresh, exe: app.exe.clone(), version, window: window.clone() }, guard, Started { epoch_ms: epoch, output, main }))
}

/// The app window's page: the first page target that is not DevTools.
pub fn main_page(browser: &Browser) -> Result<super::chromium::Page> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let targets = browser.cdp.send("Target.getTargets", json!({}), None)?;
        let found = targets["targetInfos"]
            .as_array()
            .and_then(|a| a.iter().filter(|t| t["type"] == "page" && !t["url"].as_str().unwrap_or("").starts_with("devtools://")).map(|t| t["targetId"].as_str().unwrap_or("").to_string()).next());
        if let Some(target) = found {
            let attached = browser.cdp.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
            let session = attached["sessionId"].as_str().ok_or_else(|| Error::msg("attach: no session"))?.to_string();
            let page = super::chromium::Page { cdp: browser.cdp.clone(), session, target };
            page.send("Page.enable", json!({}))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while cfg!(windows) && host::raise_windows(&browser.pids()) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            return Ok(page);
        }
        if Instant::now() > deadline {
            return Err(Error::unreachable("the app opened no window in 60 s"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
