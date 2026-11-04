//! macOS's accessibility API (`AXUIElement`) as strobe's accessibility client. A snapshot walks each
//! window of the app with one `AXUIElementCopyMultipleAttributeValues` per element (role, subrole,
//! title, description, value, identifier, frame, focus, enabled, children: one round trip to the
//! app each), in points on the global display space CoreGraphics uses. An `AXObserver` per process,
//! on a thread of its own running a run loop, hears the app's notifications (focus, value, title,
//! created and destroyed elements, layout, moves and resizes, selection, rows, windows, menus) and
//! wakes waiters.
//!
//! macOS answers only a process the user trusts for accessibility (System Settings › Privacy &
//! Security › Accessibility), and attributes strobe to the app it runs under: strobe says which.

#![allow(non_upper_case_globals, clippy::upper_case_acronyms)]

use super::{Accessibility, Action, Changes, Node, PATTERN_EXPAND, PATTERN_INVOKE, PATTERN_SCROLL, PATTERN_SCROLL_ITEM, PATTERN_SELECT, PATTERN_TOGGLE, PATTERN_VALUE, Tree};
use crate::host::macos::{CFTypeRef, CGPoint, CGSize, Cf, cf_array, cf_f64, cf_i64, cf_string, cfstr};
use crate::model::{Error, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

type AXError = i32;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFTypeRef, out: *mut CFTypeRef) -> AXError;
    fn AXUIElementCopyMultipleAttributeValues(el: CFTypeRef, attrs: CFTypeRef, options: u32, out: *mut CFTypeRef) -> AXError;
    fn AXUIElementSetAttributeValue(el: CFTypeRef, attr: CFTypeRef, value: CFTypeRef) -> AXError;
    fn AXUIElementIsAttributeSettable(el: CFTypeRef, attr: CFTypeRef, out: *mut u8) -> AXError;
    fn AXUIElementPerformAction(el: CFTypeRef, action: CFTypeRef) -> AXError;
    fn AXUIElementCopyActionNames(el: CFTypeRef, out: *mut CFTypeRef) -> AXError;
    fn AXUIElementCopyElementAtPosition(app: CFTypeRef, x: f32, y: f32, out: *mut CFTypeRef) -> AXError;
    fn AXUIElementGetPid(el: CFTypeRef, pid: *mut i32) -> AXError;
    fn AXUIElementSetMessagingTimeout(el: CFTypeRef, seconds: f32) -> AXError;
    fn AXObserverCreate(pid: i32, callback: extern "C" fn(CFTypeRef, CFTypeRef, CFTypeRef, *mut c_void), out: *mut CFTypeRef) -> AXError;
    fn AXObserverAddNotification(obs: CFTypeRef, el: CFTypeRef, notification: CFTypeRef, refcon: *mut c_void) -> AXError;
    fn AXObserverGetRunLoopSource(obs: CFTypeRef) -> CFTypeRef;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(v: CFTypeRef) -> u32;
    fn AXValueGetValue(v: CFTypeRef, kind: u32, ptr: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    static kCFRunLoopDefaultMode: CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, count: isize, callbacks: *const c_void) -> CFTypeRef;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFRunLoopGetCurrent() -> CFTypeRef;
    fn CFRunLoopAddSource(rl: CFTypeRef, source: CFTypeRef, mode: CFTypeRef);
    fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFNumberCreate(alloc: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
}

const AX_VALUE_POINT: u32 = 1;
const AX_VALUE_SIZE: u32 = 2;
const AX_VALUE_ERROR: u32 = 5;
/// kAXErrorAPIDisabled: the process is not trusted for accessibility.
const AX_API_DISABLED: AXError = -25211;

/// What a snapshot reads of every element, in this order.
const ATTRS: &[&str] = &["AXRole", "AXSubrole", "AXTitle", "AXDescription", "AXValue", "AXIdentifier", "AXPosition", "AXSize", "AXFocused", "AXEnabled", "AXChildren", "AXMinimized"];

