//! Finding and launching Chromium with an isolated profile, real GPU and a fixed window (SPEC §13.1).

use super::cdp::{Cdp, wait_event};
use crate::host;
use crate::model::{Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

pub struct Browser {
    pub cdp: Arc<Cdp>,
    /// The browser-level DevTools WebSocket URL (what Playwright's connectOverCDP takes).
    pub ws: String,
    pub child: Option<Child>,
    pub pid: u32,
    pub profile: Option<PathBuf>,
    pub exe: PathBuf,
    pub version: String,
    pub window: Window,
}

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
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    dirs.into_iter().map(|d| d.1).collect()
}

/// Launches a fresh Chromium. `cpus`, when set, is the affinity mask the browser and its children get.
pub fn launch(window: &Window, headless: bool, cpus: Option<u64>, extra: &[String]) -> Result<Browser> {
    let exe = find().ok_or_else(|| {
        Error::msg("no Chromium found").hint("install Chrome or Edge, run `bunx playwright install chromium`, or set STROBE_BROWSER")
    })?;
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
        "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,AutofillServerCommunication,EdgeCollections,msEdgeShopping,EdgeWallet,msImplicitSignin".into(),
        "--metrics-recording-only".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        format!("--window-position={},{}", window.x, window.y),
        format!("--window-size={},{}", window.width, window.height),
    ];
    if let Some(s) = window.scale {
        args.push(format!("--force-device-scale-factor={s}"));
    }
    if headless {
        args.push("--headless=new".into());
    }
    args.extend(extra.iter().cloned());
    args.push("about:blank".into());
    let previous = cpus.and_then(host::set_affinity);
    let spawned = Command::new(&exe).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if previous.is_some() {
        host::set_affinity(previous.unwrap_or(0));
    }
    let child = spawned.map_err(|e| Error::msg(format!("launch {}: {e}", exe.display())))?;
    let pid = child.id();
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
    Ok(Browser { cdp, ws, child: Some(child), pid, profile: Some(profile), exe, version, window: window.clone() })
}

/// Attaches to a running endpoint (`cdp:<port>` or a ws:// url).
pub fn attach(endpoint: &str) -> Result<Browser> {
    let url = if endpoint.starts_with("ws://") {
        endpoint.to_string()
    } else {
        let port = endpoint.trim_start_matches("cdp:");
        let body = http_get(&format!("127.0.0.1:{port}"), "/json/version")?;
        let v: Value = serde_json::from_str(&body)?;
        v.get("webSocketDebuggerUrl").and_then(Value::as_str).ok_or_else(|| Error::unreachable("no webSocketDebuggerUrl"))?.to_string()
    };
    let cdp = Cdp::connect(&url)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Browser { cdp, ws: url, child: None, pid: 0, profile: None, exe: PathBuf::new(), version, window: Window::default() })
}

fn http_get(host: &str, path: &str) -> Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(host).map_err(|e| Error::unreachable(format!("{host}: {e}")))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    Ok(buf.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

impl Browser {
    /// The first page target, attached with a flat session.
    pub fn page(&self) -> Result<Page> {
        let targets = self.cdp.send("Target.getTargets", json!({}), None)?;
        let target = targets["targetInfos"]
            .as_array()
            .and_then(|a| a.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str())
            .map(str::to_string);
        let target = match target {
            Some(t) => t,
            None => self.cdp.send("Target.createTarget", json!({ "url": "about:blank" }), None)?["targetId"].as_str().unwrap_or("").to_string(),
        };
        let attached = self.cdp.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
        let session = attached["sessionId"].as_str().ok_or_else(|| Error::msg("attach: no session"))?.to_string();
        let page = Page { cdp: self.cdp.clone(), session, target };
        page.send("Page.enable", json!({}))?;
        if self.pid != 0 {
            // An occluded Chromium window stops rendering: keep ours on top, without taking focus.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while host::raise_windows(self.pid) == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(page)
    }

    /// Resizes the window so the page's viewport, not the whole window, is `width`×`height` CSS pixels.
    pub fn fit_viewport(&self, page: &Page, width: u32, height: u32) -> Result<()> {
        let win = self.cdp.send("Browser.getWindowForTarget", json!({ "targetId": page.target }), None)?;
        let inner = page.eval("[innerWidth, innerHeight]")?;
        let num = |v: &Value| v.as_f64().unwrap_or(0.0);
        let (dw, dh) = (num(&win["bounds"]["width"]) - num(&inner[0]), num(&win["bounds"]["height"]) - num(&inner[1]));
        if dw.abs() < 1.0 && dh.abs() < 1.0 && num(&inner[0]) as u32 == width && num(&inner[1]) as u32 == height {
            return Ok(());
        }
        self.cdp.send(
            "Browser.setWindowBounds",
            json!({ "windowId": win["windowId"], "bounds": { "width": (width as f64 + dw).round() as u32, "height": (height as f64 + dh).round() as u32 } }),
            None,
        )?;
        Ok(())
    }

    /// Every process that belongs to this target, with CPU times and private bytes.
    pub fn processes(&self) -> Vec<host::ProcTimes> {
        host::process_set(self.pid, self.profile.as_deref())
    }

    /// GPU devices and whether compositing runs on a hardware adapter.
    pub fn gpu(&self) -> Result<Gpu> {
        let info = self.cdp.send("SystemInfo.getInfo", json!({}), None)?;
        let devices: Vec<String> = info["gpu"]["devices"]
            .as_array()
            .map(|a| a.iter().map(|d| d["deviceString"].as_str().unwrap_or("").to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let compositing = info["gpu"]["featureStatus"]["gpu_compositing"].as_str().unwrap_or("").to_string();
        // Element 0 is the adapter Chromium uses; the rest are every other adapter on the system.
        let active = devices.first().map(|d| d.to_lowercase()).unwrap_or_default();
        let software = active.is_empty() || active.contains("swiftshader") || active.contains("basic render") || active.contains("llvmpipe") || !compositing.starts_with("enabled");
        Ok(Gpu { devices, compositing, software })
    }

    pub fn close(mut self) {
        let _ = self.cdp.send_timeout("Browser.close", json!({}), None, Duration::from_secs(5));
        if let Some(mut c) = self.child.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if let Ok(Some(_)) = c.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // The whole tree: an Electron app's own service and helpers go with it.
            super::electron::kill_tree(self.pid);
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(p) = self.profile.take() {
            // Detached children (an Electron app's own background service) outlive the tree: end
            // everything that runs from the private home.
            #[cfg(windows)]
            for pid in crate::host::win::running_under(&p) {
                super::electron::kill_tree(pid);
            }
            // Chromium may hold files for a moment after exit.
            for _ in 0..20 {
                if std::fs::remove_dir_all(&p).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
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
