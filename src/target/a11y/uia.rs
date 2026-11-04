//! Windows UI Automation (COM) as strobe's accessibility client: one MTA thread owns the client,
//! the elements of the last snapshot and the event handlers; callers hand it jobs. Snapshots fetch a
//! window's whole tree in one cached request (one round trip to the app's provider), and the app's
//! structure, property and focus events wake waiters, so waits never poll the app.
//!
//! A provider may answer the first request with a placeholder root and build its real tree a turn
//! later (lazy providers raise a structure change then): a wait re-reads the tree on each change.

use super::{Accessibility, Action, Changes, Node, PATTERN_EXPAND, PATTERN_INVOKE, PATTERN_SCROLL, PATTERN_SCROLL_ITEM, PATTERN_SELECT, PATTERN_TOGGLE, PATTERN_VALUE, Tree};
use crate::model::{Error, Result};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, SAFEARRAY};
use windows::Win32::UI::Accessibility::*;
use windows::core::{BSTR, VARIANT, implement};

/// What a job on the UIA thread can use.
pub struct Worker {
    auto: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    events: Events,
    /// Each watched window and the root element its handlers are on.
    watched: Vec<(isize, IUIAutomationElement)>,
    /// The last snapshot's elements, by node index.
    elements: Vec<IUIAutomationElement>,
    focus_registered: bool,
}

type Job = Box<dyn FnOnce(&mut Worker) + Send>;

pub struct Uia {
    tx: Option<Sender<Job>>,
    signal: Arc<Signal>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Change events, counted; waiters sleep on the condvar.
#[derive(Default)]
struct Signal {
    changes: Mutex<(Changes, std::collections::VecDeque<u64>)>,
    cv: Condvar,
    pids: Mutex<Vec<u32>>,
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

#[implement(IUIAutomationStructureChangedEventHandler, IUIAutomationPropertyChangedEventHandler, IUIAutomationFocusChangedEventHandler)]
#[derive(Clone)]
struct Events(Arc<Signal>);

impl IUIAutomationStructureChangedEventHandler_Impl for Events_Impl {
    fn HandleStructureChangedEvent(&self, _: Option<&IUIAutomationElement>, _: StructureChangeType, _: *const SAFEARRAY) -> windows::core::Result<()> {
        self.0.bump();
        Ok(())
    }
}

impl IUIAutomationPropertyChangedEventHandler_Impl for Events_Impl {
    fn HandlePropertyChangedEvent(&self, _: Option<&IUIAutomationElement>, _: UIA_PROPERTY_ID, _: &VARIANT) -> windows::core::Result<()> {
        self.0.bump();
        Ok(())
    }
}

impl IUIAutomationFocusChangedEventHandler_Impl for Events_Impl {
    fn HandleFocusChangedEvent(&self, sender: Option<&IUIAutomationElement>) -> windows::core::Result<()> {
        // Focus events come from the whole desktop: only the app's count.
        let pid = sender.and_then(|s| unsafe { s.CachedProcessId() }.ok()).unwrap_or(0) as u32;
        if self.0.pids.lock().unwrap_or_else(|e| e.into_inner()).contains(&pid) {
            self.0.bump();
        }
        Ok(())
    }
}

/// Properties a snapshot reads for every element.
const PROPS: &[UIA_PROPERTY_ID] = &[
    UIA_AutomationIdPropertyId,
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_LocalizedControlTypePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsEnabledPropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_ProcessIdPropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_ValueValuePropertyId,
    UIA_HeadingLevelPropertyId,
    UIA_IsDialogPropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_IsScrollPatternAvailablePropertyId,
    UIA_IsScrollItemPatternAvailablePropertyId,
];

/// Properties whose changes wake a wait.
const WATCHED: &[UIA_PROPERTY_ID] = &[
    UIA_NamePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_ValueValuePropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
];

/// A snapshot stops here: no app's real tree is this large, and a runaway provider cannot stall strobe.
const MAX_NODES: usize = 50_000;

impl Uia {
    pub fn start(pids: &[u32]) -> Result<Uia> {
        let signal = Arc::new(Signal::default());
        *signal.pids.lock().unwrap_or_else(|e| e.into_inner()) = pids.to_vec();
        let (tx, rx): (Sender<Job>, Receiver<Job>) = channel();
        let (ready_tx, ready_rx) = channel::<Result<()>>();
        let sig = signal.clone();
        let thread = std::thread::Builder::new()
            .name("strobe-a11y".into())
            .spawn(move || {
                let worker = unsafe { Worker::new(sig) };
                match worker {
                    Ok(mut w) => {
                        let _ = ready_tx.send(Ok(()));
                        for job in rx {
                            job(&mut w);
                        }
                        unsafe {
                            let _ = w.auto.RemoveAllEventHandlers();
                        }
                        drop(w);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
                unsafe { CoUninitialize() };
            })
            .map_err(|e| Error::msg(format!("start the accessibility thread: {e}")))?;
        ready_rx.recv().map_err(|_| Error::msg("the accessibility thread stopped"))??;
        Ok(Uia { tx: Some(tx), signal, thread: Some(thread) })
    }

    /// Runs `f` on the UIA thread and waits for it (30 s: a provider that hangs fails the call).
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut Worker) -> Result<R> + Send + 'static) -> Result<R> {
        let (tx, rx) = channel();
        let job: Job = Box::new(move |w| {
            let _ = tx.send(f(w));
        });
        self.tx.as_ref().ok_or_else(|| Error::msg("the accessibility thread stopped"))?.send(job).map_err(|_| Error::msg("the accessibility thread stopped"))?;
        rx.recv_timeout(Duration::from_secs(30)).map_err(|_| Error { code: 124, message: "the app's accessibility provider did not answer in 30 s".into(), hint: None })?
    }
}

impl Drop for Uia {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn err(what: &str) -> impl Fn(windows::core::Error) -> Error + '_ {
    move |e| Error::msg(format!("UI Automation {what}: {}", e.message()))
}

impl Worker {
    unsafe fn new(signal: Arc<Signal>) -> Result<Worker> {
        unsafe {
            // Bounds and points in physical pixels, whatever the app's own DPI awareness.
            windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(err("init"))?;
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)).map_err(err("client"))?;
            if let Ok(a2) = windows::core::Interface::cast::<IUIAutomation2>(&auto) {
                let _ = a2.SetConnectionTimeout(10_000);
                let _ = a2.SetTransactionTimeout(10_000);
            }
            let cache = auto.CreateCacheRequest().map_err(err("cache request"))?;
            for p in PROPS {
                let _ = cache.AddProperty(*p);
            }
            cache.SetTreeScope(TreeScope_Subtree).map_err(err("cache scope"))?;
            cache.SetTreeFilter(&auto.RawViewCondition().map_err(err("raw view"))?).map_err(err("cache filter"))?;
            Ok(Worker { auto, cache, events: Events(signal), watched: Vec::new(), elements: Vec::new(), focus_registered: false })
        }
    }

