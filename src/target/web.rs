//! A page target in any engine (SPEC §13.1): one browser and its page, Chromium over CDP, Firefox over
//! WebDriver BiDi or WebKit over WebDriver Classic. Callers drive them through these enums; engine-only
//! work (a trace, the Performance domain) asks for the Chromium side and is refused elsewhere.

use super::chromium::{self, Gpu, Window};
use super::{firefox, webkit};
use crate::host;
use crate::model::{Error, Refusal, Result, ValidatorId, WebEngine};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub enum WebBrowser {
    Chromium(chromium::Browser),
    Firefox(firefox::Browser),
    Webkit(webkit::Browser),
}

#[derive(Clone)]
pub enum WebPage {
    Chromium(chromium::Page),
    Firefox(firefox::Page),
    Webkit(webkit::Page),
}

impl From<chromium::Browser> for WebBrowser {
    fn from(b: chromium::Browser) -> Self {
        WebBrowser::Chromium(b)
    }
}

impl From<chromium::Page> for WebPage {
    fn from(p: chromium::Page) -> Self {
        WebPage::Chromium(p)
    }
}

/// Why a page of `engine` cannot run here at all, known before anything builds: WebKit on Windows has
/// no browser a person runs (Safari is macOS only, Playwright's WebKit build is a test shell), and
/// Safari has no headless mode.
pub fn refusal(engine: WebEngine, headless: bool) -> Option<Error> {
    let r = match engine {
        WebEngine::Webkit if cfg!(windows) => Refusal::new(
            ValidatorId::EngineUnsupported,
            "WebKit on Windows: no WebKit browser runs there for people (Safari is macOS only, and Playwright's WebKit build is a test shell no one browses with)",
        )
        .fix("measure the page in Chromium (--engine chromium) or Firefox (--engine firefox)"),
        WebEngine::Webkit if cfg!(target_os = "macos") && headless => {
            Refusal::new(ValidatorId::EngineUnsupported, "Safari has no headless mode: a WebKit page on macOS runs headed, in an unlocked session")
                .fix("drop --headless (and the target's headless) for --engine webkit")
        }
        _ => return None,
    };
    Some(r.into())
}

/// The installed browser for an engine, if strobe finds one (WebKit: the headed or the headless one).
pub fn find(engine: WebEngine, headless: bool) -> Option<PathBuf> {
    match engine {
        WebEngine::Chromium => chromium::find(),
        WebEngine::Firefox => firefox::find(),
        WebEngine::Webkit => webkit::find(headless).map(|i| i.browser),
    }
}

/// Why the browser strobe would launch for `engine` is not one people run (`PatchedBrowser`): a
/// Playwright build named by `STROBE_FIREFOX`. strobe never picks one up by itself (WebKit: it finds only
/// the system's Safari and the distribution's MiniBrowser).
pub fn patched(engine: WebEngine) -> Option<String> {
    match engine {
        WebEngine::Firefox => find(engine, false)
            .filter(|p| firefox::patched(p))
            .map(|p| format!("{} is Playwright's patched Firefox (from STROBE_FIREFOX): its process model and preferences are not the Firefox people run", p.display())),
        WebEngine::Chromium | WebEngine::Webkit => None,
    }
}

/// What a browser install is for the rerun ledger and track's fingerprint, known before it launches:
/// Firefox's version from its `application.ini`, Safari's from its bundle, else the executable (WebKit:
/// its WebDriver server) with its size and time (a browser update changes either).
pub fn browser_identity(engine: WebEngine, headless: bool) -> String {
    let known = match engine {
        WebEngine::Webkit => return webkit::identity(headless),
        WebEngine::Firefox => find(engine, headless).and_then(|exe| firefox::version_of(&exe)),
        WebEngine::Chromium => None,
    };
    if let Some(v) = known {
        return v;
    }
    let Some(exe) = find(engine, headless) else { return format!("{engine}: not found") };
    let stamp =
        std::fs::metadata(&exe).ok().map(|m| format!("@{}-{}", m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()))).unwrap_or_default();
    format!("{} {}{stamp}", engine, exe.display())
}

/// How far a window may reach past the work area's left, right and bottom edges: Windows gives every
/// window an invisible resize border there (8 pixels at 100%), which may lie past the edge unseen.
const FRAME_SLACK: f64 = if cfg!(windows) { 8.0 } else { 0.0 };

