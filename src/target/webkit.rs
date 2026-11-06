//! WebKit pages (SPEC §13.1) over WebDriver Classic (`webdriver`): Safari Technology Preview, else
//! Safari, through its `safaridriver` on macOS; WebKitGTK's MiniBrowser through `WebKitWebDriver`
//! (headed) or WPE's MiniBrowser through `WPEWebDriver` (headless) on Linux. Classic WebDriver has no
//! preload: strobe's page scripts run in each new document the first time strobe touches it, after the
//! page's own scripts.

use super::chromium::{Gpu, Window};
use super::webdriver::{self, Session};
use crate::host;
use crate::host::spawn::Owned;
use crate::model::{Error, Refusal, Result, ValidatorId};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Which WebKit browser a page runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// Safari Technology Preview: a Safari of its own, apart from the one a person uses.
    Preview,
    Safari,
    /// WebKitGTK's MiniBrowser, headed.
    Gtk,
    /// WPE WebKit's MiniBrowser, headless.
    Wpe,
}

impl Flavor {
    fn name(self) -> &'static str {
        match self {
            Flavor::Preview => "Safari Technology Preview",
            Flavor::Safari => "Safari",
            Flavor::Gtk => "WebKitGTK",
            Flavor::Wpe => "WPE WebKit",
        }
    }
}

/// A WebKit browser strobe can drive: its WebDriver server and the browser it starts.
#[derive(Clone, Debug)]
pub struct Install {
    pub flavor: Flavor,
    pub driver: PathBuf,
    pub browser: PathBuf,
}

/// The browser for a page: on macOS Safari Technology Preview when installed (its own app, never the
/// Safari a person browses with), else Safari; on Linux WPE's MiniBrowser headless, WebKitGTK's headed.
pub fn find(headless: bool) -> Option<Install> {
    if cfg!(target_os = "macos") {
        let preview = PathBuf::from("/Applications/Safari Technology Preview.app/Contents/MacOS");
        if preview.join("safaridriver").is_file() && preview.join("Safari Technology Preview").is_file() {
            return Some(Install { flavor: Flavor::Preview, driver: preview.join("safaridriver"), browser: preview.join("Safari Technology Preview") });
        }
        let (driver, browser) = (PathBuf::from("/usr/bin/safaridriver"), PathBuf::from("/Applications/Safari.app/Contents/MacOS/Safari"));
        return (driver.exists() && browser.is_file()).then_some(Install { flavor: Flavor::Safari, driver, browser });
    }
    if !cfg!(target_os = "linux") {
        return None;
    }
    let (flavor, driver, prefixes): (Flavor, &str, &[&str]) = if headless { (Flavor::Wpe, "WPEWebDriver", &["wpe-webkit-"]) } else { (Flavor::Gtk, "WebKitWebDriver", &["webkitgtk-", "webkit2gtk-"]) };
    let driver = host::find_exe(driver, &[])?;
    let browser = minibrowser(prefixes)?;
    Some(Install { flavor, driver, browser })
}

/// A distribution's MiniBrowser for these library folder prefixes (`/usr/lib/<triplet>/webkitgtk-6.0`,
/// `/usr/libexec/wpe-webkit-2.0`, …), the newest API first.
fn minibrowser(prefixes: &[&str]) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = ["/usr/lib", "/usr/lib64", "/usr/libexec", "/usr/local/lib", "/usr/local/libexec"].iter().map(PathBuf::from).collect();
    roots.extend(std::fs::read_dir("/usr/lib").into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with("-linux-gnu")));
    let mut found: Vec<PathBuf> = roots
        .iter()
        .flat_map(|r| std::fs::read_dir(r).into_iter().flatten().flatten())
        .filter(|e| prefixes.iter().any(|p| e.file_name().to_string_lossy().starts_with(p)))
        .map(|e| e.path().join("MiniBrowser"))
        .filter(|p| p.is_file())
        .collect();
    found.sort_by_key(|p| std::cmp::Reverse(p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().replace("webkit2gtk", "webkitgtk"))));
    found.into_iter().next()
}

/// What an install is before it launches, for the rerun ledger and track's fingerprints: Safari's
/// version from its bundle, else the WebDriver server with its size and time (a package update
/// changes either).
pub fn identity(headless: bool) -> String {
    let Some(i) = find(headless) else { return "webkit: not found".into() };
    if let Some(v) = bundle_version(&i) {
        return v;
    }
    let stamp = std::fs::metadata(&i.driver)
        .ok()
        .map(|m| format!("@{}-{}", m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())))
        .unwrap_or_default();
    format!("{} {}{stamp}", i.flavor.name(), i.driver.display())
}