/// Notifications that wake a wait (caret moves and text selections are not changes: a person's
/// caret moving changes nothing the app shows).
const NOTIFICATIONS: &[&str] = &[
    "AXFocusedUIElementChanged",
    "AXValueChanged",
    "AXTitleChanged",
    "AXUIElementDestroyed",
    "AXCreated",
    "AXLayoutChanged",
    "AXMoved",
    "AXResized",
    "AXSelectedChildrenChanged",
    "AXSelectedRowsChanged",
    "AXRowCountChanged",
    "AXRowExpanded",
    "AXRowCollapsed",
    "AXWindowCreated",
    "AXFocusedWindowChanged",
    "AXWindowMiniaturized",
    "AXWindowDeminiaturized",
    "AXMenuOpened",
    "AXMenuClosed",
];

/// A snapshot stops here: no app's real tree is this large, and a runaway provider cannot stall strobe.
const MAX_NODES: usize = 50_000;

#[derive(Default)]
struct Signal {
    changes: Mutex<(Changes, std::collections::VecDeque<u64>)>,
    cv: Condvar,
}

impl Signal {
    fn bump(&self) {
        let now = crate::host::now_ns();
        let mut c = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        c.0.count += 1;
        c.0.last_ns = now;
        c.1.push_back(now);
        if c.1.len() > 65_536 {
            c.1.pop_front();
        }
        self.cv.notify_all();
    }
}

extern "C" fn on_notification(_obs: CFTypeRef, _el: CFTypeRef, _name: CFTypeRef, refcon: *mut c_void) {
    if !refcon.is_null() {
        // The observer thread holds the Arc for as long as the observer exists.
        unsafe { (*(refcon as *const Signal)).bump() };
    }
}

struct Worker {
    pids: Vec<u32>,
    /// The last snapshot's elements, by node index, and the tree they make.
    elements: Vec<Cf>,
    last: Tree,
}

pub struct Ax {
    worker: Mutex<Worker>,
    signal: Arc<Signal>,
    /// Processes the observer thread should watch (it adds observers for new ones).
    watch: Arc<Mutex<Vec<u32>>>,
    stop: Arc<AtomicBool>,
    observer: Option<std::thread::JoinHandle<()>>,
}

/// Why macOS refused, as strobe says it to a person.
fn untrusted() -> Error {
    let app = crate::host::macos::responsible_app();
    // The fix goes in the message too: a page call's error reaches the bench output without its hint.
    let pane = crate::host::macos::accessibility_pane();
    Error::usage(format!(
        "macOS lets strobe read another app's accessibility tree and send it input only with the accessibility permission, which {app} (the app strobe runs under) does not have: {pane}, turn on {app}"
    ))
    .hint(format!("{pane}: turn on {app}, then run again (once per app)"))
}

fn check(e: AXError, what: &str) -> Result<()> {
    match e {
        0 => Ok(()),
        AX_API_DISABLED => Err(untrusted()),
        _ => Err(Error::msg(format!("accessibility {what}: AXError {e}"))),
    }
}

fn copy(el: CFTypeRef, attr: &str) -> Option<Cf> {
    let mut v: CFTypeRef = std::ptr::null();
    (unsafe { AXUIElementCopyAttributeValue(el, cfstr(attr).0, &mut v) } == 0).then(|| Cf::new(v)).flatten()
}

/// A value the multiple-attribute call returned, or None where it holds an error (no such attribute).
fn present(v: CFTypeRef) -> Option<CFTypeRef> {
    if v.is_null() || unsafe { CFGetTypeID(v) == AXValueGetTypeID() && AXValueGetType(v) == AX_VALUE_ERROR } {
        return None;
    }
    Some(v)
}

fn point(v: CFTypeRef) -> Option<CGPoint> {
    let mut p = CGPoint::default();
    (unsafe { CFGetTypeID(v) == AXValueGetTypeID() && AXValueGetValue(v, AX_VALUE_POINT, &mut p as *mut _ as *mut c_void) != 0 }).then_some(p)
}

fn size(v: CFTypeRef) -> Option<CGSize> {
    let mut s = CGSize::default();
    (unsafe { CFGetTypeID(v) == AXValueGetTypeID() && AXValueGetValue(v, AX_VALUE_SIZE, &mut s as *mut _ as *mut c_void) != 0 }).then_some(s)
}

