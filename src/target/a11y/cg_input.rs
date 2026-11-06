//! Input to a native app on macOS (SPEC §13.7): Quartz events, each stamped on the host clock just
//! before it is posted. What each kind takes, and what it does not:
//!
//! - Pointer: mouse events at the point, posted to the login session's event stream
//!   (`CGEventPost` at the session tap), so the window server routes them as a real click: AppKit
//!   drops mouse events posted to a process (`CGEventPostToPid`; macOS 27 handled none). strobe
//!   first checks that the app's window is what the window server hit-tests at the point
//!   (`WindowHidden` otherwise) and moves the real cursor there (`CGWarpMouseCursorPosition`: no
//!   event, no click) since hover tracking reads it; it is put back when strobe is done. A click on
//!   a background app's window brings it to the front, as a person's does.
//! - Keys: key events with the key code of the key's position on a US keyboard and the character
//!   itself set on the event, so the app's text is exact on any layout (an app that maps key codes
//!   through a layout of its own sees the US key). Modifiers go down and up as flag changes and mark
//!   the key's flags. macOS gives keys only to the active app's key window: strobe brings the app to
//!   the front first (`AXFrontmost`) and refuses when it does not come.
//! - Wheel: scroll events in pixels (continuous, as a trackpad's), one per step.

pub use super::keys::{Button, Key, Mods, Named, parse_chord};
use crate::host::macos::{CFTypeRef, CGPoint, Cf, cf_array, cf_get, cf_i64};
use crate::model::{Error, Result};
use std::sync::Mutex;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> CFTypeRef;
    fn CGEventCreate(source: CFTypeRef) -> CFTypeRef;
    fn CGEventGetLocation(e: CFTypeRef) -> CGPoint;
    fn CGEventCreateMouseEvent(source: CFTypeRef, kind: u32, at: CGPoint, button: u32) -> CFTypeRef;
    fn CGEventCreateKeyboardEvent(source: CFTypeRef, key: u16, down: bool) -> CFTypeRef;
    fn CGEventCreateScrollWheelEvent2(source: CFTypeRef, units: u32, count: u32, w1: i32, w2: i32, w3: i32) -> CFTypeRef;
    fn CGEventSetType(e: CFTypeRef, kind: u32);
    fn CGEventSetLocation(e: CFTypeRef, at: CGPoint);
    fn CGEventSetFlags(e: CFTypeRef, flags: u64);
    fn CGEventSetIntegerValueField(e: CFTypeRef, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(e: CFTypeRef, len: usize, chars: *const u16);
    fn CGEventPostToPid(pid: i32, e: CFTypeRef);
    fn CGEventPost(tap: u32, e: CFTypeRef);
    fn CGWarpMouseCursorPosition(at: CGPoint) -> i32;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> CFTypeRef;
}

/// `kCGWindowListOptionIncludingWindow`: just the window named.
const LIST_INCLUDING: u32 = 8;

const MOUSE_MOVED: u32 = 5;
const FLAGS_CHANGED: u32 = 12;
const CLICK_STATE: u32 = 1;
const BUTTON_NUMBER: u32 = 3;
const KEYCODE: u32 = 9;
const WINDOW_UNDER_POINTER: u32 = 91;
const WINDOW_UNDER_POINTER_THAT_CAN_HANDLE: u32 = 92;
/// `kCGSessionEventTap`: where events enter the login session, routed by the window server.
const SESSION_TAP: u32 = 1;
const SHIFT: u64 = 0x20000;
const CONTROL: u64 = 0x40000;
const OPTION: u64 = 0x80000;
const COMMAND: u64 = 0x100000;

/// Where a pointer input lands: the app's window at that point (points, global display space).
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub pid: u32,
    pub window: u32,
    pub x: f64,
    pub y: f64,
}

impl Target {
    /// The same window at another point, as a drag keeps posting to the window it pressed in.
    pub fn moved(self, x: f64, y: f64) -> Target {
        Target { x, y, ..self }
    }

    /// The window the point hit-tested to.
    pub fn top(&self) -> isize {
        self.window as isize
    }

    /// The point, in whole points.
    pub fn point(&self) -> (i64, i64) {
        (self.x.round() as i64, self.y.round() as i64)
    }
}

/// macOS tracks the held button itself (`move_to` sends drags while it is down).
pub fn held_flag(_b: Button) -> usize {
    0
}

