//! Finding and launching Chromium with an isolated profile, real GPU and a fixed window (SPEC §13.1).

use super::cdp::{Cdp, wait_event};
use crate::host;
use crate::host::spawn::Owned;
use crate::model::{Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// Where and how big the window is.
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: Option<f64>,
}

impl Default for Window {
    fn default() -> Self {
        Window { x: 40, y: 40, width: 1400, height: 900, scale: None }
    }
}

/// A browser or Electron app strobe launched (or attached to: no child or profile). Dropped
/// without `close`, it still ends everything it launched and releases its profile.
pub struct Browser {
    pub cdp: Arc<Cdp>,
    /// The browser-level DevTools WebSocket URL (what Playwright's connectOverCDP takes).
    pub ws: String,
    /// The launched child with its group (`host::spawn_owned`).
    pub child: Option<Owned>,
    pub pid: u32,
    /// The launch's home or profile, released (and removed) with the browser; a caller that keeps
    /// it takes it first.
    pub profile: Option<PathBuf>,
    /// The launch made `profile`: dropped without `close`, the browser removes it only then (a warm
    /// home stays for the caller).
    pub fresh: bool,
    pub exe: PathBuf,
    pub version: String,
    pub window: Window,
    /// The browser is a webview inside the launched app (`webview.rs`): closing it leaves the app
    /// running, so `close` closes the app instead.
    pub embedded: bool,
    /// A browser `launch` started for pages: each page it opens gets `NOT_AUTOMATED` first.
    pub as_person: bool,
}

/// `navigator.webdriver` false, as for a person, where the browser says otherwise. A preload reaches the
/// page's documents in its own process; a frame from another site (its own process) and workers keep
/// the browser's answer.
pub const NOT_AUTOMATED: &str = "if (navigator.webdriver) Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => false, configurable: true })";

/// An attached page: the session id commands go to.
#[derive(Clone)]
pub struct Page {
    pub cdp: Arc<Cdp>,
    pub session: String,
    pub target: String,
}

pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("STROBE_BROWSER").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut c: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(b) = std::env::var_os(base).map(PathBuf::from) {
                c.push(b.join(r"Google\Chrome\Application\chrome.exe"));
                c.push(b.join(r"Chromium\Application\chrome.exe"));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            c.extend(playwright_chromium(&local.join("ms-playwright"), r"chrome-win64\chrome.exe"));
            c.extend(playwright_chromium(&local.join("ms-playwright"), r"chrome-win\chrome.exe"));
        }
        for base in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(b) = std::env::var_os(base).map(PathBuf::from) {
                c.push(b.join(r"Microsoft\Edge\Application\msedge.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        c.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        c.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
        c.extend(playwright_chromium(&host::home().join("Library/Caches/ms-playwright"), "chrome-mac/Chromium.app/Contents/MacOS/Chromium"));
        c.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    } else {
        for p in ["/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/snap/bin/chromium"] {
            c.push(p.into());
        }
        c.extend(playwright_chromium(&host::home().join(".cache/ms-playwright"), "chrome-linux/chrome"));
    }
    c.into_iter().find(|p| p.is_file())
}

/// Playwright's full Chromium builds, newest first (never the headless shell).
fn playwright_chromium(root: &Path, rel: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut dirs: Vec<(u32, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let rev = name.strip_prefix("chromium-")?.parse().ok()?;
            Some((rev, e.path().join(rel)))
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    dirs.into_iter().map(|d| d.1).collect()
}

/// A desktop's input devices whatever the machine has (`validity::desktop_input`): a fine pointer that
/// hovers, in Blink's `ui::PointerType` and `ui::HoverType` bits (ui/base/pointer/pointer_device.h:
/// `POINTER_TYPE_FINE` 1 << 2, `HOVER_TYPE_HOVER` 1 << 1). Without it a machine with no mouse (a server, a
/// VM) has its pages match (hover: none) and no pointer. DevTools' media emulation has no hover or pointer.
/// Chromium keeps only the last `--blink-settings` before the page's URL (one after it is ignored): any
/// other Blink setting strobe passes joins this list, and it replaces one a `STROBE_BROWSER` wrapper puts first.
const DESKTOP_INPUT: &str = "--blink-settings=primaryPointerType=4,availablePointerTypes=4,primaryHoverType=2,availableHoverTypes=2";

/// Launches a fresh Chromium. `cpus`, when set, is the affinity mask the browser and its children get.
pub fn launch(window: &Window, headless: bool, cpus: Option<u64>, extra: &[String]) -> Result<Browser> {
    let exe = find().ok_or_else(|| Error::msg("no Chromium found").hint("install Chrome or Edge, run `bunx playwright install chromium`, or set STROBE_BROWSER"))?;
    let profile = host::paths::run().join(format!("profile-{}-{}", std::process::id(), host::now_ns()));
    std::fs::create_dir_all(&profile)?;
    let mut args: Vec<String> = vec![
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--disable-component-extensions-with-background-pages".into(),
        "--disable-background-networking".into(),
        "--disable-sync".into(),
        "--disable-default-apps".into(),
        "--disable-client-side-phishing-detection".into(),
        // LaunchOnStartup*: Chrome's "launches when Windows starts" offer, a banner over the page.
        "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,AutofillServerCommunication,EdgeCollections,msEdgeShopping,EdgeWallet,msImplicitSignin,LaunchOnStartup,LaunchOnStartupInfoBar".into(),
        "--metrics-recording-only".into(),
        // A measurement never plays sound on the user's speakers; Web Audio still renders.
        "--mute-audio".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        // No banners over the page (Chrome for Testing's notice, launch-at-login offers) and nothing that
        // registers to run at login: a banner shrinks the viewport, and one dismissed mid-run resizes it.
        "--disable-infobars".into(),
        "--no-service-autorun".into(),
        "--disable-component-update".into(),
        "--disable-search-engine-choice-screen".into(),
        format!("--window-position={},{}", window.x, window.y),
        format!("--window-size={},{}", window.width, window.height),
        DESKTOP_INPUT.into(),
    ];
    if let Some(s) = window.scale {
        args.push(format!("--force-device-scale-factor={s}"));
    }
    if headless {
        args.push("--headless=new".into());
    }
    // Chromium refuses to start as root on Linux unless its sandbox is off (containers, CI machines).
    #[cfg(target_os = "linux")]
    if unsafe { libc::geteuid() } == 0 {
        args.push("--no-sandbox".into());
    }
    args.extend(extra.iter().cloned());
    args.push("about:blank".into());
    let mut cmd = Command::new(&exe);
    cmd.args(&args).envs(super::isolate::launch_env(&profile)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = match host::spawn_owned(&mut cmd, cpus, 0, Some(&profile)) {
        Ok(c) => c,
        Err(e) => {
            super::isolate::release_home(&profile);
            super::isolate::remove_home(&profile);
            return Err(Error::msg(format!("launch {}: {e}", exe.display())));
        }
    };
    let pid = child.pid();
    // A launch that fails from here ends the browser and its profile.
    let launching = super::isolate::Launching::new(child, Some(profile.clone()), true);
    let port_file = profile.join("DevToolsActivePort");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Ok(t) = std::fs::read_to_string(&port_file)
            && t.lines().count() >= 2
        {
            break t;
        }
        if std::time::Instant::now() > deadline {
            return Err(Error::unreachable("Chromium did not open its debugging port in 30 s"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut lines = text.lines();
    let (port, path) = (lines.next().unwrap_or("").trim(), lines.next().unwrap_or("").trim());
    let ws = format!("ws://127.0.0.1:{port}{path}");
    let cdp = Cdp::connect(&ws)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    let (child, profile) = launching.done();
    Ok(Browser { cdp, ws, child, pid, profile, fresh: true, exe, version, window: window.clone(), embedded: false, as_person: true })
}

/// Attaches to a running endpoint: `cdp:<port>`, `cdp:ws://…` or a bare `ws://…`.
pub fn attach(endpoint: &str) -> Result<Browser> {
    let endpoint = endpoint.strip_prefix("cdp:").unwrap_or(endpoint);
    let url = if endpoint.starts_with("ws://") {
        endpoint.to_string()
    } else {
        let port = endpoint;
        let body = http_get(&format!("127.0.0.1:{port}"), "/json/version")?;
        let v: Value = serde_json::from_str(&body)?;
        if v["Browser"].as_str().is_some_and(|b| b.starts_with("node.js")) {
            return Err(Error::usage(format!(
                "127.0.0.1:{port} is a Node inspector, not a browser: cdp:<port> records pages over Chromium's DevTools protocol. \
                 Measure a Node workload with a bench script() target, or its process from outside with pid:<pid>"
            )));
        }
        v.get("webSocketDebuggerUrl").and_then(Value::as_str).ok_or_else(|| Error::unreachable("no webSocketDebuggerUrl"))?.to_string()
    };
    let cdp = Cdp::connect(&url)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Browser { cdp, ws: url, child: None, pid: 0, profile: None, fresh: false, exe: PathBuf::new(), version, window: Window::default(), embedded: false, as_person: false })
}

/// One GET on a debugging port. Chrome and Node keep the connection open after the reply, even with
/// `Connection: close`, so the body is read by its `Content-Length` (or chunks), not to the end of the stream.
pub fn http_get(host: &str, path: &str) -> Result<String> {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(host).map_err(|e| Error::unreachable(format!("{host}: {e}")))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    read_reply(&mut s).map_err(|e| Error::unreachable(format!("{host}{path}: {e}")))
}

fn read_reply(s: &mut impl std::io::Read) -> std::io::Result<String> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    fn fill(s: &mut impl std::io::Read, buf: &mut Vec<u8>) -> std::io::Result<bool> {
        let mut chunk = [0u8; 8192];
        let n = s.read(&mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        Ok(n > 0)
    }
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if !fill(s, &mut buf)? {
            return Err(bad("connection closed before the reply's headers"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let header = |name: &str| head.lines().find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_string()));
    let mut body = buf.split_off(head_end);
    if let Some(len) = header("content-length:").and_then(|v| v.parse::<usize>().ok()) {
        while body.len() < len {
            if !fill(s, &mut body)? {
                return Err(bad("connection closed before the reply's body"));
            }
        }
        body.truncate(len);
    } else if header("transfer-encoding:").is_some_and(|v| v.contains("chunked")) {
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let Some(eol) = body[at..].windows(2).position(|w| w == b"\r\n") else {
                if !fill(s, &mut body)? {
                    return Err(bad("connection closed inside a chunk"));
                }
                continue;
            };
            let size_text = String::from_utf8_lossy(&body[at..at + eol]).to_string();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16).map_err(|_| bad("bad chunk size"))?;
            if size == 0 {
                break;
            }
            let start = at + eol + 2;
            while body.len() < start + size + 2 {
                if !fill(s, &mut body)? {
                    return Err(bad("connection closed inside a chunk"));
                }
            }
            out.extend_from_slice(&body[start..start + size]);
            at = start + size + 2;
        }
        body = out;
    } else {
        while fill(s, &mut body)? {}
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

impl Browser {
    /// The first page target, attached with a flat session.
    pub fn page(&self) -> Result<Page> {
        // A browser strobe launched opens its about:blank window a moment after its debugging port: wait for
        // that page rather than create a second window beside it, which stayed open and blank all run.
        let deadline = std::time::Instant::now() + Duration::from_secs(if self.child.is_some() { 5 } else { 0 });
        let target = loop {
            let targets = self.cdp.send("Target.getTargets", json!({}), None)?;
            let found = targets["targetInfos"].as_array().and_then(|a| a.iter().find(|t| t["type"] == "page")).and_then(|t| t["targetId"].as_str()).map(str::to_string);
            if found.is_some() || std::time::Instant::now() >= deadline {
                break found;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let target = match target {
            Some(t) => t,
            None => self.cdp.send("Target.createTarget", json!({ "url": "about:blank" }), None)?["targetId"].as_str().unwrap_or("").to_string(),
        };
        let attached = self.cdp.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
        let session = attached["sessionId"].as_str().ok_or_else(|| Error::msg("attach: no session"))?.to_string();
        let page = Page { cdp: self.cdp.clone(), session, target };
        page.send("Page.enable", json!({}))?;
        if self.as_person {
            // The page runs as it does for a person: `navigator.webdriver` false, so an app that hides its
            // menus or skips work under automation is measured the way people use it. A preload, as in
            // Firefox and WebKit: `--disable-blink-features=AutomationControlled` did it too, and put a
            // dismissable "unsupported command-line flag" bar over the page (47 points in Edge on macOS).
            page.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": NOT_AUTOMATED }))?;
            page.eval(&format!("{NOT_AUTOMATED}; true"))?;
        }
        if self.pid != 0 && cfg!(windows) {
            // An occluded Chromium window stops rendering: keep ours on top, without taking focus.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while host::raise_windows(&self.pids()) == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(page)
    }

    /// Resizes the window so the page's viewport, not the whole window, is `width`×`height` CSS pixels,
    /// and moves it to `at` when given.
    pub fn fit_viewport(&self, page: &Page, width: u32, height: u32, at: Option<(i32, i32)>) -> Result<()> {
        let win = self.cdp.send("Browser.getWindowForTarget", json!({ "targetId": page.target }), None)?;
        let inner = page.eval("[innerWidth, innerHeight]")?;
        let num = |v: &Value| v.as_f64().unwrap_or(0.0);
        let (dw, dh) = (num(&win["bounds"]["width"]) - num(&inner[0]), num(&win["bounds"]["height"]) - num(&inner[1]));
        let placed = at.is_none_or(|(x, y)| num(&win["bounds"]["left"]) as i32 == x && num(&win["bounds"]["top"]) as i32 == y);
        if placed && num(&inner[0]) as u32 == width && num(&inner[1]) as u32 == height {
            return Ok(());
        }
        let mut bounds = json!({ "width": (width as f64 + dw).round() as u32, "height": (height as f64 + dh).round() as u32 });
        if let Some((x, y)) = at {
            bounds["left"] = json!(x);
            bounds["top"] = json!(y);
        }
        self.cdp.send("Browser.setWindowBounds", json!({ "windowId": win["windowId"], "bounds": bounds }), None)?;
        Ok(())
    }

    /// Every process that belongs to this target, with CPU times and private bytes.
    pub fn processes(&self) -> Vec<host::ProcTimes> {
        host::process_set(self.pid, self.profile.as_deref())
    }

    /// GPU devices and whether compositing runs on a hardware adapter.
    pub fn gpu(&self) -> Result<Gpu> {
        let info = self.cdp.send("SystemInfo.getInfo", json!({}), None)?;
        let devices: Vec<String> =
            info["gpu"]["devices"].as_array().map(|a| a.iter().map(|d| d["deviceString"].as_str().unwrap_or("").to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let compositing = info["gpu"]["featureStatus"]["gpu_compositing"].as_str().unwrap_or("").to_string();
        // Element 0 is the adapter Chromium uses; the rest are every other adapter on the system.
        let software = devices.first().is_none_or(|d| crate::validity::is_software_gpu(d)) || !compositing.starts_with("enabled");
        Ok(Gpu { devices, compositing, software })
    }

    /// Closes the browser as a person would (`Browser.close`, up to 5 s for it to exit), then ends
    /// what is left and removes the profile (an attached browser is only asked to close).
    pub fn close(mut self) {
        // A webview's app closes as a person closes it (its windows), with a short grace; `Browser.close`
        // would end only its browser and leave the app waited on.
        let grace = if self.embedded {
            if super::webview::ask_close(&self.pids()) > 0 { 2 } else { 0 }
        } else {
            let _ = self.cdp.send_timeout("Browser.close", json!({}), None, Duration::from_secs(5));
            5
        };
        if let Some(c) = self.child.as_mut() {
            let deadline = std::time::Instant::now() + Duration::from_secs(grace);
            while std::time::Instant::now() < deadline && c.exited().is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.end(true);
    }

    /// Ends the launch: its tree, its group and everything stamped with its profile (an Electron
    /// app's own detached service outlives the tree), then releases the profile and with `remove`
    /// deletes it (Chromium may hold files for a moment after exit: `remove_home` retries).
    fn end(&mut self, remove: bool) {
        super::isolate::end_launch(self.child.take(), self.profile.take().as_deref(), remove);
    }

    /// This target's process set as pids (`processes`).
    pub fn pids(&self) -> Vec<u32> {
        self.processes().iter().map(|p| p.pid).collect()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let remove = self.fresh;
        self.end(remove);
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Gpu {
    pub devices: Vec<String>,
    pub compositing: String,
    pub software: bool,
}

impl Page {
    pub fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.send(method, params, Some(&self.session))
    }

    pub fn send_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.cdp.send_timeout(method, params, Some(&self.session), timeout)
    }

    /// Evaluates an expression and returns its value (awaiting promises).
    pub fn eval(&self, expression: &str) -> Result<Value> {
        self.eval_timeout(expression, Duration::from_secs(60))
    }

    /// Like `eval`, for expressions that wait in the page for up to `timeout`.
    pub fn eval_timeout(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let r = self.cdp.send_timeout("Runtime.evaluate", json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }), Some(&self.session), timeout)?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex["exception"]["description"].as_str().or_else(|| ex["text"].as_str()).unwrap_or("exception");
            return Err(Error::msg(format!("page: {}", text.lines().next().unwrap_or(text))));
        }
        Ok(r["result"]["value"].clone())
    }

    pub fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        let rx = self.cdp.events();
        let r = self.send("Page.navigate", json!({ "url": url }))?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            return Err(Error::unreachable(format!("navigate {url}: {err}")));
        }
        wait_event(&rx, "Page.loadEventFired", Some(&self.session), timeout)?;
        Ok(())
    }
}