/// An app's AX windows, each with its CoreGraphics window id (matched by frame: the AX API names no
/// window id publicly).
fn windows_of(pid: u32) -> Vec<(Cf, Option<u32>)> {
    let Some(app) = Cf::new(unsafe { AXUIElementCreateApplication(pid as i32) }) else { return Vec::new() };
    unsafe { AXUIElementSetMessagingTimeout(app.0, 5.0) };
    let Some(list) = copy(app.0, "AXWindows") else { return Vec::new() };
    let cg: Vec<(isize, crate::host::TopWindow)> = crate::host::top_windows(&[pid]).into_iter().filter_map(|w| Some((w.0, crate::host::window_state(w.0)?))).collect();
    cf_array(list.0)
        .into_iter()
        .map(|w| {
            let w = Cf(unsafe { CFRetain(w) });
            let pos = copy(w.0, "AXPosition").and_then(|v| point(v.0));
            let sz = copy(w.0, "AXSize").and_then(|v| size(v.0));
            let id = match (pos, sz) {
                (Some(p), Some(s)) => cg
                    .iter()
                    .find(|(_, t)| (t.rect.0 - p.x as i32).abs() <= 1 && (t.rect.1 - p.y as i32).abs() <= 1 && (t.rect.2 - s.width as i32).abs() <= 1 && (t.rect.3 - s.height as i32).abs() <= 1)
                    .map(|(id, _)| *id as u32),
                _ => None,
            };
            (w, id)
        })
        .collect()
}

/// The AX element of a CoreGraphics window, for window actions.
pub fn window_element(id: isize) -> Option<(Cf, u32)> {
    let pid = crate::host::window_state(id)?.pid;
    windows_of(pid).into_iter().find(|w| w.1 == Some(id as u32)).map(|w| (w.0, pid))
}

/// Whether the app is the active one (`AXFrontmost`).
pub fn is_front(pid: u32) -> bool {
    Cf::new(unsafe { AXUIElementCreateApplication(pid as i32) }).is_some_and(|app| copy(app.0, "AXFrontmost").and_then(|v| cf_i64(v.0)) == Some(1))
}

/// Brings the app to the front (`AXFrontmost`): macOS gives keys only to the active app's key
/// window. Returns once it is in front, or why not.
pub fn activate(pid: u32) -> Result<()> {
    let app = Cf::new(unsafe { AXUIElementCreateApplication(pid as i32) }).ok_or_else(|| Error::msg("accessibility: no such app"))?;
    let front = |app: &Cf| copy(app.0, "AXFrontmost").and_then(|v| cf_i64(v.0)) == Some(1);
    if front(&app) {
        return Ok(());
    }
    check(unsafe { AXUIElementSetAttributeValue(app.0, cfstr("AXFrontmost").0, kCFBooleanTrue) }, "bring the app to the front")?;
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        if front(&app) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(Error::refused(
        "WindowHidden",
        "macOS gives keys only to the active app, and the app did not come to the front",
        "let the app be the active one while strobe types into it",
    ))
}

/// The process whose element the window server hit-tests at a point, and what that element is.
pub fn app_at(x: f64, y: f64) -> Result<(u32, String)> {
    let wide = Cf(unsafe { AXUIElementCreateSystemWide() });
    let mut at: CFTypeRef = std::ptr::null();
    check(unsafe { AXUIElementCopyElementAtPosition(wide.0, x as f32, y as f32, &mut at) }, "hit test")?;
    let at = Cf::new(at).ok_or_else(|| Error::msg("hit test: nothing at that point"))?;
    let mut pid = 0;
    unsafe { AXUIElementGetPid(at.0, &mut pid) };
    let role = copy(at.0, "AXRole").and_then(|v| cf_string(v.0)).unwrap_or_default();
    let title = copy(at.0, "AXTitle").and_then(|v| cf_string(v.0)).unwrap_or_default();
    Ok((pid as u32, format!("{} {title:?}", role.trim_start_matches("AX").to_lowercase())))
}

/// Sets a window's or app's boolean attribute.
pub fn set_bool(el: &Cf, attr: &str, on: bool) -> Result<()> {
    check(unsafe { AXUIElementSetAttributeValue(el.0, cfstr(attr).0, if on { kCFBooleanTrue } else { kCFBooleanFalse }) }, attr)
}

pub fn perform(el: &Cf, action: &str) -> Result<()> {
    check(unsafe { AXUIElementPerformAction(el.0, cfstr(action).0) }, action)
}