/// Safari's name and version from its bundle (`Safari 27.0.1`).
fn bundle_version(i: &Install) -> Option<String> {
    let app = i.browser.ancestors().find(|a| a.extension().is_some_and(|e| e == "app"))?;
    Some(format!("{} {}", i.flavor.name(), plist_string(&app.join("Contents/Info.plist"), "CFBundleShortVersionString")?))
}

/// A string value of a property list: read as XML (`<key>k</key><string>v</string>`), else through
/// macOS's `plutil` (a binary one).
fn plist_string(path: &Path, key: &str) -> Option<String> {
    let xml = std::fs::read_to_string(path).ok().and_then(|text| {
        let after = text.split(&format!("<key>{key}</key>")).nth(1)?.to_string();
        Some(after.split("<string>").nth(1)?.split("</string>").next()?.trim().to_string())
    });
    xml.or_else(|| host::output(Command::new("/usr/bin/plutil").args(["-extract", key, "raw", "-o", "-"]).arg(path)).filter(|v| !v.is_empty()))
}

/// What `status` and `up` say about WebKit pages here.
pub fn describe() -> String {
    match (find(false), find(true)) {
        (None, None) if cfg!(windows) => "none: no WebKit browser runs on Windows for people (--engine webkit refuses)".into(),
        (None, None) if cfg!(target_os = "macos") => "none (only for --engine webkit: Safari or Safari Technology Preview)".into(),
        (None, None) => "none (only for --engine webkit: install webkit2gtk-driver for headed pages, wpewebkit-webdriver for headless ones)".into(),
        (a, b) => {
            let mut found: Vec<Install> = [a, b].into_iter().flatten().collect();
            found.dedup_by(|x, y| x.browser == y.browser);
            found.iter().map(|i| format!("{} · {}", bundle_version(i).unwrap_or_else(|| i.flavor.name().into()), i.browser.display())).collect::<Vec<_>>().join("; ")
        }
    }
}

/// A WebKit browser strobe launched, with its one WebDriver session. Dropped without `close`, it still
/// ends everything it started.
pub struct Browser {
    pub flavor: Flavor,
    session: Arc<Session>,
    driver: Option<Owned>,
    driver_pid: u32,
    /// The browser process (Safari, MiniBrowser) and its start time. Safari is started by LaunchServices
    /// at the driver's request, outside the driver's tree: strobe ends it itself.
    pub pid: u32,
    start: Option<u64>,
    /// Linux: the private home (HOME and XDG folders) the driver and browser run in.
    home: Option<PathBuf>,
    pub exe: PathBuf,
    pub version: String,
    pub window: Window,
    pub headless: bool,
    scripts: Arc<Mutex<Vec<String>>>,
    documents: Arc<AtomicU64>,
    pointer: bool,
    shift_base: bool,
    /// macOS: the windows of strobe's Safari once its session opened (the session's own). Any other is a
    /// person's (a link opened into it from another app): that Safari is theirs from then on.
    own_windows: Vec<isize>,
    /// What the browser draws with, read once at launch.
    gpu: Gpu,
}

/// The page strobe drives: the session's one window.
#[derive(Clone)]
pub struct Page {
    session: Arc<Session>,
    /// What every document runs once strobe first touches it (`add_init_script`).
    scripts: Arc<Mutex<Vec<String>>>,
    /// Documents strobe has run its scripts in so far: one more each time the page loads one.
    documents: Arc<AtomicU64>,
    /// The browser delivers pointer actions (`pointer_reaches`), and which browser it is.
    pointer: bool,
    version: String,
    /// A shifted symbol is typed as Shift and its key's own character (`shift_takes_base_key`).
    pub shift_base: bool,
}

/// Processes of `exe` that run now and were not running `before` (a Safari strobe's session request
/// started), ended when dropped while `armed`.
struct Started {
    exe: PathBuf,
    before: Vec<u32>,
    armed: bool,
}

impl Started {
    fn new(exe: &Path) -> Started {
        Started { exe: exe.to_path_buf(), before: host::running(exe), armed: cfg!(target_os = "macos") }
    }

    /// Only a Safari in automation mode (`--automation`, as safaridriver starts it) is strobe's: one a
    /// person opened meanwhile never is.
    fn pids(&self) -> Vec<u32> {
        host::running(&self.exe).into_iter().filter(|p| !self.before.contains(p) && automation(*p)).collect()
    }
}

