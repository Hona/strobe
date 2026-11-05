//! Native apps (SPEC §13.7): an executable with no browser engine, launched in an isolated home
//! (`isolate.rs`) and measured from outside: its process tree, threads, GPU and top-level window.

use super::chromium::Window;
use super::isolate::{self, App, Guard, Home};
use crate::host::{self, TopWindow};
use crate::model::{Error, Hidden, Refusal, Result};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Why a macOS window counts as hidden when other apps' windows cover it.
pub const COVERED: &str = "window is covered by other apps' windows (strobe brings it to the front only after a minute without keyboard or mouse input, and with the Accessibility permission)";

/// macOS: every block measures the app as the active one (`host::raise_windows`), so one that is
/// not is a different state, not a measurement.
pub const NOT_ACTIVE: &str = "app is not the active app (on macOS strobe measures every block with it active, and makes it so only after a minute without keyboard or mouse input, with the Accessibility permission)";

/// Whether the app owning a window is not the active one, where strobe measures it active (macOS).
fn inactive(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        !super::a11y::ax::is_front(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        false
    }
}

pub struct Native {
    pub child: Child,
    pub pid: u32,
    pub home: PathBuf,
    pub exe: PathBuf,
    /// When it was spawned: Unix epoch ms and the host clock (ns).
    pub epoch_ms: f64,
    pub at_ns: u64,
    pub output: Option<Receiver<crate::target::isolate::OutputLine>>,
    /// Its main window once found.
    pub window: Option<TopWindow>,
}

/// Starts `app` in `home` with `env` added; `output` keeps its stdout
/// and stderr lines for milestones.
pub fn launch(app: &App, home: &Home, env: &[(String, String)], output: bool, cpus: Option<u64>) -> Result<(Native, Guard)> {
    let mut guard = Guard::take(app);
    guard.shared = home.shared.clone();
    isolate::live(app, &guard)?;
    let s = isolate::spawn(app, home, &[], env, output, cpus)?;
    Ok((Native { child: s.child, pid: s.pid, home: home.dir.clone(), exe: app.exe.clone(), epoch_ms: s.epoch_ms, at_ns: s.at_ns, output: s.output, window: None }, guard))
}

impl Native {
    /// The app's processes, sorted: its tree, and whatever runs from its home.
    pub fn pids(&self) -> Vec<u32> {
        let mut p: Vec<u32> = self.processes().iter().map(|p| p.pid).collect();
        p.sort_unstable();
        p
    }

    pub fn processes(&self) -> Vec<host::ProcTimes> {
        host::process_set(self.pid, Some(&self.home))
    }

    /// The process's exit code, once it has exited.
    pub fn exited(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1))
    }

    /// Why no window may show on Linux: the app was given a Wayland display, where toolkits open a
    /// native Wayland window, which no other client sees.
    fn wayland_note(&self) -> &'static str {
        let wayland = cfg!(target_os = "linux")
            && self.pids().iter().any(|p| {
                std::fs::read(format!("/proc/{p}/environ")).is_ok_and(|e| e.split(|&b| b == 0).any(|kv| kv.strip_prefix(b"WAYLAND_DISPLAY=").is_some_and(|v| !v.is_empty())))
            });
        if wayland {
            ": the app has WAYLAND_DISPLAY, and a native Wayland window is invisible to strobe (Wayland shows no client another's windows, and takes no XTest input); run it under X11 or XWayland: unset WAYLAND_DISPLAY in the app profile (`unset`), or use the app's own switch to X11"
        } else {
            ""
        }
    }

    /// Waits for the app's main window: its largest visible top-level window.
    pub fn wait_window(&mut self, timeout: Duration, guard: &Guard) -> Result<TopWindow> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(w) = host::main_window(&self.pids()) {
                self.window = Some(w.clone());
                return Ok(w);
            }
            if let Some(code) = self.exited().filter(|_| host::process_tree(self.pid).is_empty()) {
                return Err(Error::msg(format!(
                    "{} exited (code {code}) before it opened a window: an app that hands its window to another process (a launcher, a single-instance app already running) cannot be measured this way",
                    self.exe.display()
                )));
            }
            guard.check("while the app started")?;
            if Instant::now() > deadline {
                return Err(Error::unreachable(format!("{} opened no visible window in {} s{}", self.exe.display(), timeout.as_secs(), self.wayland_note())));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Moves the main window to the block's place, keeps it on top without focus, and with `size`
    /// sizes its client area to the window's width and height in device-independent pixels where the
    /// app allows it. Returns the window as placed.
    pub fn place(&mut self, at: &Window, size: bool) -> Option<TopWindow> {
        let w = self.window.clone()?;
        let client = size.then_some((at.width, at.height));
        host::place_window(&w, at.x, at.y, client);
        let want = client.map(|(cw, ch)| ((cw as f64 * w.scale()).round() as i32, (ch as f64 * w.scale()).round() as i32));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut now = w;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            let Some(s) = host::window_state(now.hwnd) else { break };
            now = s;
            if now.rect.0 == at.x && now.rect.1 == at.y && want.is_none_or(|c| c == now.client) {
                break;
            }
        }
        self.window = Some(now.clone());
        Some(now)
    }

    /// Keeps the window on top without focus (a covered window may skip its frames).
    pub fn raise(&self) {
        host::raise_windows(self.pid);
    }

    /// `WindowHidden` (SPEC §10.1) unless the main window is still there, visible, unminimized and not
    /// cloaked by the compositor.
    pub fn check_visible(&mut self) -> Result<()> {
        let Some(w) = &self.window else { return Ok(()) };
        // An app may replace its main window (a splash, then the app): follow it to the new one.
        let state = host::window_state(w.hwnd).or_else(|| host::main_window(&self.pids()));
        if let Some(s) = &state {
            self.window = Some(s.clone());
        }
        let why = match &state {
            None => Some("window closed"),
            Some(s) if !s.visible => Some("window is hidden"),
            Some(s) if s.iconic && cfg!(target_os = "macos") => Some("window is off screen (minimized, or on another Space)"),
            Some(s) if s.iconic => Some("window is minimized"),
            Some(s) if s.cloaked && cfg!(target_os = "macos") => Some(COVERED),
            Some(s) if s.cloaked => Some("window is cloaked (another virtual desktop, or hidden by the compositor)"),
            Some(s) if inactive(s.pid) => Some(NOT_ACTIVE),
            _ => None,
        };
        // macOS: which apps' windows still cover it (after strobe brought the app forward, a window
        // that opens or floats on top), so the refusal says what to close.
        #[cfg(target_os = "macos")]
        let by = state.as_ref().filter(|_| why == Some(COVERED)).map(|s| host::macos::covering(s.hwnd)).filter(|c| !c.is_empty()).map(|c| format!("; in front of it: {}", c.join(", "))).unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        let by = String::new();
        let hidden = match why {
            Some(COVERED) => Hidden::Covered,
            Some(NOT_ACTIVE) => Hidden::NotActive,
            _ => Hidden::Other,
        };
        match why {
            Some(why) => Err(Refusal::window_hidden(hidden, format!("the app's {why}{by}")).fix("keep the window visible and unminimized while strobe measures; a hidden window may skip its frames").into()),
            None => Ok(()),
        }
    }

    /// Ends the app's tree and what runs from its home, restores its URL schemes, and with `remove`
    /// deletes the home (a warm one is kept).
    pub fn close(mut self, remove: bool) {
        isolate::kill_tree(self.pid);
        let _ = self.child.kill();
        let _ = self.child.wait();
        isolate::release_home(&self.home);
        if remove {
            isolate::remove_home(&self.home);
        }
    }
}