pub fn child(el: &Cf, attr: &str) -> Option<Cf> {
    copy(el.0, attr)
}

impl Ax {
    pub fn start(pids: &[u32]) -> Result<Ax> {
        if unsafe { AXIsProcessTrusted() } == 0 {
            return Err(untrusted());
        }
        let signal = Arc::new(Signal::default());
        let watch = Arc::new(Mutex::new(pids.to_vec()));
        let stop = Arc::new(AtomicBool::new(false));
        let (sig, w, st) = (signal.clone(), watch.clone(), stop.clone());
        let observer = std::thread::Builder::new().name("strobe-a11y".into()).spawn(move || observe(&sig, &w, &st)).map_err(|e| Error::msg(format!("start the accessibility thread: {e}")))?;
        Ok(Ax { worker: Mutex::new(Worker { pids: pids.to_vec(), elements: Vec::new(), last: Tree::default() }), signal, watch, stop, observer: Some(observer) })
    }

    fn with<R>(&self, f: impl FnOnce(&mut Worker) -> Result<R>) -> Result<R> {
        f(&mut self.worker.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for Ax {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.observer.take() {
            let _ = t.join();
        }
    }
}

/// The observer thread: an `AXObserver` per watched process on this thread's run loop.
fn observe(signal: &Arc<Signal>, watch: &Mutex<Vec<u32>>, stop: &AtomicBool) {
    let mut observers: Vec<(u32, Cf, Cf)> = Vec::new();
    let refcon = Arc::as_ptr(signal) as *mut c_void;
    while !stop.load(Ordering::Relaxed) {
        let want = watch.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for pid in want {
            if observers.iter().any(|o| o.0 == pid) {
                continue;
            }
            let mut obs: CFTypeRef = std::ptr::null();
            if unsafe { AXObserverCreate(pid as i32, on_notification, &mut obs) } != 0 || obs.is_null() {
                continue;
            }
            let obs = Cf(obs);
            let Some(app) = Cf::new(unsafe { AXUIElementCreateApplication(pid as i32) }) else { continue };
            for n in NOTIFICATIONS {
                unsafe { AXObserverAddNotification(obs.0, app.0, cfstr(n).0, refcon) };
            }
            unsafe { CFRunLoopAddSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(obs.0), kCFRunLoopDefaultMode) };
            observers.push((pid, obs, app));
        }
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.2, 0) };
    }
}

/// One element's attributes, read in one call.
struct Read {
    role: String,
    subrole: String,
    title: String,
    description: String,
    value: Option<String>,
    number: Option<f64>,
    id: String,
    rect: [f64; 4],
    focused: bool,
    enabled: bool,
    minimized: bool,
    children: Vec<Cf>,
}

fn read(el: CFTypeRef, names: CFTypeRef) -> Option<Read> {
    let mut out: CFTypeRef = std::ptr::null();
    if unsafe { AXUIElementCopyMultipleAttributeValues(el, names, 0, &mut out) } != 0 {
        return None;
    }
    let out = Cf::new(out)?;
    let v = cf_array(out.0);
    let at = |i: usize| v.get(i).copied().and_then(present);
    let s = |i: usize| at(i).and_then(cf_string).unwrap_or_default();
    let pos = at(6).and_then(point).unwrap_or_default();
    let sz = at(7).and_then(size).unwrap_or_default();
    Some(Read {
        role: s(0),
        subrole: s(1),
        title: s(2),
        description: s(3),
        value: at(4).and_then(cf_string),
        number: at(4).and_then(|x| cf_f64(x).or_else(|| cf_i64(x).map(|i| i as f64))),
        id: s(5),
        rect: [pos.x, pos.y, sz.width, sz.height],
        focused: at(8).and_then(cf_i64) == Some(1),
        enabled: at(9).and_then(cf_i64) != Some(0),
        minimized: at(11).and_then(cf_i64) == Some(1),
        children: at(10).map(cf_array).unwrap_or_default().into_iter().map(|c| Cf(unsafe { CFRetain(c) })).collect(),
    })
}

