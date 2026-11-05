//! Apps that embed the OS's webview (SPEC §13.8), launched in an isolated home (`isolate.rs`) with no
//! change to the app. Windows' WebView2 is Chromium: the launch's environment asks it for its user data
//! folder in the home and a debugging port, and strobe drives and measures the page over CDP as an
//! Electron app's. WebKit webviews (Linux WebKitGTK, macOS WKWebView) are probed for the remote
//! inspector and for a WebDriver server the profile names, then refused until strobe drives WebKit:
//! `kind: "native"` measures such an app through the OS accessibility tree (§13.7).

use super::chromium::{Browser, Window};
use super::electron::{Extra, Started};
use super::isolate::{self, App, Guard, Home};
use crate::model::{Error, Result};
use std::path::Path;

/// What WebView2 reads when the app creates its environment: the user data folder replaces the one
/// the app passes, and the browser arguments are appended to the app's own.
#[cfg(windows)]
const DATA: &str = "WEBVIEW2_USER_DATA_FOLDER";
#[cfg(windows)]
const ARGS: &str = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";

/// What every refusal of a webview target suggests instead.
const NATIVE: &str = "measure it as a native app, through the OS accessibility tree: `kind: \"native\"` in its profile, native(\"<id>\") in a bench, or exe:<path> --native";

/// Launches a webview `app` in a fresh private home (seeded from `seed`, or `extra`'s home) and
/// attaches to its page's browser over CDP.
pub fn launch_with(app: &App, seed: Option<&Path>, window: &Window, cpus: Option<u64>, extra: &Extra) -> Result<(Browser, Guard, Started)> {
    #[cfg(windows)]
    {
        webview2(app, seed, window, cpus, extra)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, cpus);
        Err(webkit(app, seed, extra))
    }
}

/// The home `extra` names, else a new one: what every launch here starts from.
fn home(app: &App, seed: Option<&Path>, extra: &Extra) -> Result<Home> {
    match &extra.prepared {
        Some(h) => Ok(h.clone()),
        None => isolate::prepare_home(app, seed, extra.home.clone()),
    }
}

/// Ends a launch that failed before it started anything: its home goes when the launch made it.
fn abandon(home: &Home) {
    isolate::release_home(&home.dir);
    if home.fresh {
        isolate::remove_home(&home.dir);
    }
}

/// WebView2's environment for this launch: its user data folder in the home (the profile may name
/// another folder inside it), and `--remote-debugging-port=0` after any arguments the profile or the
/// user's environment give it. Returns the variables and the folder.
#[cfg(windows)]
fn webview2_env(app: &App, home: &Home) -> Result<(Vec<(String, String)>, std::path::PathBuf)> {
    let env = home.environment(app);
    let named = |name: &str| env.iter().find(|(k, _)| isolate::env_name_is(k, name)).map(|(_, v)| v.clone());
    // Only the profile's own value (`home.env`): one inherited from the user is their webview data.
    let data = match home.env.iter().rev().find(|(k, _)| isolate::env_name_is(k, DATA)).map(|(_, v)| v.clone()) {
        Some(d) => {
            if !Path::new(&d).starts_with(&home.dir) {
                return Err(Error::usage(format!("app profile \"{}\": env {DATA} is {d}, outside the launch's private home: the copy would share that webview data", app.id))
                    .hint(format!("name a folder in the home ({{home}}/…), or leave {DATA} out: strobe uses {{home}}/WebView2")));
            }
            std::path::PathBuf::from(d)
        }
        None => home.dir.join("WebView2"),
    };
    let args = named(ARGS).unwrap_or_default();
    if args.contains("--remote-debugging-") {
        return Err(Error::usage(format!("app profile \"{}\": {ARGS} already names a debugging port ({args}): strobe opens its own", app.id)));
    }
    let args = format!("{args} --remote-debugging-port=0").trim().to_string();
    Ok((vec![(DATA.into(), data.display().to_string()), (ARGS.into(), args)], data))
}