/// Whether a browser process runs in automation mode, as its WebDriver server starts it (macOS: Safari's
/// `--automation` argument). Elsewhere the browser is always the driver's own child.
fn automation(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        host::macos::args_of(pid).is_some_and(|a| a.iter().any(|x| x == "--automation"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        true
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        if self.armed {
            for pid in self.pids() {
                end_browser(pid, host::start_time(pid));
            }
        }
    }
}

/// Ends a browser process strobe started and what it left: SIGTERM and up to 5 s to go, then its tree
/// and (macOS) every XPC service responsible to it (`host::kill_launch`).
fn end_browser(pid: u32, start: Option<u64>) {
    if pid == 0 || host::start_time(pid) != start {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && host::start_time(pid) == start {
        std::thread::sleep(Duration::from_millis(50));
    }
    host::kill_launch(pid, start);
}

/// A loopback port nothing listens on now.
fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Launches a fresh WebKit browser through its WebDriver server and opens one session.
pub fn launch(window: &Window, headless: bool, cpus: Option<u64>) -> Result<Browser> {
    let unsupported = |what: &str, fix: &str| -> Error { Refusal::new(ValidatorId::EngineUnsupported, what.to_string()).fix(fix).into() };
    if cfg!(target_os = "macos") && headless {
        return Err(unsupported("Safari has no headless mode: a WebKit page on macOS runs headed, in an unlocked session", "drop --headless (and the target's headless)"));
    }
    // A locked screen takes no input (Safari's pointer actions then do nothing) and shows nothing.
    if !headless && host::session_locked() {
        return Err(crate::validity::locked());
    }
    if window.scale.is_some() {
        return Err(unsupported("WebKit pages draw at the display's own scale: WebDriver has no device scale override", "drop the window's scale for --engine webkit"));
    }
    let install = find(headless).ok_or_else(|| {
        if cfg!(target_os = "macos") {
            Error::msg("no Safari found (/Applications/Safari.app)").hint("Safari ships with macOS; Safari Technology Preview: brew install --cask safari-technology-preview")
        } else if headless {
            Error::msg("no WPE WebKit WebDriver found (WPEWebDriver and WPE's MiniBrowser)")
                .hint("install wpewebkit-webdriver (Debian, Ubuntu) or wpewebkit (Fedora, Arch), or run headed (WebKitGTK) with a display")
        } else {
            Error::msg("no WebKitGTK WebDriver found (WebKitWebDriver and WebKitGTK's MiniBrowser)")
                .hint("install webkit2gtk-driver (Debian, Ubuntu) or webkitgtk6.0 / webkit2gtk4.1 (Fedora, Arch), or run --headless (WPE)")
        }
    })?;
    if install.flavor == Flavor::Gtk && std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(Refusal::new(ValidatorId::NoPresentSource, "a headed WebKitGTK page needs a display, and this session has none (no DISPLAY or WAYLAND_DISPLAY)")
            .fix("run --headless (WPE WebKit) or on a desktop session")
            .into());
    }
    let mut started = Started::new(&install.browser);
    // Safari's web content processes are shared by its windows: a session in a Safari a person runs
    // would run beside their tabs, in their Safari.
    let live = |running: &[u32]| -> Error {
        let pids: Vec<String> = running.iter().map(u32::to_string).collect();
        let fix = if install.flavor == Flavor::Safari {
            "quit Safari while strobe measures, or install Safari Technology Preview (brew install --cask safari-technology-preview), which strobe uses instead"
        } else {
            "quit Safari Technology Preview while strobe measures"
        };
        Error::refused(
            "LiveChannel",
            format!("{} is running (pid {}): a WebDriver session would run in that same app, beside your tabs and their web content processes", install.flavor.name(), pids.join(", ")),
            fix,
        )
    };
    if cfg!(target_os = "macos") && !started.before.is_empty() {
        return Err(live(&started.before));
    }
    let port = free_port()?;
    let mut cmd = Command::new(&install.driver);
    match install.flavor {
        Flavor::Safari | Flavor::Preview => cmd.args(["-p", &port.to_string()]),
        Flavor::Gtk | Flavor::Wpe => cmd.arg(format!("--port={port}")),
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let home = (cfg!(target_os = "linux")).then(super::isolate::new_home);
    if let Some(h) = &home {
        for d in ["config", "data", "cache", "state", "run"] {
            std::fs::create_dir_all(h.join(d))?;
        }
        let text = |p: PathBuf| p.display().to_string();
        cmd.env("HOME", text(h.clone()))
            .env("XDG_CONFIG_HOME", text(h.join("config")))
            .env("XDG_DATA_HOME", text(h.join("data")))
            .env("XDG_CACHE_HOME", text(h.join("cache")))
            .env("XDG_STATE_HOME", text(h.join("state")))
            // A measurement never plays sound on the user's speakers: GStreamer's automatic audio sink picks
            // its fake sink, which keeps the clock, so media and Web Audio still run.
            .env("GST_PLUGIN_FEATURE_RANK", "fakeaudiosink:MAX")
            // GSettings in memory: nothing the browser sets reaches the user's dconf database.
            .env("GSETTINGS_BACKEND", "memory")
            .envs(super::isolate::launch_env(h));
        if headless {
            // A headless page talks to no desktop: none of the session's services (portals, the
            // accessibility bus), which a server's session may answer only after a long timeout. No
            // display either, and a bus address that leads nowhere, so GDBus never autolaunches a bus
            // through X11 onto the user's session.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(h.join("run"), std::fs::Permissions::from_mode(0o700));
            }
            cmd.env("XDG_RUNTIME_DIR", text(h.join("run")))
                .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", h.join("run").join("no-bus").display()))
                .env("NO_AT_BRIDGE", "1")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY");
        }
    }
    let child = match host::spawn_owned(&mut cmd, cpus, 0, home.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            if let Some(h) = &home {
                super::isolate::release_home(h);
                super::isolate::remove_home(h);
            }
            return Err(Error::msg(format!("launch {}: {e}", install.driver.display())));
        }
    };
    let driver_pid = child.pid();
    let mut launching = super::isolate::Launching::new(child, home.clone(), true);
    let server = webdriver::Driver::new(port);
    server.wait_ready(Duration::from_secs(15), &mut || launching.child.as_mut().is_some_and(|c| c.exited().is_none()))?;
    let w = window;
    let caps = match install.flavor {
        Flavor::Safari => json!({ "browserName": "safari" }),
        Flavor::Preview => json!({ "browserName": "Safari Technology Preview" }),
        Flavor::Gtk => json!({ "browserName": "MiniBrowser", "webkitgtk:browserOptions": { "binary": install.browser, "args": ["--automation"] } }),
        Flavor::Wpe => json!({ "browserName": "MiniBrowser", "wpe:browserOptions": { "binary": install.browser, "args": ["--automation", "--headless", format!("--size={}x{}", w.width, w.height)] } }),
    };
    // Again right before the session: LaunchServices would hand it to a Safari a person opened while the
    // driver started.
    if cfg!(target_os = "macos") {
        let running = host::running(&install.browser);
        if !running.is_empty() {
            started.before = running.clone();
            return Err(live(&running));
        }
    }
    let session = match server.new_session(caps, Duration::from_secs(60))? {
        Ok(s) => s,
        Err(f) if f.message.to_lowercase().contains("remote automation") => {
            let enable = if install.flavor == Flavor::Preview { format!("\"{}\" --enable", install.driver.display()) } else { "safaridriver --enable".into() };
            return Err(Error::new(3, format!("{}'s remote automation is off: {}", install.flavor.name(), f.message))
                .hint(format!("run `{enable}` once (it asks for an administrator's password), or turn on Settings › Developer › Allow remote automation in {}", install.flavor.name())));
        }
        Err(f) => return Err(Error::unreachable(format!("{} started no WebDriver session: {}", install.flavor.name(), f.text()))),
    };
    let session = Arc::new(session);
    let caps = session.capabilities.clone();
    // The browser process: WebKitGTK names it; Safari is the new process of its executable; else the
    // driver's child that runs the browser.
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        let found = caps["webkit:processID"].as_u64().map(|p| p as u32).or_else(|| match install.flavor {
            Flavor::Safari | Flavor::Preview => started.pids().first().copied(),
            _ => host::process_tree(driver_pid).into_iter().map(|p| p.pid).find(|&p| host::exe_of(p).is_some_and(|e| e == install.browser)),
        });
        if let Some(p) = found {
            break p;
        }
        if Instant::now() > deadline {
            let _ = session.delete(Duration::from_secs(5));
            return Err(Error::unreachable(format!("{} opened a session, but strobe found no process of {}", install.flavor.name(), install.browser.display())));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let start = host::start_time(pid);
    // strobe keeps its own deadlines: a script runs as long as the call that waits for it allows (a day;
    // safaridriver takes no null, "no limit", for it).
    session.send("POST", "/timeouts", Some(&json!({ "script": 86_400_000, "pageLoad": 3_600_000 })), Duration::from_secs(10))?;
    let version = format!("{} {}", install.flavor.name(), caps["browserVersion"].as_str().unwrap_or(""));
    if !headless {
        session.set_window_rect(w.x, w.y, w.width, w.height)?;
    }
    let gpu = read_gpu(&session)?;
    // The input checks run on a blank page strobe loaded itself, with the window brought forward where
    // macOS lets strobe (nobody used the Mac for a minute). A Safari window behind other apps may not
    // answer them: the block waits, as for any window that is not the active one.
    session.send("POST", "/url", Some(&json!({ "url": "about:blank" })), Duration::from_secs(30))?;
    if !headless {
        let set: Vec<u32> = host::process_set(pid, home.as_deref()).iter().map(|p| p.pid).filter(|&p| p != driver_pid).collect();
        host::raise_windows(&set);
    }
    let checked = pointer_reaches(&session).and_then(|p| Ok((p, shift_takes_base_key(&session)?)));
    let (pointer, shift_base) = checked.map_err(|e| match e.code {
        124 if cfg!(target_os = "macos") => Refusal::window_hidden(
            crate::model::Hidden::NotActive,
            format!("{} did not answer strobe's input check on a blank page ({}): its window is behind other apps", install.flavor.name(), e.message),
        )
        .fix("let Safari be the active app: strobe brings it forward once nobody has used the Mac for a minute")
        .into(),
        _ => Error { message: format!("{}'s input check on a blank page: {}", install.flavor.name(), e.message), ..e },
    })?;
    // macOS: the session's own windows, so a window a person opens in this Safari later shows (`own`).
    let own_windows: Vec<isize> = if cfg!(target_os = "macos") { host::top_windows(&[pid]).into_iter().map(|w| w.0).collect() } else { Vec::new() };
    let (driver, home) = launching.done();
    started.armed = false;
    Ok(Browser {
        flavor: install.flavor,
        session,
        driver,
        driver_pid,
        pid,
        start,
        home,
        exe: install.browser,
        version: version.trim().to_string(),
        window: window.clone(),
        headless,
        scripts: Arc::new(Mutex::new(vec![NOT_AUTOMATED.to_string()])),
        documents: Arc::new(AtomicU64::new(0)),
        pointer,
        shift_base,
        own_windows,
        gpu,
    })
}

/// What the browser draws with, read once at launch. macOS: the Mac's GPU (WebKit composites through Core
/// Animation on Metal; a software renderer only in a virtual machine). Linux: WebKit's own `webkit://gpu`
/// page (its GL renderer and how it composites), before the target loads.
fn read_gpu(session: &Session) -> Result<Gpu> {
    if cfg!(target_os = "macos") {
        let devices: Vec<String> = crate::probe::gpu::adapters().into_iter().map(|a| a.name).collect();
        let software = devices.is_empty() || devices.iter().any(|d| crate::validity::is_software_gpu(d) || d.to_lowercase().contains("paravirtual"));
        return Ok(Gpu { devices, compositing: "Core Animation".into(), software });
    }
    session.send("POST", "/url", Some(&json!({ "url": "webkit://gpu" })), Duration::from_secs(30))?;
    let text = session.send("POST", "/execute/sync", Some(&json!({ "script": "return document.body.innerText", "args": [] })), Duration::from_secs(10))?;
    let lines: Vec<&str> = text.as_str().unwrap_or("").lines().map(str::trim).collect();
    let after = |key: &str| lines.iter().position(|l| *l == key).and_then(|i| lines.get(i + 1)).map(|s| s.to_string()).filter(|s| !s.is_empty());
    let devices: Vec<String> = after("GL_RENDERER").into_iter().collect();
    let compositing = [after("Renderer"), after("API"), after("Platform")].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let software = devices.first().is_none_or(|d| crate::validity::is_software_gpu(d));
    Ok(Gpu { devices, compositing, software })
}

/// Whether the browser delivers WebDriver pointer input to the page, checked on the blank page the
/// session opened, before the target loads: two pointer moves and a listener for them. WPE WebKit's
/// headless view (2.48) takes keys and wheel steps but drops pointer actions without an error.
fn pointer_reaches(session: &Session) -> Result<bool> {
    let t = Duration::from_secs(30);
    let js = |script: &str| session.send("POST", "/execute/sync", Some(&json!({ "script": script, "args": [] })), t);
    js("window.__strobePointer = false; addEventListener('pointermove', () => { window.__strobePointer = true }, { capture: true }); return true")?;
    let moves = json!([{ "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" }, "actions": [
        { "type": "pointerMove", "x": 3, "y": 3, "origin": "viewport" },
        { "type": "pointerMove", "x": 6, "y": 6, "origin": "viewport" },
    ] }]);
    session.actions(&moves, t)?;
    let seen = js("return window.__strobePointer === true")?;
    session.send("DELETE", "/actions", None, t)?;
    Ok(seen == Value::Bool(true))
}