/// An AX role (and subrole) as the UIA control type `ROLES` names it.
fn role_id(role: &str, subrole: &str) -> i32 {
    match (role, subrole) {
        ("AXRadioButton", "AXTabButton") => 50019,
        ("AXCheckBox", "AXSwitch") => 50000,
        ("AXButton" | "AXDisclosureTriangle" | "AXMenuButton", _) => 50000,
        ("AXCheckBox", _) => 50002,
        ("AXComboBox" | "AXPopUpButton", _) => 50003,
        ("AXTextField" | "AXTextArea" | "AXSearchField" | "AXSecureTextField", _) => 50004,
        ("AXLink", _) => 50005,
        ("AXImage", _) => 50006,
        ("AXOutline", _) => 50023,
        ("AXList", _) => 50008,
        ("AXRow", "AXOutlineRow") => 50024,
        ("AXRow", _) => 50029,
        ("AXMenu", _) => 50009,
        ("AXMenuBar", _) => 50010,
        ("AXMenuItem" | "AXMenuBarItem", _) => 50011,
        ("AXProgressIndicator" | "AXBusyIndicator" | "AXLevelIndicator" | "AXRelevanceIndicator", _) => 50012,
        ("AXRadioButton", _) => 50013,
        ("AXScrollBar", _) => 50014,
        ("AXSlider", _) => 50015,
        ("AXIncrementor" | "AXStepper", _) => 50016,
        ("AXTabGroup", _) => 50018,
        ("AXStaticText" | "AXHeading", _) => 50020,
        ("AXToolbar", _) => 50021,
        ("AXHelpTag", _) => 50022,
        ("AXGroup" | "AXRadioGroup" | "AXSection" | "AXLandmark", _) => 50026,
        ("AXWebArea" | "AXDocument", _) => 50030,
        ("AXWindow" | "AXSheet" | "AXDrawer" | "AXDialog", _) => 50032,
        ("AXScrollArea" | "AXSplitGroup" | "AXLayoutArea" | "AXBrowser", _) => 50033,
        ("AXTable" | "AXGrid", _) => 50036,
        ("AXSplitter", _) => 50038,
        ("AXDateField", _) => 50001,
        _ => 50025,
    }
}

fn node(r: &Read, parent: Option<usize>, window: usize, hidden: bool) -> Node {
    let role = r.role.as_str();
    let toggle = matches!(role, "AXCheckBox" | "AXRadioButton") || r.subrole == "AXSwitch" || r.subrole == "AXToggle";
    let mut patterns = PATTERN_SCROLL_ITEM;
    if matches!(role, "AXButton" | "AXMenuItem" | "AXMenuBarItem" | "AXLink" | "AXPopUpButton" | "AXMenuButton" | "AXDisclosureTriangle" | "AXCheckBox" | "AXRadioButton" | "AXComboBox") {
        patterns |= PATTERN_INVOKE;
    }
    if toggle {
        patterns |= PATTERN_TOGGLE;
    }
    if matches!(role, "AXTextField" | "AXTextArea" | "AXSearchField" | "AXSecureTextField" | "AXComboBox" | "AXSlider" | "AXIncrementor" | "AXStepper" | "AXDateField") {
        patterns |= PATTERN_VALUE;
    }
    if matches!(role, "AXDisclosureTriangle" | "AXPopUpButton" | "AXComboBox" | "AXMenuButton") || r.subrole == "AXOutlineRow" {
        patterns |= PATTERN_EXPAND;
    }
    if matches!(role, "AXRow" | "AXCell") || r.subrole == "AXTabButton" {
        patterns |= PATTERN_SELECT;
    }
    if role == "AXScrollArea" {
        patterns |= PATTERN_SCROLL;
    }
    // A label is its title, else its description (AccessKit and SwiftUI often put it there).
    let name = if r.title.is_empty() { r.description.clone() } else { r.title.clone() };
    Node {
        id: r.id.clone(),
        name,
        role: role_id(role, &r.subrole),
        role_text: role.trim_start_matches("AX").to_lowercase(),
        value: r.value.clone().or_else(|| r.number.filter(|_| !toggle).map(|d| if d.fract() == 0.0 { format!("{d:.0}") } else { d.to_string() })),
        rect: r.rect,
        focused: r.focused,
        enabled: r.enabled,
        offscreen: hidden || r.minimized,
        heading: role == "AXHeading",
        dialog: matches!(r.subrole.as_str(), "AXDialog" | "AXSystemDialog" | "AXFloatingWindow") || role == "AXSheet",
        patterns,
        toggled: toggle.then(|| r.number.unwrap_or(0.0) != 0.0),
        hwnd: 0,
        parent,
        window,
    }
}

