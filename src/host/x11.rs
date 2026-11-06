//! Top-level windows on Linux through X11 (`x11rb`'s pure-Rust connection to `$DISPLAY`), with the
//! window manager's EWMH properties: whose a window is (`_NET_WM_PID`), whether it is minimized
//! (`_NET_WM_STATE_HIDDEN`), the stacking order for occlusion, and placement that keeps a window on top
//! without focusing it (`_NET_WM_STATE_ABOVE`). Native Wayland windows are invisible here: Wayland
//! gives no client a view of another's windows, so a Wayland-only app has no main window for strobe.

use super::TopWindow;
use std::sync::Mutex;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, ImageFormat, MapState, StackMode, Window};
use x11rb::rust_connection::RustConnection;

struct X {
    conn: RustConnection,
    root: Window,
    dpi: u32,
}

static CONN: Mutex<Option<X>> = Mutex::new(None);

/// Runs `f` on the display's connection (opened once; reopened after an error).
fn with<T>(f: impl FnOnce(&X) -> Option<T>) -> Option<T> {
    std::env::var_os("DISPLAY")?;
    let mut guard = CONN.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let mut x = X { conn, root, dpi: 96 };
        x.dpi = resource_dpi(&x).unwrap_or(96);
        *guard = Some(x);
    }
    let out = f(guard.as_ref()?);
    if out.is_none() && guard.as_ref().is_some_and(|x| x.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).is_none()) {
        *guard = None;
    }
    out
}

fn atom(x: &X, name: &str) -> Option<u32> {
    Some(x.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom)
}

fn prop32(x: &X, win: Window, name: &str) -> Vec<u32> {
    let Some(a) = atom(x, name) else { return Vec::new() };
    x.conn.get_property(false, win, a, AtomEnum::ANY, 0, 4096).ok().and_then(|c| c.reply().ok()).and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default()
}

/// `Xft.dpi` from the root window's resources (what toolkits scale by), else 96.
fn resource_dpi(x: &X) -> Option<u32> {
    let r = x.conn.get_property(false, x.root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 65536).ok()?.reply().ok()?;
    let text = String::from_utf8_lossy(&r.value).to_string();
    text.lines().find_map(|l| l.strip_prefix("Xft.dpi:")).and_then(|v| v.trim().parse::<f64>().ok()).map(|d| d.round() as u32)
}

/// Managed top-level windows, bottom to top: the window manager's stacking list, else the root's
/// children, each frame stood for by the client window in it (ICCCM's `WM_STATE`), since a manager
/// without the list (an XWayland manager) still reparents clients into frames.
fn clients(x: &X) -> Vec<Window> {
    let stacked = prop32(x, x.root, "_NET_CLIENT_LIST_STACKING");
    if !stacked.is_empty() {
        return stacked;
    }
    children(x, x.root).into_iter().map(|w| client_in(x, w, 2).unwrap_or(w)).collect()
}

fn children(x: &X, win: Window) -> Vec<Window> {
    x.conn.query_tree(win).ok().and_then(|c| c.reply().ok()).map(|t| t.children).unwrap_or_default()
}

/// The window at or under `win` (at most `depth` levels down) that has `WM_STATE`.
fn client_in(x: &X, win: Window, depth: u32) -> Option<Window> {
    if !prop32(x, win, "WM_STATE").is_empty() {
        return Some(win);
    }
    if depth == 0 {
        return None;
    }
    children(x, win).into_iter().rev().find_map(|c| client_in(x, c, depth - 1))
}

struct Info {
    win: Window,
    pid: u32,
    /// Outer rect on the root (frame included when the manager reports it).
    rect: (i32, i32, i32, i32),
    client: (i32, i32),
    viewable: bool,
    hidden: bool,
}

fn info(x: &X, win: Window) -> Option<Info> {
    let g = x.conn.get_geometry(win).ok()?.reply().ok()?;
    let at = x.conn.translate_coordinates(win, x.root, 0, 0).ok()?.reply().ok()?;
    let attrs = x.conn.get_window_attributes(win).ok()?.reply().ok()?;
    let frame = prop32(x, win, "_NET_FRAME_EXTENTS");
    let (l, r, t, b) = if frame.len() == 4 { (frame[0] as i32, frame[1] as i32, frame[2] as i32, frame[3] as i32) } else { (0, 0, 0, 0) };
    let hidden_atom = atom(x, "_NET_WM_STATE_HIDDEN");
    let hidden = hidden_atom.is_some_and(|h| prop32(x, win, "_NET_WM_STATE").contains(&h)) || prop32(x, win, "WM_STATE").first() == Some(&3);
    Some(Info {
        win,
        pid: prop32(x, win, "_NET_WM_PID").first().copied().unwrap_or(0),
        rect: (at.dst_x as i32 - l, at.dst_y as i32 - t, g.width as i32 + l + r, g.height as i32 + t + b),
        client: (g.width as i32, g.height as i32),
        viewable: attrs.map_state == MapState::VIEWABLE,
        hidden,
    })
}

