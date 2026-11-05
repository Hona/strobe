//! A page target in any engine (SPEC §13.1): one browser and its page, Chromium over CDP or Firefox
//! over WebDriver BiDi. Callers drive both through these enums; engine-only work (a trace, the
//! Performance domain) asks for the Chromium side and is refused elsewhere.

use super::chromium::{self, Gpu, Window};
use super::firefox;
use crate::host;
use crate::model::{Error, Refusal, Result, ValidatorId, WebEngine};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub enum WebBrowser {
    Chromium(chromium::Browser),
    Firefox(firefox::Browser),
}

#[derive(Clone)]
pub enum WebPage {
    Chromium(chromium::Page),
    Firefox(firefox::Page),
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

/// The refusal for an engine strobe cannot run here: WebKit on Windows has no browser a person runs
/// (Safari is macOS only, Playwright's WebKit build is a test shell), and elsewhere it is not built yet.
pub fn webkit_refusal() -> Error {
    let summary = if cfg!(windows) {
        "WebKit on Windows: no WebKit browser runs there for people (Safari is macOS only, and Playwright's WebKit build is a test shell no one browses with)"
    } else {
        "WebKit pages (Safari, WebKitGTK, WPE) are not measured yet"
    };
    Refusal::new(ValidatorId::EngineUnsupported, summary).fix("measure the page in Chromium (--engine chromium) or Firefox (--engine firefox)").into()
}

/// The installed browser for an engine, if strobe finds one.
pub fn find(engine: WebEngine) -> Option<PathBuf> {
    match engine {
        WebEngine::Chromium => chromium::find(),
        WebEngine::Firefox => firefox::find(),
        WebEngine::Webkit => None,
    }
}

/// Why the browser strobe would launch for `engine` is not one people run (`PatchedBrowser`): a
/// Playwright build named by `STROBE_FIREFOX`. strobe never picks one up by itself.
pub fn patched(engine: WebEngine) -> Option<String> {
    match engine {
        WebEngine::Firefox => find(engine).filter(|p| firefox::patched(p)).map(|p| format!("{} is Playwright's patched Firefox (from STROBE_FIREFOX): its process model and preferences are not the Firefox people run", p.display())),
        WebEngine::Chromium | WebEngine::Webkit => None,
    }
}

/// What a browser install is for the rerun ledger and track's fingerprint, known before it launches:
/// Firefox's version from its `application.ini`, else the executable with its size and time (a
/// browser update changes either).
pub fn browser_identity(engine: WebEngine) -> String {
    let Some(exe) = find(engine) else { return format!("{engine}: not found") };
    let known = match engine {
        WebEngine::Firefox => firefox::version_of(&exe),
        WebEngine::Chromium | WebEngine::Webkit => None,
    };
    if let Some(v) = known {
        return v;
    }
    let stamp = std::fs::metadata(&exe).ok().map(|m| format!("@{}-{}", m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()))).unwrap_or_default();
    format!("{} {}{stamp}", engine, exe.display())
}

/// Launches a fresh browser of `engine` with an isolated profile. `cpus` pins it and its children.
pub fn launch(engine: WebEngine, window: &Window, headless: bool, cpus: Option<u64>) -> Result<WebBrowser> {
    Ok(match engine {
        WebEngine::Chromium => WebBrowser::Chromium(chromium::launch(window, headless, cpus, &[])?),
        WebEngine::Firefox => WebBrowser::Firefox(firefox::launch(window, headless, cpus)?),
        WebEngine::Webkit => return Err(webkit_refusal()),
    })
}

impl WebBrowser {
    pub fn engine(&self) -> WebEngine {
        match self {
            WebBrowser::Chromium(_) => WebEngine::Chromium,
            WebBrowser::Firefox(_) => WebEngine::Firefox,
        }
    }

    /// The launched child's pid (0 when attached).
    pub fn pid(&self) -> u32 {
        match self {
            WebBrowser::Chromium(b) => b.pid,
            WebBrowser::Firefox(b) => b.pid,
        }
    }

    /// The launch's home or profile, which its process set follows.
    pub fn profile(&self) -> Option<PathBuf> {
        match self {
            WebBrowser::Chromium(b) => b.profile.clone(),
            WebBrowser::Firefox(b) => b.profile.clone(),
        }
    }

    pub fn version(&self) -> &str {
        match self {
            WebBrowser::Chromium(b) => &b.version,
            WebBrowser::Firefox(b) => &b.version,
        }
    }

    /// The Chromium side, for what only CDP gives (a trace, the Performance domain, heap snapshots).
    pub fn cdp(&self) -> Option<&chromium::Browser> {
        match self {
            WebBrowser::Chromium(b) => Some(b),
            WebBrowser::Firefox(_) => None,
        }
    }

    /// The page strobe drives.
    pub fn page(&self) -> Result<WebPage> {
        Ok(match self {
            WebBrowser::Chromium(b) => WebPage::Chromium(b.page()?),
            WebBrowser::Firefox(b) => WebPage::Firefox(b.page()?),
        })
    }

    pub fn fit_viewport(&self, page: &WebPage, width: u32, height: u32) -> Result<()> {
        match (self, page) {
            (WebBrowser::Chromium(b), WebPage::Chromium(p)) => b.fit_viewport(p, width, height),
            (WebBrowser::Firefox(b), WebPage::Firefox(p)) => b.fit_viewport(p, width, height),
            (WebBrowser::Chromium(_), WebPage::Firefox(_)) | (WebBrowser::Firefox(_), WebPage::Chromium(_)) => Err(Error::msg("internal: a page of another browser")),
        }
    }

    pub fn processes(&self) -> Vec<host::ProcTimes> {
        match self {
            WebBrowser::Chromium(b) => b.processes(),
            WebBrowser::Firefox(b) => b.processes(),
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
        }
    }

    pub fn gpu(&self) -> Result<Gpu> {
        match self {
            WebBrowser::Chromium(b) => b.gpu(),
            WebBrowser::Firefox(b) => b.gpu(),
        }
    }

    pub fn close(self) {
        match self {
            WebBrowser::Chromium(b) => b.close(),
            WebBrowser::Firefox(b) => b.close(),
        }
    }
}

impl WebPage {
    pub fn engine(&self) -> WebEngine {
        match self {
            WebPage::Chromium(_) => WebEngine::Chromium,
            WebPage::Firefox(_) => WebEngine::Firefox,
        }
    }

    /// The Chromium side, for CDP-only work.
    pub fn cdp(&self) -> Option<&chromium::Page> {
        match self {
            WebPage::Chromium(p) => Some(p),
            WebPage::Firefox(_) => None,
        }
    }

    pub fn firefox(&self) -> Option<&firefox::Page> {
        match self {
            WebPage::Chromium(_) => None,
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
        }
    }

    /// Navigates and waits for the load event.
    pub fn navigate(&self, url: &str, timeout: Duration) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.navigate(url, timeout),
            WebPage::Firefox(p) => p.navigate(url, timeout),
        }
    }

    /// A script every document runs before its own.
    pub fn add_init_script(&self, source: &str) -> Result<()> {
        match self {
            WebPage::Chromium(p) => p.send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": source })).map(|_| ()),
            WebPage::Firefox(p) => p.add_init_script(source),
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
        }
    }
}