impl Worker {
    fn snapshot(&mut self, windows: &[isize]) -> Result<Tree> {
        let mut tree = Tree { nodes: Vec::new(), windows: Vec::new(), at_ns: crate::host::now_ns() };
        self.elements.clear();
        let names: Vec<Cf> = ATTRS.iter().map(|a| cfstr(a)).collect();
        let ptrs: Vec<CFTypeRef> = names.iter().map(|n| n.0).collect();
        let names = Cf(unsafe { CFArrayCreate(std::ptr::null(), ptrs.as_ptr(), ptrs.len() as isize, &kCFTypeArrayCallBacks) });
        let mut all: Vec<(Cf, Option<u32>)> = Vec::new();
        for &pid in &self.pids {
            let app = Cf::new(unsafe { AXUIElementCreateApplication(pid as i32) });
            if let Some(app) = &app {
                let mut role: CFTypeRef = std::ptr::null();
                if unsafe { AXUIElementCopyAttributeValue(app.0, cfstr("AXRole").0, &mut role) } == AX_API_DISABLED {
                    return Err(untrusted());
                }
                drop(Cf::new(role));
            }
            all.extend(windows_of(pid));
        }
        // The windows asked for, in that order; an AX window CoreGraphics does not list is left out.
        for &want in windows {
            let Some(k) = all.iter().position(|w| w.1 == Some(want as u32)) else { continue };
            let (root, _) = all.swap_remove(k);
            let w = tree.windows.len();
            tree.windows.push(want);
            let mut stack: Vec<(Cf, Option<usize>, bool)> = vec![(root, None, false)];
            while let Some((el, parent, hidden)) = stack.pop() {
                if tree.nodes.len() >= MAX_NODES {
                    break;
                }
                let Some(r) = read(el.0, names.0) else { continue };
                let i = tree.nodes.len();
                let n = node(&r, parent, w, hidden);
                let hide = n.offscreen;
                tree.nodes.push(n);
                self.elements.push(el);
                for c in r.children.into_iter().rev() {
                    stack.push((c, Some(i), hide));
                }
            }
        }
        self.last = tree.clone();
        Ok(tree)
    }

    fn hit(&self, i: usize, x: f64, y: f64) -> Result<std::result::Result<(), String>> {
        let target = self.elements.get(i).ok_or_else(|| Error::msg("hit test: no such element in the last snapshot"))?;
        let wide = Cf(unsafe { AXUIElementCreateSystemWide() });
        let mut at: CFTypeRef = std::ptr::null();
        check(unsafe { AXUIElementCopyElementAtPosition(wide.0, x as f32, y as f32, &mut at) }, "hit test")?;
        let at = Cf::new(at).ok_or_else(|| Error::msg("hit test: nothing at that point"))?;
        // The element or one inside it.
        let mut cur = Some(Cf(unsafe { CFRetain(at.0) }));
        for _ in 0..64 {
            let Some(c) = cur else { break };
            if unsafe { CFEqual(c.0, target.0) } != 0 {
                return Ok(Ok(()));
            }
            cur = copy(c.0, "AXParent");
        }
        // One of its ancestors, when the app hit-tests no finer.
        let mut p = self.last.nodes[i].parent;
        while let Some(k) = p {
            if unsafe { CFEqual(self.elements[k].0, at.0) } != 0 {
                return Ok(Ok(()));
            }
            p = self.last.nodes[k].parent;
        }
        let mut pid = 0;
        unsafe { AXUIElementGetPid(at.0, &mut pid) };
        let role = copy(at.0, "AXRole").and_then(|v| cf_string(v.0)).unwrap_or_default();
        let title = copy(at.0, "AXTitle").and_then(|v| cf_string(v.0)).unwrap_or_default();
        Ok(Err(format!("{} {title:?} (process {pid})", role.trim_start_matches("AX").to_lowercase())))
    }

