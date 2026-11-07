//! Reduced motion (SPEC 13.1 "Reduced motion"): a target's `motion: "reduce"` asks its page or app to
//! prefer reduced motion in strobe's launch alone, never through the OS's own setting, which is every
//! app's and the person's (rule 10). Chromium (pages, Electron, WebView2) takes a switch at launch, an
//! attached one DevTools' media emulation; Firefox a profile preference; WebKitGTK a GTK setting in its
//! isolated config folder. Each page is read with `matchMedia` before anything is measured. A native app
//! takes what its app profile declares (`apps.<id>.motion.reduce`), which strobe cannot read back.

use crate::model::{Error, Refusal, Result, ValidatorId, WebEngine};
use serde_json::{Value, json};
use std::path::Path;

/// What a target asks of the motion its app shows: the OS's own setting as the person left it, or
/// reduced (`motion: "reduce"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Motion {
    #[default]
    System,
    Reduce,
}

impl Motion {
    /// A target's `motion`: absent, or `"reduce"`.
    pub fn of(target: &Value, name: &str) -> Result<Motion> {
        match target["motion"].as_str() {
            None if target["motion"].is_null() => Ok(Motion::System),
            Some("reduce") => Ok(Motion::Reduce),
            _ => Err(Error::usage(format!("target {name}: motion is \"reduce\", not {} (without it the app follows the OS's own setting)", target["motion"]))),
        }
    }

    pub fn reduced(self) -> bool {
        self == Motion::Reduce
    }

    /// Chromium's launch switches for it, after strobe's own (`chromium::launch_as`'s `extra`).
    pub fn chromium_args(self) -> Vec<String> {
        if self.reduced() { vec![SWITCH.to_string()] } else { Vec::new() }
    }
}

/// Chromium's switch (`switches::kForcePrefersReducedMotion`): the browser process answers every page's
/// `prefers-reduced-motion` as `reduce`, whatever the OS says. Electron and WebView2 keep it too.
pub const SWITCH: &str = "--force-prefers-reduced-motion";

/// What a series' labels and reports say of it.
pub const LABEL: &str = "reduced motion";

/// Firefox's LookAndFeel override for the media query (1: reduce), set in the profile's `user.js`.
pub const FIREFOX_PREF: (&str, &str) = ("ui.prefersReducedMotion", "1");

/// `EngineUnsupported` for reduced motion in a WebKit that has no setting of its own for it, known
/// before anything builds: Safari (only the OS's Reduce motion, which strobe never changes) and WPE's
/// headless view (no GTK settings).
pub fn engine_refusal(engine: WebEngine, headless: bool) -> Option<Refusal> {
    if engine != WebEngine::Webkit {
        return None;
    }
    if cfg!(target_os = "macos") {
        return Some(
            Refusal::new(ValidatorId::EngineUnsupported, "reduced motion in Safari: Safari follows only the OS's Reduce motion, which is every app's setting and strobe never changes it")
                .fix("measure reduced motion in Chromium or Firefox, or drop the target's motion"),
        );
    }
    let wpe = headless && super::webkit::find(true).is_some_and(|i| i.flavor == super::webkit::Flavor::Wpe);
    wpe.then(|| {
        Refusal::new(ValidatorId::EngineUnsupported, "reduced motion in WPE WebKit: its headless view has no GTK settings to ask for it")
            .fix("run WebKitGTK on a virtual display (xvfb-run -a strobe …), or measure reduced motion in Chromium or Firefox")
    })
}

/// The page matched `(prefers-reduced-motion: reduce)` (`seen`, read with its input devices:
/// `validity::InputSeen`), as strobe asked for (`how`), or the block is refused `EngineUnsupported`: a
/// page that animates as usual is not the reduced-motion page the target declared. Read before setup
/// (and before a launch's timed load where the page exists before it).
pub fn require(seen: bool, how: &str) -> Result<()> {
    if seen {
        return Ok(());
    }
    Err(Refusal::new(ValidatorId::EngineUnsupported, format!("the page does not match (prefers-reduced-motion: reduce) after strobe asked for it ({how})"))
        .fix("measure reduced motion where it takes (Chromium or Firefox pages, WebKitGTK on a display without a settings daemon, an app that keeps Chromium's switches), or drop the target's motion")
        .into())
}

/// How a page of `engine` was asked, for `check`'s refusal.
pub fn how(engine: WebEngine) -> &'static str {
    match engine {
        WebEngine::Chromium => "Chromium's --force-prefers-reduced-motion",
        WebEngine::Firefox => "Firefox's ui.prefersReducedMotion in its profile",
        WebEngine::Webkit => "gtk-enable-animations=0 in WebKitGTK's isolated GTK settings",
    }
}

/// An attached Chromium page (a `command` target's): DevTools' media emulation, for this page and
/// session; strobe launched nothing it could give a switch.
pub fn emulate(page: &super::chromium::Page) -> Result<()> {
    page.send("Emulation.setEmulatedMedia", json!({ "features": [{ "name": "prefers-reduced-motion", "value": "reduce" }] })).map(|_| ())
}

/// GTK's settings in a WebKitGTK launch's isolated config folder (`XDG_CONFIG_HOME`), read by GTK 3 and
/// 4 where no settings daemon answers: animations off, which WebKitGTK answers `prefers-reduced-motion`
/// from. GSettings stays in memory (`GSETTINGS_BACKEND`), so nothing reaches the person's.
pub fn gtk_settings(config: &Path) -> Result<()> {
    for gtk in ["gtk-3.0", "gtk-4.0"] {
        let dir = config.join(gtk);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("settings.ini"), "[Settings]\ngtk-enable-animations=0\n")?;
    }
    Ok(())
}
