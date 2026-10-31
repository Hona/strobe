//! Launching an installed Electron app fully isolated, with guards on the user's own state (SPEC §13.2).
//!
//! Generic: every per-user folder points into a fresh private home. What one app needs beyond that
//! (its own env vars, seed files, the user state to guard, URL schemes it registers) is an app
//! profile in strobe.json `apps`, never code here.

use super::cdp::Cdp;
use super::chromium::{Browser, Window};
use crate::host;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One app to launch: an executable plus what its profile adds to the generic isolation.
#[derive(Clone, Debug, Default)]
pub struct App {
    pub id: String,
    pub exe: PathBuf,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    dirs: Vec<String>,
    files: Vec<(String, Value)>,
    guards: Vec<GuardSpec>,
    protocols: Vec<String>,
    args: Vec<String>,
}

#[derive(Clone, Debug)]
struct GuardSpec {
    file: PathBuf,
    pid: Option<String>,
    url: Option<String>,
}

impl App {
    /// An executable with no profile: generic isolation only.
    pub fn bare(exe: &Path) -> App {
        App { id: exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), exe: exe.to_path_buf(), ..App::default() }
    }

    /// The same profile with another build of the app, such as a variant's `exe:<path>`.
    pub fn with_exe(mut self, exe: &Path) -> App {
        self.exe = exe.to_path_buf();
        self
    }
}

/// The app profile `id` from `config["apps"]`, following `extends`.
pub fn app(id: &str, config: &Value) -> Result<App> {
    let apps = config["apps"].as_object().cloned().unwrap_or_default();
    let known = || apps.keys().cloned().collect::<Vec<_>>().join(", ");
    let mut chain = vec![apps.get(id).cloned().ok_or_else(|| Error::usage(format!("no app profile \"{id}\" in strobe.json apps (known: {})", if apps.is_empty() { "none".into() } else { known() })))?];
    while let Some(parent) = chain.last().and_then(|p| p["extends"].as_str()).map(str::to_string) {
        if chain.len() > 8 {
            return Err(Error::usage(format!("app profile \"{id}\": extends loops")));
        }
        chain.push(apps.get(&parent).cloned().ok_or_else(|| Error::usage(format!("app profile \"{id}\" extends unknown \"{parent}\"")))?);
    }
    let mut merged = serde_json::Map::new();
    for p in chain.iter().rev() {
        for (k, v) in p.as_object().cloned().unwrap_or_default() {
            match (merged.get_mut(&k), v) {
                (Some(Value::Object(into)), Value::Object(from)) => into.extend(from),
                (_, v) => {
                    merged.insert(k, v);
                }
            }
        }
    }
    let p = Value::Object(merged);
    let os = if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let exe = match &p["exe"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get(os).and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
    .map(|s| expand(&s))
    .ok_or_else(|| Error::usage(format!("app profile \"{id}\" has no exe for {os}")))?;
    if !exe.is_file() {
        return Err(Error::usage(format!("{id} is not installed ({} does not exist)", exe.display())));
    }
    let strings = |k: &str| p[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let guards = p["guard"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|g| Some(GuardSpec { file: expand(g["file"].as_str()?), pid: g["pid"].as_str().map(str::to_string), url: g["url"].as_str().map(str::to_string) }))
                .collect()
        })
        .unwrap_or_default();
    Ok(App {
        id: id.to_string(),
        exe,
        env: p["env"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default(),
        unset: strings("unset"),
        dirs: strings("dirs"),
        files: p["files"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
        guards,
        protocols: strings("protocols"),
        args: strings("args"),
    })
}

/// `~`, `%VAR%` and `${VAR}` from strobe's own environment: the real user's, never a launched copy's.
fn expand(s: &str) -> PathBuf {
    let mut out = match s.strip_prefix('~') {
        Some(rest) => format!("{}{rest}", host::home().display()),
        None => s.to_string(),
    };
    for (k, v) in std::env::vars() {
        out = out.replace(&format!("%{k}%"), &v).replace(&format!("${{{k}}}"), &v);
    }
    PathBuf::from(out)
}

/// What the profile's guarded files held before launch: the user's own state, which must not change.
#[derive(Clone, Debug, Default)]
pub struct Guard {
    checks: Vec<Check>,
}

#[derive(Clone, Debug)]
struct Check {
    file: PathBuf,
    before: Option<String>,
    pid: Option<u32>,
    addr: Option<SocketAddr>,
}

impl Guard {
    fn take(specs: &[GuardSpec]) -> Guard {
        let checks = specs
            .iter()
            .map(|g| {
                let before = std::fs::read_to_string(&g.file).ok();
                let v: Value = before.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
                let pid = g.pid.as_deref().and_then(|k| v[k].as_u64()).map(|p| p as u32);
                let addr = g.url.as_deref().and_then(|k| v[k].as_str()).and_then(|u| u.split("://").nth(1)).and_then(|h| h.split('/').next()).and_then(|h| h.parse().ok());
                Check { file: g.file.clone(), before, pid, addr }
            })
            .collect();
        Guard { checks }
    }

    /// Fails when a guarded file changed, or its process stopped running or answering.
    pub fn check(&self, when: &str) -> Result<()> {
        for c in &self.checks {
            let changed = std::fs::read_to_string(&c.file).ok() != c.before;
            let stopped = c.pid.is_some_and(|p| !alive(p));
            let silent = c.addr.is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_err());
            if changed || stopped || silent {
                let what = if changed { "changed" } else if stopped { "lost its process" } else { "stopped answering" };
                return Err(Error::msg(format!("stopped {when}: {} {what}; strobe never measures past disturbed user state", c.file.display()))
                    .hint("an isolation gap: the app reached your own state; add the env var or file it escaped through to its profile in strobe.json apps"));
            }
        }
        Ok(())
    }

    fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.checks.iter().filter_map(|c| c.pid)
    }
}

fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    return host::win::exe_of(pid).is_some();
    #[cfg(not(windows))]
    return Command::new("kill").args(["-0", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let p = e.path();
        let dst = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dst)?;
        } else {
            std::fs::copy(&p, &dst)?;
        }
    }
    Ok(())
}

/// Launches `app` in a fresh private home (seeded from `seed`) with a debugging port.
pub fn launch(app: &App, seed: Option<&Path>, window: &Window, cpus: Option<u64>) -> Result<(Browser, Guard)> {
    let guard = Guard::take(&app.guards);
    live(app, &guard)?;
    let home = host::paths::run().join(format!("home-{}-{}", std::process::id(), host::now_ns()));
    let appdata = home.join("AppData").join("Roaming");
    let local = home.join("AppData").join("Local");
    let temp = local.join("Temp");
    let (data, state, config, cache) = (home.join(".local").join("share"), home.join(".local").join("state"), home.join(".config"), home.join(".cache"));
    for d in [&appdata, &temp, &data, &state, &config, &cache] {
        std::fs::create_dir_all(d)?;
    }
    // Windows resolves known folders under the redirected profile; Electron's app.getPath("documents")
    // throws when the folder is missing (an isolated desktop app failed to start on exactly that).
    for d in ["Documents", "Desktop", "Downloads", "Pictures", "Music", "Videos"] {
        std::fs::create_dir_all(home.join(d))?;
    }
    for d in &app.dirs {
        std::fs::create_dir_all(home.join(d))?;
    }
    if let Some(s) = seed {
        copy_dir(s, &home)?;
    }
    // A seed copied from a used profile may hold a stale port file.
    for stale in find(&home, "DevToolsActivePort", 6) {
        let _ = std::fs::remove_file(stale);
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let fill = |s: &str| s.replace("{home}", &home.display().to_string()).replace("{port}", &port.to_string());
    for (rel, content) in &app.files {
        let path = home.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = match content {
            Value::String(s) => fill(s),
            v => fill_json(v, &fill, port).to_string(),
        };
        std::fs::write(path, text)?;
    }
    save_protocols(&home, &app.protocols);
    let mut cmd = Command::new(&app.exe);
    for (k, _) in std::env::vars() {
        if app.unset.iter().any(|u| u.strip_suffix('*').map_or(*u == k, |prefix| k.starts_with(prefix))) {
            cmd.env_remove(&k);
        }
    }
    cmd.env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("APPDATA", &appdata)
        .env("LOCALAPPDATA", &local)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_CACHE_HOME", &cache);
    for (k, v) in &app.env {
        cmd.env(k, fill(v));
    }
    for a in &app.args {
        cmd.arg(fill(a));
    }
    cmd.arg("--remote-debugging-port=0").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let previous = cpus.and_then(host::set_affinity);
    let spawned = cmd.spawn();
    if previous.is_some() {
        host::set_affinity(previous.unwrap_or(0));
    }
    let child = spawned.map_err(|e| Error::msg(format!("launch {}: {e}", app.exe.display())))?;
    let pid = child.id();
    let mut launching = Launching { pid, home: Some(home.clone()) };
    // Electron writes DevToolsActivePort into its userData folder, wherever the app names it.
    let deadline = Instant::now() + Duration::from_secs(60);
    let text = loop {
        guard.check("while the app started")?;
        if let Some(t) = find(&home, "DevToolsActivePort", 6).into_iter().find_map(|p| std::fs::read_to_string(p).ok().filter(|t| t.lines().count() >= 2)) {
            break t;
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
    launching.home = None;
    std::mem::forget(launching);
    Ok((Browser { cdp, ws, child: Some(child), pid, profile: Some(home), exe: app.exe.clone(), version, window: window.clone() }, guard))
}

/// A JSON seed file with placeholders filled; a string that is exactly `{port}` becomes a number.
fn fill_json(v: &Value, fill: &dyn Fn(&str) -> String, port: u16) -> Value {
    match v {
        Value::String(s) if s == "{port}" => json!(port),
        Value::String(s) => Value::String(fill(s)),
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_json(x, fill, port)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), fill_json(x, fill, port))).collect()),
        other => other.clone(),
    }
}