/// What a headed page's window has on the display strobe places it on (SPEC 9.6 "Declared viewport"), in
/// the page's CSS pixels: the display's work area (`host::work_area`: less the menu bar and Dock, the
/// taskbar, the panels), the window's own part (its size less the page's: title bar, tabs, toolbar,
/// frame), the page, and where the window is.
#[derive(Clone, Copy, Debug)]
pub struct Room {
    pub area: [f64; 4],
    pub chrome: (f64, f64),
    pub page: (f64, f64),
    window: [f64; 4],
    display: host::WorkArea,
}

impl Room {
    /// The largest page this browser's window shows inside the work area.
    pub fn most(&self) -> (f64, f64) {
        ((self.area[2] + 2.0 * FRAME_SLACK - self.chrome.0).floor(), (self.area[3] + FRAME_SLACK - self.chrome.1).floor())
    }

    /// The window lies inside the work area (within a point, and the frame's slack).
    fn inside(&self) -> bool {
        let ([x, y, w, h], [ax, ay, aw, ah], s) = (self.window, self.area, FRAME_SLACK + 1.0);
        x >= ax - s && y >= ay - 1.0 && x + w <= ax + aw + s && y + h <= ay + ah + s
    }

    /// Where a window of `outer` size goes so it lies inside the work area: `at`, moved no more than it
    /// must (to the area's top left when it is larger).
    fn place(&self, at: (i32, i32), outer: (f64, f64)) -> (i32, i32) {
        let [ax, ay, aw, ah] = self.area;
        let x = (at.0 as f64).min(ax + aw + FRAME_SLACK - outer.0).max(ax - FRAME_SLACK);
        let y = (at.1 as f64).min(ay + ah + FRAME_SLACK - outer.1).max(ay);
        (x.round() as i32, y.round() as i32)
    }

    /// `DisplayTooSmall` when a page of `asked` CSS pixels does not fit in `most`.
    pub fn too_small(&self, browser: &str, asked: (u32, u32)) -> Option<Refusal> {
        let (mw, mh) = self.most();
        if asked.0 as f64 <= mw + 1.0 && asked.1 as f64 <= mh + 1.0 {
            return None;
        }
        Some(
            Refusal::new(
                ValidatorId::DisplayTooSmall,
                format!(
                    "this display shows at most {mw}×{mh} of a page in {browser}, not the viewport {}×{} the target asks for: its work area is {:.0}×{:.0} and the browser's window takes {:.0}×{:.0} of it",
                    asked.0, asked.1, self.area[2], self.area[3], self.chrome.0, self.chrome.1
                ),
            )
            .fix(format!("declare a viewport no larger than {mw}×{mh}, run on a larger display, or run the page headless (--headless), which has no window to fit")),
        )
    }
}

/// The page's room now, or None without a display strobe can read.
pub fn room(page: &WebPage) -> Result<Option<Room>> {
    let Some(display) = host::work_area() else { return Ok(None) };
    let v = page.eval("[innerWidth, innerHeight, outerWidth, outerHeight, screenX, screenY, screen.width]")?;
    let n = |i: usize| v[i].as_f64().unwrap_or(0.0);
    // CSS pixels per device-independent pixel: 1, unless the target forces another scale.
    let k = if display.screen[2] > 0.0 && n(6) > 0.0 { n(6) / display.screen[2] } else { 1.0 };
    Ok(Some(Room { area: display.work.map(|a| a * k), chrome: (n(2) - n(0), n(3) - n(1)), page: (n(0), n(1)), window: [n(4), n(5), n(2), n(3)], display }))
}

/// Where strobe keeps each browser's room per work area, for a plan or a bench before it opens one.
fn rooms_file() -> PathBuf {
    host::paths::state_dir().join("windows.json")
}

fn room_key(engine: WebEngine, scale: Option<f64>, display: &host::WorkArea) -> String {
    let r = |a: [f64; 4]| a.map(|v| v.round() as i64);
    format!("{} · scale {} · work area {:?} of {:?}", browser_identity(engine, false), scale.map_or("display".to_string(), |s| s.to_string()), r(display.work), r(display.screen))
}

