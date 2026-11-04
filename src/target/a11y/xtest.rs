//! Input to a native app on X11 through XTest (SPEC §13.7): the X server makes each input a device
//! event of its own, so it takes the real path: the server hit-tests the pointer, toolkits get the
//! same core and XInput 2 events a mouse and keyboard make, and keys reach the window with the input
//! focus. Each input is stamped on the host clock just before its request is written.
//!
//! What that costs, and what strobe does about it:
//!
//! - The real cursor moves to each point (there is no other way to hover on X11), and is put back
//!   when strobe is done.
//! - Keys go to the focused window: before keys strobe makes the app's window the active one
//!   (`_NET_ACTIVE_WINDOW`, else `SetInputFocus` without a window manager) and refuses when it does
//!   not take the focus, rather than type into another app.
//! - A click lands on whatever is at the point: strobe checks the window stacking first and refuses
//!   a point another app's window covers (`WindowHidden`).
//! - The wheel moves in clicks (XTest's device has no smooth scrolling): one click per 100 px, the
//!   steps of a `scroll` adding up into clicks.
//!
//! Native Wayland windows take no XTest input (and strobe sees no Wayland window at all); an XWayland
//! app does.

pub use super::keys::{Button, Key, Mods, Named, parse_chord};
use crate::model::{Error, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, InputFocus, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION: u8 = 6;

/// Where a pointer input lands: the app's window at that root point.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub win: isize,
    pub x: i32,
    pub y: i32,
}

pub struct Poster {
    conn: RustConnection,
    root: Window,
    /// keysym → (keycode, needs Shift, needs AltGr), from the server's keyboard mapping.
    keys: Vec<(u32, u8, bool, bool)>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Keys held across calls (`page.down`), as keycodes.
    held: Vec<u8>,
    /// Modifiers a click pressed, released with its button.
    click_mods: Vec<u8>,
    /// The cursor where the user left it.
    cursor0: Option<(i16, i16)>,
    /// Wheel movement not yet sent as a click, in clicks, and when the wheel last turned.
    wheel: (f64, f64),
    wheel_at: Option<Instant>,
}

fn x_err(e: impl std::fmt::Display) -> Error {
    Error::msg(format!("X11 input: {e}"))
}

/// The keysym a named key makes.
fn named_sym(n: Named) -> u32 {
    match n {
        Named::Enter => 0xff0d,
        Named::Tab => 0xff09,
        Named::Escape => 0xff1b,
        Named::Backspace => 0xff08,
        Named::Delete => 0xffff,
        Named::Insert => 0xff63,
        Named::Home => 0xff50,
        Named::End => 0xff57,
        Named::PageUp => 0xff55,
        Named::PageDown => 0xff56,
        Named::ArrowUp => 0xff52,
        Named::ArrowDown => 0xff54,
        Named::ArrowLeft => 0xff51,
        Named::ArrowRight => 0xff53,
        Named::Space => 0x20,
        Named::Shift => 0xffe1,
        Named::Control => 0xffe3,
        Named::Alt => 0xffe9,
        Named::Meta => 0xffeb,
        Named::ContextMenu => 0xff67,
        Named::CapsLock => 0xffe5,
        Named::F(n) => 0xffbe + n as u32 - 1,
        Named::Letter(c) => c.to_ascii_lowercase() as u32,
        Named::Digit(c) => c as u32,
    }
}

/// The keysym a character is (Latin-1 directly, the rest as Unicode keysyms).
fn char_sym(c: char) -> u32 {
    match c as u32 {
        0x20..=0x7e | 0xa0..=0xff => c as u32,
        u => 0x0100_0000 | u,
    }
}