/// The share of `w` that other processes' viewable windows above it cover (a 16×16 grid).
fn covered(x: &X, w: &Info, stack: &[Window]) -> f64 {
    let above: Vec<Info> = stack.iter().skip_while(|&&s| s != w.win).skip(1).filter_map(|&s| info(x, s)).filter(|o| o.viewable && o.pid != w.pid).collect();
    if above.is_empty() {
        return 0.0;
    }
    let (rx, ry, rw, rh) = w.rect;
    let mut hit = 0;
    for i in 0..16 {
        for j in 0..16 {
            let (px, py) = (rx + rw * (2 * i + 1) / 32, ry + rh * (2 * j + 1) / 32);
            if above.iter().any(|o| px >= o.rect.0 && px < o.rect.0 + o.rect.2 && py >= o.rect.1 && py < o.rect.1 + o.rect.3) {
                hit += 1;
            }
        }
    }
    hit as f64 / 256.0
}

fn top(x: &X, w: &Info, stack: &[Window]) -> TopWindow {
    TopWindow {
        hwnd: w.win as isize,
        pid: w.pid,
        rect: w.rect,
        client: w.client,
        visible: w.viewable,
        iconic: w.hidden,
        cloaked: w.viewable && covered(x, w, stack) >= 0.99,
        dpi: x.dpi,
    }
}

/// The largest viewable window of `pids` (at least 100×100 pixels: a strip such as a menu bar is no
/// main window, whatever its area).
pub fn main_window(pids: &[u32]) -> Option<TopWindow> {
    with(|x| {
        let stack = clients(x);
        let best = stack
            .iter()
            .filter_map(|&w| info(x, w))
            .filter(|i| pids.contains(&i.pid) && i.viewable && !i.hidden && i.rect.2 >= 100 && i.rect.3 >= 100)
            .max_by_key(|i| i.rect.2 as i64 * i.rect.3 as i64)?;
        Some(top(x, &best, &stack))
    })
}

pub fn window_state(id: isize) -> Option<TopWindow> {
    with(|x| {
        let stack = clients(x);
        let i = info(x, id as Window)?;
        Some(top(x, &i, &stack))
    })
}

/// Asks the window manager to keep `win` above other windows without focusing it (EWMH), or with no
/// manager, raises it.
fn keep_above(x: &X, win: Window) -> Option<()> {
    let (state, above) = (atom(x, "_NET_WM_STATE")?, atom(x, "_NET_WM_STATE_ABOVE")?);
    if prop32(x, x.root, "_NET_SUPPORTED").contains(&above) {
        let ev = ClientMessageEvent::new(32, win, state, [1, above, 0, 1, 0]);
        x.conn.send_event(false, x.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev).ok()?;
    } else {
        x.conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE)).ok()?;
    }
    Some(())
}

/// Moves the window to `(x, y)` and with `client` sizes its client area (device-independent pixels at
/// the display's DPI), kept above without focus. Through `_NET_MOVERESIZE_WINDOW` when the manager has
/// it (a managed window ignores direct moves), else directly.
pub fn place_window(w: &TopWindow, px: i32, py: i32, client: Option<(u32, u32)>) {
    with(|x| {
        let win = w.hwnd as Window;
        let scale = |v: u32| (v as f64 * w.dpi as f64 / 96.0).round() as u32;
        let size = client.map(|(cw, ch)| (scale(cw), scale(ch)));
        let moveresize = atom(x, "_NET_MOVERESIZE_WINDOW")?;
        if prop32(x, x.root, "_NET_SUPPORTED").contains(&moveresize) {
            // Gravity static (10): x, y place the client's frame; flags: x, y and, with a size, width and height.
            let flags = 10 | (1 << 8) | (1 << 9) | if size.is_some() { (1 << 10) | (1 << 11) } else { 0 } | (2 << 12);
            let (cw, ch) = size.unwrap_or((0, 0));
            let ev = ClientMessageEvent::new(32, win, moveresize, [flags, px as u32, py as u32, cw, ch]);
            x.conn.send_event(false, x.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev).ok()?;
        } else {
            let mut aux = ConfigureWindowAux::new().x(px).y(py);
            if let Some((cw, ch)) = size {
                aux = aux.width(cw).height(ch);
            }
            x.conn.configure_window(win, &aux).ok()?;
        }
        keep_above(x, win);
        x.conn.flush().ok()
    });
}