/// Files named `name` under `dir`, at most `depth` levels down.
fn find(dir: &Path, name: &str, depth: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .flat_map(|e| {
            let p = e.path();
            if p.is_dir() {
                return if depth > 0 { find(&p, name, depth - 1) } else { Vec::new() };
            }
            if e.file_name() == name { vec![p] } else { Vec::new() }
        })
        .collect()
}

/// Refuses to launch an app the user is using: its executable is running, or a guarded process
/// (such as the app's own background service) runs from its install folder or a folder named like
/// it. Isolation should make it safe, but the user's own app is never the thing to experiment on.
fn live(app: &App, guard: &Guard) -> Result<()> {
    #[cfg(windows)]
    {
        if !host::win::running(&app.exe).is_empty() {
            return Err(Error::refused("LiveChannel", format!("{} is running: that is your app", app.exe.display()), "measure another build or channel, or close it"));
        }
        let dir = app.exe.parent().map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
        let folder = app.exe.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        for pid in guard.pids() {
            let Some(exe) = host::win::exe_of(pid) else { continue };
            let path = exe.to_string_lossy().to_lowercase();
            if (!folder.is_empty() && path.contains(&format!("\\{folder}\\"))) || (!dir.is_empty() && path.starts_with(&dir)) {
                return Err(Error::refused("LiveChannel", format!("your live {} runs from this app ({})", app.id, exe.display()), "measure another build or channel"));
            }
        }
    }
    let _ = (app, guard);
    Ok(())
}

/// Kills a half-started launch when `launch` returns early.
struct Launching {
    pid: u32,
    home: Option<PathBuf>,
}

impl Drop for Launching {
    fn drop(&mut self) {
        kill_tree(self.pid);
        if let Some(h) = self.home.take() {
            restore_protocols(&h);
            let _ = std::fs::remove_dir_all(h);
        }
    }
}

/// URL schemes an app registers on start (`protocols` in its profile) live in the per-user registry,
/// which no environment variable redirects: strobe saves each key into the private home first.
fn save_protocols(home: &Path, schemes: &[String]) {
    #[cfg(windows)]
    for scheme in schemes {
        let key = format!(r"HKCU\Software\Classes\{scheme}");
        let saved = Command::new("reg").args(["export", &key]).arg(home.join(format!(".strobe-protocol-{scheme}.reg"))).arg("/y").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        if !saved {
            let _ = std::fs::write(home.join(format!(".strobe-protocol-{scheme}.absent")), "");
        }
    }
    let _ = (home, schemes);
}

/// Puts every saved scheme back as it was before `launch` (deleting one that did not exist).
pub fn restore_protocols(home: &Path) {
    #[cfg(windows)]
    for e in std::fs::read_dir(home).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(".strobe-protocol-") else { continue };
        let (scheme, reg) = match (rest.strip_suffix(".reg"), rest.strip_suffix(".absent")) {
            (Some(s), _) => (s.to_string(), true),
            (_, Some(s)) => (s.to_string(), false),
            _ => continue,
        };
        let key = format!(r"HKCU\Software\Classes\{scheme}");
        let _ = Command::new("reg").args(["delete", &key, "/f"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        if reg {
            let _ = Command::new("reg").arg("import").arg(e.path()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
    let _ = home;
}

pub fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    } else {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// The app window's page: the first page target that is not DevTools.
pub fn main_page(browser: &Browser) -> Result<super::chromium::Page> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let targets = browser.cdp.send("Target.getTargets", json!({}), None)?;
        let found = targets["targetInfos"].as_array().and_then(|a| {
            a.iter()
                .filter(|t| t["type"] == "page" && !t["url"].as_str().unwrap_or("").starts_with("devtools://"))
                .map(|t| t["targetId"].as_str().unwrap_or("").to_string())
                .next()
        });
        if let Some(target) = found {
            let attached = browser.cdp.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
            let session = attached["sessionId"].as_str().ok_or_else(|| Error::msg("attach: no session"))?.to_string();
            let page = super::chromium::Page { cdp: browser.cdp.clone(), session, target };
            page.send("Page.enable", json!({}))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while host::raise_windows(browser.pid) == 0 && Instant::now() < deadline {
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
