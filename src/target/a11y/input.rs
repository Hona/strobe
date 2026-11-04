//! Input to a native app through its own message loop (SPEC §13.7): window messages posted to the
//! window under a point (the pointer) or to the UI thread's focus window (keys), never `SendInput`, so
//! the app never takes focus. Each input is stamped on the host clock just before it is posted.
//!
//! What posting alone cannot reach, and what strobe does about it:
//!
//! - Key state. A posted `WM_KEYDOWN` does not change the thread's key state, which toolkits read
//!   for modifiers (`GetKeyState`, `GetKeyboardState`) and `TranslateMessage` reads to make text.
//!   For a key with modifiers strobe attaches its input thread to the app's UI thread
//!   (`AttachThreadInput`, which shares the key state and nothing it changes here), sets the state
//!   (`SetKeyboardState`), posts, waits until the app's loop has taken the messages, and puts the
//!   state back. `GetAsyncKeyState` reads the physical keyboard and still sees no modifier.
//! - The cursor. Hover tracking compares with the OS cursor (`TrackMouseEvent` posts a leave at once
//!   when the cursor is elsewhere, `GetCursorPos`). strobe parks the OS cursor on the point it posts
//!   to (`SetCursorPos`; no click, no focus), and says when it could not (a locked session).
//! - Activation. A real click on an inactive window activates it (`WM_MOUSEACTIVATE`); a posted one
//!   does not, so the app stays in the background, as strobe needs.

pub use super::keys::{Button, Key, Mods, Named, parse_chord};
use crate::model::{Error, Result};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// The key and button flags of mouse messages' wParam.
const MK_LBUTTON: u32 = 0x01;
const MK_RBUTTON: u32 = 0x02;
const MK_SHIFT: u32 = 0x04;
const MK_CONTROL: u32 = 0x08;
const MK_MBUTTON: u32 = 0x10;

fn button_messages(b: Button) -> (u32, u32, u32, usize) {
    match b {
        Button::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, WM_LBUTTONDBLCLK, MK_LBUTTON as usize),
        Button::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_RBUTTONDBLCLK, MK_RBUTTON as usize),
        Button::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MBUTTONDBLCLK, MK_MBUTTON as usize),
    }
}

fn vks(m: Mods) -> Vec<u16> {
    m.keys().into_iter().map(named_vk).collect()
}

/// The virtual key of a named key.
fn named_vk(n: Named) -> u16 {
    match n {
        Named::Enter => VK_RETURN,
        Named::Tab => VK_TAB,
        Named::Escape => VK_ESCAPE,
        Named::Backspace => VK_BACK,
        Named::Delete => VK_DELETE,
        Named::Insert => VK_INSERT,
        Named::Home => VK_HOME,
        Named::End => VK_END,
        Named::PageUp => VK_PRIOR,
        Named::PageDown => VK_NEXT,
        Named::ArrowUp => VK_UP,
        Named::ArrowDown => VK_DOWN,
        Named::ArrowLeft => VK_LEFT,
        Named::ArrowRight => VK_RIGHT,
        Named::Space => VK_SPACE,
        Named::Shift => VK_SHIFT,
        Named::Control => VK_CONTROL,
        Named::Alt => VK_MENU,
        Named::Meta => VK_LWIN,
        Named::ContextMenu => VK_APPS,
        Named::CapsLock => VK_CAPITAL,
        Named::F(n) => VK_F1 + n as u16 - 1,
        Named::Letter(c) | Named::Digit(c) => c as u16,
    }
}

/// Keys whose scan code has the 0xE0 prefix (the lParam's extended bit).
fn extended(vk: u16) -> bool {
    matches!(vk, VK_INSERT | VK_DELETE | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_RCONTROL | VK_RMENU | VK_LWIN | VK_RWIN | VK_APPS | VK_DIVIDE | VK_NUMLOCK)
}

/// A key message's lParam: repeat count 1, scan code, extended bit, context (Alt held, for the
/// `WM_SYS*` messages), previous state and transition (both set for a release).
pub fn key_lparam(scan: u32, ext: bool, alt: bool, up: bool) -> isize {
    let mut l: u32 = 1 | (scan & 0xFF) << 16;
    if ext {
        l |= 1 << 24;
    }
    if alt {
        l |= 1 << 29;
    }
    if up {
        l |= 1 << 30 | 1 << 31;
    }
    l as i32 as isize
}