fn remember(engine: WebEngine, scale: Option<f64>, browser: &str, r: &Room) {
    let mut all: Value = std::fs::read(rooms_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let entry = json!({ "browser": browser, "area": r.area, "chrome": [r.chrome.0, r.chrome.1] });
    let key = room_key(engine, scale, &r.display);
    if all[&key] != entry {
        all[&key] = entry;
        let _ = host::paths::write_atomic(&rooms_file(), &serde_json::to_vec_pretty(&all).unwrap_or_default());
    }
}

/// The room a headed `engine` page had on this work area before, and the browser it was: what a plan
/// and a bench check a declared viewport against before they open a window.
pub fn known_room(engine: WebEngine, scale: Option<f64>) -> Option<(String, Room)> {
    let display = host::work_area()?;
    let all: Value = serde_json::from_slice(&std::fs::read(rooms_file()).ok()?).ok()?;
    let e = &all[room_key(engine, scale, &display)];
    let num = |v: &Value| v.as_f64();
    let area = [num(&e["area"][0])?, num(&e["area"][1])?, num(&e["area"][2])?, num(&e["area"][3])?];
    let room = Room { area, chrome: (num(&e["chrome"][0])?, num(&e["chrome"][1])?), page: (0.0, 0.0), window: [0.0; 4], display };
    Some((e["browser"].as_str().unwrap_or("the browser").to_string(), room))
}

/// Launches a fresh browser of `engine` with an isolated profile. `cpus` pins it and its children.
pub fn launch(engine: WebEngine, window: &Window, headless: bool, cpus: Option<u64>) -> Result<WebBrowser> {
    if let Some(e) = refusal(engine, headless) {
        return Err(e);
    }
    Ok(match engine {
        WebEngine::Chromium => WebBrowser::Chromium(chromium::launch(window, headless, cpus, &[])?),
        WebEngine::Firefox => WebBrowser::Firefox(firefox::launch(window, headless, cpus)?),
        WebEngine::Webkit => WebBrowser::Webkit(webkit::launch(window, headless, cpus)?),
    })
}

impl WebBrowser {
    pub fn engine(&self) -> WebEngine {
        match self {
            WebBrowser::Chromium(_) => WebEngine::Chromium,
            WebBrowser::Firefox(_) => WebEngine::Firefox,
            WebBrowser::Webkit(_) => WebEngine::Webkit,
        }
    }

    /// The launched browser's pid (0 when attached; WebKit: the browser process, not its WebDriver
    /// server).
    pub fn pid(&self) -> u32 {
        match self {
            WebBrowser::Chromium(b) => b.pid,
            WebBrowser::Firefox(b) => b.pid,
            WebBrowser::Webkit(b) => b.pid,
        }
    }

    /// The launch's home or profile, which its process set follows.
    pub fn profile(&self) -> Option<PathBuf> {
        match self {
            WebBrowser::Chromium(b) => b.profile.clone(),
            WebBrowser::Firefox(b) => b.profile.clone(),
            // The browser's tree holds all its processes; the home's stamp would add the WebDriver server.
            WebBrowser::Webkit(_) => None,
        }
    }

    /// Whether the browser is still only strobe's (`LiveChannel` once a person opened a window in a
    /// Safari strobe launched).
    pub fn own(&self) -> Result<()> {
        match self {
            WebBrowser::Chromium(_) | WebBrowser::Firefox(_) => Ok(()),
            WebBrowser::Webkit(b) => b.own(),
        }
    }

    pub fn version(&self) -> &str {
        match self {
            WebBrowser::Chromium(b) => &b.version,
            WebBrowser::Firefox(b) => &b.version,
            WebBrowser::Webkit(b) => &b.version,
        }
    }

    /// The Chromium side, for what only CDP gives (a trace, the Performance domain, heap snapshots).
    pub fn cdp(&self) -> Option<&chromium::Browser> {
        match self {
            WebBrowser::Chromium(b) => Some(b),
            WebBrowser::Firefox(_) | WebBrowser::Webkit(_) => None,
        }
    }

    /// The page strobe drives.
    pub fn page(&self) -> Result<WebPage> {
        Ok(match self {
            WebBrowser::Chromium(b) => WebPage::Chromium(b.page()?),
            WebBrowser::Firefox(b) => WebPage::Firefox(b.page()?),
            WebBrowser::Webkit(b) => WebPage::Webkit(b.page()?),
        })
    }

    /// The window strobe asked the browser for (its place, size and scale).
    pub fn window(&self) -> &Window {
        match self {
            WebBrowser::Chromium(b) => &b.window,
            WebBrowser::Firefox(b) => &b.window,
            WebBrowser::Webkit(b) => &b.window,
        }
    }

    /// Sizes the window so the page is `width`×`height` CSS pixels, at `at` (a screen point in CSS pixels)
    /// when given, else where the window was asked to be.
    fn fit_viewport_at(&self, page: &WebPage, width: u32, height: u32, at: Option<(i32, i32)>) -> Result<()> {
        match (self, page) {
            (WebBrowser::Chromium(b), WebPage::Chromium(p)) => b.fit_viewport(p, width, height, at),
            (WebBrowser::Firefox(b), WebPage::Firefox(p)) => b.fit_viewport(p, width, height, at),
            (WebBrowser::Webkit(b), WebPage::Webkit(p)) => b.fit_viewport(p, width, height, at),
            (WebBrowser::Chromium(_), WebPage::Firefox(_) | WebPage::Webkit(_))
            | (WebBrowser::Firefox(_), WebPage::Chromium(_) | WebPage::Webkit(_))
            | (WebBrowser::Webkit(_), WebPage::Chromium(_) | WebPage::Firefox(_)) => Err(Error::msg("internal: a page of another browser")),
        }
    }

    /// Sizes the window so the page is the target's viewport, `width`×`height` CSS pixels (SPEC 9.6
    /// "Declared viewport"), never by emulating a viewport the window does not have. Headed, the window
    /// is moved inside the display's work area where it fits there, and the page is read until it takes
    /// the size (at most 2 s): a size this display cannot show refuses `DisplayTooSmall`, naming the
    /// largest page this browser's window shows on it, which strobe keeps for the next plan and bench
    /// (`known_room`). Any other difference is left to the block's viewport check.
    pub fn fit_viewport(&self, page: &WebPage, width: u32, height: u32, headless: bool) -> Result<()> {
        let before = if headless { None } else { room(page)? };
        let at = before.map(|r| r.place((self.window().x, self.window().y), (width as f64 + r.chrome.0, height as f64 + r.chrome.1)));
        self.fit_viewport_at(page, width, height, at)?;
        if before.is_none() {
            return Ok(());
        }
        // Chromium names itself by its product token (`Edg/156.0.4314.58`).
        let browser = self.version().replacen('/', " ", 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let r = loop {
            let Some(r) = room(page)? else { return Ok(()) };
            let shown = (r.page.0 - width as f64).abs() <= 1.0 && (r.page.1 - height as f64).abs() <= 1.0 && r.inside();
            if shown {
                remember(self.engine(), self.window().scale, &browser, &r);
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                break r;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        remember(self.engine(), self.window().scale, &browser, &r);
        match r.too_small(&browser, (width, height)) {
            Some(refusal) => Err(refusal.into()),
            None => Ok(()),
        }
    }

    pub fn processes(&self) -> Vec<host::ProcTimes> {
        match self {
            WebBrowser::Chromium(b) => b.processes(),
            WebBrowser::Firefox(b) => b.processes(),
            WebBrowser::Webkit(b) => b.processes(),
        }
    }

    pub fn pids(&self) -> Vec<u32> {
        self.processes().iter().map(|p| p.pid).collect()
    }

    /// Each process's role (`renderer`, `gpu`, `browser`) by pid, where the browser says.
    pub fn process_roles(&self) -> Vec<(u32, &'static str)> {
        match self {
            WebBrowser::Chromium(b) => {
                let info = b.cdp.send("SystemInfo.getProcessInfo", json!({}), None).unwrap_or(Value::Null);
                info["processInfo"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| {
                        let role = match p["type"].as_str()? {
                            "renderer" => "renderer",
                            "GPU" => "gpu",
                            "browser" => "browser",
                            _ => return None,
                        };
                        Some((p["id"].as_u64()? as u32, role))
                    })
                    .collect()
            }
            WebBrowser::Firefox(b) => b.process_roles(),
            WebBrowser::Webkit(b) => b.process_roles(),
        }
    }

    pub fn gpu(&self) -> Result<Gpu> {
        match self {
            WebBrowser::Chromium(b) => b.gpu(),
            WebBrowser::Firefox(b) => b.gpu(),
            WebBrowser::Webkit(b) => b.gpu(),
        }
    }

    pub fn close(self) {
        match self {
            WebBrowser::Chromium(b) => b.close(),
            WebBrowser::Firefox(b) => b.close(),
            WebBrowser::Webkit(b) => b.close(),
        }
    }
}

impl WebPage {
    pub fn engine(&self) -> WebEngine {
        match self {
            WebPage::Chromium(_) => WebEngine::Chromium,
            WebPage::Firefox(_) => WebEngine::Firefox,
            WebPage::Webkit(_) => WebEngine::Webkit,
        }
    }

    /// The Chromium side, for CDP-only work.
    pub fn cdp(&self) -> Option<&chromium::Page> {
        match self {
            WebPage::Chromium(p) => Some(p),
            WebPage::Firefox(_) | WebPage::Webkit(_) => None,
        }
    }

    pub fn firefox(&self) -> Option<&firefox::Page> {
        match self {
            WebPage::Chromium(_) | WebPage::Webkit(_) => None,
            WebPage::Firefox(p) => Some(p),
        }
    }

    pub fn eval(&self, expression: &str) -> Result<Value> {
        self.eval_timeout(expression, Duration::from_secs(60))
    }

    /// Evaluates an expression and returns its value (awaiting promises).
    pub fn eval_timeout(&self, expression: &str, timeout: Duration) -> Result<Value> {
        match self {
            WebPage::Chromium(p) => p.eval_timeout(expression, timeout),
            WebPage::Firefox(p) => p.eval_timeout(expression, timeout),
            WebPage::Webkit(p) => p.eval_timeout(expression, timeout),
        }
    }

    /// Navigates and waits for the load event.
    pub fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.navigate(url, timeout),
            WebPage::Firefox(p) => p.navigate(url, timeout),
            WebPage::Webkit(p) => p.navigate(url, timeout),
        }
    }

    /// Starts a navigation and returns once the browser took it, without waiting for the document: a
    /// launch case times the load itself. WebKit's Classic navigation returns only after the load.
    pub fn start_navigation(&self, url: &str) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.send("Page.navigate", json!({ "url": url })).map(|_| ()),
            WebPage::Firefox(p) => p.bidi.send("browsingContext.navigate", json!({ "context": p.context, "url": url, "wait": "none" })).map(|_| ()),
            WebPage::Webkit(_) => Err(Error::msg("internal: a WebKit page cannot start a navigation without waiting for its load")),
        }
    }

    /// A script every document runs before its own (WebKit, which has no preload: once strobe first
    /// touches the document, after its own).
    pub fn add_init_script(&self, source: &str) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": source })).map(|_| ()),
            WebPage::Firefox(p) => p.add_init_script(source),
            WebPage::Webkit(p) => p.add_init_script(source),
        }
    }

    /// The viewport as PNG bytes.
    pub fn screenshot(&self, timeout: Duration) -> Result<Vec<u8>> {
        match self {
            WebPage::Chromium(p) => {
                let shot = p.send_timeout("Page.captureScreenshot", json!({ "format": "png" }), timeout)?;
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, shot["data"].as_str().unwrap_or("")).map_err(|e| Error::msg(format!("screenshot: {e}")))
            }
            WebPage::Firefox(p) => p.screenshot(timeout),
            WebPage::Webkit(p) => p.screenshot(timeout),
        }
    }

    /// WebDriver input actions (BiDi's `input.performActions`, Classic's `POST /actions`), back once the
    /// page handled them. Chromium's input goes over CDP instead.
    pub fn actions(&self, actions: Value) -> Result<()> {
        match self {
            WebPage::Chromium(_) => Err(Error::msg("internal: a Chromium page takes CDP input events, not WebDriver actions")),
            WebPage::Firefox(p) => p.actions(actions),
            WebPage::Webkit(p) => p.actions(actions),
        }
    }

    /// Input actions sent without waiting for the page (a paced drag's moves, a scroll's wheel steps), in
    /// order. WebDriver Classic answers each list only once it ran, so on WebKit they are waited for.
    pub fn post_actions(&self, actions: Value) -> Result<()> {
        match self {
            WebPage::Chromium(_) => Err(Error::msg("internal: a Chromium page takes CDP input events, not WebDriver actions")),
            WebPage::Firefox(p) => p.post_actions(actions),
            WebPage::Webkit(p) => p.actions(actions),
        }
    }

    /// Whether a shifted symbol (`!`) is typed as Shift and its key's own character (`1`), as Safari
    /// takes it, rather than Shift and the symbol.
    pub fn shift_takes_base_key(&self) -> bool {
        matches!(self, WebPage::Webkit(p) if p.shift_base)
    }

    /// WebKit: how many documents strobe has run its page scripts in, which a run that loads a page
    /// changes (WebDriver Classic has no preload: what such a run did in the new document before strobe's
    /// scripts ran there went unseen). None where the browser runs them before the page's own.
    pub fn documents(&self) -> Option<u64> {
        match self {
            WebPage::Webkit(p) => Some(p.documents()),
            WebPage::Chromium(_) | WebPage::Firefox(_) => None,
        }
    }
}