    /// The trees of `windows`, registering for each one's events the first time (and again when its
    /// root element changed: a provider that replaced a placeholder root).
    fn snapshot(&mut self, windows: &[isize]) -> Result<Tree> {
        let mut tree = Tree { nodes: Vec::new(), windows: Vec::new(), at_ns: crate::host::now_ns() };
        self.elements.clear();
        unsafe {
            if !self.focus_registered {
                let c = self.auto.CreateCacheRequest().map_err(err("cache request"))?;
                let _ = c.AddProperty(UIA_ProcessIdPropertyId);
                let h: IUIAutomationFocusChangedEventHandler = self.events.clone().into();
                self.auto.AddFocusChangedEventHandler(&c, &h).map_err(err("focus events"))?;
                self.focus_registered = true;
            }
            for &hwnd in windows {
                let Ok(root) = self.auto.ElementFromHandleBuildCache(HWND(hwnd as _), &self.cache) else { continue };
                let same = self.watched.iter().find(|(h, _)| *h == hwnd).is_some_and(|(_, old)| self.auto.CompareElements(old, &root).is_ok_and(|b| b.as_bool()));
                if !same {
                    let s: IUIAutomationStructureChangedEventHandler = self.events.clone().into();
                    let p: IUIAutomationPropertyChangedEventHandler = self.events.clone().into();
                    self.auto.AddStructureChangedEventHandler(&root, TreeScope_Subtree, None, &s).map_err(err("structure events"))?;
                    self.auto.AddPropertyChangedEventHandlerNativeArray(&root, TreeScope_Subtree, None, &p, WATCHED).map_err(err("property events"))?;
                    self.watched.retain(|(h, _)| *h != hwnd);
                    self.watched.push((hwnd, root.clone()));
                }
                let w = tree.windows.len();
                tree.windows.push(hwnd);
                let mut stack = vec![(root, None)];
                while let Some((e, parent)) = stack.pop() {
                    if tree.nodes.len() >= MAX_NODES {
                        break;
                    }
                    let i = tree.nodes.len();
                    tree.nodes.push(read(&e, parent, w));
                    if let Ok(kids) = e.GetCachedChildren() {
                        let n = kids.Length().unwrap_or(0);
                        // Reversed onto the stack, so they come off in order: preorder.
                        for k in (0..n).rev() {
                            if let Ok(c) = kids.GetElement(k) {
                                stack.push((c, Some(i)));
                            }
                        }
                    }
                    self.elements.push(e);
                }
            }
        }
        Ok(tree)
    }