/// A mouse message's lParam: x and y as signed 16-bit words.
pub fn point_lparam(x: i32, y: i32) -> isize {
    ((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as i32 as isize
}

/// `WM_MOUSEWHEEL`'s wParam: the delta (120 per notch; + is away from the user) and the key flags.
pub fn wheel_wparam(delta: i32, keys: usize) -> usize {
    ((delta.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16 as usize) << 16) | (keys & 0xFFFF)
}

/// Wheel units for a delta in pixels: one notch (120) per 100 px, Chromium's scroll per notch.
/// What a native app scrolls per notch is its own.
pub fn wheel_delta(px: f64) -> i32 {
    (super::keys::notches(px) * 120.0).round() as i32
}

/// One message to post.
#[derive(Clone, Copy, Debug)]
struct Msg {
    hwnd: isize,
    msg: u32,
    w: usize,
    l: isize,
}

/// Where a pointer input lands: the window the OS would hit-test there, in the app, with the point
/// in that window's client coordinates (its own DPI awareness) and in screen coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub hwnd: isize,
    pub client: (i32, i32),
    pub screen: (i32, i32),
}

type Job = Box<dyn FnOnce(&mut State) + Send>;

/// The input thread: per-monitor DPI aware, with a message queue (so it can share key state).
pub struct Poster {
    tx: Option<Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct State {
    tid: u32,
    /// Keys held across calls (`page.down`), as virtual keys.
    held: Vec<u16>,
    /// The cursor where the user left it, put back when strobe is done.
    cursor0: Option<POINT>,
}

impl Poster {
    pub fn start() -> Result<Poster> {
        let (tx, rx) = channel::<Job>();
        let (ready_tx, ready_rx) = channel::<u32>();
        let thread = std::thread::Builder::new()
            .name("strobe-input".into())
            .spawn(move || {
                unsafe {
                    windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                    let mut msg = std::mem::zeroed();
                    PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                }
                let mut st = State { tid: unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, held: Vec::new(), cursor0: None };
                let _ = ready_tx.send(st.tid);
                for job in rx {
                    job(&mut st);
                }
                if let Some(p) = st.cursor0 {
                    unsafe { SetCursorPos(p.x, p.y) };
                }
            })
            .map_err(|e| Error::msg(format!("start the input thread: {e}")))?;
        ready_rx.recv().map_err(|_| Error::msg("the input thread stopped"))?;
        Ok(Poster { tx: Some(tx), thread: Some(thread) })
    }

    fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut State) -> Result<R> + Send + 'static) -> Result<R> {
        let (tx, rx) = channel();
        self.tx
            .as_ref()
            .ok_or_else(|| Error::msg("the input thread stopped"))?
            .send(Box::new(move |s| {
                let _ = tx.send(f(s));
            }))
            .map_err(|_| Error::msg("the input thread stopped"))?;
        rx.recv_timeout(Duration::from_secs(60)).map_err(|_| Error::msg("the input thread did not answer"))?
    }

    /// The window of `pids` the OS hit-tests at screen point (x, y) (physical pixels), or why not.
    pub fn target(&self, pids: &[u32], x: f64, y: f64) -> Result<Target> {
        let pids = pids.to_vec();
        self.call(move |_| target_at(&pids, x.round() as i32, y.round() as i32))
    }

    /// A point in `top`'s client area, in device-independent pixels, as a screen point in physical ones.
    pub fn client_point(&self, top: isize, x: f64, y: f64) -> Result<(f64, f64)> {
        self.call(move |_| unsafe {
            let h = top as HWND;
            let mut o = POINT { x: 0, y: 0 };
            windows_sys::Win32::Graphics::Gdi::ClientToScreen(h, &mut o);
            // Physical pixels per device-independent one: the monitor's DPI over 96.
            let px = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(h).max(96) as f64 / 96.0 / window_scale(h).max(0.01);
            Ok((o.x as f64 + x * px, o.y as f64 + y * px))
        })
    }

    /// Moves the OS cursor onto the point (no click, no focus): true when it got there.
    pub fn park(&self, x: f64, y: f64) -> Result<bool> {
        self.call(move |s| {
            let (x, y) = (x.round() as i32, y.round() as i32);
            unsafe {
                let mut now = POINT { x: 0, y: 0 };
                if s.cursor0.is_none() && GetCursorPos(&mut now) != 0 {
                    s.cursor0 = Some(now);
                }
                SetCursorPos(x, y);
                let mut at = POINT { x: 0, y: 0 };
                Ok(GetCursorPos(&mut at) != 0 && at.x == x && at.y == y)
            }
        })
    }

    /// A pointer move to `t`; returns when it was posted.
    pub fn move_to(&self, t: Target, buttons: usize) -> Result<u64> {
        self.call(move |s| {
            let held = s.held_mk();
            post(&[Msg { hwnd: t.hwnd, msg: WM_MOUSEMOVE, w: buttons | held, l: point_lparam(t.client.0, t.client.1) }]).map(|v| v[0])
        })
    }

    /// A button edge at `t`, preceded by a move to the same point (a toolkit takes a button's position
    /// from the last move). With `mods`, the modifiers are held in the key state and as key messages
    /// around it. `dbl`: the second press of a double click. Returns when the button was posted.
    pub fn button(&self, t: Target, b: Button, down: bool, dbl: bool, mods: Mods, top: isize) -> Result<u64> {
        self.call(move |s| {
            let (dn, up, dblclk, mk) = button_messages(b);
            let msg = match (down, dbl && class_dblclks(t.hwnd)) {
                (true, true) => dblclk,
                (true, false) => dn,
                (false, _) => up,
            };
            let held = s.held_mk() | mods_mk(mods);
            let pressed = if down { mk } else { 0 };
            let msgs = [
                Msg { hwnd: t.hwnd, msg: WM_MOUSEMOVE, w: held | if down { 0 } else { mk }, l: point_lparam(t.client.0, t.client.1) },
                Msg { hwnd: t.hwnd, msg, w: held | pressed, l: point_lparam(t.client.0, t.client.1) },
            ];
            if !mods.any() {
                return post(&msgs).map(|v| v[1]);
            }
            let keys = keys_window(top);
            let at = s.with_state(top, mods, |s| {
                let mut downs: Vec<Msg> = vks(mods).into_iter().filter(|vk| !s.held.contains(vk)).map(|vk| key_msg(keys, vk, false, mods)).collect();
                let n = downs.len();
                downs.extend(msgs);
                let stamps = post(&downs)?;
                settle(keys, downs.len())?;
                Ok(stamps[n + 1])
            })?;
            let ups: Vec<Msg> = vks(mods).into_iter().rev().filter(|vk| !s.held.contains(vk)).map(|vk| key_msg(keys, vk, true, Mods::default())).collect();
            post(&ups)?;
            settle(keys, ups.len())?;
            Ok(at)
        })
    }

    /// One wheel step of `dx`, `dy` pixels at `t` (+ is right and down, as a page's deltas). Returns
    /// when it was posted.
    pub fn wheel(&self, t: Target, dx: f64, dy: f64) -> Result<u64> {
        self.call(move |s| {
            let mk = s.held_mk();
            let l = screen_lparam(t);
            let mut msgs = Vec::new();
            if dy != 0.0 {
                msgs.push(Msg { hwnd: t.hwnd, msg: WM_MOUSEWHEEL, w: wheel_wparam(-wheel_delta(dy), mk), l });
            }
            if dx != 0.0 {
                msgs.push(Msg { hwnd: t.hwnd, msg: WM_MOUSEHWHEEL, w: wheel_wparam(wheel_delta(dx), mk), l });
            }
            post(&msgs).map(|v| v.first().copied().unwrap_or_else(crate::host::now_ns))
        })
    }

    /// Presses and releases a key with modifiers, to `to` (an element's own window) or else the focus
    /// window of `top`'s UI thread. Returns when the key went down.
    pub fn press(&self, top: isize, to: Option<isize>, mods: Mods, key: Key) -> Result<u64> {
        self.call(move |s| {
            let keys = to.unwrap_or_else(|| keys_window(top));
            let (vk, mods) = resolve(top, key, mods)?;
            if !mods.any() && s.held.is_empty() {
                let mut up = key_msg(keys, vk, true, mods);
                if vk == VK_MENU {
                    // Alt tapped alone comes up as a system key (the menu bar's cue).
                    up.msg = WM_SYSKEYUP;
                    up.l |= 1 << 29;
                }
                let v = post(&[key_msg(keys, vk, false, mods), up])?;
                return Ok(v[0]);
            }
            let fresh: Vec<u16> = vks(mods).into_iter().filter(|m| !s.held.contains(m)).collect();
            let all = s.held_mods().or(mods);
            let at = s.with_state(top, mods, |_| {
                let mut msgs: Vec<Msg> = fresh.iter().map(|m| key_msg(keys, *m, false, all)).collect();
                let n = msgs.len();
                msgs.push(key_msg(keys, vk, false, all));
                msgs.push(key_msg(keys, vk, true, all));
                let stamps = post(&msgs)?;
                // TranslateMessage makes a character message from each key down.
                settle(keys, msgs.len() + 1)?;
                Ok(stamps[n])
            })?;
            let ups: Vec<Msg> = fresh.iter().rev().map(|m| key_msg(keys, *m, true, s.held_mods())).collect();
            post(&ups)?;
            settle(keys, ups.len())?;
            Ok(at)
        })
    }

    /// Holds a key across calls (`page.down`): it stays down in the key state until `release`.
    pub fn hold(&self, top: isize, key: Key) -> Result<u64> {
        self.call(move |s| {
            let keys = keys_window(top);
            let (vk, _) = resolve(top, key, Mods::default())?;
            if s.held.contains(&vk) {
                return Ok(crate::host::now_ns());
            }
            if s.held.is_empty() {
                attach(s.tid, thread_of(top), true)?;
            }
            s.held.push(vk);
            set_state(&s.held, Mods::default());
            let all = s.held_mods();
            let at = post(&[key_msg(keys, vk, false, all)])?[0];
            settle(keys, 2)?;
            Ok(at)
        })
    }

    pub fn release(&self, top: isize, key: Key) -> Result<u64> {
        self.call(move |s| {
            let keys = keys_window(top);
            let (vk, _) = resolve(top, key, Mods::default())?;
            let Some(i) = s.held.iter().position(|k| *k == vk) else { return Ok(crate::host::now_ns()) };
            s.held.remove(i);
            set_state(&s.held, Mods::default());
            let at = post(&[key_msg(keys, vk, true, s.held_mods())])?[0];
            settle(keys, 1)?;
            if s.held.is_empty() {
                attach(s.tid, thread_of(top), false)?;
            }
            Ok(at)
        })
    }

    /// Releases every held key (a block's end).
    pub fn release_all(&self, top: isize) {
        let _ = self.call(move |s| {
            let keys = keys_window(top);
            for vk in std::mem::take(&mut s.held).into_iter().rev() {
                let _ = post(&[key_msg(keys, vk, true, Mods::default())]);
            }
            set_state(&[], Mods::default());
            let _ = attach(s.tid, thread_of(top), false);
            Ok(())
        });
    }

    /// `show`, `hide`, `focus` or `close` for a top-level window; returns when it was asked.
    pub fn window(&self, top: isize, action: &str) -> Result<u64> {
        let action = action.to_string();
        self.call(move |_| {
            let h = top as HWND;
            let at = crate::host::now_ns();
            let ok = unsafe {
                match action.as_str() {
                    // What the title bar's close button and Alt+F4 send: the app's own close path.
                    "close" => PostMessageW(h, WM_SYSCOMMAND, SC_CLOSE as usize, 0) != 0,
                    "show" if IsIconic(h) != 0 => PostMessageW(h, WM_SYSCOMMAND, SC_RESTORE as usize, 0) != 0,
                    "show" => ShowWindowAsync(h, SW_SHOWNOACTIVATE) != 0 || IsWindowVisible(h) != 0,
                    "hide" => ShowWindowAsync(h, SW_HIDE) != 0 || IsWindowVisible(h) == 0,
                    "minimize" => PostMessageW(h, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0) != 0,
                    "focus" => {
                        SetForegroundWindow(h);
                        GetForegroundWindow() == h
                    }
                    other => return Err(Error::usage(format!("window({other}): show, hide, minimize, focus or close"))),
                }
            };
            if !ok {
                return Err(Error::msg(if action == "focus" {
                    "window(focus): Windows kept another window in front (a background process may not take the foreground)".to_string()
                } else {
                    format!("window({action}) failed")
                }));
            }
            Ok(at)
        })
    }
}

