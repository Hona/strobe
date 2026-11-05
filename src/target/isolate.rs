//! Launching an installed app fully isolated, with guards on the user's own state (SPEC §13.2).
//!
//! Generic: every per-user folder points into a fresh private home. What one app needs beyond that
//! (its own env vars, seed files, the user state to guard, URL schemes it registers) is an app
//! profile in strobe.json `apps`, never code here. Electron apps (`electron.rs`), apps on the OS's
//! webview (`webview.rs`) and native apps (`native.rs`) all launch through this.

use crate::host;
pub use crate::host::kill_tree;
use crate::host::spawn::{HOME_VAR, Owned, OWNER_VAR, Stamp, home_key as key, owner_stamp, parse_owner, stamp_of, stamped};
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// How strobe reaches an app's UI: the profile's `kind`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppKind {
    /// An Electron app: a debugging port (`--remote-debugging-port`), CDP.
    #[default]
    Electron,
    /// An app that embeds the OS's webview: the webview's own debugging port (`webview.rs`).
    Webview,
    /// No browser engine (or one strobe does not attach to): from outside and through the OS's
    /// accessibility tree (§13.7).
    Native,
}

impl AppKind {
    pub const NAMES: &[&str] = &["electron", "webview", "native"];

    pub fn name(self) -> &'static str {
        match self {
            AppKind::Electron => "electron",
            AppKind::Webview => "webview",
            AppKind::Native => "native",
        }
    }

    fn parse(name: &str) -> Option<AppKind> {
        [AppKind::Electron, AppKind::Webview, AppKind::Native].into_iter().find(|k| k.name() == name)
    }
}

impl std::fmt::Display for AppKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One app to launch: an executable plus what its profile adds to the generic isolation.
#[derive(Clone, Debug, Default)]
pub struct App {
    pub id: String,
    pub exe: PathBuf,
    pub kind: AppKind,
    env: Vec<(String, String)>,
    unset: Vec<String>,
    dirs: Vec<String>,
    files: Vec<(String, Value)>,
    guards: Vec<GuardSpec>,
    protocols: Vec<String>,
    args: Vec<String>,
    /// Folders of the user's whose data the profile's `env` moves into the home (`moved`), so the
    /// macOS Library rule does not count them; the profile's `guard` watches them instead.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    moved: Vec<PathBuf>,
    /// The profile says `window: false`: the app opens no window (a tray app, a service).
    pub windowless: bool,
    /// The profile's `gpuCache` (`warm` or `cold`), when it names one.
    pub gpu_cache_mode: Option<String>,
    /// Where the GPU shader caches of this app's launches persist between them (`gpuCache: "warm"`):
    /// lent into each home before the launch and taken back when the home goes (`lend_gpu_caches`).
    pub gpu_cache: Option<PathBuf>,
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

    /// The same profile reached another way (`exe:<path> --native`, a `webview(path)` target).
    pub fn as_kind(mut self, kind: AppKind) -> App {
        self.kind = kind;
        self
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

    /// Files written into the home over the profile's (a bench target's own `files`).
    pub fn with_files(mut self, files: &serde_json::Map<String, Value>) -> App {
        for (k, v) in files {
            self.files.retain(|(f, _)| f != k);
            self.files.push((k.clone(), v.clone()));
        }
        self
    }

    /// Variables (or `PREFIX_*` families) the launch removes, after the profile's.
    pub fn with_unset(mut self, unset: &[String]) -> App {
        self.unset.extend(unset.iter().cloned());
        self
    }

    fn unsets(&self, key: &str) -> bool {
        env_unset(&self.unset, key)
    }
}

/// This OS's key in a per-OS value (`exe`, a build's `output`).
pub fn os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// A path for every OS, or one per OS (`{ "windows": …, "macos": …, "linux": … }`): this OS's.
pub fn per_os(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Object(m) => m.get(os()).and_then(Value::as_str),
        _ => None,
    }
}