pub struct Poster {
    source: Cf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Keys held across calls (`page.down`): key code and the flag it sets.
    held: Vec<(u16, u64)>,
    /// The process held keys went to.
    held_pid: u32,
    /// Buttons down, for the dragged events of moves between a press and its release.
    down: Option<Button>,
    cursor0: Option<CGPoint>,
}

/// The key code of a named key (Carbon's `kVK_*`, US positions).
fn named_code(n: Named) -> u16 {
    match n {
        Named::Enter => 36,
        Named::Tab => 48,
        Named::Escape => 53,
        Named::Backspace => 51,
        Named::Delete => 117,
        Named::Insert => 114,
        Named::Home => 115,
        Named::End => 119,
        Named::PageUp => 116,
        Named::PageDown => 121,
        Named::ArrowUp => 126,
        Named::ArrowDown => 125,
        Named::ArrowLeft => 123,
        Named::ArrowRight => 124,
        Named::Space => 49,
        Named::Shift => 56,
        Named::Control => 59,
        Named::Alt => 58,
        Named::Meta => 55,
        Named::ContextMenu => 110,
        Named::CapsLock => 57,
        Named::F(n) => [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90, 90, 90, 90, 90][(n as usize - 1).min(23)],
        Named::Letter(c) | Named::Digit(c) => ascii_code(c.to_ascii_lowercase()).map_or(0, |k| k.0),
    }
}

/// An ASCII character's key on a US keyboard and whether it needs Shift.
fn ascii_code(c: u8) -> Option<(u16, bool)> {
    const PLAIN: &[(u8, u16)] = &[
        (b'a', 0), (b's', 1), (b'd', 2), (b'f', 3), (b'h', 4), (b'g', 5), (b'z', 6), (b'x', 7), (b'c', 8), (b'v', 9), (b'b', 11), (b'q', 12), (b'w', 13), (b'e', 14), (b'r', 15),
        (b'y', 16), (b't', 17), (b'1', 18), (b'2', 19), (b'3', 20), (b'4', 21), (b'6', 22), (b'5', 23), (b'=', 24), (b'9', 25), (b'7', 26), (b'-', 27), (b'8', 28), (b'0', 29),
        (b']', 30), (b'o', 31), (b'u', 32), (b'[', 33), (b'i', 34), (b'p', 35), (b'l', 37), (b'j', 38), (b'\'', 39), (b'k', 40), (b';', 41), (b'\\', 42), (b',', 43), (b'/', 44),
        (b'n', 45), (b'm', 46), (b'.', 47), (b' ', 49), (b'`', 50), (b'\t', 48), (b'\n', 36),
    ];
    let find = |c: u8| PLAIN.iter().find(|p| p.0 == c).map(|p| p.1);
    match super::keys::us_key(c as char) {
        Some((base, shift)) => find(base as u8).map(|k| (k, shift)),
        None => find(c).map(|k| (k, false)),
    }
}

fn flag(n: Named) -> u64 {
    match n {
        Named::Shift => SHIFT,
        Named::Control => CONTROL,
        Named::Alt => OPTION,
        _ => COMMAND,
    }
}

fn flags(m: Mods) -> u64 {
    m.keys().into_iter().map(flag).fold(0, |a, b| a | b)
}

