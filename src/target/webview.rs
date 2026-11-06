//! Apps that embed the OS's webview (SPEC §13.8), launched in an isolated home (`isolate.rs`) with no
//! change to the app. Windows' WebView2 is Chromium: the launch's environment asks it for its user data
//! folder in the home and a debugging port, and strobe drives and measures the page over CDP as an
//! Electron app's. WebKit webviews (Linux WebKitGTK, macOS WKWebView) are refused until strobe drives
//! WebKit: `kind: "native"` measures such an app through the OS accessibility tree (§13.7).

use super::chromium::{Browser, Window};
use super::electron::{Extra, Started};
use super::isolate::App;
use crate::model::Error;
#[cfg(windows)]
use super::isolate::{self, Guard, Home};
#[cfg(windows)]
use crate::model::Result;
#[cfg(windows)]
use std::path::{Path, PathBuf};

/// What every refusal of a webview target suggests instead.
const NATIVE: &str = "measure it as a native app, through the OS accessibility tree: `kind: \"native\"` in its profile, native(\"<id>\") in a bench, or exe:<path> --native";

/// What WebView2 reads when the app creates its environment: the user data folder replaces the one
/// the app passes, and the browser arguments are appended to the app's own.
#[cfg(windows)]
const DATA: &str = "WEBVIEW2_USER_DATA_FOLDER";
#[cfg(windows)]
const ARGS: &str = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";

/// What strobe appends to the webview's arguments, as for a browser it launches (§13.1): its debugging
/// port, no sound, no first-run work, and no background traffic or component updates mid-run.
#[cfg(windows)]
const STROBE_ARGS: &[&str] = &["--remote-debugging-port=0", "--mute-audio", "--no-first-run", "--disable-background-networking", "--disable-component-update"];

/// How long WebView2's own processes may run in the launch without a debugging port in strobe's
/// folder before the launch counts as overridden.
#[cfg(windows)]
const OVERRIDE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Launches a webview `app` in a fresh private home (seeded from `seed`, or `extra`'s home) and
/// attaches to its page's browser over CDP.
pub fn launch_with(app: &App, seed: Option<&std::path::Path>, window: &Window, cpus: Option<u64>, extra: &Extra) -> crate::model::Result<(Browser, super::isolate::Guard, Started)> {
    #[cfg(windows)]
    {
        webview2(app, seed, window, cpus, extra)
    }
    #[cfg(not(windows))]
    {
        let _ = (seed, window, cpus, extra);
        Err(webkit_refusal(app))
    }
}

/// `EngineUnsupported` for a webview target off Windows, before anything launches.
#[cfg(not(windows))]
fn webkit_refusal(app: &App) -> Error {
    let (webview, route) = if cfg!(target_os = "macos") {
        ("macOS's WKWebView", "a WebDriver server the app serves itself: WKWebView opens no inspector to another process")
    } else {
        ("WebKitGTK", "the remote inspector WEBKIT_INSPECTOR_HTTP_SERVER opens, which a release app's webview answers")
    };
    Error::refused("EngineUnsupported", format!("{}: a webview target on {webview} is driven over WebKit's own protocols, which strobe does not drive yet (it will through {route})", app.id), NATIVE)
}

/// Asks the windows of `pids` to close (`WM_CLOSE`), as a person closing the app; returns how many.
/// A webview's browser belongs to its app: closing the browser leaves the app running.
pub fn ask_close(pids: &[u32]) -> usize {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
        crate::host::top_windows(pids).iter().filter(|w| unsafe { PostMessageW(w.0 as _, WM_CLOSE, 0, 0) } != 0).count()
    }
    #[cfg(not(windows))]
    {
        let _ = pids;
        0
    }
}

