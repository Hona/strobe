//! Finding and launching Firefox with an isolated profile and driving it over WebDriver BiDi (SPEC
//! §13.1): the browser's own remote agent, no geckodriver.

use super::bidi::{self, Bidi};
use super::chromium::{Gpu, Window};
use crate::host;
use crate::host::spawn::Owned;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// What every strobe profile sets before Firefox starts: nothing that phones home, updates, studies,
/// first-run pages, session restore, the default-browser offer or sound; everything that shapes how a
/// page runs stays Firefox's own default. `remote.prefs.recommended` off: the remote agent would set
/// its test preferences (no hang monitor, no process priority manager, focus test mode, no window
/// occlusion tracking), which a person's Firefox does not run with.
const USER_JS: &[(&str, &str)] = &[
    ("remote.prefs.recommended", "false"),
    ("app.update.disabledForTesting", "true"),
    ("app.update.auto", "false"),
    ("app.normandy.enabled", "false"),
    ("app.normandy.api_url", "\"\""),
    ("app.shield.optoutstudies.enabled", "false"),
    ("browser.shell.checkDefaultBrowser", "false"),
    ("browser.startup.homepage_override.mstone", "\"ignore\""),
    ("startup.homepage_welcome_url", "\"about:blank\""),
    ("startup.homepage_welcome_url.additional", "\"\""),
    ("browser.aboutwelcome.enabled", "false"),
    ("browser.preonboarding.enabled", "false"),
    ("termsofuse.bypassNotification", "true"),
    ("browser.sessionstore.resume_from_crash", "false"),
    ("browser.startup.couldRestoreSession.count", "-1"),
    ("toolkit.startup.max_resumed_crashes", "-1"),
    ("datareporting.policy.dataSubmissionEnabled", "false"),
    ("datareporting.policy.dataSubmissionPolicyBypassNotification", "true"),
    ("datareporting.healthreport.uploadEnabled", "false"),
    ("datareporting.usage.uploadEnabled", "false"),
    ("toolkit.telemetry.enabled", "false"),
    ("toolkit.telemetry.unified", "false"),
    ("toolkit.telemetry.server", "\"\""),
    ("browser.newtabpage.enabled", "false"),
    ("browser.newtabpage.activity-stream.feeds.telemetry", "false"),
    ("browser.newtabpage.activity-stream.telemetry", "false"),
    ("browser.newtabpage.activity-stream.showSponsoredTopSites", "false"),
    ("browser.newtabpage.activity-stream.feeds.section.topstories", "false"),
    ("extensions.pocket.enabled", "false"),
    ("extensions.update.enabled", "false"),
    ("extensions.systemAddon.update.enabled", "false"),
    ("extensions.getAddons.cache.enabled", "false"),
    ("browser.discovery.enabled", "false"),
    ("browser.search.update", "false"),
    ("browser.safebrowsing.malware.enabled", "false"),
    ("browser.safebrowsing.phishing.enabled", "false"),
    ("browser.safebrowsing.downloads.enabled", "false"),
    ("browser.safebrowsing.blockedURIs.enabled", "false"),
    ("browser.safebrowsing.provider.mozilla.updateURL", "\"\""),
    ("network.captive-portal-service.enabled", "false"),
    ("network.connectivity-service.enabled", "false"),
    ("dom.push.connection.enabled", "false"),
    ("services.settings.server", "\"data:,#remote-settings-dummy/v1\""),
    ("browser.ml.enable", "false"),
    ("browser.translations.enable", "false"),
    ("browser.urlbar.suggest.searches", "false"),
    ("browser.backup.enabled", "false"),
    ("browser.tabs.warnOnClose", "false"),
    ("browser.warnOnQuit", "false"),
    ("signon.rememberSignons", "false"),
    // No OpenH264 or Widevine download into a fresh profile in the middle of a block.
    ("media.gmp-manager.url", "\"\""),
    // A measurement never plays sound on the user's speakers; the page's audio graph still runs.
    ("media.volume_scale", "\"0.0\""),
];

