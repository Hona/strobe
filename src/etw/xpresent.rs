//! What reached the screen of an X11 app, from outside (SPEC §7.2): the X Present extension's
//! completion events. Any client may select them on any window, so strobe asks for them on the app's
//! windows and hears each `PresentPixmap` the app's GPU driver completes, with its mode and its time
//! (`ust`, CLOCK_MONOTONIC microseconds: the host clock).
//!
//! When that is a screen time:
//!
//! - `Flip`: the app's buffer was scanned out from that vertical blank: displayed then.
//! - `Copy` with no compositing manager: the server copied the frame into the scanned-out screen at
//!   that vertical blank: displayed then.
//! - `Copy` under a compositing manager: the frame reached the app's window, and the compositor shows
//!   it in a later frame of its own. Not a screen time: strobe refuses such a display.
//! - `Skip`: replaced before it was shown (discarded).
//!
//! What it cannot see: when the app handed the frame over (no begin), and frames drawn without
//! Present (core drawing, MIT-SHM images, drivers that do not use Present). A virtual server (Xvfb)
//! has no display: its Present times come from a timer, and strobe refuses it.

use super::{Live, Present, Presents};
use crate::model::{Error, Result};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::present::{CompleteKind, CompleteMode, ConnectionExt as _, EventMask};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

#[derive(Default)]
struct Shared {
    presents: Vec<Present>,
    events: u64,
    /// The process each watched window belongs to.
    owners: HashMap<Window, u32>,
    consumer_ns: u64,
}

pub struct Capture {
    conn: Arc<RustConnection>,
    shared: Arc<Mutex<Shared>>,
    pub live: Arc<Live>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pids: Vec<u32>,
    tracked: Option<Instant>,
    t0: u64,
}

impl Capture {
    pub fn start() -> Result<Capture> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(Error::msg(if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                "a Wayland session gives no client a view of another's frames (presentation feedback goes to the app itself), and this app has no X11 window"
            } else {
                "no X11 display (DISPLAY is unset)"
            }));
        }
        let (conn, screen) = x11rb::connect(None).map_err(|e| Error::msg(format!("X11: {e}")))?;
        if conn.extension_information(x11rb::protocol::present::X11_EXTENSION_NAME).ok().flatten().is_none() {
            return Err(Error::msg("the X server has no Present extension: strobe reads frames from its completion events"));
        }
        // A real display's server does direct rendering (DRI3); a virtual one (Xvfb) presents on a timer.
        let dri3 = conn.query_extension(b"DRI3").ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.present);
        let real = crate::host::linux::outputs().iter().any(|o| !o.is_virtual());
        if !dri3 || !real {
            return Err(Error::msg(
                "the X server shows on no real display (no DRI3, or no display output that is not virtual: Xvfb and VM consoles time Present on a timer), so its frame times are not screen times",
            ));
        }
        let cm = conn.intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
        if let Some(owner) = cm.and_then(|a| conn.get_selection_owner(a).ok()?.reply().ok()).map(|r| r.owner).filter(|&o| o != 0) {
            return Err(Error::msg(format!(
                "a compositing manager (window {owner:#x}) draws the app's frames into its own: strobe would see when a frame reached the app's window, not the screen. Run without a compositor (or with it off) for frame metrics"
            )));
        }
        let conn = Arc::new(conn);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let live = Arc::new(Live::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (c, s, l, st) = (conn.clone(), shared.clone(), live.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("strobe-present".into()).spawn(move || consume(&c, &s, &l, &st)).map_err(|e| Error::msg(format!("start the present thread: {e}")))?;
        Ok(Capture { conn, shared, live, stop, thread: Some(thread), pids: Vec::new(), tracked: None, t0: crate::host::now_ns() })
    }

    /// Watches the windows of `pids` (and their child windows, where a GPU driver may present); read
    /// again at most every 500 ms, so windows the app opens later are watched too.
    pub fn track(&mut self, pids: &[u32]) {
        if self.pids == pids && self.tracked.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        self.pids = pids.to_vec();
        self.tracked = Some(Instant::now());
        let tops = crate::host::top_windows(pids);
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        for (top, _, _) in tops {
            let Some(pid) = crate::host::window_state(top).map(|w| w.pid) else { continue };
            let mut stack = vec![top as Window];
            while let Some(w) = stack.pop() {
                if sh.owners.len() > 4096 {
                    break;
                }
                if let Some(t) = self.conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                    stack.extend(t.children);
                }
                if sh.owners.contains_key(&w) {
                    continue;
                }
                let Ok(eid) = self.conn.generate_id() else { continue };
                if self.conn.present_select_input(eid, w, EventMask::COMPLETE_NOTIFY).is_ok() {
                    sh.owners.insert(w, pid);
                }
            }
        }
        let _ = self.conn.flush();
    }

    pub fn stop(mut self) -> Result<Presents> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let sh = std::mem::take(&mut *self.shared.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(Presents {
            presents: sh.presents,
            vsyncs: Vec::new(),
            events: sh.events,
            events_lost: 0,
            consumer_cpu_ms: Some(sh.consumer_ns as f64 / 1e6),
            t0_ns: self.t0,
            t1_ns: crate::host::now_ns(),
            no_begin: true,
            only: Some("the X Present extension".into()),
        })
    }
}

fn consume(conn: &RustConnection, shared: &Mutex<Shared>, live: &Live, stop: &AtomicBool) {
    let cpu0 = crate::host::thread_cycles();
    let fd = conn.stream().as_raw_fd();
    while !stop.load(Ordering::Relaxed) {
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut p, 1, 100) };
        while let Ok(Some(ev)) = conn.poll_for_event() {
            let Event::PresentCompleteNotify(c) = ev else { continue };
            if c.kind != CompleteKind::PIXMAP {
                continue;
            }
            let at = c.ust * 1000;
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.events += 1;
            let pid = sh.owners.get(&c.window).copied().unwrap_or(0);
            let (mode, shown) = match c.mode {
                CompleteMode::FLIP => ("X_Present_Flip", true),
                CompleteMode::COPY => ("X_Present_Copy", true),
                CompleteMode::SUBOPTIMAL_COPY => ("X_Present_Suboptimal_Copy", true),
                _ => ("X_Present_Skip", false),
            };
            sh.presents.push(Present {
                pid,
                tid: 0,
                start_ns: at,
                stop_ns: 0,
                ready_ns: 0,
                screen_ns: if shown { at } else { 0 },
                swapchain: c.window as u64,
                sync_interval: 1,
                mode: mode.into(),
                state: if shown { "displayed" } else { "discarded" }.into(),
            });
            drop(sh);
            live.push(pid, at, if shown { at } else { 0 });
        }
    }
    shared.lock().unwrap_or_else(|e| e.into_inner()).consumer_ns = crate::host::thread_cycles().saturating_sub(cpu0);
}