/// strobe.json's `build` for this OS: its `windows`, `macos` or `linux` object overlays the rest
/// (`env` merged key by key, other keys replaced), so a repository pins what its build needs on
/// one OS (a toolchain, a compiler) without every user setting it.
pub fn build_for_os(build: &Value) -> Value {
    let Some(base) = build.as_object() else { return build.clone() };
    let mut out = base.clone();
    for k in ["windows", "macos", "linux"] {
        out.remove(k);
    }
    if let Some(overlay) = base.get(os()).and_then(Value::as_object) {
        for (k, v) in overlay {
            match (k.as_str(), out.get_mut(k)) {
                ("env", Some(Value::Object(env))) if v.is_object() => env.extend(v.as_object().cloned().unwrap_or_default()),
                _ => {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Value::Object(out)
}

/// The app profile `id` from `config["apps"]`, following `extends`.
pub fn app(id: &str, config: &Value) -> Result<App> {
    crate::config::check_app(config, id)?;
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
    let exe = per_os(&p["exe"]).map(str::to_string).ok_or_else(|| Error::usage(format!("app profile \"{id}\" has no exe for {}", os())))?;
    let exe = expand(&exe).map_err(|e| Error::usage(format!("app profile \"{id}\" exe: {}", e.message)))?;
    let strings = |k: &str| p[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let mut guards = Vec::new();
    for g in p["guard"].as_array().into_iter().flatten() {
        let Some(file) = g["file"].as_str() else { continue };
        // A guard for another OS's state (`"os": "windows"`) guards nothing here: one profile serves
        // every OS.
        if g["os"].as_str().is_some_and(|o| o != os()) {
            continue;
        }
        let file = expand(file).map_err(|e| Error::usage(format!("app profile \"{id}\" guard: {}", e.message)))?;
        guards.push(GuardSpec { file, pid: g["pid"].as_str().map(str::to_string), url: g["url"].as_str().map(str::to_string) });
    }
    let moved: Vec<String> = strings("moved");
    let moved = moved.iter().map(|m| expand(m).map_err(|e| Error::usage(format!("app profile \"{id}\" moved: {}", e.message)))).collect::<Result<Vec<_>>>()?;
    let kind = p["kind"].as_str().unwrap_or("electron");
    let kind = AppKind::parse(kind).ok_or_else(|| Error::usage(format!("app profile \"{id}\": kind is {}, not {kind}", AppKind::NAMES.join(", "))))?;
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
        moved,
        windowless: p["window"] == false,
        gpu_cache_mode: p["gpuCache"].as_str().map(str::to_string),
        gpu_cache: None,
    })
}

/// Fails unless the app's executable exists (a profile names it before any build does).
pub fn installed(app: &App) -> Result<()> {
    if !app.exe.is_file() {
        return Err(Error::usage(format!("{} is not installed ({} does not exist)", app.id, app.exe.display())));
    }
    Ok(())
}

/// Whether environment variable `key` is `name`: in any case on Windows, which reads names that way
/// (`%APPDATA%` is `AppData`), exactly elsewhere.
pub fn env_name_is(key: &str, name: &str) -> bool {
    if cfg!(windows) { key.eq_ignore_ascii_case(name) } else { key == name }
}

/// Whether `key` matches an `unset` pattern (`NAME`, or `PREFIX*`), cased as `env_name_is`.
pub fn env_unset(patterns: &[String], key: &str) -> bool {
    patterns.iter().any(|u| match u.strip_suffix('*') {
        Some(prefix) => key.len() >= prefix.len() && key.is_char_boundary(prefix.len()) && env_name_is(&key[..prefix.len()], prefix),
        None => env_name_is(key, u),
    })
}

/// `~`, `%VAR%` and `${VAR}` from strobe's own environment: the real user's, never a launched copy's.
/// A variable that is not set is an error: a path left holding `%VAR%` names nothing, and a guard on
/// it would protect nothing while passing every check.
fn expand(s: &str) -> Result<PathBuf> {
    let home = match s.strip_prefix('~') {
        Some(rest) => format!("{}{rest}", host::home().display()),
        None => s.to_string(),
    };
    let mut out = String::with_capacity(home.len());
    let mut rest = home.as_str();
    loop {
        let (at, open, close) = match (rest.find('%'), rest.find("${")) {
            (Some(p), Some(b)) if b < p => (b, 2, '}'),
            (Some(p), _) => (p, 1, '%'),
            (None, Some(b)) => (b, 2, '}'),
            (None, None) => break,
        };
        out.push_str(&rest[..at]);
        let after = &rest[at + open..];
        let name = after.find(close).map(|end| &after[..end]).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "_()".contains(c)));
        let Some(name) = name else {
            out.push_str(&rest[at..at + open]);
            rest = after;
            continue;
        };
        let value = std::env::vars_os().find(|(k, _)| k.to_str().is_some_and(|k| env_name_is(k, name))).map(|(_, v)| v.to_string_lossy().to_string());
        let Some(value) = value else {
            return Err(Error::usage(format!("{s}: environment variable {name} is not set, so the path names nothing")));
        };
        out.push_str(&value);
        rest = &after[name.len() + 1..];
    }
    out.push_str(rest);
    Ok(PathBuf::from(out))
}

/// What the profile's guarded files held before launch: the user's own state, which must not change.
#[derive(Clone, Debug, Default)]
pub struct Guard {
    checks: Vec<Check>,
    /// Seed files the home shares with other launches: none may change.
    pub shared: Vec<Shared>,
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
                let before = match std::fs::read_to_string(&g.file) {
                    Ok(t) => Some(t),
                    // Absent is a state a guard compares; a file strobe may not read is no state at all.
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(UNREADABLE.to_string()),
                    Err(_) => None,
                };
                let v: Value = before.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
                let pid = g.pid.as_deref().and_then(|k| v[k].as_u64()).map(|p| p as u32);
                let addr = g.url.as_deref().and_then(|k| v[k].as_str()).and_then(|u| u.split("://").nth(1)).and_then(|h| h.split('/').next()).and_then(|h| h.parse().ok());
                Check { file: g.file.clone(), before, pid, addr }
            })
            .collect();
        Guard { checks, shared: Vec::new() }
    }

    /// Fails when a guarded file changed, or its process stopped running or answering, or the app
    /// wrote to a seed file its home shares with other launches.
    pub fn check(&self, when: &str) -> Result<()> {
        if let Some(s) = self.shared.iter().find(|s| stamp(&s.cache) != Some((s.len, s.modified))) {
            return Err(Error::msg(format!(
                "stopped {when}: the app changed {}, a large seed file strobe shares read-only between launches (as {}), so later launches would not start from the same seed",
                s.link.display(),
                s.cache.display()
            ))
            .hint("the next launch makes a fresh shared copy; an app that must write a seed file this large needs it below 64 MB or outside the seed"));
        }
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

    /// A guarded file strobe may not read (macOS 27 denies reading another developer's app data
    /// without a prompt): it could change unseen, so nothing launches. A bench asks before it waits
    /// for the slot or builds.
    pub fn unreadable(&self) -> Result<()> {
        let Some(c) = self.checks.iter().find(|c| c.before.as_deref() == Some(UNREADABLE)) else { return Ok(()) };
        Err(Error::refused(
            "LiveChannel",
            format!("{} is guarded but strobe may not read it (permission denied), so a change to it would go unseen", c.file.display()),
            if cfg!(target_os = "macos") {
                "macOS denies reading another developer's app data (containers, group containers): guard a file outside them, or grant strobe Full Disk Access (System Settings › Privacy & Security › Full Disk Access)"
            } else {
                "guard a file strobe can read, or let this user read it"
            },
        ))
    }
}