/// The window an app's page shows in (a webview's, an Electron app's), as the block has it: with
/// `place`, its main window moved
/// to the block's place and, with `size`, its client area sized so the page's viewport is the block's
/// width × height CSS pixels (a second pass takes off what the app draws around its webview). Returns
/// the main window's place and the page's own viewport and scale, which is what result.json records.
/// An app that keeps its size is refused: its page never had the asked viewport.
pub fn fit(browser: &Browser, page: &super::chromium::Page, place: Option<&Window>, size: bool) -> crate::model::Result<Window> {
    use std::time::{Duration, Instant};
    // The page may be there before its window shows (an app's other window, such as macOS's menu bar
    // strip, would be taken for it): the visible main window, waited for up to 5 s.
    let deadline = Instant::now() + Duration::from_secs(5);
    let main = loop {
        match crate::host::main_window(&browser.pids()) {
            Some(w) if w.visible && !w.iconic => break w,
            found if Instant::now() > deadline => break found.ok_or_else(|| Error::msg(format!("{} shows no window", browser.exe.display())))?,
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let inner = || -> crate::model::Result<(f64, f64, f64)> {
        let v = page.eval("[innerWidth, innerHeight, devicePixelRatio]")?;
        Ok((v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0), v[2].as_f64().unwrap_or(1.0)))
    };
    let Some(at) = place else {
        let (w, h, scale) = inner()?;
        return Ok(Window { x: main.rect.0, y: main.rect.1, width: w as u32, height: h as u32, scale: Some(scale) });
    };
    // macOS moves and sizes another app's window only through the accessibility API: without the
    // permission nothing would move, and the app would read as one that keeps its size.
    #[cfg(target_os = "macos")]
    if size && !crate::host::macos::accessibility_trusted() {
        let (app, pane) = (crate::host::macos::responsible_app(), crate::host::macos::accessibility_pane());
        return Err(Error::usage(format!(
            "sizing {}'s window to the target's viewport needs the accessibility permission, which {app} (the app strobe runs under) does not have",
            browser.exe.display()
        ))
        .hint(format!("{pane}: turn on {app}, or drop viewport from the target")));
    }
    let want = (at.width as f64, at.height as f64);
    let settled = |client: Option<(u32, u32)>| -> crate::model::Result<(f64, f64, f64)> {
        // As it is now: the first pass moved it.
        crate::host::place_window(&crate::host::window_state(main.hwnd).unwrap_or_else(|| main.clone()), at.x, at.y, client);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let now = inner()?;
            if !size || ((now.0 - want.0).abs() < 1.0 && (now.1 - want.1).abs() < 1.0) || Instant::now() > deadline {
                return Ok(now);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut got = settled(size.then_some((at.width, at.height)))?;
    if size && ((got.0 - want.0).abs() >= 1.0 || (got.1 - want.1).abs() >= 1.0) && got.0 > 0.0 {
        let client = ((2.0 * want.0 - got.0).max(1.0).round() as u32, (2.0 * want.1 - got.1).max(1.0).round() as u32);
        got = settled(Some(client))?;
    }
    if size && ((got.0 - want.0).abs() >= 1.0 || (got.1 - want.1).abs() >= 1.0) {
        return Err(Error::usage(format!(
            "{} keeps its page at {}×{} CSS pixels, not the target's viewport {}×{}: the app fixes its window's size",
            browser.exe.display(),
            got.0,
            got.1,
            at.width,
            at.height
        ))
        .hint("drop viewport from the target: the app's own size is measured then"));
    }
    let placed = crate::host::window_state(main.hwnd).unwrap_or(main);
    Ok(Window { x: placed.rect.0, y: placed.rect.1, width: got.0 as u32, height: got.1 as u32, scale: Some(got.2) })
}

/// `path` with `\` as separators, lowercase and without a trailing separator: how Windows compares
/// two folder names.
#[cfg(windows)]
fn folded(path: &Path) -> String {
    path.display().to_string().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// WebView2's environment for this launch: its user data folder in the home (the profile may name
/// another folder inside it), and strobe's arguments after any the profile gives. Only the profile's
/// own values count: the user's own WebView2 variables are removed from the launch (`webview2`).
/// Returns the variables and the folder.
#[cfg(windows)]
fn webview2_env(app: &App, home: &Home) -> Result<(Vec<(String, String)>, PathBuf)> {
    let profile = |name: &str| home.env.iter().rev().find(|(k, _)| isolate::env_name_is(k, name)).map(|(_, v)| v.clone());
    let data = match profile(DATA) {
        Some(d) => {
            let p = PathBuf::from(&d);
            let inside = folded(&p).strip_prefix(&folded(&home.dir)).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'));
            if p.components().any(|c| c == std::path::Component::ParentDir) || !inside {
                return Err(Error::usage(format!("app profile \"{}\": env {DATA} is {d}, not a folder inside the launch's private home: the copy would share that webview data", app.id))
                    .hint(format!("name a folder in the home ({{home}}/…, no ..), or leave {DATA} out: strobe uses {{home}}/WebView2")));
            }
            p
        }
        None => home.dir.join("WebView2"),
    };
    let own = profile(ARGS).unwrap_or_default();
    if own.contains("--remote-debugging-") {
        return Err(Error::usage(format!("app profile \"{}\": {ARGS} already names a debugging port ({own}): strobe opens its own", app.id)));
    }
    let args = std::iter::once(own.as_str()).chain(STROBE_ARGS.iter().copied()).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(" ");
    Ok((vec![(DATA.into(), data.display().to_string()), (ARGS.into(), args)], data))
}

/// Ends a launch that failed before it started anything: its home goes when the launch made it.
#[cfg(windows)]
fn abandon(home: &Home) {
    isolate::release_home(&home.dir);
    if home.fresh {
        isolate::remove_home(&home.dir);
    }
}

/// WebView2's own processes (its browser and every child of it) are `msedgewebview2.exe`.
#[cfg(windows)]
fn runtime(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("msedgewebview2")
}

#[cfg(windows)]
fn webview2(app: &App, seed: Option<&Path>, window: &Window, cpus: Option<u64>, extra: &Extra) -> Result<(Browser, Guard, Started)> {
    use super::cdp::Cdp;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};
    // The user's own WebView2 variables (a browser folder, a release channel, a script debugger) stay
    // out of the copy; the profile's own are set again from the home's environment.
    let app = &app.clone().with_unset(&["WEBVIEW2_*".to_string()]);
    let mut guard = Guard::take(app);
    isolate::live(app, &guard)?;
    let prepared = match &extra.prepared {
        Some(h) => h.clone(),
        None => isolate::prepare_home(app, seed, extra.home.clone())?,
    };
    guard.shared = prepared.shared.clone();
    let (env, data) = match webview2_env(app, &prepared) {
        Ok(x) => x,
        Err(e) => {
            abandon(&prepared);
            return Err(e);
        }
    };
    for stale in isolate::find(&data, "DevToolsActivePort", 3) {
        let _ = std::fs::remove_file(stale);
    }
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
    let (started, deadline) = (Instant::now(), Instant::now() + Duration::from_secs(60));
    let (mut runtime_since, mut looked) = (None::<Instant>, Instant::now());
    let text = loop {
        guard.check("while the app started")?;
        if let Some(t) = isolate::find(&data, "DevToolsActivePort", 3).into_iter().find_map(|p| std::fs::read_to_string(p).ok().filter(|t| t.lines().count() >= 2)) {
            break t;
        }
        // A launcher stub that starts the real app and exits succeeds: a clean exit keeps waiting while
        // something it launched runs.
        if let Some(code) = launching.child.as_mut().and_then(|c| c.exited())
            && (code != 0 || crate::host::process_set(pid, Some(&prepared.dir)).is_empty())
        {
            return Err(Error::unreachable(format!("{} exited (code {code}) before its webview opened a debugging port", app.exe.display()))
                .hint(format!("a webview target is an app that embeds WebView2 and keeps running; for any other app, {NATIVE}")));
        }
        // WebView2 runs, but not with strobe's folder or arguments: the app passes its own in-process
        // (it sets the variables itself) or a policy overrides them. Its webview may be using data
        // outside the home: stop now, not after a minute against it.
        if looked.elapsed() >= Duration::from_millis(500) {
            looked = Instant::now();
            if crate::host::process_set(pid, Some(&prepared.dir)).iter().any(|p| runtime(&p.name)) {
                runtime_since.get_or_insert_with(Instant::now);
            }
        }
        if runtime_since.is_some_and(|t| t.elapsed() >= OVERRIDE_AFTER) {
            guard.check("after its webview started")?;
            return Err(Error::refused(
                "LiveChannel",
                format!(
                    "{}'s WebView2 runs ({} s after the launch) but opened no debugging port in {}: the app sets its webview's user data folder or arguments itself, or a WebView2 policy (LoaderOverride, a registry override) replaces them, so the webview may use data outside the isolated home",
                    app.exe.display(),
                    started.elapsed().as_secs(),
                    data.display()
                ),
                NATIVE,
            ));
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
    // The port strobe found must be this launch's own webview, not a browser that wrote into the folder
    // from outside it.
    let info = cdp.send("SystemInfo.getProcessInfo", json!({}), None)?;
    let browser_pid = info["processInfo"].as_array().into_iter().flatten().find(|p| p["type"] == "browser").and_then(|p| p["id"].as_u64()).map(|p| p as u32);
    let set: Vec<u32> = crate::host::process_set(pid, Some(&prepared.dir)).iter().map(|p| p.pid).collect();
    if !browser_pid.is_some_and(|b| set.contains(&b)) {
        return Err(Error::refused(
            "LiveChannel",
            format!("the debugging port in {} belongs to a browser (pid {}) outside this launch of {}", data.display(), browser_pid.map_or("?".into(), |p| p.to_string()), app.exe.display()),
            "end what else uses that folder, and launch again",
        ));
    }
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    let fresh = launching.fresh;
    let (child, profile) = launching.done();
    Ok((Browser { cdp, ws, child, pid, profile, fresh, exe: app.exe.clone(), version, window: window.clone(), embedded: true }, guard, Started { epoch_ms: epoch, output, main: None }))
}