    fn hit(&self, i: usize, x: f64, y: f64) -> Result<std::result::Result<(), String>> {
        let target = self.elements.get(i).ok_or_else(|| Error::msg("hit test: no such element in the last snapshot"))?;
        unsafe {
            let at = self.auto.ElementFromPoint(POINT { x: x.round() as i32, y: y.round() as i32 }).map_err(err("hit test"))?;
            let walker = self.auto.RawViewWalker().map_err(err("tree walker"))?;
            let mut e = Some(at.clone());
            for _ in 0..64 {
                let Some(cur) = e else { break };
                if self.auto.CompareElements(&cur, target).is_ok_and(|b| b.as_bool()) {
                    return Ok(Ok(()));
                }
                e = walker.GetParentElement(&cur).ok();
            }
            // A provider that hit-tests no finer than a container answers with one of the target's
            // ancestors: the point is still on the target.
            let mut e = walker.GetParentElement(target).ok();
            let pid = target.CachedProcessId().unwrap_or(-1);
            for _ in 0..64 {
                // Up to the app's own top-level element (the desktop above it holds every point).
                let Some(cur) = e.filter(|c| c.CurrentProcessId().is_ok_and(|p| p == pid)) else { break };
                if self.auto.CompareElements(&cur, &at).is_ok_and(|b| b.as_bool()) {
                    return Ok(Ok(()));
                }
                e = walker.GetParentElement(&cur).ok();
            }
            let name = at.CurrentName().map(|b| b.to_string()).unwrap_or_default();
            let kind = at.CurrentLocalizedControlType().map(|b| b.to_string()).unwrap_or_default();
            let pid = at.CurrentProcessId().unwrap_or(0);
            Ok(Err(format!("{kind} {name:?} (process {pid})")))
        }
    }

    fn act(&self, i: usize, action: &Action) -> Result<u64> {
        let e = self.elements.get(i).ok_or_else(|| Error::msg("act: no such element in the last snapshot"))?;
        unsafe {
            Ok(match action {
                Action::Invoke => {
                    let p: IUIAutomationInvokePattern = e.GetCurrentPatternAs(UIA_InvokePatternId).map_err(missing("Invoke"))?;
                    let t = crate::host::now_ns();
                    p.Invoke().map_err(err("Invoke"))?;
                    t
                }
                Action::Toggle => {
                    let p: IUIAutomationTogglePattern = e.GetCurrentPatternAs(UIA_TogglePatternId).map_err(missing("Toggle"))?;
                    let t = crate::host::now_ns();
                    p.Toggle().map_err(err("Toggle"))?;
                    t
                }
                Action::SetValue(v) => {
                    let p: IUIAutomationValuePattern = e.GetCurrentPatternAs(UIA_ValuePatternId).map_err(missing("Value"))?;
                    let t = crate::host::now_ns();
                    p.SetValue(&BSTR::from(v.as_str())).map_err(err("SetValue"))?;
                    t
                }
                Action::Expand | Action::Collapse => {
                    let p: IUIAutomationExpandCollapsePattern = e.GetCurrentPatternAs(UIA_ExpandCollapsePatternId).map_err(missing("ExpandCollapse"))?;
                    let t = crate::host::now_ns();
                    if matches!(action, Action::Expand) { p.Expand() } else { p.Collapse() }.map_err(err("ExpandCollapse"))?;
                    t
                }
                Action::Select => {
                    let p: IUIAutomationSelectionItemPattern = e.GetCurrentPatternAs(UIA_SelectionItemPatternId).map_err(missing("SelectionItem"))?;
                    let t = crate::host::now_ns();
                    p.Select().map_err(err("Select"))?;
                    t
                }
                Action::ScrollIntoView => {
                    let p: IUIAutomationScrollItemPattern = e.GetCurrentPatternAs(UIA_ScrollItemPatternId).map_err(missing("ScrollItem"))?;
                    let t = crate::host::now_ns();
                    p.ScrollIntoView().map_err(err("ScrollIntoView"))?;
                    t
                }
                Action::Scroll(n) => {
                    let p: IUIAutomationScrollPattern = e.GetCurrentPatternAs(UIA_ScrollPatternId).map_err(missing("Scroll"))?;
                    let amount = match n {
                        1 => ScrollAmount_SmallIncrement,
                        2.. => ScrollAmount_LargeIncrement,
                        -1 => ScrollAmount_SmallDecrement,
                        _ => ScrollAmount_LargeDecrement,
                    };
                    let t = crate::host::now_ns();
                    p.Scroll(ScrollAmount_NoAmount, amount).map_err(err("Scroll"))?;
                    t
                }
                Action::Focus => {
                    let t = crate::host::now_ns();
                    e.SetFocus().map_err(err("SetFocus"))?;
                    t
                }
            })
        }
    }
}