impl Poster {
    pub fn start() -> Result<Poster> {
        // Posting events to another process needs the same Accessibility permission; without it macOS drops them silently.
        if !unsafe { CGPreflightPostEventAccess() } {
            let app = crate::host::macos::responsible_app();
            let pane = crate::host::macos::accessibility_pane();
            return Err(Error::usage(format!(
                "macOS drops input strobe posts to another app without the accessibility permission, which {app} (the app strobe runs under) does not have: {pane}, turn on {app}"
            )));
        }
        // A private event state: the user's own held keys do not mark strobe's events.
        let source = Cf::new(unsafe { CGEventSourceCreate(-1) }).ok_or_else(|| Error::msg("Quartz: no event source"))?;
        Ok(Poster { source, state: Mutex::new(State::default()) })
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post(&self, pid: u32, e: Cf) -> u64 {
        crate::host::macos::note_posted();
        let at = crate::host::now_ns();
        unsafe { CGEventPostToPid(pid as i32, e.0) };
        at
    }

    /// A pointer event, into the session's event stream at its location.
    fn post_pointer(&self, e: Cf) -> u64 {
        crate::host::macos::note_posted();
        let at = crate::host::now_ns();
        unsafe { CGEventPost(SESSION_TAP, e.0) };
        at
    }

    fn mouse(&self, kind: u32, t: Target, button: u32, clicks: i64, flags: u64) -> Result<Cf> {
        let e = Cf::new(unsafe { CGEventCreateMouseEvent(self.source.0, kind, CGPoint { x: t.x, y: t.y }, button) }).ok_or_else(|| Error::msg("Quartz: no mouse event"))?;
        unsafe {
            CGEventSetIntegerValueField(e.0, WINDOW_UNDER_POINTER, t.window as i64);
            CGEventSetIntegerValueField(e.0, WINDOW_UNDER_POINTER_THAT_CAN_HANDLE, t.window as i64);
            if clicks > 0 {
                CGEventSetIntegerValueField(e.0, CLICK_STATE, clicks);
            }
            if button > 1 {
                CGEventSetIntegerValueField(e.0, BUTTON_NUMBER, button as i64);
            }
            CGEventSetFlags(e.0, flags);
        }
        Ok(e)
    }

    fn key(&self, code: u16, down: bool, flags: u64, text: Option<char>) -> Result<Cf> {
        let e = Cf::new(unsafe { CGEventCreateKeyboardEvent(self.source.0, code, down) }).ok_or_else(|| Error::msg("Quartz: no key event"))?;
        unsafe {
            CGEventSetFlags(e.0, flags);
            if let Some(c) = text {
                let mut units = [0u16; 2];
                let s = c.encode_utf16(&mut units);
                CGEventKeyboardSetUnicodeString(e.0, s.len(), s.as_ptr());
            }
        }
        Ok(e)
    }

    /// A modifier's own key going down or up: a flag change with the flags after it.
    fn modifier(&self, pid: u32, n: Named, now: u64) -> Result<u64> {
        let e = self.key(named_code(n), true, now, None)?;
        unsafe {
            CGEventSetType(e.0, FLAGS_CHANGED);
            CGEventSetIntegerValueField(e.0, KEYCODE, named_code(n) as i64);
        }
        Ok(self.post(pid, e))
    }

    /// The window of `pids` at (x, y) in points, or why a click there would reach something else:
    /// what the window server hit-tests there (which honours click-through overlays) must be the app's.
    /// An app whose accessibility implements no hit test there (Safari's web content, kept in another
    /// process, answered `kAXErrorNotImplemented` on air) is hit-tested by the window list instead: the
    /// frontmost window at the point, at any level, which must be the app's. Every other accessibility
    /// error stays one.
    pub fn target(&self, pids: &[u32], x: f64, y: f64) -> Result<Target> {
        let covered = |what: String, pid: u32| -> Error {
            Error::refused("WindowHidden", format!("another app's window covers the point ({x:.0}, {y:.0}): {what} of process {pid}"), "keep the app's window uncovered while strobe drives it")
        };
        let pid = match super::ax::app_at(x, y)? {
            Some((pid, at)) if !pids.contains(&pid) => return Err(covered(super::ax::describe(&at), pid)),
            Some((pid, _)) => pid,
            None => match crate::host::macos::front_window_at(x, y) {
                Some((pid, _, owner)) if !pids.contains(&pid) => return Err(covered(format!("a window of {owner}"), pid)),
                Some((pid, window, _)) => return Ok(Target { pid, window, x, y }),
                None => return Err(Error::msg(format!("no window at ({x:.0}, {y:.0}), and the app there answers no accessibility hit test"))),
            },
        };
        let window = crate::host::macos::window_at(&[pid], x, y).ok_or_else(|| Error::msg(format!("no window of the app at ({x:.0}, {y:.0})")))?;
        Ok(Target { pid, window, x, y })
    }

    /// A point in `top`'s frame, in points (CoreGraphics gives no separate content area).
    pub fn client_point(&self, top: isize, x: f64, y: f64) -> Result<(f64, f64)> {
        let w = crate::host::window_state(top).ok_or_else(|| Error::msg("the app's window closed"))?;
        Ok((w.rect.0 as f64 + x, w.rect.1 as f64 + y))
    }

    /// `top`'s frame (x, y, width, height), in points.
    pub fn frame(&self, top: isize) -> Result<(f64, f64, f64, f64)> {
        let r = crate::host::window_state(top).ok_or_else(|| Error::msg("the app's window closed"))?.rect;
        Ok((r.0 as f64, r.1 as f64, r.2 as f64, r.3 as f64))
    }

    /// Moves the real cursor onto the point without an event: true when it got there.
    pub fn park(&self, x: f64, y: f64) -> Result<bool> {
        let mut st = self.st();
        if st.cursor0.is_none()
            && let Some(e) = Cf::new(unsafe { CGEventCreate(std::ptr::null()) })
        {
            st.cursor0 = Some(unsafe { CGEventGetLocation(e.0) });
        }
        crate::host::macos::note_posted();
        Ok(unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) } == 0)
    }

    pub fn move_to(&self, t: Target, _buttons: usize) -> Result<u64> {
        let st = self.st();
        let (kind, button) = match st.down {
            Some(Button::Left) => (6, 0),
            Some(Button::Right) => (7, 1),
            Some(Button::Middle) => (27, 2),
            None => (MOUSE_MOVED, 0),
        };
        let e = self.mouse(kind, t, button, 0, flags(Mods::default()))?;
        Ok(self.post_pointer(e))
    }

    /// A button edge at `t`, after a move there; `dbl` is a double click's second press. Returns
    /// when the button was posted.
    pub fn button(&self, t: Target, b: Button, down: bool, dbl: bool, mods: Mods, _top: isize) -> Result<u64> {
        let (kinds, button) = match b {
            Button::Left => ((1, 2), 0),
            Button::Right => ((3, 4), 1),
            Button::Middle => ((25, 26), 2),
        };
        let held: u64 = self.st().held.iter().map(|h| h.1).fold(0, |a, b| a | b);
        let f = flags(mods) | held;
        let mv = self.mouse(if down { MOUSE_MOVED } else { [6, 7, 27][button as usize] }, t, button, 0, f)?;
        self.post_pointer(mv);
        let e = self.mouse(if down { kinds.0 } else { kinds.1 }, t, button, if dbl { 2 } else { 1 }, f)?;
        let at = self.post_pointer(e);
        self.st().down = down.then_some(b);
        Ok(at)
    }

    /// One wheel step of `dx`, `dy` pixels at `t` (+ is right and down). Returns when it was posted.
    pub fn wheel(&self, t: Target, dx: f64, dy: f64) -> Result<u64> {
        let e = Cf::new(unsafe { CGEventCreateScrollWheelEvent2(self.source.0, 0, 2, (-dy).round() as i32, (-dx).round() as i32, 0) }).ok_or_else(|| Error::msg("Quartz: no scroll event"))?;
        unsafe {
            CGEventSetLocation(e.0, CGPoint { x: t.x, y: t.y });
            CGEventSetIntegerValueField(e.0, WINDOW_UNDER_POINTER, t.window as i64);
            CGEventSetIntegerValueField(e.0, WINDOW_UNDER_POINTER_THAT_CAN_HANDLE, t.window as i64);
        }
        Ok(self.post_pointer(e))
    }

    /// The process that owns window `top`, from that window's entry alone (not the whole window list
    /// at every key).
    fn pid_of(top: isize) -> Result<u32> {
        let entry = Cf::new(unsafe { CGWindowListCopyWindowInfo(LIST_INCLUDING, top as u32) });
        entry
            .and_then(|l| cf_array(l.0).into_iter().find(|d| cf_i64(cf_get(*d, "kCGWindowNumber")) == Some(top as i64)).and_then(|d| cf_i64(cf_get(d, "kCGWindowOwnerPID"))))
            .map(|p| p as u32)
            .ok_or_else(|| Error::msg("the app's window closed"))
    }

    /// The key code, the modifiers it needs and the character it types. The character is set on the
    /// event, so a chord's Shift changes it as the keyboard would (`Shift+a` types `A`).
    fn resolve(key: Key, mods: Mods) -> Result<(u16, Mods, Option<char>)> {
        match key {
            Key::Named(n) => Ok((named_code(n), mods, None)),
            Key::Char(c) => match u8::try_from(c).ok().filter(u8::is_ascii).and_then(ascii_code) {
                Some((code, shift)) => Ok((code, mods.or(Mods { shift, ..Mods::default() }), Some(if mods.shift { super::keys::us_shifted(c) } else { c }))),
                // Any other character: the key event carries it as its text.
                None => Ok((0, mods, Some(c))),
            },
        }
    }

    /// Presses and releases a key with modifiers in the app (brought to the front first). Returns
    /// when the key went down.
    pub fn press(&self, top: isize, _to: Option<isize>, mods: Mods, key: Key) -> Result<u64> {
        let pid = Self::pid_of(top)?;
        super::ax::activate(pid)?;
        let (code, mods, text) = Self::resolve(key, mods)?;
        let held: u64 = self.st().held.iter().map(|h| h.1).fold(0, |a, b| a | b);
        let mut now = held;
        let fresh: Vec<Named> = mods.keys().into_iter().filter(|n| held & flag(*n) == 0).collect();
        for n in &fresh {
            now |= flag(*n);
            self.modifier(pid, *n, now)?;
        }
        let at = self.post(pid, self.key(code, true, now, text)?);
        self.post(pid, self.key(code, false, now, text)?);
        for n in fresh.iter().rev() {
            now &= !flag(*n);
            self.modifier(pid, *n, now)?;
        }
        Ok(at)
    }

    /// Holds a key across calls (`page.down`) until `release`.
    pub fn hold(&self, top: isize, key: Key) -> Result<u64> {
        let pid = Self::pid_of(top)?;
        super::ax::activate(pid)?;
        let (code, _, text) = Self::resolve(key, Mods::default())?;
        let mut st = self.st();
        if st.held.iter().any(|h| h.0 == code) {
            return Ok(crate::host::now_ns());
        }
        let f = match key {
            Key::Named(n) if n.modifier() => flag(n),
            _ => 0,
        };
        let now = st.held.iter().map(|h| h.1).fold(f, |a, b| a | b);
        st.held.push((code, f));
        st.held_pid = pid;
        drop(st);
        Ok(match key {
            Key::Named(n) if n.modifier() => self.modifier(pid, n, now)?,
            _ => self.post(pid, self.key(code, true, now, text)?),
        })
    }

    pub fn release(&self, top: isize, key: Key) -> Result<u64> {
        let pid = Self::pid_of(top)?;
        let (code, _, text) = Self::resolve(key, Mods::default())?;
        let mut st = self.st();
        let Some(i) = st.held.iter().position(|h| h.0 == code) else { return Ok(crate::host::now_ns()) };
        st.held.remove(i);
        let now = st.held.iter().map(|h| h.1).fold(0, |a, b| a | b);
        drop(st);
        Ok(match key {
            Key::Named(n) if n.modifier() => self.modifier(pid, n, now)?,
            _ => self.post(pid, self.key(code, false, now, text)?),
        })
    }

    /// Releases every held key (a block's end) and puts the cursor back.
    pub fn release_all(&self, _top: isize) {
        let mut st = self.st();
        let pid = st.held_pid;
        for (code, f) in std::mem::take(&mut st.held).into_iter().rev() {
            if let Ok(e) = self.key(code, f == 0, 0, None) {
                if f != 0 {
                    unsafe { CGEventSetType(e.0, FLAGS_CHANGED) };
                }
                self.post(pid, e);
            }
        }
        if let Some(p) = st.cursor0.take() {
            crate::host::macos::note_posted();
            unsafe { CGWarpMouseCursorPosition(p) };
        }
    }

    /// `show`, `hide`, `minimize`, `focus` or `close` for a window, through its accessibility element;
    /// returns when it was asked.
    pub fn window(&self, top: isize, action: &str) -> Result<u64> {
        use super::ax;
        let (win, pid) = ax::window_element(top).ok_or_else(|| Error::msg(format!("window({action}): the app's window has no accessibility element")))?;
        let at = crate::host::now_ns();
        match action {
            // The title bar's close button: the app's own close path.
            "close" => {
                let b = ax::child(&win, "AXCloseButton").ok_or_else(|| Error::msg("window(close): the window has no close button"))?;
                ax::perform(&b, "AXPress")?;
            }
            "minimize" => ax::set_bool(&win, "AXMinimized", true)?,
            "show" => {
                ax::set_bool(&win, "AXMinimized", false)?;
                ax::perform(&win, "AXRaise")?;
            }
            // macOS hides apps, not windows: Hide (⌘H) hides all of the app's windows.
            "hide" => {
                let app = Cf::new(unsafe { AXUIElementCreateApplication(pid as i32) }).ok_or_else(|| Error::msg("window(hide): no such app"))?;
                ax::set_bool(&app, "AXHidden", true)?;
            }
            "focus" => {
                ax::activate(pid)?;
                ax::set_bool(&win, "AXMain", true)?;
                ax::perform(&win, "AXRaise")?;
            }
            other => return Err(Error::usage(format!("window({other}): show, hide, minimize, focus or close"))),
        }
        Ok(at)
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
}

impl Drop for Poster {
    fn drop(&mut self) {
        self.release_all(0);
    }
}