impl Drop for Poster {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl State {
    fn held_mods(&self) -> Mods {
        Mods { ctrl: self.held.contains(&VK_CONTROL), shift: self.held.contains(&VK_SHIFT), alt: self.held.contains(&VK_MENU), meta: self.held.contains(&VK_LWIN) }
    }

    fn held_mk(&self) -> usize {
        mods_mk(self.held_mods())
    }

    /// Runs `f` with `mods` (and every held key) down in the key state the app's UI thread reads,
    /// then puts back the held keys alone. Attached only for as long as needed.
    fn with_state<R>(&mut self, top: isize, mods: Mods, f: impl FnOnce(&mut State) -> Result<R>) -> Result<R> {
        let target = thread_of(top);
        let was = !self.held.is_empty();
        if !was {
            attach(self.tid, target, true)?;
        }
        set_state(&self.held, mods);
        let r = f(self);
        set_state(&self.held, Mods::default());
        if !was {
            let _ = attach(self.tid, target, false);
        }
        r
    }
}

fn mods_mk(m: Mods) -> usize {
    (if m.ctrl { MK_CONTROL as usize } else { 0 }) | if m.shift { MK_SHIFT as usize } else { 0 }
}

fn thread_of(hwnd: isize) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd as HWND, std::ptr::null_mut()) }
}