/// What a guard holds for a file it may not read.
const UNREADABLE: &str = "\u{0}strobe: unreadable";

pub fn alive(pid: u32) -> bool {
    host::exe_of(pid).is_some()
}

/// Seed files this large are shared, not copied into each home: a copy per launch writes the whole
/// file again, an antivirus scans each new copy on every CPU while the bench measures, and deleting
/// it after costs again.
const SHARE_BYTES: u64 = 64 << 20;

/// A large seed file shared into a home: one read-only copy per seed version in strobe's own
/// `<state>/seeds`, hard-linked into each home, with its size and time when linked. The app reads it
/// like any file of its home; it cannot write through to the seed (read-only), and a write that got
/// through anyway shows in `Guard::check`.
#[derive(Clone, Debug)]
pub struct Shared {
    pub link: PathBuf,
    pub cache: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp(p: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()))
}

/// The read-only cached copy of a large seed file (made once per seed version), linked to `dst`.
/// None when it cannot be shared (another volume, no hard links): the caller copies it.
fn share(seed: &Path, dst: &Path) -> Option<Shared> {
    use sha2::Digest;
    let (len, modified) = stamp(seed)?;
    let id: String = sha2::Sha256::digest(format!("{}\n{len}\n{modified:?}", seed.display()).as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let cache = host::paths::sub("seeds").join(id).join(seed.file_name()?);
    // A cached copy that changed since it was made (a write that got through) is made again.
    let fresh = stamp(&cache).is_some_and(|(l, _)| l == len) && std::fs::read_to_string(cache.with_extension("stamp")).ok() == Some(format!("{:?}", stamp(&cache)));
    if !fresh {
        if let Ok(m) = std::fs::metadata(&cache) {
            let mut p = m.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            let _ = std::fs::set_permissions(&cache, p);
            let _ = std::fs::remove_file(&cache);
        }
        // Copies of seeds no bench linked for a week go (a regenerated seed is a new version).
        let week = std::time::Duration::from_secs(7 * 86_400);
        for e in std::fs::read_dir(host::paths::sub("seeds")).into_iter().flatten().flatten() {
            let used = std::fs::read_dir(e.path()).into_iter().flatten().flatten().filter_map(|f| f.metadata().ok()?.accessed().ok()).max();
            if used.and_then(|t| t.elapsed().ok()).is_some_and(|age| age > week) {
                for f in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    if let Ok(m) = f.metadata() {
                        let mut p = m.permissions();
                        #[allow(clippy::permissions_set_readonly_false)]
                        p.set_readonly(false);
                        let _ = std::fs::set_permissions(f.path(), p);
                    }
                }
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        std::fs::create_dir_all(cache.parent()?).ok()?;
        std::fs::copy(seed, &cache).ok()?;
        let mut p = std::fs::metadata(&cache).ok()?.permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&cache, p).ok()?;
        std::fs::write(cache.with_extension("stamp"), format!("{:?}", stamp(&cache))).ok()?;
    }
    std::fs::hard_link(&cache, dst).ok()?;
    let (len, modified) = stamp(&cache)?;
    Some(Shared { link: dst.to_path_buf(), cache, len, modified })
}

fn copy_dir(from: &Path, to: &Path, shared: &mut Vec<Shared>) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let p = e.path();
        let dst = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dst, shared)?;
        } else if let Some(s) = std::fs::metadata(&p).ok().filter(|m| m.len() >= SHARE_BYTES).and_then(|_| share(&p, &dst)) {
            shared.push(s);
        } else {
            std::fs::copy(&p, &dst)?;
        }
    }
    Ok(())
}