#[cfg(windows)]
fn webview2(app: &App, seed: Option<&Path>, window: &Window, cpus: Option<u64>, extra: &Extra) -> Result<(Browser, Guard, Started)> {
    use super::cdp::Cdp;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};
    let mut guard = Guard::take(app);
    isolate::live(app, &guard)?;
    let prepared = home(app, seed, extra)?;
    guard.shared = prepared.shared.clone();
    let (env, data) = match webview2_env(app, &prepared) {
        Ok(x) => x,
        Err(e) => {
            abandon(&prepared);
            return Err(e);
        }
    };
    // `--inspect-brk` and `profile_main` read an Electron main process's Node: a webview app has none.
    let spawned = match isolate::spawn(app, &prepared, &extra.args, &env, extra.output, cpus) {
        Ok(s) => s,
        Err(e) => {
            abandon(&prepared);
            return Err(e);
        }
    };
    let (pid, epoch, output) = (spawned.pid, spawned.epoch_ms, spawned.output);
    let mut launching = isolate::Launching::new(spawned.child, Some(prepared.dir.clone()), prepared.fresh);
    // WebView2 writes DevToolsActivePort into the profile folder it makes in the user data folder
    // (`EBWebView`) once the app creates its webview.
    let deadline = Instant::now() + Duration::from_secs(60);
    let text = loop {
        guard.check("while the app started")?;
        if let Some(t) = isolate::find(&data, "DevToolsActivePort", 3).into_iter().find_map(|p| std::fs::read_to_string(p).ok().filter(|t| t.lines().count() >= 2)) {
            break t;
        }
        // A launcher stub that starts the real app and exits succeeds: a clean exit keeps waiting.
        if let Some(code) = launching.child.as_mut().and_then(|c| c.exited())
            && code != 0
        {
            return Err(Error::unreachable(format!("{} exited (code {code}) before its webview opened a debugging port", app.exe.display())));
        }
        if Instant::now() > deadline {
            return Err(Error::unreachable(format!("{} opened no WebView2 debugging port in 60 s (none in {})", app.exe.display(), data.display()))
                .hint(format!("a webview target is an app that embeds WebView2; for any other app, {NATIVE}")));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut lines = text.lines();
    let (port, path) = (lines.next().unwrap_or("").trim(), lines.next().unwrap_or("").trim());
    let ws = format!("ws://127.0.0.1:{port}{path}");
    let cdp = Cdp::connect(&ws)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    let fresh = launching.fresh;
    let (child, profile) = launching.done();
    Ok((Browser { cdp, ws, child, pid, profile, fresh, exe: app.exe.clone(), version, window: window.clone() }, guard, Started { epoch_ms: epoch, output, main: None }))
}

/// A WebKit webview (Linux WebKitGTK, macOS WKWebView): the app launched in its home with the remote
/// inspector's HTTP server asked for (Linux) and its profile's WebDriver server (`webdriver`), each
/// probed until it answers or the app has had 15 s, then ended. Always a refusal: strobe does not
/// drive WebKit yet, and the refusal says which way the app could be driven once it does.
#[cfg(not(windows))]
fn webkit(app: &App, seed: Option<&Path>, extra: &Extra) -> Error {
    use std::time::{Duration, Instant};
    let os = if cfg!(target_os = "macos") { "macOS's WKWebView" } else { "WebKitGTK" };
    let refuse = |found: String| Error::refused("NotOnThisOs", format!("{}: a webview target on {} is driven over WebKit's own protocols, which strobe does not drive yet; {found}", app.id, os), NATIVE);
    // macOS opens no inspector to a process outside the app: only a WebDriver server the app serves.
    if cfg!(target_os = "macos") && app.webdriver.is_none() {
        return refuse("WKWebView opens no debugging port the app does not open itself, and the profile names no WebDriver server (`webdriver`) it serves".into());
    }
    let guard = Guard::take(app);
    if let Err(e) = isolate::live(app, &guard) {
        return e;
    }
    let prepared = match home(app, seed, extra) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let inspector = cfg!(target_os = "linux").then(|| std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr())).and_then(|a| a.ok());
    let env: Vec<(String, String)> = inspector.map(|a| ("WEBKIT_INSPECTOR_HTTP_SERVER".to_string(), a.to_string())).into_iter().collect();
    let webdriver = app.webdriver.as_deref().map(|u| prepared.fill(u));
    let spawned = match isolate::spawn(app, &prepared, &extra.args, &env, false, None) {
        Ok(s) => s,
        Err(e) => {
            abandon(&prepared);
            return e;
        }
    };
    let mut launching = isolate::Launching::new(spawned.child, Some(prepared.dir.clone()), prepared.fresh);
    let deadline = Instant::now() + Duration::from_secs(15);
    let (mut seen, mut exited) = (Probe::default(), None);
    while Instant::now() < deadline {
        seen = Probe::read(inspector, webdriver.as_deref());
        exited = launching.child.as_mut().and_then(|c| c.exited());
        if seen.done() || exited.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    drop(launching);
    let mut found = Vec::new();
    if let Some(code) = exited {
        found.push(format!("the app exited (code {code}) within 15 s"));
    }
    if inspector.is_some() {
        found.push(match seen.pages {
            Some(n) if n > 0 => format!("its remote inspector lists {n} inspectable page{}", if n == 1 { "" } else { "s" }),
            Some(_) => "its remote inspector lists no inspectable page (an app turns its webview's developer extras on only in debug builds)".into(),
            None if seen.listening => "its remote inspector listens but answered nothing in 15 s (the webview is not inspectable: developer extras off)".into(),
            None => "it opened no remote inspector (WEBKIT_INSPECTOR_HTTP_SERVER): not a WebKitGTK webview, or one built without it".into(),
        });
    }
    if let Some(url) = &webdriver {
        found.push(if seen.webdriver { format!("its WebDriver server answers at {url}") } else { format!("no WebDriver server answered at {url}") });
    }
    refuse(found.join("; "))
}

/// What a WebKit webview app answered.
#[cfg(not(windows))]
#[derive(Default)]
struct Probe {
    /// The remote inspector's HTTP server accepts connections.
    listening: bool,
    /// The pages its index lists, once it answered.
    pages: Option<usize>,
    /// The profile's WebDriver server answered `/status`.
    webdriver: bool,
}

#[cfg(not(windows))]
impl Probe {
    fn read(inspector: Option<std::net::SocketAddr>, webdriver: Option<&str>) -> Probe {
        let mut p = Probe::default();
        if let Some(addr) = inspector {
            p.listening = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_ok();
            // The index links each inspectable page to its socket (`/socket/<connection>/<target>/…`).
            p.pages = p.listening.then(|| super::chromium::http_get(&addr.to_string(), "/").ok()).flatten().map(|html| html.matches("/socket/").count());
        }
        if let Some(url) = webdriver {
            let host = url.trim_start_matches("http://").split('/').next().unwrap_or("");
            p.webdriver = super::chromium::http_get(host, "/status").is_ok_and(|b| b.contains("\"value\""));
        }
        p
    }

    fn done(&self) -> bool {
        self.pages.is_some_and(|n| n > 0) || self.webdriver
    }
}