fn attach(me: u32, target: u32, on: bool) -> Result<()> {
    if unsafe { windows_sys::Win32::System::Threading::AttachThreadInput(me, target, on as i32) } == 0 && on {
        return Err(Error::msg(format!("share key state with the app's UI thread (AttachThreadInput): error {}; modifier keys cannot reach a window that is not focused without it", unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        })));
    }
    Ok(())
}

/// The key state: `held` keys and `mods` down, everything else as the keyboard has it (toggles kept).
fn set_state(held: &[u16], mods: Mods) {
    let mut ks = [0u8; 256];
    unsafe { GetKeyboardState(ks.as_mut_ptr()) };
    for vk in [VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN] {
        ks[vk as usize] &= 0x01;
    }
    let mut down = |vk: u16| {
        ks[vk as usize] |= 0x80;
        // The generic modifier is also its left key, as a physical left Shift reports both.
        match vk {
            VK_SHIFT => ks[VK_LSHIFT as usize] |= 0x80,
            VK_CONTROL => ks[VK_LCONTROL as usize] |= 0x80,
            VK_MENU => ks[VK_LMENU as usize] |= 0x80,
            _ => {}
        }
    };
    for vk in held.iter().copied().chain(vks(mods)) {
        down(vk);
    }
    unsafe { SetKeyboardState(ks.as_ptr()) };
}