/// Unlinks a home's shared seed files: a read-only file's link may need its attribute cleared first,
/// which clears it on the cached copy too, so it is set again right after.
fn unshare(home: &Path) {
    let Ok(text) = std::fs::read_to_string(home.join(".strobe-shared")) else { return };
    for line in text.lines() {
        let Some((link, cache)) = line.split_once('\t') else { continue };
        let link = Path::new(link);
        if std::fs::remove_file(link).is_ok() || !link.exists() {
            continue;
        }
        let set = |ro: bool| {
            if let Ok(m) = std::fs::metadata(cache) {
                let mut p = m.permissions();
                p.set_readonly(ro);
                let _ = std::fs::set_permissions(cache, p);
            }
        };
        set(false);
        let _ = std::fs::remove_file(link);
        set(true);
    }
}

/// A new private home folder; nothing is in it yet.
pub fn new_home() -> PathBuf {
    host::paths::run().join(format!("home-{}-{}", std::process::id(), host::now_ns()))
}

/// The file in a home or profile that names its strobe the way `host::spawn::OWNER_VAR` does.
const OWNER_FILE: &str = ".strobe-owner";

/// Homes and profiles this strobe launched into and has not released.
static HELD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn held(dir: &str) -> bool {
    HELD.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|h| h == dir)
}

/// The variables a launch for `dir` carries (`host::spawn::HOME_VAR`, `OWNER_VAR`); `dir` is held
/// until it is released, and names this strobe in its owner file.
pub fn launch_env(dir: &Path) -> [(String, String); 2] {
    let _ = std::fs::write(dir.join(OWNER_FILE), owner_stamp());
    let k = key(dir);
    let mut h = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if !h.contains(&k) {
        h.push(k);
    }
    [(HOME_VAR.into(), dir.display().to_string()), (OWNER_VAR.into(), owner_stamp().into())]
}

fn let_go(dir: &Path) {
    let k = key(dir);
    HELD.lock().unwrap_or_else(|e| e.into_inner()).retain(|h| *h != k);
}

/// Whether the strobe a stamp names runs: that pid, started at that time.
fn owner_alive(pid: u32, start: u64) -> bool {
    host::start_time(pid) == Some(start)
}

/// A stamped process is a leftover when its strobe is gone, or when it is this strobe's and the
/// folder it was launched for was released (a detached child that outlived its launch).
fn leftover(s: &Stamp) -> bool {
    if s.pid == std::process::id() { !held(&s.home) } else { !owner_alive(s.pid, s.start) }
}

/// Ends the trees of `procs` from their roots (a process whose parent is not among them), each
/// checked again right before it ends (its stamp unchanged: not a reused pid). Returns the ended
/// roots, named before any ended.
fn end_roots(procs: &[(u32, u32, Stamp)]) -> Vec<(u32, String, Stamp)> {
    let roots: Vec<(u32, String, Stamp)> = procs
        .iter()
        .filter(|(_, parent, _)| !procs.iter().any(|(p, _, _)| p == parent))
        .map(|(pid, _, s)| (*pid, host::exe_of(*pid).map(|e| e.display().to_string()).unwrap_or_else(|| format!("process {pid}")), s.clone()))
        .collect();
    roots
        .into_iter()
        .filter(|(pid, _, s)| {
            let same = stamp_of(*pid).as_ref() == Some(s);
            if same {
                kill_tree(*pid);
            }
            same
        })
        .collect()
}

/// Ends what dead strobes left behind, and the folders they left: every stamped process whose
/// strobe is gone (a crash, a kill, a signal before it closed its target) or that outlived a folder
/// this strobe released, then every home or profile under `run/` whose strobe is gone (its GPU
/// caches go back, shared seeds unlink, URL schemes are restored). Returns a line per process ended.
pub fn reap_dead_owners() -> Vec<String> {
    let dead: Vec<(u32, u32, Stamp)> = stamped().into_iter().filter(|(_, _, s)| leftover(s)).collect();
    let mut lines: Vec<String> = end_roots(&dead)
        .into_iter()
        .map(|(pid, exe, s)| {
            if s.pid == std::process::id() {
                format!("ended {exe} (pid {pid}): this strobe launched it for a folder it already released")
            } else {
                format!("ended {exe} (pid {pid}): the strobe that launched it (pid {}) is gone and left it running", s.pid)
            }
        })
        .collect();
    let Ok(entries) = std::fs::read_dir(host::paths::run()) else { return lines };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(named) = name.strip_prefix("profile-").or_else(|| name.strip_prefix("home-")).and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        let gone = match std::fs::read_to_string(e.path().join(OWNER_FILE)).ok().as_deref().and_then(parse_owner) {
            Some((pid, start)) => pid != std::process::id() && !owner_alive(pid, start),
            // A folder from before owner files: its strobe by the pid in its name.
            None => named != std::process::id() && !host::exe_of(named).is_some_and(|exe| exe.file_stem().is_some_and(|s| s.to_string_lossy().to_lowercase().starts_with("strobe"))),
        };
        if !gone {
            continue;
        }
        let k = key(&e.path());
        let left: Vec<(u32, u32, Stamp)> = stamped().into_iter().filter(|(_, _, s)| s.home == k).collect();
        lines.extend(end_roots(&left).into_iter().map(|(pid, exe, _)| format!("ended {exe} (pid {pid}): it ran from {}, a folder of a strobe that is gone", e.path().display())));
        restore_protocols(&e.path());
        remove_home(&e.path());
    }
    lines
}