/// Whether the browser types a shifted symbol only from its key's own character with Shift held
/// (Shift and `1` for `!`), checked in a field on the blank page before the target loads. Safari 27
/// turns Shift with `!` into a dead key and types nothing, and Shift with `1` into `!`; WPE types `1`
/// for that, and WebKitGTK both.
fn shift_takes_base_key(session: &Session) -> Result<bool> {
    let t = Duration::from_secs(30);
    let js = |script: &str| session.send("POST", "/execute/sync", Some(&json!({ "script": script, "args": [] })), t);
    js("const i = document.createElement('input'); i.id = '__strobeKeys'; document.documentElement.append(i); i.focus(); return true")?;
    let shift = |c: &str| {
        json!([{ "type": "key", "id": "keyboard", "actions": [
        { "type": "keyDown", "value": "\u{E008}" }, { "type": "keyDown", "value": c }, { "type": "keyUp", "value": c }, { "type": "keyUp", "value": "\u{E008}" },
    ] }])
    };
    session.actions(&shift("!"), t)?;
    let shifted = js("return document.getElementById('__strobeKeys').value")?;
    let base = if shifted == "!" {
        false
    } else {
        js("document.getElementById('__strobeKeys').value = ''; return true")?;
        session.actions(&shift("1"), t)?;
        js("return document.getElementById('__strobeKeys').value")? == "!"
    };
    js("document.getElementById('__strobeKeys').remove(); return true")?;
    session.send("DELETE", "/actions", None, t)?;
    Ok(base)
}