/// The window keys go to: the app UI thread's focus window, else the top-level window itself.
fn keys_window(top: isize) -> isize {
    unsafe {
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(thread_of(top), &mut info) != 0 && !info.hwndFocus.is_null() {
            return info.hwndFocus as isize;
        }
    }
    top
}

/// The virtual key and the modifiers a key needs on the layout of `top`'s thread (a capital letter
/// is Shift and its key; AltGr characters are Control+Alt).
fn resolve(top: isize, key: Key, mods: Mods) -> Result<(u16, Mods)> {
    match key {
        Key::Named(n) => Ok((named_vk(n), mods)),
        Key::Char(c) => {
            let mut units = [0u16; 2];
            if c.encode_utf16(&mut units).len() != 1 {
                return Err(Error::usage(format!("character {c:?} is not on a keyboard; strobe types what the app's keyboard layout has keys for")));
            }
            let hkl = unsafe { GetKeyboardLayout(thread_of(top)) };
            let r = unsafe { VkKeyScanExW(units[0], hkl) };
            if r == -1 {
                return Err(Error::usage(format!("character {c:?} has no key on the app's keyboard layout: strobe types keys, as a person does (an input method's text is not built)")));
            }
            let (vk, shift) = ((r as u16) & 0xFF, ((r as u16) >> 8) & 0xFF);
            Ok((vk, mods.or(Mods { shift: shift & 1 != 0, ctrl: shift & 2 != 0, alt: shift & 4 != 0, meta: false })))
        }
    }
}