/// Ends what still runs from `home` (an app's detached service, anything stamped with it) and
/// restores the URL schemes saved in it, without deleting it: a warm home is launched again.
pub fn release_home(home: &Path) {
    let k = key(home);
    let left: Vec<(u32, u32, Stamp)> = stamped().into_iter().filter(|(_, _, s)| s.home == k).collect();
    end_roots(&left);
    restore_protocols(home);
    let_go(home);
}

/// Ends what a launch started, whatever state it is in: the child, its tree and its group
/// (`Owned::end`), then everything stamped with its folder; releases the folder and, with `remove`,
/// deletes it.
pub fn end_launch(child: Option<Owned>, home: Option<&Path>, remove: bool) {
    drop(child);
    if let Some(h) = home {
        release_home(h);
        if remove {
            remove_home(h);
        }
    }
}

/// Where GPU drivers keep compiled shaders, relative to a home whose per-user folders the launch
/// redirects: Windows resolves `%LOCALAPPDATA%` (and LocalLow, from `%USERPROFILE%`) under it, Mesa
/// and NVIDIA's Linux driver use `$XDG_CACHE_HOME` (older NVIDIA drivers `~/.nv`). macOS keeps Metal's
/// cache per app in the per-user cache folder the OS assigns (`getconf DARWIN_USER_CACHE_DIR`), which
/// no environment variable moves: every launch there uses the user's own, warm.
const DRIVER_CACHES: &[&str] = if cfg!(windows) {
    &[
        "AppData/Local/NVIDIA/DXCache",
        "AppData/Local/NVIDIA/GLCache",
        "AppData/Local/D3DSCache",
        "AppData/Local/AMD/DxCache",
        "AppData/Local/AMD/DxcCache",
        "AppData/Local/AMD/VkCache",
        "AppData/Local/AMD/OglCache",
        "AppData/LocalLow/Intel/ShaderCache",
    ]
} else if cfg!(target_os = "linux") {
    &[".cache/mesa_shader_cache", ".cache/mesa_shader_cache_db", ".cache/nvidia", ".cache/radv_builtin_shaders", ".nv"]
} else {
    &[]
};

/// Folders Chromium (and so an Electron app) keeps its own GPU caches in, wherever its profile is:
/// program binaries, Skia's and Dawn's compiled shaders.
const CHROMIUM_GPU_CACHES: &[&str] = &["GPUCache", "GrShaderCache", "GraphiteDawnCache", "DawnGraphiteCache", "DawnWebGPUCache", "ShaderCache"];