impl Poster {
    pub fn start() -> Result<Poster> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(Error::usage("no X11 display (DISPLAY is unset): strobe sends input through XTest, which native Wayland windows do not take"));
        }
        let (conn, screen) = x11rb::connect(None).map_err(x_err)?;
        let root = conn.setup().roots.get(screen).ok_or_else(|| x_err("no screen"))?.root;
        conn.xtest_get_version(2, 2).map_err(x_err)?.reply().map_err(|_| Error::usage("the X server has no XTEST extension: strobe sends input through it"))?;
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let map = conn.get_keyboard_mapping(min, max - min + 1).map_err(x_err)?.reply().map_err(x_err)?;
        let per = map.keysyms_per_keycode.max(1) as usize;
        let mut keys = Vec::new();
        for (k, syms) in map.keysyms.chunks(per).enumerate() {
            let code = min + k as u8;
            // Columns: plain, Shift, then (group 2 or AltGr) plain and Shift.
            for (col, &sym) in syms.iter().enumerate().take(6) {
                if sym != 0 {
                    keys.push((sym, code, col % 2 == 1, col >= 2));
                }
            }
        }
        Ok(Poster { conn, root, keys, state: Mutex::new(State::default()) })
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The keycode of a keysym, preferring one without Shift or AltGr.
    fn code(&self, sym: u32) -> Option<(u8, bool, bool)> {
        self.keys.iter().filter(|k| k.0 == sym).min_by_key(|k| (k.3, k.2)).map(|k| (k.1, k.2, k.3))
    }

    fn modifier_code(&self, n: Named) -> Result<u8> {
        let alts: &[u32] = match n {
            Named::Shift => &[0xffe1, 0xffe2],
            Named::Control => &[0xffe3, 0xffe4],
            Named::Alt => &[0xffe9, 0xffea, 0xff7e],
            _ => &[0xffeb, 0xffec, 0xffe7],
        };
        alts.iter().find_map(|&s| self.code(s)).map(|c| c.0).ok_or_else(|| Error::usage(format!("the X server's keyboard has no {n:?} key")))
    }

    /// The keycode and the modifiers a key needs on the server's keyboard mapping (a capital letter
    /// is Shift and its key; AltGr characters add ISO_Level3_Shift).
    fn resolve(&self, key: Key, mods: Mods) -> Result<(u8, Mods, bool)> {
        match key {
            Key::Named(n) => {
                let (code, _, _) = self.code(named_sym(n)).ok_or_else(|| Error::usage(format!("the X server's keyboard has no key for {n:?}")))?;
                Ok((code, mods, false))
            }
            Key::Char(c) => {
                let found = self.code(char_sym(c)).or_else(|| {
                    // A core mapping may list only the lower case of a letter.
                    let lower = c.to_lowercase().next().filter(|l| *l != c)?;
                    self.code(char_sym(lower)).map(|(k, _, g)| (k, true, g))
                });
                let (code, shift, altgr) = found.ok_or_else(|| Error::usage(format!("character {c:?} has no key on the X server's keyboard mapping: strobe types keys, as a person does (an input method's text is not built)")))?;
                Ok((code, mods.or(Mods { shift, ..Mods::default() }), altgr))
            }
        }
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(kind, detail, 0, self.root, x, y, 0).map_err(x_err)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        self.conn.flush().map_err(x_err)
    }

    /// The window of `pids` at root point (x, y), or why a click there would reach something else.
    pub fn target(&self, pids: &[u32], x: f64, y: f64) -> Result<Target> {
        let (x, y) = (x.round() as i32, y.round() as i32);
        let (win, pid, title) = crate::host::x11::window_at(x, y).ok_or_else(|| Error::msg(format!("no window at ({x}, {y})")))?;
        if !pids.contains(&pid) {
            return Err(Error::refused("WindowHidden", format!("another window covers the point ({x}, {y}): {title:?} of process {pid}"), "keep the app's window uncovered while strobe drives it"));
        }
        Ok(Target { win, x, y })
    }

    /// A point in `top`'s client area, in device-independent pixels, as a root point in pixels.
    pub fn client_point(&self, top: isize, x: f64, y: f64) -> Result<(f64, f64)> {
        let (ox, oy) = crate::host::x11::client_origin(top).ok_or_else(|| Error::msg("the app's window closed"))?;
        let scale = crate::host::window_state(top).map_or(1.0, |w| w.scale());
        Ok((ox as f64 + x * scale, oy as f64 + y * scale))
    }

    /// On X11 the pointer's move is the real cursor's: this only remembers where the user left it.
    pub fn park(&self, _x: f64, _y: f64) -> Result<bool> {
        let mut st = self.st();
        if st.cursor0.is_none()
            && let Ok(p) = self.conn.query_pointer(self.root).map_err(x_err).and_then(|c| c.reply().map_err(x_err))
        {
            st.cursor0 = Some((p.root_x, p.root_y));
        }
        Ok(true)
    }

    pub fn move_to(&self, t: Target, _buttons: usize) -> Result<u64> {
        let at = crate::host::now_ns();
        self.fake(MOTION, 0, t.x as i16, t.y as i16)?;
        self.flush()?;
        Ok(at)
    }

    /// A button edge at `t`, after a move there. Modifiers go down before the press and up after the
    /// release. Returns when the button was sent.
    pub fn button(&self, t: Target, b: Button, down: bool, _dbl: bool, mods: Mods, _top: isize) -> Result<u64> {
        let detail = match b {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        };
        let mut st = self.st();
        self.fake(MOTION, 0, t.x as i16, t.y as i16)?;
        if down {
            for n in mods.keys() {
                let c = self.modifier_code(n)?;
                if !st.held.contains(&c) {
                    self.fake(KEY_PRESS, c, 0, 0)?;
                    st.click_mods.push(c);
                }
            }
        }
        let at = crate::host::now_ns();
        self.fake(if down { BUTTON_PRESS } else { BUTTON_RELEASE }, detail, 0, 0)?;
        if !down {
            for c in std::mem::take(&mut st.click_mods).into_iter().rev() {
                self.fake(KEY_RELEASE, c, 0, 0)?;
            }
        }
        self.flush()?;
        Ok(at)
    }

    /// The wheel clicks for `dx`, `dy` pixels at `t` (+ is right and down), with what earlier steps
    /// left over. Returns when the first click was sent, or 0 when the step adds up to no click yet.
    pub fn wheel(&self, t: Target, dx: f64, dy: f64) -> Result<u64> {
        let mut st = self.st();
        // What a gesture left over does not carry into the next one.
        if st.wheel_at.is_none_or(|w| w.elapsed() > Duration::from_millis(200)) {
            st.wheel = (0.0, 0.0);
        }
        st.wheel_at = Some(Instant::now());
        st.wheel.0 += super::keys::notches(dx);
        st.wheel.1 += super::keys::notches(dy);
        let mut at = 0;
        self.fake(MOTION, 0, t.x as i16, t.y as i16)?;
        for (axis, (neg, pos)) in [(1usize, (4u8, 5u8)), (0, (6, 7))] {
            let v = if axis == 1 { st.wheel.1 } else { st.wheel.0 };
            let n = v.round();
            for _ in 0..n.abs() as i64 {
                let b = if n < 0.0 { neg } else { pos };
                if at == 0 {
                    at = crate::host::now_ns();
                }
                self.fake(BUTTON_PRESS, b, 0, 0)?;
                self.fake(BUTTON_RELEASE, b, 0, 0)?;
            }
            if axis == 1 {
                st.wheel.1 -= n;
            } else {
                st.wheel.0 -= n;
            }
        }
        self.flush()?;
        Ok(at)
    }

    /// The top-level window of `pids` (by `_NET_WM_PID` up the window tree) that holds the input focus.
    fn focused(&self, top: isize) -> bool {
        let Ok(f) = self.conn.get_input_focus().map_err(x_err).and_then(|c| c.reply().map_err(x_err)) else { return false };
        let mut w = f.focus;
        for _ in 0..32 {
            if w == top as Window {
                return true;
            }
            if w == 0 || w == 1 || w == self.root {
                return false;
            }
            match self.conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                Some(t) => w = t.parent,
                None => return false,
            }
        }
        false
    }

    /// Makes `top` the window keys go to: the window manager's activation when it has one, else
    /// the input focus directly.
    fn activate(&self, top: isize) -> Result<()> {
        if self.focused(top) {
            return Ok(());
        }
        let active = self.atom("_NET_ACTIVE_WINDOW");
        let supported = self.prop32(self.root, "_NET_SUPPORTED");
        match active {
            Some(a) if supported.contains(&a) => {
                // Source 2: a pager acting for the user, which window managers honour without focus stealing rules.
                let ev = ClientMessageEvent::new(32, top as Window, a, [2, 0, 0, 0, 0]);
                self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev).map_err(x_err)?;
            }
            _ => {
                self.conn.set_input_focus(InputFocus::PARENT, top as Window, 0u32).map_err(x_err)?;
            }
        }
        self.flush()?;
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            if self.focused(top) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(Error::refused(
            "WindowHidden",
            "keys go to the window with the X input focus, and the app's window did not take it (the window manager kept another window focused)",
            "let the app's window be focused while strobe types into it",
        ))
    }

    fn atom(&self, name: &str) -> Option<u32> {
        Some(self.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom)
    }

    fn prop32(&self, win: Window, name: &str) -> Vec<u32> {
        let Some(a) = self.atom(name) else { return Vec::new() };
        self.conn.get_property(false, win, a, AtomEnum::ANY, 0, 4096).ok().and_then(|c| c.reply().ok()).and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default()
    }

    /// Presses and releases a key with modifiers in `top` (made the focused window first). Returns
    /// when the key went down.
    pub fn press(&self, top: isize, _to: Option<isize>, mods: Mods, key: Key) -> Result<u64> {
        self.activate(top)?;
        let (code, mods, altgr) = self.resolve(key, mods)?;
        let st = self.st();
        let mut downs: Vec<u8> = Vec::new();
        for n in mods.keys() {
            let c = self.modifier_code(n)?;
            if !st.held.contains(&c) && !downs.contains(&c) {
                downs.push(c);
            }
        }
        if altgr {
            let c = self.code(0xfe03).or_else(|| self.code(0xff7e)).map(|c| c.0).ok_or_else(|| Error::usage("the X server's keyboard has no AltGr key"))?;
            downs.push(c);
        }
        for &c in &downs {
            self.fake(KEY_PRESS, c, 0, 0)?;
        }
        let at = crate::host::now_ns();
        self.fake(KEY_PRESS, code, 0, 0)?;
        self.fake(KEY_RELEASE, code, 0, 0)?;
        for &c in downs.iter().rev() {
            self.fake(KEY_RELEASE, c, 0, 0)?;
        }
        self.flush()?;
        Ok(at)
    }

    /// Holds a key across calls (`page.down`) until `release`.
    pub fn hold(&self, top: isize, key: Key) -> Result<u64> {
        self.activate(top)?;
        let (code, _, _) = self.resolve(key, Mods::default())?;
        let mut st = self.st();
        if st.held.contains(&code) {
            return Ok(crate::host::now_ns());
        }
        let at = crate::host::now_ns();
        self.fake(KEY_PRESS, code, 0, 0)?;
        self.flush()?;
        st.held.push(code);
        Ok(at)
    }

    pub fn release(&self, _top: isize, key: Key) -> Result<u64> {
        let (code, _, _) = self.resolve(key, Mods::default())?;
        let mut st = self.st();
        let Some(i) = st.held.iter().position(|k| *k == code) else { return Ok(crate::host::now_ns()) };
        st.held.remove(i);
        let at = crate::host::now_ns();
        self.fake(KEY_RELEASE, code, 0, 0)?;
        self.flush()?;
        Ok(at)
    }

    /// Releases every held key (a block's end) and puts the cursor back.
    pub fn release_all(&self, _top: isize) {
        let mut st = self.st();
        for c in std::mem::take(&mut st.held).into_iter().rev().chain(std::mem::take(&mut st.click_mods)) {
            let _ = self.fake(KEY_RELEASE, c, 0, 0);
        }
        if let Some((x, y)) = st.cursor0.take() {
            let _ = self.fake(MOTION, 0, x, y);
        }
        let _ = self.flush();
    }

    /// `show`, `hide`, `minimize`, `focus` or `close` for a top-level window, as the window manager
    /// does them (EWMH and ICCCM messages); returns when it was asked.
    pub fn window(&self, top: isize, action: &str) -> Result<u64> {
        let win = top as Window;
        let at = crate::host::now_ns();
        let to_root = |ev: ClientMessageEvent| -> Result<()> {
            self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev).map_err(x_err)?;
            Ok(())
        };
        match action {
            // The title bar's close: the window manager asks the app (WM_DELETE_WINDOW), its own close path.
            "close" => match (self.atom("_NET_CLOSE_WINDOW"), self.atom("WM_PROTOCOLS"), self.atom("WM_DELETE_WINDOW")) {
                (Some(c), _, _) if self.prop32(self.root, "_NET_SUPPORTED").contains(&c) => to_root(ClientMessageEvent::new(32, win, c, [0, 2, 0, 0, 0]))?,
                (_, Some(p), Some(d)) if self.prop32(win, "WM_PROTOCOLS").contains(&d) => {
                    self.conn.send_event(false, win, EventMask::NO_EVENT, ClientMessageEvent::new(32, win, p, [d, 0, 0, 0, 0])).map_err(x_err)?;
                }
                _ => return Err(Error::msg("window(close): the window takes no close request (WM_DELETE_WINDOW)")),
            },
            "show" => {
                self.conn.map_window(win).map_err(x_err)?;
            }
            "hide" => {
                self.conn.unmap_window(win).map_err(x_err)?;
            }
            "minimize" => {
                let change = self.atom("WM_CHANGE_STATE").ok_or_else(|| x_err("WM_CHANGE_STATE"))?;
                // IconicState.
                to_root(ClientMessageEvent::new(32, win, change, [3, 0, 0, 0, 0]))?;
            }
            "focus" => self.activate(top)?,
            other => return Err(Error::usage(format!("window({other}): show, hide, minimize, focus or close"))),
        }
        self.flush()?;
        Ok(at)
    }
}

impl Drop for Poster {
    fn drop(&mut self) {
        self.release_all(0);
    }
}
