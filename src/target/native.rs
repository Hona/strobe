//! Native apps (SPEC §13.7): an executable with no browser engine, launched in an isolated home
//! (`isolate.rs`) and measured from outside: its process tree, threads, GPU and top-level window.

use super::chromium::Window;
use super::isolate::{self, App, Guard, Home};
use crate::host::{self, TopWindow};
use crate::model::{Error, Result};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub struct Native {
    pub child: Child,
    pub pid: u32,
    pub home: PathBuf,
    pub exe: PathBuf,
    /// When it was spawned: Unix epoch ms and the host clock (ns).
    pub epoch_ms: f64,
    pub at_ns: u64,
    pub output: Option<Receiver<(f64, String)>>,
    /// Its main window once found.
    pub window: Option<TopWindow>,
}

/// Starts `app` in `home` with `env` added (such as the bridge's address); `output` keeps its stdout
/// and stderr lines for milestones.
pub fn launch(app: &App, home: &Home, env: &[(String, String)], output: bool, cpus: Option<u64>) -> Result<(Native, Guard)> {
    let guard = Guard::take(app);
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
                return Err(Error::unreachable(format!("{} opened no visible window in {} s", self.exe.display(), timeout.as_secs())));
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
            None => Some("its window closed"),
            Some(s) if !s.visible => Some("its window is hidden"),
            Some(s) if s.iconic => Some("its window is minimized"),
            Some(s) if s.cloaked => Some("its window is cloaked (another virtual desktop, or hidden by the compositor)"),
            _ => None,
        };
        match why {
            Some(why) => Err(Error::refused("WindowHidden", format!("the app's {why}"), "keep the window visible and unminimized while strobe measures; a hidden window may skip its frames")),
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