/// Moves `from` to `to` (same volume: strobe's state folder), retrying while a just-closed app's
/// files are still held.
fn move_dir(from: &Path, to: &Path) -> bool {
    if let Some(p) = to.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    for _ in 0..20 {
        if std::fs::rename(from, to).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Lends the GPU shader caches kept in `store` to `home` before a launch: moved in (no copy), so the
/// app's driver and Chromium find what earlier launches of the same variant compiled, as on a
/// person's machine. Never the user's own caches, and never another variant's (each has its store).
/// What the home already holds (a seed's own) wins. True when the store held anything.
pub fn lend_gpu_caches(home: &Path, store: &Path) -> bool {
    let _ = std::fs::create_dir_all(store);
    let _ = std::fs::write(home.join(".strobe-gpu-cache"), store.display().to_string());
    let held = std::fs::read_to_string(store.join("paths")).unwrap_or_default();
    let mut warm = false;
    for rel in held.lines().filter(|l| !l.is_empty()) {
        let (from, to) = (store.join("dirs").join(rel), home.join(rel));
        if from.is_dir() && !to.exists() && move_dir(&from, &to) {
            warm = true;
        }
    }
    warm
}

/// Takes a home's GPU shader caches back into the store it borrowed them from (`lend_gpu_caches`),
/// once nothing runs from it: the driver folders, and every Chromium GPU cache folder in it.
pub fn keep_gpu_caches(home: &Path) {
    let Ok(store) = std::fs::read_to_string(home.join(".strobe-gpu-cache")) else { return };
    let store = PathBuf::from(store.trim());
    let mut rels: Vec<String> = DRIVER_CACHES.iter().map(|r| r.to_string()).filter(|r| home.join(r).is_dir()).collect();
    fn chromium(dir: &Path, home: &Path, depth: usize, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(home).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            if !p.is_dir() || DRIVER_CACHES.iter().any(|d| rel == *d) {
                continue;
            }
            if CHROMIUM_GPU_CACHES.iter().any(|n| e.file_name() == *n) {
                out.push(rel);
            } else if depth > 0 {
                chromium(&p, home, depth - 1, out);
            }
        }
    }
    chromium(home, home, 6, &mut rels);
    let mut kept: Vec<String> = std::fs::read_to_string(store.join("paths")).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    for rel in rels {
        let to = store.join("dirs").join(&rel);
        if to.exists() {
            let _ = std::fs::remove_dir_all(&to);
        }
        if move_dir(&home.join(&rel), &to) && !kept.contains(&rel) {
            kept.push(rel);
        }
    }
    let _ = std::fs::write(store.join("paths"), kept.join("\n"));
}

/// The GPU cache store of one executable, which recordings of it share (`record`, `record --bench`):
/// a recording after the first finds what the earlier ones compiled.
pub fn exe_gpu_store(exe: &Path) -> PathBuf {
    use sha2::Digest;
    let key: String = sha2::Sha256::digest(exe.display().to_string().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    host::paths::sub("gpu-cache").join(format!("rec-{key}"))
}

/// Bytes a GPU cache store holds (its caches, while no launch has them lent).
pub fn gpu_cache_bytes(store: &Path) -> u64 {
    fn size(p: &Path) -> u64 {
        std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| if e.path().is_dir() { size(&e.path()) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }).sum()
    }
    size(&store.join("dirs"))
}

/// Stores of GPU caches (`gpu-cache/<key>`) no launch used for a week go.
pub fn prune_gpu_caches() {
    let week = Duration::from_secs(7 * 86_400);
    for e in std::fs::read_dir(host::paths::sub("gpu-cache")).into_iter().flatten().flatten() {
        let used = std::fs::metadata(e.path().join("paths")).or_else(|_| e.metadata()).ok().and_then(|m| m.modified().ok());
        if used.and_then(|t| t.elapsed().ok()).is_some_and(|age| age > week) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Removes a home once nothing holds its files any more.
pub fn remove_home(home: &Path) {
    let_go(home);
    keep_gpu_caches(home);
    unshare(home);
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

/// A line an app wrote: when strobe read it, as epoch ms (beside a log line's own time) and on the
/// host clock in ns (beside phase marks and presents), and its text.
pub type OutputLine = (f64, u64, String);

/// Lines of a launch's stdout and stderr with their arrival time and terminal colour codes removed;
/// the pipes are always drained, so a chatty app never blocks on a full pipe.
pub fn lines(child: &mut Owned) -> std::sync::mpsc::Receiver<OutputLine> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (out, err) = child.output();
    let readers: Vec<Box<dyn std::io::Read + Send>> = [out.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), err.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)].into_iter().flatten().collect();
    for r in readers {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(r).lines().map_while(std::result::Result::ok) {
                let _ = tx.send((epoch_ms(), host::now_ns(), strip_ansi(&line)));
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
    /// Large seed files shared into the home rather than copied.
    pub shared: Vec<Shared>,
    /// With `App::gpu_cache`: whether the launch found GPU shader caches from earlier launches.
    pub gpu_warm: Option<bool>,
    /// `prepare_home` made it now (not a warm home launched before): a launch that ends without
    /// being closed removes it, and keeps a warm one for the caller.
    pub fresh: bool,
}

impl Home {
    /// The app's whole environment, for a hook that starts something the app would see as its own
    /// (a background service).
    pub fn environment(&self, app: &App) -> std::collections::BTreeMap<String, String> {
        let mut out: std::collections::BTreeMap<String, String> = std::env::vars().filter(|(k, _)| !app.unsets(k)).collect();
        // One entry per name: the home's `APPDATA` replaces strobe's `AppData` on Windows.
        out.retain(|k, _| !self.env.iter().any(|(e, _)| env_name_is(k, e)));
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
    let mut shared = Vec::new();
    if let Some(s) = seed.filter(|_| fresh) {
        copy_dir(s, &home, &mut shared)?;
        let list: String = shared.iter().map(|s| format!("{}\t{}\n", s.link.display(), s.cache.display())).collect();
        std::fs::write(home.join(".strobe-shared"), list)?;
    } else if let Ok(text) = std::fs::read_to_string(home.join(".strobe-shared")) {
        // A warm home keeps its links: watch them for this launch too.
        for (link, cache) in text.lines().filter_map(|l| l.split_once('\t')) {
            if let Some((len, modified)) = stamp(Path::new(cache)) {
                shared.push(Shared { link: link.into(), cache: cache.into(), len, modified });
            }
        }
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
    #[cfg(target_os = "macos")]
    macos_library::save(&home, app);
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
    // Last, so no profile replaces them: what tells strobe's launches (and what they start) apart.
    env.extend(launch_env(&home));
    let gpu_warm = app.gpu_cache.as_deref().map(|store| lend_gpu_caches(&home, store));
    Ok(Home { dir: home, port, env, shared, gpu_warm, fresh })
}

/// A spawned app: the child with its group, when it was spawned (epoch ms and the host clock), and
/// its output lines when asked for.
pub struct Spawned {
    pub child: Owned,
    pub pid: u32,
    pub epoch_ms: f64,
    pub at_ns: u64,
    pub output: Option<std::sync::mpsc::Receiver<crate::target::isolate::OutputLine>>,
}

/// Starts `app` in `home`: strobe's environment less the profile's `unset`, then the home's, then
/// `env`; the profile's `args`, then `args`; pinned to `cpus` when given (children inherit it); in a
/// group of its own (`host::spawn_owned`).
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
    let (epoch, at_ns) = (epoch_ms(), host::now_ns());
    let mut child = host::spawn_owned(&mut cmd, cpus, 0, Some(&home.dir)).map_err(|e| Error::msg(format!("launch {}: {e}", app.exe.display())))?;
    let output = output.then(|| lines(&mut child));
    Ok(Spawned { pid: child.pid(), child, epoch_ms: epoch, at_ns, output })
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

/// A script runtime (`node`, `bun`, `python3.12`, …) rather than an app of its own.
pub fn interpreter(exe: &Path) -> bool {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    ["python", "pythonw", "py", "node", "bun", "deno", "ruby", "perl", "sh", "bash", "java", "javaw", "pwsh", "powershell"]
        .iter()
        .any(|i| stem == *i || stem.strip_prefix(i).is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.')))
}

/// Refuses to launch an app the user is using: its executable is running, or a guarded process
/// (such as the app's own background service) runs from its install folder or a folder named like
/// it. Isolation should make it safe, but the user's own app is never the thing to experiment on.
pub fn live(app: &App, guard: &Guard) -> Result<()> {
    guard.unreadable()?;
    let mut running = host::running(&app.exe);
    if !running.is_empty() {
        // An interpreter runs every script of its kind: a running one says nothing about the app.
        if interpreter(&app.exe) {
            return Err(Error::refused(
                "LiveChannel",
                format!("{} is running, and it is an interpreter: strobe cannot tell your app's copy from any other script it runs", app.exe.display()),
                "launch the script itself as the executable (a shebang line and chmod +x on Linux and macOS, or a small launcher), so the app has an executable of its own",
            ));
        }
        // A copy strobe launched (stamped) is not the user's: a leftover is ended; one that a
        // running strobe (this one included) still holds is named.
        let stamps: Vec<(u32, Option<Stamp>)> = running.iter().map(|&p| (p, stamp_of(p))).collect();
        let left: Vec<(u32, u32, Stamp)> = stamps.iter().filter_map(|(p, s)| s.clone().filter(leftover).map(|s| (*p, 0, s))).collect();
        if !end_roots(&left).is_empty() {
            for _ in 0..50 {
                running = host::running(&app.exe);
                if !running.iter().any(|p| left.iter().any(|l| l.0 == *p)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if let Some((pid, Some(s))) = stamps.iter().find(|(p, s)| running.contains(p) && s.as_ref().is_some_and(|s| !leftover(s))) {
            return Err(if s.pid == std::process::id() {
                Error::refused(
                    "LiveChannel",
                    format!("{} is running (pid {pid}) from {}, a home this strobe launched it into and has not released", app.exe.display(), s.home),
                    "a launch did not close: end that process",
                )
            } else {
                Error::refused(
                    "LiveChannel",
                    format!("{} is running (pid {pid}) in the isolated home of another strobe (pid {}) that is measuring it", app.exe.display(), s.pid),
                    "wait for that strobe to finish, or stop it; strobe ends such a copy by itself once that strobe is gone",
                )
            });
        }
        if !running.is_empty() {
            let pids: Vec<String> = running.iter().map(u32::to_string).collect();
            let how = if cfg!(windows) { format!("taskkill /PID {}", pids.join(" /PID ")) } else { format!("kill {}", pids.join(" ")) };
            return Err(Error::refused(
                "LiveChannel",
                format!("{} is running (pid {}): that is your app (no strobe launched it)", app.exe.display(), pids.join(", ")),
                format!("measure another build or channel, or close it ({how})"),
            ));
        }
    }
    // The install folder: a macOS app's bundle, else the executable's folder.
    let install = app.exe.ancestors().find(|a| a.extension().is_some_and(|e| e == "app")).or(app.exe.parent());
    let sep = std::path::MAIN_SEPARATOR;
    let dir = install.map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
    let folder = install.and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    for pid in guard.pids() {
        let Some(exe) = host::exe_of(pid) else { continue };
        let path = exe.to_string_lossy().to_lowercase();
        if (!folder.is_empty() && path.contains(&format!("{sep}{folder}{sep}"))) || (!dir.is_empty() && path.starts_with(&dir)) {
            return Err(Error::refused("LiveChannel", format!("your live {} runs from this app ({})", app.id, exe.display()), "measure another build or channel"));
        }
    }
    #[cfg(target_os = "macos")]
    macos_library::check(app)?;
    Ok(())
}

/// A launch until its target takes it (`done`): dropped before (a failed start, an early return), it
/// ends what the launch started (`end_launch`) and releases its folder, removing it when `fresh`.
pub struct Launching {
    pub child: Option<Owned>,
    pub home: Option<PathBuf>,
    /// The launch made the folder: it goes with a failed launch.
    pub fresh: bool,
    done: bool,
}

impl Launching {
    pub fn new(child: Owned, home: Option<PathBuf>, fresh: bool) -> Launching {
        Launching { child: Some(child), home, fresh, done: false }
    }

    /// The launch started: its target owns the child and the folder from here.
    pub fn done(mut self) -> (Option<Owned>, Option<PathBuf>) {
        self.done = true;
        (self.child.take(), self.home.take())
    }
}

impl Drop for Launching {
    fn drop(&mut self) {
        if !self.done {
            end_launch(self.child.take(), self.home.take().as_deref(), self.fresh);
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
    #[cfg(target_os = "macos")]
    macos_library::restore(home);
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

/// macOS keeps an app's data where no environment variable reaches: Cocoa resolves the home from the
/// user's account, never `HOME` (`NSHomeDirectory`, Application Support, caches, saved window state),
/// and `cfprefsd` writes its preferences for the user. A launched copy writes into the user's own
/// `~/Library` folders for its bundle id. So strobe launches such an app only while none of those
/// folders exist (a build with its own bundle id, or an app this user never ran), records which it
/// may create, and removes them (and the preferences domain) when the launch is released. A sandboxed
/// app's container is in the user's Library too: refused.
#[cfg(target_os = "macos")]
mod macos_library {
    use super::*;

    /// The bundle id (an app bundle's `CFBundleIdentifier`), else the executable's name, which is
    /// what Cocoa uses for a bare executable's preferences.
    fn ids(app: &App) -> (String, Option<String>) {
        let bundle = app.exe.ancestors().find(|a| a.extension().is_some_and(|e| e == "app"));
        let id = bundle.and_then(|b| host::output(Command::new("plutil").args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"]).arg(b.join("Contents/Info.plist"))));
        let name = bundle.and_then(|b| b.file_stem()).map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| app.exe.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        (id.clone().unwrap_or_else(|| name.clone()), id.is_some().then_some(name))
    }

    fn places(app: &App) -> (String, Vec<PathBuf>) {
        let (id, name) = ids(app);
        let lib = host::home().join("Library");
        let mut p = vec![
            lib.join(format!("Preferences/{id}.plist")),
            lib.join(format!("Application Support/{id}")),
            lib.join(format!("Caches/{id}")),
            lib.join(format!("Saved Application State/{id}.savedState")),
            lib.join(format!("HTTPStorages/{id}")),
            lib.join(format!("WebKit/{id}")),
            lib.join(format!("Cookies/{id}.binarycookies")),
            lib.join(format!("Containers/{id}")),
        ];
        if let Some(n) = name {
            p.push(lib.join(format!("Application Support/{n}")));
        }
        // A folder the profile's env moves into the home (an app that reads its data folder from an
        // environment variable) is the app's own choice, not Cocoa's: its guards watch it.
        p.retain(|place| !app.moved.iter().any(|m| m == place));
        (id, p)
    }

    fn sandboxed(app: &App) -> bool {
        let bundle = app.exe.ancestors().find(|a| a.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf).unwrap_or_else(|| app.exe.clone());
        host::output(Command::new("codesign").args(["-d", "--entitlements", "-", "--xml"]).arg(&bundle).stderr(Stdio::null())).is_some_and(|x| x.contains("com.apple.security.app-sandbox"))
    }

    pub fn check(app: &App) -> Result<()> {
        let (id, places) = places(app);
        if sandboxed(app) {
            return Err(Error::refused("LiveChannel", format!("{id} is sandboxed: its container lives in your ~/Library, which no launch can redirect"), "measure a build without the App Sandbox entitlement"));
        }
        // Only a folder known to be absent counts as absent: one macOS will not let strobe look at
        // (another developer's app data, macOS 27) may exist, and `restore` would remove it.
        if let Some(p) = places.iter().find(|p| !matches!(p.try_exists(), Ok(false))) {
            return Err(Error::refused(
                "LiveChannel",
                format!(
                    "{} {}: macOS keeps {id}'s data in your ~/Library whatever HOME says, so a launched copy would use and change it",
                    p.display(),
                    if p.try_exists().is_err() { "may exist (macOS would not let strobe look)" } else { "exists" }
                ),
                "measure a build with its own bundle id (or one this account never ran)",
            ));
        }
        Ok(())
    }

    /// Records what the launch may create, for `restore`.
    pub fn save(home: &Path, app: &App) {
        let (id, places) = places(app);
        let list: Vec<String> = places.iter().filter(|p| matches!(p.try_exists(), Ok(false))).map(|p| p.display().to_string()).collect();
        let _ = std::fs::write(home.join(".strobe-macos-library.json"), json!({ "id": id, "created": list }).to_string());
    }

    /// Removes what the launch created in the user's Library, and its preferences domain.
    pub fn restore(home: &Path) {
        let Some(v) = std::fs::read_to_string(home.join(".strobe-macos-library.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
        let id = v["id"].as_str().unwrap_or("");
        let created: Vec<&str> = v["created"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !id.is_empty() && created.iter().any(|p| p.ends_with(&format!("Preferences/{id}.plist"))) {
            let _ = Command::new("defaults").args(["delete", id]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        for p in created {
            let p = Path::new(p);
            let _ = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        }
    }
}
