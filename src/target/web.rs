//! A page target in any engine (SPEC §13.1): one browser and its page, Chromium over CDP, Firefox over
//! WebDriver BiDi or WebKit over WebDriver Classic. Callers drive them through these enums; engine-only
//! work (a trace, the Performance domain) asks for the Chromium side and is refused elsewhere.

use super::chromium::{self, Emulation, Gpu, Window};
use super::motion::Motion;
use super::{firefox, webkit};
use crate::host;
use crate::model::{Error, Refusal, Result, ValidatorId, WebEngine};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
/// Safari has no headless mode; a headed page on Linux with no display has no window to open.
pub fn refusal(engine: WebEngine, headless: bool) -> Option<Error> {
    if !headless && cfg!(target_os = "linux") && let Some(why) = chromium::no_display() {
        let (name, fix) = match engine {
            WebEngine::Chromium => ("Chromium", "run it headless (--headless), or give it a virtual display (xvfb-run -a strobe …), where frame and GPU metrics are refused as software rendering"),
            WebEngine::Firefox => ("Firefox", "run it headless (--headless), or give it a virtual display (xvfb-run -a strobe …), where frame and GPU metrics are refused as software rendering"),
            WebEngine::Webkit => ("WebKitGTK", "run on a desktop session or under a virtual X server (xvfb-run -a strobe ...), or measure the page in Chromium or Firefox"),
        };
        return Some(Refusal::new(ValidatorId::NoPresentSource, format!("a headed {name} page needs a display, and {why}")).fix(fix).into());
    }
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
        // Without a display a headless WebKit page would run in WPE, whose headless view is a touch device
        // to its pages ((hover: none), a coarse pointer, no fine one): every desktop block would refuse.
        WebEngine::Webkit if headless && webkit::find(true).is_some_and(|i| i.flavor == webkit::Flavor::Wpe) => Refusal::new(
            ValidatorId::DeviceMismatch,
            "a headless WebKit page here runs in WPE WebKit (no display for WebKitGTK), whose headless view is a touch device to its pages, not a desktop",
        )
        .fix("run WebKitGTK on a virtual display (xvfb-run -a strobe … --headless), or measure the page in Chromium or Firefox"),
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

/// What a headed page's window has on the display it shows on (SPEC 9.10 "Declared viewport"), in the
/// page's CSS pixels: the display's work area (`host::work_area`: less the menu bar and Dock, the
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

    /// A read worth keeping and judging by: a page and a window, the window no smaller than the page,
    /// on an unlocked session (a locked one may report a window it never laid out).
    fn sane(&self) -> bool {
        self.page.0 > 0.0 && self.page.1 > 0.0 && self.chrome.0 >= 0.0 && self.chrome.1 >= 0.0 && self.window[2] >= self.page.0 && self.window[3] >= self.page.1 && !host::session_locked()
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
    let v = page.eval("[innerWidth, innerHeight, outerWidth, outerHeight, screenX, screenY, screen.width]")?;
    let n = |i: usize| v[i].as_f64().unwrap_or(0.0);
    // The display holding the window's centre.
    let Some(display) = host::work_area((n(4) + n(2) / 2.0, n(5) + n(3) / 2.0)) else { return Ok(None) };
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

/// Keeps `r` for its room key, unless the room kept was read in a taller window: a browser's part may
/// be smaller in a short window (Firefox's compact toolbars under 720 CSS pixels), and what a plan and
/// a bench judge by is the most a page can be, in the tallest window, so a small page's fit never
/// makes a large one look as if it fits.
fn remember(engine: WebEngine, scale: Option<f64>, browser: &str, r: &Room) {
    let mut all: Value = std::fs::read(rooms_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let entry = json!({ "browser": browser, "area": r.area, "chrome": [r.chrome.0, r.chrome.1], "window_height": r.window[3] });
    let key = room_key(engine, scale, &r.display);
    if all[&key]["window_height"].as_f64().is_some_and(|h| h > r.window[3] + 1.0) {
        return;
    }
    if all[&key] != entry {
        all[&key] = entry;
        let _ = host::paths::write_atomic(&rooms_file(), &serde_json::to_vec_pretty(&all).unwrap_or_default());
    }
}

/// The room a headed `engine` page had before on the work area where strobe places its windows, and the
/// browser it was: what a plan and a bench check a declared viewport against before they open a window.
pub fn known_room(engine: WebEngine, scale: Option<f64>) -> Option<(String, Room)> {
    let at = Window::default();
    let display = host::work_area((at.x as f64, at.y as f64))?;
    let all: Value = serde_json::from_slice(&std::fs::read(rooms_file()).ok()?).ok()?;
    let e = &all[room_key(engine, scale, &display)];
    let num = |v: &Value| v.as_f64();
    let area = [num(&e["area"][0])?, num(&e["area"][1])?, num(&e["area"][2])?, num(&e["area"][3])?];
    let room = Room { area, chrome: (num(&e["chrome"][0])?, num(&e["chrome"][1])?), page: (0.0, 0.0), window: [0.0; 4], display };
    Some((e["browser"].as_str().unwrap_or("the browser").to_string(), room))
}

/// Launches a fresh browser of `engine` with an isolated profile. `cpus` pins it and its children.
pub fn launch(engine: WebEngine, window: &Window, headless: bool, cpus: Option<u64>) -> Result<WebBrowser> {
    launch_as(engine, window, headless, cpus, &Emulation::default(), Motion::System)
}

/// `refusal` for a page that emulates `emulation`, known before anything builds: WebKit has no setting for
/// any of it over WebDriver Classic (`EngineUnsupported`, said before WPE's desktop refusal, which is
/// not the reason then).
pub fn refusal_as(engine: WebEngine, headless: bool, emulation: &Emulation) -> Option<Error> {
    if engine == WebEngine::Webkit && *emulation != Emulation::default() {
        return Some(
            Refusal::new(ValidatorId::EngineUnsupported, format!("an emulated {} page in WebKit: WebDriver Classic has no setting for the input devices, touch points or user agent a page sees", emulation.kind()))
                .fix("emulate the phone or touch screen in Chromium or Firefox, or measure the page as a desktop in WebKit")
                .into(),
        );
    }
    refusal(engine, headless)
}

/// `launch` as `emulation` (SPEC 13.1 "Emulated phones and touch screens"): Chromium takes its input
/// devices at launch and the rest per page (`WebPage::emulate`, then `emulate_viewport` once sized),
/// Firefox all of it in its profile; WebKit refuses (`refusal_as`). With `motion` reduced, each engine's
/// own setting for it in this launch alone (`motion`; Safari and WPE refuse it).
pub fn launch_as(engine: WebEngine, window: &Window, headless: bool, cpus: Option<u64>, emulation: &Emulation, motion: Motion) -> Result<WebBrowser> {
    if let Some(e) = refusal_as(engine, headless, emulation) {
        return Err(e);
    }
    if let Some(r) = super::motion::engine_refusal(engine, headless).filter(|_| motion.reduced()) {
        return Err(r.into());
    }
    Ok(match engine {
        WebEngine::Chromium => WebBrowser::Chromium(chromium::launch_as(window, headless, cpus, &motion.chromium_args(), emulation)?),
        WebEngine::Firefox => WebBrowser::Firefox(firefox::launch_as(window, headless, cpus, emulation, motion)?),
        WebEngine::Webkit => WebBrowser::Webkit(webkit::launch(window, headless, cpus, motion)?),
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

    /// Sizes the window so the page is the target's viewport, `width`×`height` CSS pixels (SPEC 9.10
    /// "Declared viewport"), never by emulating a viewport the window does not have. Headed, the window
    /// is moved inside the display's work area where it fits there. Each ask is sized from a read of the
    /// page and window that held still for 100 ms (at most 1 s before the first), so a window still
    /// taking its last size lends no part of its own; then the page is read until it shows the size and
    /// holds still 100 ms (at most 2 s after each ask). A browser may lay its own part out again once its
    /// window takes a size (Firefox makes its toolbars compact in a window under 720 CSS pixels tall, and
    /// normal again above): a page that holds still at another size, its window's part changed since the
    /// ask, is asked again with the part it has now, at most twice. A size this display cannot show
    /// refuses `DisplayTooSmall`, naming the largest page this browser's window shows on it, which strobe
    /// keeps for the next plan and bench (`known_room`). A read that is no measure (`Room::sane`: no page,
    /// a window smaller than its page, a locked session) is neither kept nor refused by. Any other
    /// difference is left to the block's viewport check.
    pub fn fit_viewport(&self, page: &WebPage, width: u32, height: u32, headless: bool) -> Result<()> {
        if headless {
            return self.fit_viewport_at(page, width, height, None);
        }
        let ask = |r: &Room| self.fit_viewport_at(page, width, height, Some(r.place((self.window().x, self.window().y), (width as f64 + r.chrome.0, height as f64 + r.chrome.1))));
        // Chromium names itself by its product token (`Edg/156.0.4314.58`).
        let browser = self.version().replacen('/', " ", 1);
        // The read the last ask was sized from (none before the first), and until when the page may take it.
        let (mut asked, mut asks, mut deadline): (Option<Room>, u32, Instant) = (None, 0, Instant::now() + Duration::from_secs(1));
        // The page and window as last read, and since when they hold still.
        let (mut last, mut still) = (None, Instant::now());
        let r = loop {
            let Some(r) = room(page)? else {
                // No display strobe can read: sized by the browser's own reads alone.
                return if asked.is_none() { self.fit_viewport_at(page, width, height, None) } else { Ok(()) };
            };
            let now = Some((r.page, r.window));
            if now != last {
                (last, still) = (now, Instant::now());
            }
            let held = still.elapsed() >= Duration::from_millis(100);
            let late = Instant::now() >= deadline;
            let ask_now = match asked {
                None => held || late,
                Some(a) => {
                    let shown = (r.page.0 - width as f64).abs() <= 1.0 && (r.page.1 - height as f64).abs() <= 1.0 && r.inside();
                    if shown && held {
                        if r.sane() {
                            remember(self.engine(), self.window().scale, &browser, &r);
                        }
                        return Ok(());
                    }
                    let relaid = (r.chrome.0 - a.chrome.0).abs() > 1.0 || (r.chrome.1 - a.chrome.1).abs() > 1.0;
                    let again = relaid && held && asks < 3 && r.sane();
                    if !again && late {
                        break r;
                    }
                    again
                }
            };
            if ask_now {
                ask(&r)?;
                (asked, asks, deadline, last) = (Some(r), asks + 1, Instant::now() + Duration::from_secs(2), None);
                continue;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if !r.sane() {
            return Ok(());
        }
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

    /// The part of `emulation` a page takes after the launch, before anything sizes or loads it (Chromium:
    /// `chromium::Page::emulate`). Firefox took all of it at launch; WebKit a desktop only.
    pub fn emulate(&self, emulation: &Emulation) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.emulate(emulation),
            WebPage::Firefox(_) | WebPage::Webkit(_) => Ok(()),
        }
    }

    /// The part a page takes once its window shows the declared `viewport` (`fit_viewport` first): a
    /// phone's viewport flag in Chromium (`chromium::Page::emulate_viewport`).
    pub fn emulate_viewport(&self, emulation: &Emulation, viewport: (u32, u32), scale: Option<f64>) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.emulate_viewport(emulation, viewport, scale),
            WebPage::Firefox(_) | WebPage::Webkit(_) => Ok(()),
        }
    }

    /// `DisplayTooSmall` when the page is not the `viewport` it was fitted to: a desktop browser keeps its
    /// window some 500 pixels wide at least (492 for Chromium and Edge on Windows), so a headed phone of
    /// 390 cannot be one.
    pub fn check_viewport(&self, viewport: (u32, u32)) -> Result<()> {
        let v = self.eval("[innerWidth, innerHeight]")?;
        let got = (v[0].as_u64().unwrap_or(0), v[1].as_u64().unwrap_or(0));
        if got == (viewport.0 as u64, viewport.1 as u64) {
            return Ok(());
        }
        Err(Refusal::new(ValidatorId::DisplayTooSmall, format!("the page is {}×{}, not the {}×{} viewport it was fitted to: a desktop browser's window has a minimum size", got.0, got.1, viewport.0, viewport.1))
            .fix("run a phone headless (--headless), which has no window to keep wide")
            .into())
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