fn missing(what: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |_| Error::usage(format!("the element has no {what} pattern in its accessibility tree"))
}

fn bool_of(e: &IUIAutomationElement, p: UIA_PROPERTY_ID) -> bool {
    unsafe { e.GetCachedPropertyValue(p) }.ok().and_then(|v| bool::try_from(&v).ok()).unwrap_or(false)
}

/// One element's cached properties.
unsafe fn read(e: &IUIAutomationElement, parent: Option<usize>, window: usize) -> Node {
    unsafe {
        let s = |r: windows::core::Result<BSTR>| r.map(|b| b.to_string()).unwrap_or_default();
        let r = e.CachedBoundingRectangle().unwrap_or_default();
        let value = e.GetCachedPropertyValue(UIA_ValueValuePropertyId).ok().and_then(|v| BSTR::try_from(&v).ok()).map(|b| b.to_string());
        let pattern = |p: UIA_PROPERTY_ID, bit: u32| if bool_of(e, p) { bit } else { 0 };
        let patterns = pattern(UIA_IsInvokePatternAvailablePropertyId, PATTERN_INVOKE)
            | pattern(UIA_IsTogglePatternAvailablePropertyId, PATTERN_TOGGLE)
            | pattern(UIA_IsValuePatternAvailablePropertyId, PATTERN_VALUE)
            | pattern(UIA_IsExpandCollapsePatternAvailablePropertyId, PATTERN_EXPAND)
            | pattern(UIA_IsSelectionItemPatternAvailablePropertyId, PATTERN_SELECT)
            | pattern(UIA_IsScrollPatternAvailablePropertyId, PATTERN_SCROLL)
            | pattern(UIA_IsScrollItemPatternAvailablePropertyId, PATTERN_SCROLL_ITEM);
        let heading = e.GetCachedPropertyValue(UIA_HeadingLevelPropertyId).ok().and_then(|v| i32::try_from(&v).ok()).is_some_and(|l| l != HeadingLevel_None.0 && l != 0);
        let role = e.CachedControlType().map(|c| c.0).unwrap_or(0);
        let toggled = if patterns & PATTERN_TOGGLE != 0 {
            Some(e.GetCachedPropertyValue(UIA_ToggleToggleStatePropertyId).ok().and_then(|v| i32::try_from(&v).ok()) == Some(ToggleState_On.0))
        } else {
            // A radio button is checked through SelectionItem.
            (role == UIA_RadioButtonControlTypeId.0 && patterns & PATTERN_SELECT != 0).then(|| bool_of(e, UIA_SelectionItemIsSelectedPropertyId))
        };
        Node {
            id: s(e.CachedAutomationId()),
            name: s(e.CachedName()),
            role,
            role_text: s(e.CachedLocalizedControlType()),
            value: value.filter(|_| patterns & PATTERN_VALUE != 0),
            rect: [r.left as f64, r.top as f64, (r.right - r.left) as f64, (r.bottom - r.top) as f64],
            focused: e.CachedHasKeyboardFocus().is_ok_and(|b| b.as_bool()),
            enabled: e.CachedIsEnabled().is_ok_and(|b| b.as_bool()),
            offscreen: e.CachedIsOffscreen().is_ok_and(|b| b.as_bool()),
            heading,
            dialog: bool_of(e, UIA_IsDialogPropertyId),
            patterns,
            toggled,
            hwnd: e.CachedNativeWindowHandle().map(|h| h.0 as isize).unwrap_or(0),
            parent,
            window,
        }
    }
}

impl Accessibility for Uia {
    fn snapshot(&self, windows: &[isize]) -> Result<Tree> {
        let w = windows.to_vec();
        self.call(move |s| s.snapshot(&w))
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
        self.call(move |s| s.hit(i, x, y))
    }

    fn act(&self, i: usize, action: &Action) -> Result<u64> {
        let a = action.clone();
        self.call(move |s| s.act(i, &a))
    }

    fn set_pids(&self, pids: &[u32]) {
        *self.signal.pids.lock().unwrap_or_else(|e| e.into_inner()) = pids.to_vec();
    }
}