/// An async script that evaluates `value` and answers `{ v }` (its value as JSON) or `{ e }`, or
/// `{ fresh }` without evaluating it in a document strobe's page scripts have not run in yet.
fn script(value: &str) -> String {
    format!(
        "const __done = arguments[arguments.length - 1];\nif (!window.__strobeDoc) {{ __done({{ fresh: true }}); return }}\nconst __json = (v) => {{ try {{ return v === undefined ? null : JSON.parse(JSON.stringify(v)) ?? null }} catch {{ return null }} }};\nlet __v;\ntry {{ __v = {value}; }} catch (e) {{ __done({{ e: String(e && e.message || e) }}); return }}\nPromise.resolve(__v).then((v) => __done({{ v: __json(v) }}), (e) => __done({{ e: String(e && e.message || e) }}));"
    )
}

/// `navigator.webdriver` false, as for a person (WebKit reports true under WebDriver). Without a
/// preload the page's own first scripts still see true.
const NOT_AUTOMATED: &str = "Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => false, configurable: true })";

impl Browser {
    /// The page strobe drives. Headed, its window is kept on top (macOS: the active app, once nobody
    /// uses the Mac), as an occluded window may stop painting.
    pub fn page(&self) -> Result<Page> {
        let page =
            Page { session: self.session.clone(), scripts: self.scripts.clone(), documents: self.documents.clone(), pointer: self.pointer, version: self.version.clone(), shift_base: self.shift_base };
        if !self.headless {
            let deadline = Instant::now() + Duration::from_secs(5);
            while host::raise_windows(&self.pids()) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(page)
    }

    /// Sizes the window so the page's viewport, not the whole window, is `width`×`height` CSS pixels.
    /// WPE's headless view is the viewport, sized at launch.
    pub fn fit_viewport(&self, page: &Page, width: u32, height: u32) -> Result<()> {
        let inner = page.eval("[innerWidth, innerHeight, outerWidth, outerHeight]")?;
        let num = |i: usize| inner[i].as_f64().unwrap_or(0.0);
        if num(0) as u32 == width && num(1) as u32 == height {
            return Ok(());
        }
        if self.flavor == Flavor::Wpe {
            return Err(Error::msg(format!("WPE's view is {}×{}, not the {width}×{height} it was launched with", num(0), num(1))));
        }
        let (dw, dh) = (num(2) - num(0), num(3) - num(1));
        let w = &self.window;
        self.session.set_window_rect(w.x, w.y, (width as f64 + dw).round() as u32, (height as f64 + dh).round() as u32)
    }

    /// Every process of this browser: its tree and, on macOS, the XPC services responsible to it (web
    /// content, GPU, networking); on Linux also what carries its home's stamp. Never the WebDriver
    /// server: that is strobe's tool, not the page's.
    pub fn processes(&self) -> Vec<host::ProcTimes> {
        host::process_set(self.pid, self.home.as_deref()).into_iter().filter(|p| p.pid != self.driver_pid).collect()
    }

    pub fn pids(&self) -> Vec<u32> {
        self.processes().iter().map(|p| p.pid).collect()
    }

    /// Each process's role by its executable: web content processes are the renderer (Safari's
    /// `WebContent`, `WebContent.Development`, `WebContent.CaptivePortal`; WebKitGTK's and WPE's
    /// `WebProcess`), the GPU process the GPU, the browser itself the browser (networking has none).
    pub fn process_roles(&self) -> Vec<(u32, &'static str)> {
        self.processes()
            .into_iter()
            .filter_map(|p| {
                let name = host::exe_of(p.pid).and_then(|e| e.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or(p.name);
                let role = match name.as_str() {
                    _ if p.pid == self.pid => "browser",
                    n if n.contains("WebContent") || n.contains("WebProcess") => "renderer",
                    n if n.contains("WebKit.GPU") || n.contains("GPUProcess") => "gpu",
                    _ => return None,
                };
                Some((p.pid, role))
            })
            .collect()
    }

    /// What the browser draws with, as read at launch (`read_gpu`).
    pub fn gpu(&self) -> Result<Gpu> {
        Ok(self.gpu.clone())
    }

    /// macOS: the windows of strobe's Safari that are not its session's: a person opened them (a link from
    /// another app lands in the running Safari), so that Safari holds their tabs now.
    fn foreign_windows(&self) -> usize {
        if !cfg!(target_os = "macos") || self.pid == 0 {
            return 0;
        }
        host::top_windows(&[self.pid]).into_iter().filter(|w| !self.own_windows.contains(&w.0)).count()
    }

    /// `LiveChannel` once a person has opened a window in strobe's Safari: what the block measured ran
    /// beside their tabs, and strobe leaves that Safari to them when it closes.
    pub fn own(&self) -> Result<()> {
        match self.foreign_windows() {
            0 => Ok(()),
            n => Err(Error::refused(
                "LiveChannel",
                format!("{n} window(s) not strobe's opened in the {} strobe launched (a link opened from another app lands there): the block ran beside them", self.flavor.name()),
                "leave the Mac alone while strobe measures in Safari, or install Safari Technology Preview; strobe leaves that Safari running for whoever opened them",
            )),
        }
    }

    /// The processes now with their start times, read while the browser still runs: macOS answers which
    /// app an XPC service is responsible to only while that app runs.
    fn snapshot(&self) -> Vec<(u32, Option<u64>)> {
        self.processes().iter().map(|p| (p.pid, host::start_time(p.pid))).collect()
    }

    /// Ends the session (the driver closes its window), stops the driver, ends the browser strobe started
    /// and every process it left, and removes the private home. A Safari a person opened a window in by
    /// then is left running, with its services.
    pub fn close(mut self) {
        let set = self.snapshot();
        let deleted = self.session.delete(Duration::from_secs(10)).is_ok();
        // The session's windows close with it: one still there is a person's.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut theirs = deleted && cfg!(target_os = "macos") && !host::top_windows(&[self.pid]).is_empty();
        while theirs && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            theirs = !host::top_windows(&[self.pid]).is_empty();
        }
        if theirs || self.foreign_windows() > 0 {
            self.pid = 0;
        }
        self.end_with(set);
    }

    fn end(&mut self) {
        if self.foreign_windows() > 0 {
            self.pid = 0;
        }
        let set = self.snapshot();
        self.end_with(set);
    }

    /// Stops the driver and ends the browser (`pid`, none when it is a person's now) and every process of
    /// `set` still running, by start time: an XPC service of a Safari that already quit too.
    fn end_with(&mut self, set: Vec<(u32, Option<u64>)>) {
        drop(self.driver.take());
        let pid = std::mem::take(&mut self.pid);
        if pid != 0 {
            end_browser(pid, self.start);
            for (p, start) in set.into_iter().filter(|(_, s)| s.is_some()) {
                if host::start_time(p) == start {
                    host::kill_launch(p, start);
                }
            }
        }
        if let Some(h) = self.home.take() {
            super::isolate::end_launch(None, Some(&h), true);
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.end();
    }
}

impl Page {
    pub fn eval(&self, expression: &str) -> Result<Value> {
        self.eval_timeout(expression, Duration::from_secs(60))
    }

    /// Evaluates an expression in the page and returns its value as JSON (awaiting promises). In a
    /// document strobe has not touched yet, strobe's page scripts run first (`install`): the call itself
    /// stays small, as the page parses it on its main thread every time. The value comes back through a
    /// JSON round trip in the page, so the driver never fails to send it: a WebDriver `javascript error`
    /// then means the expression did not compile as one, and it runs again as statements (their
    /// completion value, through an indirect `eval`).
    pub fn eval_timeout(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let value = |body: &str| -> Result<webdriver::Reply> { self.session.execute_async(&script(body), timeout) };
        let once = || -> Result<webdriver::Reply> {
            Ok(match value(&format!("(\n{expression}\n)"))? {
                Err(f) if f.error == "javascript error" => value(&format!("(0, eval)({})", json!(expression)))?,
                r => r,
            })
        };
        let mut reply = once()?;
        // A new document: strobe's scripts first, then the call (once more if the page replaced it again).
        for _ in 0..2 {
            if !matches!(&reply, Ok(r) if r["fresh"] == true) {
                break;
            }
            self.install(timeout)?;
            reply = once()?;
        }
        let r = reply.map_err(|f| Error::msg(format!("page: {}", f.text())))?;
        if r["fresh"] == true {
            return Err(Error::msg("page: the page kept loading new documents while strobe called it"));
        }
        if let Some(e) = r["e"].as_str() {
            return Err(Error::msg(format!("page: {}", e.lines().next().unwrap_or(e))));
        }
        Ok(r["v"].clone())
    }

    /// Runs strobe's page scripts in the current document (once: it marks the document) and counts it.
    fn install(&self, timeout: Duration) -> Result<()> {
        let scripts: String = self.scripts.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|s| format!("try {{ (() => {{\n{s}\n}})() }} catch {{}}\n")).collect();
        let body = format!("if (window.__strobeDoc) return false;\nwindow.__strobeDoc = true;\n{scripts}return true;");
        if self.session.send("POST", "/execute/sync", Some(&json!({ "script": body, "args": [] })), timeout)? == Value::Bool(true) {
            self.documents.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Navigates and waits for the load event, then runs strobe's page scripts in the new document.
    pub fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        self.session.send("POST", "/url", Some(&json!({ "url": url })), timeout).map_err(|e| match e.code {
            124 => e,
            _ => Error::unreachable(format!("navigate {url}: {}", e.message)),
        })?;
        self.install(Duration::from_secs(30))
    }

    /// A script each document runs once strobe first touches it: WebDriver Classic has no preload, so
    /// after the document's own scripts.
    pub fn add_init_script(&self, source: &str) -> Result<()> {
        let mut s = self.scripts.lock().unwrap_or_else(|e| e.into_inner());
        if !s.iter().any(|x| x == source) {
            s.push(source.to_string());
        }
        Ok(())
    }

    /// The viewport as PNG bytes.
    pub fn screenshot(&self, timeout: Duration) -> Result<Vec<u8>> {
        self.session.screenshot(timeout)
    }

    /// Input actions, back once the browser dispatched them. Classic WebDriver answers each list only
    /// after it ran, so a paced stream's moves go one list at a time.
    pub fn actions(&self, actions: Value) -> Result<()> {
        if !self.pointer && actions.as_array().into_iter().flatten().any(|s| s["type"] == "pointer") {
            return Err(Error::refused(
                "EngineUnsupported",
                format!("{} delivers no WebDriver pointer input to the page (strobe's check at launch: a pointer move reached no listener), so a click, hover or drag would do nothing", self.version),
                "keep the case to keys, scrolling and eval there, run it headed (WebKitGTK, with a display), or measure it in Chromium or Firefox",
            ));
        }
        self.session.actions(&actions, Duration::from_secs(120))
    }

    /// How many documents strobe has run its page scripts in: a run that changes it loaded a page.
    pub fn documents(&self) -> u64 {
        self.documents.load(Ordering::Relaxed)
    }
}