    fn act(&self, i: usize, action: &Action) -> Result<u64> {
        let el = self.elements.get(i).ok_or_else(|| Error::msg("act: no such element in the last snapshot"))?.0;
        let actions = || -> Vec<String> {
            let mut list: CFTypeRef = std::ptr::null();
            if unsafe { AXUIElementCopyActionNames(el, &mut list) } != 0 {
                return Vec::new();
            }
            let list = Cf(list);
            cf_array(list.0).into_iter().filter_map(cf_string).collect()
        };
        let settable = |attr: &str| {
            let mut ok = 0u8;
            unsafe { AXUIElementIsAttributeSettable(el, cfstr(attr).0, &mut ok) == 0 && ok != 0 }
        };
        let run = |name: &str, pattern: &str| -> Result<u64> {
            let have = actions();
            if !have.iter().any(|a| a == name) {
                return Err(Error::usage(format!("the element has no {pattern} action in its accessibility tree (its actions: {})", if have.is_empty() { "none".into() } else { have.join(", ") })));
            }
            let t = crate::host::now_ns();
            check(unsafe { AXUIElementPerformAction(el, cfstr(name).0) }, name)?;
            Ok(t)
        };
        let set = |attr: &str, value: CFTypeRef| -> Result<u64> {
            let t = crate::host::now_ns();
            check(unsafe { AXUIElementSetAttributeValue(el, cfstr(attr).0, value) }, attr)?;
            Ok(t)
        };
        match action {
            Action::Invoke => run("AXPress", "Invoke"),
            Action::Toggle => run("AXPress", "Toggle"),
            Action::SetValue(v) => {
                if !settable("AXValue") {
                    return Err(Error::usage("the element's value cannot be set through its accessibility tree"));
                }
                let current = copy(el, "AXValue");
                if current.as_ref().is_some_and(|c| cf_string(c.0).is_none() && cf_f64(c.0).is_some()) {
                    let d: f64 = v.trim().parse().map_err(|_| Error::usage(format!("setValue {v:?}: this element holds a number")))?;
                    let n = Cf(unsafe { CFNumberCreate(std::ptr::null(), 13, &d as *const f64 as *const c_void) });
                    set("AXValue", n.0)
                } else {
                    set("AXValue", cfstr(v).0)
                }
            }
            Action::Expand | Action::Collapse => {
                let open = matches!(action, Action::Expand);
                for attr in ["AXExpanded", "AXDisclosing"] {
                    if settable(attr) {
                        return set(attr, if open { unsafe { kCFBooleanTrue } } else { unsafe { kCFBooleanFalse } });
                    }
                }
                if open { run("AXShowMenu", "ExpandCollapse") } else { run("AXCancel", "ExpandCollapse") }
            }
            Action::Select => {
                if settable("AXSelected") {
                    set("AXSelected", unsafe { kCFBooleanTrue })
                } else {
                    run("AXPick", "SelectionItem").or_else(|_| run("AXPress", "SelectionItem"))
                }
            }
            Action::ScrollIntoView => run("AXScrollToVisible", "ScrollItem"),
            Action::Focus => set("AXFocused", unsafe { kCFBooleanTrue }),
            Action::Scroll(_) => Err(Error::usage("macOS's accessibility API has no scroll action: scroll with page.scroll or page.wheel")),
        }
    }
}

impl Accessibility for Ax {
    fn snapshot(&self, windows: &[isize]) -> Result<Tree> {
        self.with(|w| w.snapshot(windows))
    }

    fn changes(&self) -> Changes {
        self.signal.changes.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    fn change_times(&self) -> Vec<u64> {
        self.signal.changes.lock().unwrap_or_else(|e| e.into_inner()).1.iter().copied().collect()
    }

    fn wait_change(&self, seen: u64, deadline: Instant) -> Changes {
        let mut c = self.signal.changes.lock().unwrap_or_else(|e| e.into_inner());
        while c.0.count <= seen {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            c = self.signal.cv.wait_timeout(c, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        c.0
    }

    fn hit(&self, i: usize, x: f64, y: f64) -> Result<std::result::Result<(), String>> {
        self.with(|w| w.hit(i, x, y))
    }

    fn act(&self, i: usize, action: &Action) -> Result<u64> {
        self.with(|w| w.act(i, action))
    }

    fn set_pids(&self, pids: &[u32]) {
        *self.watch.lock().unwrap_or_else(|e| e.into_inner()) = pids.to_vec();
        self.with(|w| {
            w.pids = pids.to_vec();
            Ok(())
        })
        .ok();
    }
}