/// Keeps every viewable window of `pids` above others without focusing it; returns how many.
pub fn raise_windows(pids: &[u32]) -> usize {
    with(|x| {
        let wins: Vec<Window> = clients(x).into_iter().filter_map(|w| info(x, w)).filter(|i| pids.contains(&i.pid) && i.viewable).map(|i| i.win).collect();
        for &w in &wins {
            keep_above(x, w);
        }
        x.conn.flush().ok()?;
        Some(wins.len())
    })
    .unwrap_or(0)
}

/// A window's title: `_NET_WM_NAME` (UTF-8), else `WM_NAME`.
fn title(x: &X, win: Window) -> String {
    let utf8 = atom(x, "UTF8_STRING").unwrap_or(0);
    for (name, kind) in [("_NET_WM_NAME", utf8), ("WM_NAME", AtomEnum::STRING.into())] {
        let Some(a) = atom(x, name) else { continue };
        if let Some(r) = x.conn.get_property(false, win, a, kind, 0, 1024).ok().and_then(|c| c.reply().ok()).filter(|r| !r.value.is_empty()) {
            return String::from_utf8_lossy(&r.value).to_string();
        }
    }
    String::new()
}

/// The viewable windows of `pids`, front to back: id, title, click-through (never, on X11).
pub fn top_windows(pids: &[u32]) -> Vec<(isize, String, bool)> {
    with(|x| {
        let mut v: Vec<(isize, String, bool)> =
            clients(x).into_iter().filter_map(|w| info(x, w)).filter(|i| pids.contains(&i.pid) && i.viewable && !i.hidden).map(|i| (i.win as isize, title(x, i.win), false)).collect();
        v.reverse();
        Some(v)
    })
    .unwrap_or_default()
}

/// Where a window's client area starts on the root window.
pub fn client_origin(win: isize) -> Option<(i32, i32)> {
    with(|x| {
        let at = x.conn.translate_coordinates(win as Window, x.root, 0, 0).ok()?.reply().ok()?;
        Some((at.dst_x as i32, at.dst_y as i32))
    })
}

/// The topmost viewable managed window at a root point: (window, pid, title).
pub fn window_at(px: i32, py: i32) -> Option<(isize, u32, String)> {
    with(|x| {
        let stack = clients(x);
        let hit = stack.iter().rev().filter_map(|&w| info(x, w)).find(|i| i.viewable && px >= i.rect.0 && py >= i.rect.1 && px < i.rect.0 + i.rect.2 && py < i.rect.1 + i.rect.3)?;
        Some((hit.win as isize, hit.pid, title(x, hit.win)))
    })
}

/// A window's client area as it is drawn now (`GetImage`, 32-bit pixels), for `snapshot`.
pub fn window_image(win: isize) -> Option<crate::png::Image> {
    with(|x| {
        let g = x.conn.get_geometry(win as Window).ok()?.reply().ok()?;
        let r = x.conn.get_image(ImageFormat::Z_PIXMAP, win as Window, 0, 0, g.width, g.height, !0).ok()?.reply().ok()?;
        let (w, h) = (g.width as usize, g.height as usize);
        if r.data.len() < w * h * 4 {
            return None;
        }
        let mut rgba = Vec::with_capacity(w * h * 4);
        for px in r.data[..w * h * 4].chunks(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        Some(crate::png::Image { width: w, height: h, rgba })
    })
}

/// The largest window of `pids` as 16×16 averaged BGRA cells, from its own pixels (`GetImage` of the
/// one-pixel row through each cell row's middle, so a sample moves a few kilobytes, not the window).
pub fn window_pixels(pids: &[u32]) -> Option<Vec<u8>> {
    let w = main_window(pids)?;
    with(|x| {
        let (cw, ch) = (w.client.0.max(0) as u16, w.client.1.max(0) as u16);
        if cw < 16 || ch < 16 {
            return None;
        }
        let rows: Vec<u16> = (0..16).map(|i| ((2 * i + 1) as u32 * ch as u32 / 32) as u16).collect();
        let mut strips = Vec::with_capacity(rows.len());
        for &y in &rows {
            let r = x.conn.get_image(ImageFormat::Z_PIXMAP, w.hwnd as Window, 0, y as i16, cw, 1, !0).ok()?.reply().ok()?;
            if r.data.len() < cw as usize * 4 {
                return None;
            }
            strips.extend_from_slice(&r.data[..cw as usize * 4]);
        }
        Some(super::grid16(&strips, cw as usize, 16, cw as usize * 4))
    })
}