/// A Firefox strobe launched, with its one BiDi session. Dropped without `close`, it still ends
/// everything it launched and releases its profile.
pub struct Browser {
    pub bidi: Arc<Bidi>,
    pub child: Option<Owned>,
    pub pid: u32,
    pub profile: Option<PathBuf>,
    pub exe: PathBuf,
    pub version: String,
    pub window: Window,
    pub headless: bool,
    /// The top-level browsing context strobe drives, and its window.
    context: String,
    client_window: String,
}

/// The page strobe drives: the top-level browsing context.
#[derive(Clone)]
pub struct Page {
    pub bidi: Arc<Bidi>,
    pub context: String,
}

/// Stock Firefox only (release, then Developer Edition and Nightly, all Mozilla's): `STROBE_FIREFOX`,
/// else the standard install locations. Playwright's patched Firefox is never picked up (it changes the
/// process model and preferences); `STROBE_FIREFOX` naming one is the user's choice, and `patched` says so.
pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("STROBE_FIREFOX").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut c: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in ["ProgramFiles", "LOCALAPPDATA", "ProgramFiles(x86)"] {
            if let Some(b) = std::env::var_os(base).map(PathBuf::from) {
                for dir in ["Mozilla Firefox", "Firefox Developer Edition", "Firefox Nightly"] {
                    c.push(b.join(dir).join("firefox.exe"));
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        for app in ["Firefox", "Firefox Developer Edition", "Firefox Nightly"] {
            c.push(PathBuf::from(format!("/Applications/{app}.app/Contents/MacOS/firefox")));
            c.push(host::home().join(format!("Applications/{app}.app/Contents/MacOS/firefox")));
        }
    } else {
        for p in ["/usr/lib/firefox/firefox", "/usr/lib64/firefox/firefox", "/opt/firefox/firefox", "/usr/lib/firefox-esr/firefox-esr", "/usr/bin/firefox", "/usr/bin/firefox-esr", "/snap/bin/firefox"] {
            c.push(p.into());
        }
    }
    c.into_iter().find(|p| p.is_file())
}

/// A test build (Playwright's patched Firefox), named by its path.
pub fn patched(exe: &Path) -> bool {
    exe.to_string_lossy().to_lowercase().contains("ms-playwright")
}

/// The Snap package's Firefox (Ubuntu's default): AppArmor lets it read only its own folders under the
/// home, so its profile lives in `~/snap/firefox/common`, not in strobe's state folder.
fn snap(exe: &Path) -> bool {
    let real = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let script = std::fs::read(exe).ok().filter(|b| b.starts_with(b"#!")).is_some_and(|b| String::from_utf8_lossy(&b).contains("/snap/"));
    real.starts_with("/snap/") || real.file_name().is_some_and(|n| n == "snap") || script
}

/// Where Snap Firefox profiles go (`~/snap/firefox/common/strobe`), which strobe reaps as it reaps
/// `run/`.
pub fn snap_profiles() -> PathBuf {
    host::home().join("snap/firefox/common/strobe")
}

/// The version an install says it is (`application.ini` beside the executable, or in `Resources` on
/// macOS), without starting it: `Firefox 159.0.1`.
pub fn version_of(exe: &Path) -> Option<String> {
    let real = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let dir = real.parent()?;
    let ini = [dir.join("application.ini"), dir.join("../Resources/application.ini")].into_iter().find_map(|p| std::fs::read_to_string(p).ok())?;
    let v = ini.lines().find_map(|l| l.strip_prefix("Version="))?.trim().to_string();
    Some(format!("Firefox {v}"))
}

/// Launches a fresh Firefox in an isolated profile and opens its BiDi session. `cpus`, when set, is the
/// affinity mask the browser and its children get.
pub fn launch(window: &Window, headless: bool, cpus: Option<u64>) -> Result<Browser> {
    let exe = find().ok_or_else(|| Error::msg("no Firefox found").hint("install Firefox from mozilla.org (winget install Mozilla.Firefox, brew install --cask firefox, or the distribution's package), or set STROBE_FIREFOX"))?;
    let name = format!("profile-{}-{}", std::process::id(), host::now_ns());
    let profile = if snap(&exe) { snap_profiles().join(name) } else { host::paths::run().join(name) };
    // The user's alone: the profile names the BiDi port and allows chrome-scope scripts on it, which any
    // local process that could read it could drive.
    if let Some(root) = profile.parent() {
        host::paths::owner_only_dir(root);
    }
    host::paths::owner_only_dir(&profile);
    let mut prefs: Vec<(String, String)> = USER_JS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    if let Some(s) = window.scale {
        prefs.push(("layout.css.devPixelsPerPx".into(), format!("\"{s}\"")));
    }
    let user_js: String = prefs.iter().map(|(k, v)| format!("user_pref(\"{k}\", {v});\n")).collect();
    std::fs::write(profile.join("user.js"), user_js)?;
    let mut args: Vec<String> = vec!["-no-remote".into(), "-profile".into(), profile.display().to_string(), "--remote-debugging-port".into(), "0".into(), "--new-instance".into()];
    if cfg!(windows) {
        // Windows starts Firefox through a launcher process: it waits for the browser, so the child
        // strobe holds is the browser's lifetime.
        args.push("-wait-for-browser".into());
    }
    if headless {
        args.push("-headless".into());
    }
    args.push("about:blank".into());
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .envs(super::isolate::launch_env(&profile))
        // Chrome-scope reads only (process roles, the compositor): nothing a page can reach.
        .env("MOZ_REMOTE_ALLOW_SYSTEM_ACCESS", "1")
        .env("MOZ_CRASHREPORTER_DISABLE", "1")
        .env("MOZ_HEADLESS_WIDTH", window.width.to_string())
        .env("MOZ_HEADLESS_HEIGHT", window.height.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = match host::spawn_owned(&mut cmd, cpus, 0, Some(&profile)) {
        Ok(c) => c,
        Err(e) => {
            super::isolate::release_home(&profile);
            super::isolate::remove_home(&profile);
            return Err(Error::msg(format!("launch {}: {e}", exe.display())));
        }
    };
    let pid = child.pid();
    let launching = super::isolate::Launching::new(child, Some(profile.clone()), true);
    // Firefox writes its BiDi server's address into the profile once it listens.
    let port_file = profile.join("WebDriverBiDiServer.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let port = loop {
        if let Some(p) = std::fs::read_to_string(&port_file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["ws_port"].as_u64()) {
            break p;
        }
        if std::time::Instant::now() > deadline {
            return Err(Error::unreachable("Firefox did not open its WebDriver BiDi port in 30 s"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let bidi = Bidi::connect(&format!("ws://127.0.0.1:{port}/session"))?;
    let session = bidi.send("session.new", json!({ "capabilities": {} }))?;
    let version = format!("Firefox {}", session["capabilities"]["browserVersion"].as_str().unwrap_or(""));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let (context, client_window) = loop {
        let tree = bidi.send("browsingContext.getTree", json!({ "maxDepth": 0 }))?;
        if let Some(c) = tree["contexts"].as_array().and_then(|a| a.first()) {
            break (c["context"].as_str().unwrap_or("").to_string(), c["clientWindow"].as_str().unwrap_or("").to_string());
        }
        if std::time::Instant::now() > deadline {
            return Err(Error::unreachable("Firefox opened no window in 10 s"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let (child, profile) = launching.done();
    Ok(Browser { bidi, child, pid, profile, exe, version, window: window.clone(), headless, context, client_window })
}

impl Browser {
    /// The page strobe drives, placed where the window says. Before any page loads it runs strobe's
    /// preload: `navigator.webdriver` false, as for a person (an app that branches on it is measured the
    /// way people use it), as Chromium's launch flag does.
    pub fn page(&self) -> Result<Page> {
        let page = Page { bidi: self.bidi.clone(), context: self.context.clone() };
        page.add_init_script("Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => false, configurable: true })")?;
        let w = &self.window;
        self.bidi.send("browser.setClientWindowState", json!({ "clientWindow": self.client_window, "state": "normal", "x": w.x, "y": w.y, "width": w.width, "height": w.height }))?;
        if !self.headless {
            // An occluded Firefox window stops painting and reports itself hidden (window occlusion tracking):
            // keep it on top (Windows: without focus; macOS: the active app, once nobody uses the Mac, as
            // for native apps).
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while host::raise_windows(&self.pids()) == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(page)
    }

    /// Sizes the window so the page's viewport, not the whole window, is `width`×`height` CSS pixels.
    pub fn fit_viewport(&self, page: &Page, width: u32, height: u32) -> Result<()> {
        let inner = page.eval("[innerWidth, innerHeight, outerWidth, outerHeight]")?;
        let num = |i: usize| inner[i].as_f64().unwrap_or(0.0);
        if num(0) as u32 == width && num(1) as u32 == height {
            return Ok(());
        }
        let (dw, dh) = (num(2) - num(0), num(3) - num(1));
        let w = &self.window;
        self.bidi.send(
            "browser.setClientWindowState",
            json!({ "clientWindow": self.client_window, "state": "normal", "x": w.x, "y": w.y, "width": (width as f64 + dw).round() as u32, "height": (height as f64 + dh).round() as u32 }),
        )?;
        Ok(())
    }

    /// Every process of this Firefox (its tree, what carries its profile's stamp, its job).
    pub fn processes(&self) -> Vec<host::ProcTimes> {
        host::process_set(self.pid, self.profile.as_deref())
    }

    pub fn pids(&self) -> Vec<u32> {
        self.processes().iter().map(|p| p.pid).collect()
    }

    /// An expression in the browser's own (chrome) scope, read-only: process roles and the compositor.
    fn chrome_eval(&self, expression: &str) -> Result<Value> {
        let tree = self.bidi.send("browsingContext.getTree", json!({ "maxDepth": 0, "moz:scope": "chrome" }))?;
        let context = tree["contexts"][0]["context"].as_str().ok_or_else(|| Error::msg("Firefox gave no chrome context"))?.to_string();
        Page { bidi: self.bidi.clone(), context }.eval(expression)
    }

    /// Each process's role by pid (`ChromeUtils.requestProcInfo`): its content processes are
    /// `renderer`, its GPU process (Windows; macOS and Linux composite in the browser process, so they
    /// have none: no `cpu.gpu` rather than 0) `gpu`, the browser itself `browser`.
    pub fn process_roles(&self) -> Vec<(u32, &'static str)> {
        let Ok(info) = self.chrome_eval("ChromeUtils.requestProcInfo().then(i => JSON.stringify({ pid: i.pid, children: i.children.map(c => [c.pid, c.type]) }))") else { return Vec::new() };
        let Some(v) = info.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) else { return Vec::new() };
        let mut out: Vec<(u32, &'static str)> = v["pid"].as_u64().map(|p| (p as u32, "browser")).into_iter().collect();
        for c in v["children"].as_array().into_iter().flatten() {
            let role = match c[1].as_str().unwrap_or("") {
                t if t.starts_with("web") || t == "file" || t == "withCoopCoep" => "renderer",
                "gpu" => "gpu",
                _ => continue,
            };
            if let Some(p) = c[0].as_u64() {
                out.push((p as u32, role));
            }
        }
        out
    }

    /// The adapter Firefox draws with and whether its compositor runs on it (WebRender on the GPU) or
    /// in software.
    pub fn gpu(&self) -> Result<Gpu> {
        let v = self.chrome_eval(
            r#"(() => { const g = Cc["@mozilla.org/gfx/info;1"].getService(Ci.nsIGfxInfo); const f = (k) => { try { return String(g[k] ?? "") } catch { return "" } }; return JSON.stringify({ lm: window.windowUtils.layerManagerType, adapter: f("adapterDescription"), vendor: f("adapterVendorID"), device: f("adapterDeviceID") }) })()"#,
        )?;
        let v: Value = v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        let compositing = v["lm"].as_str().unwrap_or("").to_string();
        // macOS gives no adapter description, only its PCI ids.
        let name = match (v["adapter"].as_str().unwrap_or(""), v["vendor"].as_str().unwrap_or(""), v["device"].as_str().unwrap_or("")) {
            ("", "", "") => String::new(),
            ("", vendor, device) => format!("vendor {vendor} device {device}"),
            (a, _, _) => a.to_string(),
        };
        let devices: Vec<String> = Some(name).filter(|n| !n.is_empty()).into_iter().collect();
        let c = compositing.to_lowercase();
        // The compositor says whether Firefox draws on the GPU; an adapter named as a software one too.
        let software = c.contains("software") || c.contains("basic") || c.is_empty() || c == "none" || devices.first().is_some_and(|d| crate::validity::is_software_gpu(d));
        Ok(Gpu { devices, compositing, software })
    }

    /// Closes Firefox as a person would (`browser.close`, up to 5 s for it to exit), then ends what is
    /// left and removes the profile.
    pub fn close(mut self) {
        let _ = self.bidi.send_timeout("browser.close", json!({}), Duration::from_secs(5));
        if let Some(c) = self.child.as_mut() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline && c.exited().is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.end();
    }

    fn end(&mut self) {
        super::isolate::end_launch(self.child.take(), self.profile.take().as_deref(), true);
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

    /// Evaluates an expression in the page and returns its value (awaiting promises).
    pub fn eval_timeout(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let r = self.bidi.send_timeout(
            "script.evaluate",
            json!({ "expression": expression, "target": { "context": self.context }, "awaitPromise": true, "resultOwnership": "none" }),
            timeout + Duration::from_millis(500),
        )?;
        if r["type"] == "exception" {
            let text = r["exceptionDetails"]["text"].as_str().unwrap_or("exception");
            return Err(Error::msg(format!("page: {}", text.lines().next().unwrap_or(text))));
        }
        Ok(bidi::value(&r["result"]))
    }

    /// Navigates and waits for the load event.
    pub fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        self.bidi.send_timeout("browsingContext.navigate", json!({ "context": self.context, "url": url, "wait": "complete" }), timeout).map_err(|e| match e.code {
            124 => e,
            _ => Error::unreachable(format!("navigate {url}: {}", e.message)),
        })?;
        Ok(())
    }

    /// A script every document of the page runs before its own (`script.addPreloadScript`).
    pub fn add_init_script(&self, source: &str) -> Result<()> {
        self.bidi.send("script.addPreloadScript", json!({ "functionDeclaration": format!("() => {{ {source} }}"), "contexts": [self.context] }))?;
        Ok(())
    }

    /// The viewport as PNG bytes.
    pub fn screenshot(&self, timeout: Duration) -> Result<Vec<u8>> {
        let shot = self.bidi.send_timeout("browsingContext.captureScreenshot", json!({ "context": self.context }), timeout)?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, shot["data"].as_str().unwrap_or("")).map_err(|e| Error::msg(format!("screenshot: {e}")))
    }

    /// Input actions (`input.performActions`), waited for until the page has handled them.
    pub fn actions(&self, actions: Value) -> Result<()> {
        self.bidi.send_timeout("input.performActions", json!({ "context": self.context, "actions": actions }), Duration::from_secs(120)).map(|_| ())
    }

    /// Input actions sent without waiting (a paced drag's moves): they run in order in the browser.
    pub fn post_actions(&self, actions: Value) -> Result<()> {
        self.bidi.post("input.performActions", json!({ "context": self.context, "actions": actions }))
    }

    /// Subscribes to the page's network events: what `net.*` counts.
    pub fn enable_network(&self) -> Result<()> {
        self.bidi.send("session.subscribe", json!({ "events": ["network.beforeRequestSent", "network.responseCompleted"], "contexts": [self.context] })).map(|_| ())
    }
}
