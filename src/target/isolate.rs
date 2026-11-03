//! Launching an installed app fully isolated, with guards on the user's own state (SPEC §13.2).
//!
//! Generic: every per-user folder points into a fresh private home. What one app needs beyond that
//! (its own env vars, seed files, the user state to guard, URL schemes it registers) is an app
//! profile in strobe.json `apps`, never code here. Electron apps (`electron.rs`) and native apps
//! (`native.rs`) both launch through this.

use crate::host;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// One app to launch: an executable plus what its profile adds to the generic isolation.
#[derive(Clone, Debug, Default)]
pub struct App {
    pub id: String,
    pub exe: PathBuf,
    /// `electron` (the default: a debugging port, CDP) or `native` (no browser engine).
    pub kind: String,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    dirs: Vec<String>,
    files: Vec<(String, Value)>,
    guards: Vec<GuardSpec>,
    protocols: Vec<String>,
    args: Vec<String>,
    /// The profile's `headless` object: the `args`, `env` and `unset` that select the app's headless mode.
    headless: Value,
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
        App { id: exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), exe: exe.to_path_buf(), kind: "electron".into(), ..App::default() }
    }

    /// The same profile with another build of the app, such as a variant's `exe:<path>`.
    pub fn with_exe(mut self, exe: &Path) -> App {
        self.exe = exe.to_path_buf();
        self
    }

    /// Extra arguments and environment on top of the profile's (a bench target's own `args` and `env`).
    pub fn with_extra(mut self, args: &[String], env: &[(String, String)]) -> App {
        self.args.extend(args.iter().cloned());
        self.env.extend(env.iter().cloned());
        self
    }

    /// The profile in its headless mode: its `headless` object's `args`, `env` and `unset` added. An
    /// error when the profile has none, since strobe cannot know how an app selects headless.
    pub fn headless(mut self) -> Result<App> {
        let h = self.headless.clone();
        if !h.is_object() {
            return Err(Error::usage(format!("app profile \"{}\" has no headless object: add the args or env that start the app headless (\"headless\": {{ \"args\": [...] }})", self.id)));
        }
        let strings = |k: &str| h[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default();
        self.args.extend(strings("args"));
        self.unset.extend(strings("unset"));
        self.env.extend(h["env"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())));
        Ok(self)
    }

    fn unsets(&self, key: &str) -> bool {
        self.unset.iter().any(|u| u.strip_suffix('*').map_or(u == key, |prefix| key.starts_with(prefix)))
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
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let exe = match &p["exe"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get(os).and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
    .map(|s| expand(&s))
    .ok_or_else(|| Error::usage(format!("app profile \"{id}\" has no exe for {os}")))?;
    let strings = |k: &str| p[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let guards = p["guard"]
        .as_array()
        .map(|a| a.iter().filter_map(|g| Some(GuardSpec { file: expand(g["file"].as_str()?), pid: g["pid"].as_str().map(str::to_string), url: g["url"].as_str().map(str::to_string) })).collect())
        .unwrap_or_default();
    let kind = p["kind"].as_str().unwrap_or("electron").to_string();
    if kind != "electron" && kind != "native" {
        return Err(Error::usage(format!("app profile \"{id}\": kind is electron or native, not {kind}")));
    }
    Ok(App {
        id: id.to_string(),
        exe,
        kind,
        env: p["env"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default(),
        unset: strings("unset"),
        dirs: strings("dirs"),
        files: p["files"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
        guards,
        protocols: strings("protocols"),
        args: strings("args"),
        headless: p["headless"].clone(),
    })
}

/// Fails unless the app's executable exists (a profile names it before any build does).
pub fn installed(app: &App) -> Result<()> {
    if !app.exe.is_file() {
        return Err(Error::usage(format!("{} is not installed ({} does not exist)", app.id, app.exe.display())));
    }
    Ok(())
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
    pub fn take(app: &App) -> Guard {
        let checks = app
            .guards
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
                let what = if changed {
                    "changed"
                } else if stopped {
                    "lost its process"
                } else {
                    "stopped answering"
                };
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

pub fn alive(pid: u32) -> bool {
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

/// A new private home folder; nothing is in it yet.
pub fn new_home() -> PathBuf {
    host::paths::run().join(format!("home-{}-{}", std::process::id(), host::now_ns()))
}

/// Ends what still runs from `home` (an app's detached service) and restores the URL schemes saved
/// in it, without deleting it: a warm home is launched again.
pub fn release_home(home: &Path) {
    #[cfg(windows)]
    for pid in host::win::running_under(home) {
        kill_tree(pid);
    }
    restore_protocols(home);
}

/// Removes a home once nothing holds its files any more.
pub fn remove_home(home: &Path) {
    for _ in 0..20 {
        if std::fs::remove_dir_all(home).is_ok() || !home.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Unix epoch time in ms, the clock pages report in (`performance.timeOrigin`).
pub fn epoch_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1e3).unwrap_or(0.0)
}

/// Lines of `child`'s stdout and stderr with their arrival time and terminal colour codes removed;
/// the pipes are always drained, so a chatty app never blocks on a full pipe.
pub fn lines(child: &mut Child) -> std::sync::mpsc::Receiver<(f64, String)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let readers: Vec<Box<dyn std::io::Read + Send>> =
        [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)].into_iter().flatten().collect();
    for r in readers {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(r).lines().map_while(std::result::Result::ok) {
                let _ = tx.send((epoch_ms(), strip_ansi(&line)));
            }
        });
    }
    rx
}

/// `text` without ANSI escape sequences (`ESC [ … letter`, `ESC ] … BEL`).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A home ready for a launch: its folders, seed and profile files are in place, and `env` is what
/// the app's environment sets (on top of strobe's own, less the profile's `unset`).
#[derive(Clone, Debug)]
pub struct Home {
    pub dir: PathBuf,
    pub port: u16,
    pub env: Vec<(String, String)>,
}

impl Home {
    /// The app's whole environment, for a hook that starts something the app would see as its own
    /// (a background service).
    pub fn environment(&self, app: &App) -> std::collections::BTreeMap<String, String> {
        let mut out: std::collections::BTreeMap<String, String> = std::env::vars().filter(|(k, _)| !app.unsets(k)).collect();
        out.extend(self.env.iter().cloned());
        out
    }

    /// `{home}` and `{port}` filled into `s`.
    pub fn fill(&self, s: &str) -> String {
        s.replace("{home}", &self.dir.display().to_string()).replace("{port}", &self.port.to_string())
    }
}

/// Prepares `home` (a new one when `None`) for `app`. A home launched before (a warm profile) keeps
/// its state, and keeps its `{port}`: something started for it (a service) is still found there.
pub fn prepare_home(app: &App, seed: Option<&Path>, home: Option<PathBuf>) -> Result<Home> {
    let home = home.unwrap_or_else(new_home);
    let fresh = !home.join(".strobe-home").exists();
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
    if let Some(s) = seed.filter(|_| fresh) {
        copy_dir(s, &home)?;
    }
    std::fs::write(home.join(".strobe-home"), "")?;
    // A seed copied from a used profile may hold a stale port file.
    for stale in find(&home, "DevToolsActivePort", 6) {
        let _ = std::fs::remove_file(stale);
    }
    let kept = std::fs::read_to_string(home.join(".strobe-port")).ok().and_then(|p| p.trim().parse::<u16>().ok());
    let port = match kept {
        Some(p) => p,
        None => std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port(),
    };
    std::fs::write(home.join(".strobe-port"), port.to_string())?;
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
    let text = |p: &Path| p.display().to_string();
    let mut env: Vec<(String, String)> = [
        ("USERPROFILE", text(&home)),
        ("HOME", text(&home)),
        ("APPDATA", text(&appdata)),
        ("LOCALAPPDATA", text(&local)),
        ("TEMP", text(&temp)),
        ("TMP", text(&temp)),
        ("XDG_DATA_HOME", text(&data)),
        ("XDG_STATE_HOME", text(&state)),
        ("XDG_CONFIG_HOME", text(&config)),
        ("XDG_CACHE_HOME", text(&cache)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    env.extend(app.env.iter().map(|(k, v)| (k.clone(), fill(v))));
    Ok(Home { dir: home, port, env })
}

/// A spawned app: the child, when it was spawned (epoch ms and the host clock), and its output lines
/// when asked for.
pub struct Spawned {
    pub child: Child,
    pub pid: u32,
    pub epoch_ms: f64,
    pub at_ns: u64,
    pub output: Option<std::sync::mpsc::Receiver<(f64, String)>>,
}

/// Starts `app` in `home`: strobe's environment less the profile's `unset`, then the home's, then
/// `env`; the profile's `args`, then `args`; pinned to `cpus` when given (children inherit it).
pub fn spawn(app: &App, home: &Home, args: &[String], env: &[(String, String)], output: bool, cpus: Option<u64>) -> Result<Spawned> {
    installed(app)?;
    let mut cmd = Command::new(&app.exe);
    for (k, _) in std::env::vars() {
        if app.unsets(&k) {
            cmd.env_remove(&k);
        }
    }
    for (k, v) in home.env.iter().chain(env) {
        cmd.env(k, v);
    }
    for a in app.args.iter().chain(args) {
        cmd.arg(home.fill(a));
    }
    let piped = || if output { Stdio::piped() } else { Stdio::null() };
    cmd.stdin(Stdio::null()).stdout(piped()).stderr(piped());
    let previous = cpus.and_then(host::set_affinity);
    let (epoch, at_ns) = (epoch_ms(), host::now_ns());
    let spawned = cmd.spawn();
    if previous.is_some() {
        host::set_affinity(previous.unwrap_or(0));
    }
    let mut child = spawned.map_err(|e| Error::msg(format!("launch {}: {e}", app.exe.display())))?;
    let output = output.then(|| lines(&mut child));
    Ok(Spawned { pid: child.id(), child, epoch_ms: epoch, at_ns, output })
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
pub fn find(dir: &Path, name: &str, depth: usize) -> Vec<PathBuf> {
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
pub fn live(app: &App, guard: &Guard) -> Result<()> {
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

/// Kills a half-started launch when the caller returns early: the tree, then the home.
pub struct Launching {
    pub pid: u32,
    pub home: Option<PathBuf>,
}

impl Drop for Launching {
    fn drop(&mut self) {
        kill_tree(self.pid);
        if let Some(h) = self.home.take() {
            release_home(&h);
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
        let saved = Command::new("reg")
            .args(["export", &key])
            .arg(home.join(format!(".strobe-protocol-{scheme}.reg")))
            .arg("/y")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !saved {
            let _ = std::fs::write(home.join(format!(".strobe-protocol-{scheme}.absent")), "");
        }
    }
    let _ = (home, schemes);
}

/// Puts every saved scheme back as it was before the launch (deleting one that did not exist).
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
        // A command target leads its own process group: end the group, then the process.
        let _ = Command::new("kill").args(["-9", &format!("-{pid}")]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}