/// A key message as the keyboard would produce it: `WM_SYSKEY*` while Alt is down without Control
/// (and for F10), with its scan code from the layout.
fn key_msg(hwnd: isize, vk: u16, up: bool, mods: Mods) -> Msg {
    let hkl = unsafe { GetKeyboardLayout(thread_of(hwnd)) };
    let sc = unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC_EX, hkl) };
    let ext = sc & 0xFF00 == 0xE000 || extended(vk);
    // Alt itself goes down as a system key; released after another key, it comes up as a plain one.
    let alt = (mods.alt && !mods.ctrl && vk != VK_MENU) || (vk == VK_MENU && !up && !mods.ctrl);
    let sys = alt || vk == VK_F10;
    let msg = match (sys, up) {
        (true, false) => WM_SYSKEYDOWN,
        (true, true) => WM_SYSKEYUP,
        (false, false) => WM_KEYDOWN,
        (false, true) => WM_KEYUP,
    };
    Msg { hwnd, msg, w: vk as usize, l: key_lparam(sc, ext, alt, up) }
}

/// Posts `msgs` in order; each one's post time (host clock), stamped just before it is posted.
fn post(msgs: &[Msg]) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        let t = crate::host::now_ns();
        if unsafe { PostMessageW(m.hwnd as HWND, m.msg, m.w, m.l) } == 0 {
            return Err(Error::msg(format!("post a message to the app's window: error {}", unsafe { windows_sys::Win32::Foundation::GetLastError() })));
        }
        out.push(t);
    }
    Ok(out)
}

/// Returns once the app's message loop has taken `n` posted messages from its queue. A sent message
/// is handled at the app's next look at its queue, before it takes the next posted one, so after
/// 2n + 2 sent `WM_NULL`s have come back every message posted before them has been dispatched (each
/// look may also answer one while a key handler peeks ahead). It costs the app's UI thread a few
/// microseconds per message, and fails when the app does not look at its queue for 2 s.
fn settle(hwnd: isize, n: usize) -> Result<()> {
    for _ in 0..2 * n + 2 {
        let mut r = 0usize;
        if unsafe { SendMessageTimeoutW(hwnd as HWND, WM_NULL, 0, 0, SMTO_NORMAL | SMTO_ABORTIFHUNG, 2000, &mut r) } == 0 {
            return Err(Error { code: 124, message: "the app's UI thread did not take the posted input within 2 s".into(), hint: None });
        }
    }
    Ok(())
}

fn class_dblclks(hwnd: isize) -> bool {
    unsafe { GetClassLongPtrW(hwnd as HWND, GCL_STYLE) as u32 & CS_DBLCLKS != 0 }
}

fn target_at(pids: &[u32], x: i32, y: i32) -> Result<Target> {
    unsafe {
        let hwnd = WindowFromPhysicalPoint(POINT { x, y });
        if hwnd.is_null() {
            return Err(Error::msg(format!("no window at ({x}, {y})")));
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if !pids.contains(&pid) {
            let mut title = [0u16; 128];
            let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
            return Err(Error::refused(
                "WindowHidden",
                format!("another window covers the point ({x}, {y}): {:?} of process {pid}", String::from_utf16_lossy(&title[..n])),
                "keep the app's window uncovered while strobe drives it",
            ));
        }
        let mut c = POINT { x, y };
        windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut c);
        // This thread sees physical pixels; the window gets its own DPI's.
        let f = window_scale(hwnd);
        Ok(Target { hwnd: hwnd as isize, client: ((c.x as f64 * f).round() as i32, (c.y as f64 * f).round() as i32), screen: (x, y) })
    }
}

/// The window's DPI over its monitor's: 1 for a per-monitor aware window, less for one the system
/// scales (it gets coordinates in its own, smaller pixels).
fn window_scale(hwnd: HWND) -> f64 {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
    unsafe {
        let (mut mx, mut my) = (0u32, 0u32);
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let w = GetDpiForWindow(hwnd);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut mx, &mut my) < 0 || mx == 0 || w == 0 {
            return 1.0;
        }
        w as f64 / mx as f64
    }
}

/// A wheel message's lParam: the screen point in the window's own coordinates.
fn screen_lparam(t: Target) -> isize {
    let mut p = POINT { x: t.screen.0, y: t.screen.1 };
    unsafe { windows_sys::Win32::UI::HiDpi::PhysicalToLogicalPointForPerMonitorDPI(t.hwnd as HWND, &mut p) };
    point_lparam(p.x, p.y)
}
