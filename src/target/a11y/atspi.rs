//! Linux AT-SPI as strobe's accessibility client, over the accessibility bus with strobe's own D-Bus
//! client (`host::dbus`, pure Rust). The registry lists each app's root; a snapshot reads the app's
//! windows breadth first, one level at a time, sending every element's calls of a level before
//! reading their replies (properties, role, states, interfaces, children, bounds), so a tree costs a
//! round trip per level rather than per element. The app's events (children, property, state, text,
//! bounds, selection, focus and window changes) arrive on a second connection and wake waiters.
//!
//! Toolkits emit events only for what a client registered with the registry, and some (AccessKit,
//! Qt) expose a tree only while the session says assistive technology is on (`org.a11y.Status`
//! `IsEnabled`): strobe registers its events, and turns `IsEnabled` on while it drives an app when it
//! was off, putting it back when done.
//!
//! Bounds are read relative to the element's window and placed on the screen from that window's X11
//! client origin: toolkits that cannot know their window's position (GTK 4) still give it right.

use super::{Accessibility, Action, Changes, Node, PATTERN_EXPAND, PATTERN_INVOKE, PATTERN_SCROLL, PATTERN_SCROLL_ITEM, PATTERN_SELECT, PATTERN_TOGGLE, PATTERN_VALUE, Tree};
use crate::host::dbus::{Arg, Conn, Value};
use crate::model::{Error, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const NULL: &str = "/org/a11y/atspi/null";
const ACC: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const PROPS: &str = "org.freedesktop.DBus.Properties";

/// A snapshot stops here: no app's real tree is this large, and a runaway provider cannot stall strobe.
const MAX_NODES: usize = 50_000;
/// Calls in flight before strobe reads their replies (the bus queues the rest).
const BATCH: usize = 1024;

// AT-SPI states (bit numbers of `GetState`'s two words).
const ENABLED: u32 = 8;
const EXPANDABLE: u32 = 9;
const EXPANDED: u32 = 10;
const FOCUSED: u32 = 12;
const CHECKED: u32 = 4;
const PRESSED: u32 = 20;
const SELECTABLE: u32 = 22;
const SENSITIVE: u32 = 24;
const SHOWING: u32 = 25;
const CHECKABLE: u32 = 41;

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

/// An element: its connection's unique name and object path.
type Ref = (String, String);

struct Worker {
    bus: Conn,
    /// Each app connection's process, once asked.
    pids: HashMap<String, u32>,
    /// The last snapshot's elements by node index, and back.
    elements: Vec<Ref>,
    index: HashMap<Ref, usize>,
    /// Per snapshot window: its root element and the screen offset its nodes' window coordinates
    /// were placed by (None: screen coordinates were read).
    windows: Vec<(Ref, Option<(i32, i32)>)>,
    last: Tree,
}

pub struct Atspi {
    worker: Mutex<Worker>,
    signal: Arc<Signal>,
    stop: Arc<AtomicBool>,
    events: Option<std::thread::JoinHandle<()>>,
    /// The session bus, when strobe turned `IsEnabled` on and must turn it off again.
    restore: Mutex<Option<Conn>>,
}

fn err(what: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::msg(format!("AT-SPI {what}: {e}"))
}

/// The events a waiter wakes on (the registry's names for them).
const EVENTS: &[&str] = &[
    "object:children-changed",
    "object:property-change",
    "object:state-changed",
    "object:text-changed",
    "object:bounds-changed",
    "object:selection-changed",
    "object:active-descendant-changed",
    "object:value-changed",
    "window:",
    "focus:",
];

/// Starts the session's accessibility bus if it is not running yet (asking for its address does).
pub fn wake_bus() {
    if let Ok(mut s) = Conn::session() {
        let _ = s.call("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", &[]);
    }
}

impl Atspi {
    pub fn start(pids: &[u32]) -> Result<Atspi> {
        let mut session = Conn::session().map_err(|e| {
            Error::msg(format!("AT-SPI: no session D-Bus ({e}); the accessibility bus is found through it (DBUS_SESSION_BUS_ADDRESS or $XDG_RUNTIME_DIR/bus)"))
        })?;
        let addr = session
            .call("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", &[])
            .ok()
            .and_then(|v| v.first().and_then(Value::as_str).map(str::to_string))
            .ok_or_else(|| Error::msg("AT-SPI: the session has no accessibility bus (at-spi2-core's at-spi-bus-launcher answers org.a11y.Bus): install at-spi2-core, or the app exposes no tree"))?;
        let enabled = matches!(session.get_property("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled"), Ok(Value::Bool(true)));
        let mut restore = None;
        if !enabled
            && session
                .call("org.a11y.Bus", "/org/a11y/bus", PROPS, "Set", &[Arg::Str("org.a11y.Status"), Arg::Str("IsEnabled"), Arg::Var(Box::new(Arg::Bool(true)))])
                .is_ok()
        {
            restore = Some(session);
        }
        let mut bus = Conn::open(&addr, false).map_err(err("connect to the accessibility bus"))?;
        bus.set_timeout(Duration::from_secs(30)).map_err(err("timeout"))?;
        for ev in EVENTS {
            let full = bus.call(REGISTRY, "/org/a11y/atspi/registry", "org.a11y.atspi.Registry", "RegisterEvent", &[Arg::Str(ev), Arg::Strs(&[]), Arg::Str("")]);
            if full.is_err() {
                let _ = bus.call(REGISTRY, "/org/a11y/atspi/registry", "org.a11y.atspi.Registry", "RegisterEvent", &[Arg::Str(ev)]);
            }
        }
        let mut ev = Conn::open(&addr, false).map_err(err("connect to the accessibility bus"))?;
        for iface in ["Object", "Window", "Focus"] {
            ev.add_match(&format!("type='signal',interface='org.a11y.atspi.Event.{iface}'")).map_err(err("event match"))?;
        }
        let signal = Arc::new(Signal::default());
        *signal.pids.lock().unwrap_or_else(|e| e.into_inner()) = pids.to_vec();
        let stop = Arc::new(AtomicBool::new(false));
        let (sig, st) = (signal.clone(), stop.clone());
        let events = std::thread::Builder::new()
            .name("strobe-a11y".into())
            .spawn(move || listen(ev, &sig, &st))
            .map_err(|e| Error::msg(format!("start the accessibility thread: {e}")))?;
        let worker = Worker { bus, pids: HashMap::new(), elements: Vec::new(), index: HashMap::new(), windows: Vec::new(), last: Tree::default() };
        Ok(Atspi { worker: Mutex::new(worker), signal, stop, events: Some(events), restore: Mutex::new(restore) })
    }

    fn with<R>(&self, f: impl FnOnce(&mut Worker) -> Result<R>) -> Result<R> {
        f(&mut self.worker.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for Atspi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.events.take() {
            let _ = t.join();
        }
        if let Some(mut s) = self.restore.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = s.call("org.a11y.Bus", "/org/a11y/bus", PROPS, "Set", &[Arg::Str("org.a11y.Status"), Arg::Str("IsEnabled"), Arg::Var(Box::new(Arg::Bool(false)))]);
        }
    }
}

/// The event thread: each signal from one of the app's connections is a change. Caret moves and
/// text selections are not (a person's caret blinking or moving changes nothing the app shows).
fn listen(mut ev: Conn, signal: &Signal, stop: &AtomicBool) {
    let mut owners: HashMap<String, u32> = HashMap::new();
    while !stop.load(Ordering::Relaxed) {
        let s = match ev.signal(Duration::from_millis(200)) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if matches!(s.member.as_str(), "TextCaretMoved" | "TextSelectionChanged") {
            continue;
        }
        let pid = match owners.get(&s.sender) {
            Some(p) => *p,
            None => {
                let p = process_of(&mut ev, &s.sender).unwrap_or(0);
                owners.insert(s.sender.clone(), p);
                p
            }
        };
        if signal.pids.lock().unwrap_or_else(|e| e.into_inner()).contains(&pid) {
            signal.bump();
        }
    }
}

fn process_of(bus: &mut Conn, name: &str) -> Option<u32> {
    bus.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "GetConnectionUnixProcessID", &[Arg::Str(name)]).ok()?.first()?.as_i64().map(|p| p as u32)
}

fn obj(v: &Value) -> Option<Ref> {
    let f = v.fields();
    Some((f.first()?.as_str()?.to_string(), f.get(1)?.as_str()?.to_string()))
}

/// One element as read, before it is placed in tree order.
#[derive(Default)]
struct Raw {
    at: Ref,
    parent: Option<usize>,
    name: String,
    id: String,
    role: u32,
    states: u64,
    ifaces: Vec<String>,
    kids: Vec<Ref>,
    /// Window-relative bounds (and for top-level windows, screen bounds too).
    rect: Option<[i32; 4]>,
    screen: Option<[i32; 4]>,
    value: Option<String>,
}

impl Raw {
    fn has(&self, iface: &str) -> bool {
        self.ifaces.iter().any(|i| i == iface)
    }

    fn state(&self, bit: u32) -> bool {
        self.states >> bit & 1 == 1
    }
}

fn extents(v: &[Value]) -> Option<[i32; 4]> {
    let f = v.first()?.fields();
    Some([f.first()?.as_i64()? as i32, f.get(1)?.as_i64()? as i32, f.get(2)?.as_i64()? as i32, f.get(3)?.as_i64()? as i32])
}

impl Worker {
    /// The app roots of `pids` the registry lists.
    fn apps(&mut self, pids: &[u32]) -> Result<Vec<Ref>> {
        let kids = self.bus.call(REGISTRY, ROOT, ACC, "GetChildren", &[]).map_err(err("list apps"))?;
        let mut out = Vec::new();
        for r in kids.first().map(Value::fields).unwrap_or(&[]).iter().filter_map(obj) {
            let pid = match self.pids.get(&r.0) {
                Some(p) => *p,
                None => {
                    let p = process_of(&mut self.bus, &r.0).unwrap_or(0);
                    self.pids.insert(r.0.clone(), p);
                    p
                }
            };
            if pids.contains(&pid) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Sends `calls` (each one element's) and reads their replies in order: Err for a call the
    /// element does not answer.
    fn batch(&mut self, calls: &[(Ref, &str, &str, Vec<Arg>)]) -> Vec<std::io::Result<Vec<Value>>> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(BATCH) {
            let serials: Vec<std::io::Result<u32>> = chunk.iter().map(|(r, iface, member, args)| self.bus.send(&r.0, &r.1, iface, member, args)).collect();
            for s in serials {
                out.push(s.and_then(|s| self.bus.reply(s)));
            }
        }
        out
    }

    /// Reads the app's windows breadth first, one batch of calls per level.
    fn read(&mut self, apps: &[Ref]) -> Result<Vec<Raw>> {
        let tops = self.batch(&apps.iter().map(|a| (a.clone(), ACC, "GetChildren", vec![])).collect::<Vec<_>>());
        let mut raws: Vec<Raw> = Vec::new();
        let mut level: Vec<(Ref, Option<usize>)> = tops.into_iter().flatten().flat_map(|v| v.first().map(Value::fields).unwrap_or(&[]).iter().filter_map(obj).collect::<Vec<_>>()).map(|r| (r, None)).collect();
        let mut depth = 0;
        while !level.is_empty() && raws.len() < MAX_NODES {
            level.truncate(MAX_NODES - raws.len());
            let mut calls = Vec::with_capacity(level.len() * 7);
            for (r, _) in &level {
                calls.push((r.clone(), PROPS, "GetAll", vec![Arg::Str(ACC)]));
                calls.push((r.clone(), ACC, "GetRole", vec![]));
                calls.push((r.clone(), ACC, "GetState", vec![]));
                calls.push((r.clone(), ACC, "GetInterfaces", vec![]));
                calls.push((r.clone(), ACC, "GetChildren", vec![]));
                calls.push((r.clone(), COMPONENT, "GetExtents", vec![Arg::U32(1)]));
                calls.push((r.clone(), COMPONENT, "GetExtents", vec![Arg::U32(0)]));
            }
            let replies = self.batch(&calls);
            let first = raws.len();
            for (k, (r, parent)) in level.iter().enumerate() {
                let got = &replies[k * 7..k * 7 + 7];
                let mut raw = Raw { at: r.clone(), parent: *parent, ..Raw::default() };
                if let Ok(v) = &got[0] {
                    for kv in v.first().map(Value::fields).unwrap_or(&[]) {
                        let f = kv.fields();
                        match (f.first().and_then(Value::as_str), f.get(1)) {
                            (Some("Name"), Some(x)) => raw.name = x.as_str().unwrap_or("").to_string(),
                            (Some("AccessibleId"), Some(x)) => raw.id = x.as_str().unwrap_or("").to_string(),
                            _ => {}
                        }
                    }
                }
                raw.role = got[1].as_ref().ok().and_then(|v| v.first()?.as_i64()).unwrap_or(0) as u32;
                if let Some(words) = got[2].as_ref().ok().and_then(|v| v.first()).map(Value::fields) {
                    raw.states = words.first().and_then(Value::as_i64).unwrap_or(0) as u64 | (words.get(1).and_then(Value::as_i64).unwrap_or(0) as u64) << 32;
                }
                raw.ifaces = got[3].as_ref().ok().and_then(|v| v.first()).map(|a| a.fields().iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
                raw.kids = got[4].as_ref().ok().and_then(|v| v.first()).map(|a| a.fields().iter().filter_map(obj).collect()).unwrap_or_default();
                raw.rect = got[5].as_ref().ok().and_then(|v| extents(v));
                raw.screen = got[6].as_ref().ok().and_then(|v| extents(v));
                raws.push(raw);
            }
            // Text and values of what holds them.
            let wants: Vec<usize> = (first..raws.len()).filter(|&i| raws[i].has("org.a11y.atspi.EditableText") || raws[i].has("org.a11y.atspi.Value")).collect();
            let calls: Vec<(Ref, &str, &str, Vec<Arg>)> = wants
                .iter()
                .map(|&i| {
                    if raws[i].has("org.a11y.atspi.EditableText") {
                        (raws[i].at.clone(), "org.a11y.atspi.Text", "GetText", vec![Arg::I32(0), Arg::I32(-1)])
                    } else {
                        (raws[i].at.clone(), PROPS, "Get", vec![Arg::Str("org.a11y.atspi.Value"), Arg::Str("CurrentValue")])
                    }
                })
                .collect();
            for (&i, r) in wants.iter().zip(self.batch(&calls)) {
                raws[i].value = r.ok().and_then(|v| match v.first()? {
                    Value::Str(s) => Some(s.clone()),
                    x => x.as_f64().map(|d| if d.fract() == 0.0 { format!("{d:.0}") } else { d.to_string() }),
                });
            }
            depth += 1;
            if depth > 256 {
                break;
            }
            level = (first..raws.len()).flat_map(|i| raws[i].kids.iter().map(move |k| (k.clone(), Some(i))).collect::<Vec<_>>()).collect();
        }
        Ok(raws)
    }

    fn snapshot(&mut self, windows: &[isize], pids: &[u32]) -> Result<Tree> {
        let at = crate::host::now_ns();
        let apps = self.apps(pids)?;
        let raws = self.read(&apps)?;
        // Each top-level AT-SPI window placed on its X11 window: the one its screen bounds overlap
        // most, else the one with its title, else none (its own screen bounds are used).
        let roots: Vec<usize> = (0..raws.len()).filter(|&i| raws[i].parent.is_none()).collect();
        let titles = crate::host::top_windows(pids);
        let mut placed: Vec<Option<isize>> = Vec::new();
        for &r in &roots {
            let raw = &raws[r];
            let by_rect = raw.screen.filter(|s| s[2] > 0 && s[3] > 0).and_then(|s| {
                windows
                    .iter()
                    .filter_map(|&w| Some((w, crate::host::window_state(w)?)))
                    .map(|(w, st)| {
                        let (x0, y0, x1, y1) = (s[0].max(st.rect.0), s[1].max(st.rect.1), (s[0] + s[2]).min(st.rect.0 + st.rect.2), (s[1] + s[3]).min(st.rect.1 + st.rect.3));
                        (w, (x1 - x0).max(0) as i64 * (y1 - y0).max(0) as i64)
                    })
                    .filter(|x| x.1 > 0)
                    .max_by_key(|x| x.1)
                    .map(|x| x.0)
            });
            let by_title = || titles.iter().find(|t| !raw.name.is_empty() && t.1 == raw.name).map(|t| t.0).filter(|w| windows.contains(w));
            placed.push(by_rect.or_else(by_title));
        }
        // A lone window that matched nothing is the main one (a toolkit that gives no screen bounds).
        if roots.len() == 1 && placed[0].is_none() && !windows.is_empty() && raws[roots[0]].state(SHOWING) {
            placed[0] = Some(windows[0]);
        }
        // Windows in the order asked for (the main one first), then the ones on no X11 window.
        let mut order: Vec<usize> = (0..roots.len()).collect();
        order.sort_by_key(|&k| placed[k].and_then(|w| windows.iter().position(|x| *x == w)).unwrap_or(usize::MAX));
        let mut tree = Tree { nodes: Vec::new(), windows: Vec::new(), at_ns: at };
        self.elements.clear();
        self.index.clear();
        self.windows.clear();
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); raws.len()];
        for (i, r) in raws.iter().enumerate() {
            if let Some(p) = r.parent {
                kids[p].push(i);
            }
        }
        for k in order {
            let r = roots[k];
            let w = tree.windows.len();
            tree.windows.push(placed[k].unwrap_or(0));
            let origin = placed[k].and_then(crate::host::x11::client_origin);
            self.windows.push((raws[r].at.clone(), origin));
            let mut stack = vec![(r, None)];
            while let Some((i, parent)) = stack.pop() {
                let raw = &raws[i];
                let rect = match (origin, raw.rect, raw.screen) {
                    (Some((ox, oy)), Some(b), _) => [(ox + b[0]) as f64, (oy + b[1]) as f64, b[2] as f64, b[3] as f64],
                    (_, _, Some(b)) => [b[0] as f64, b[1] as f64, b[2] as f64, b[3] as f64],
                    _ => [0.0; 4],
                };
                let n = tree.nodes.len();
                tree.nodes.push(node(raw, rect, parent, w));
                self.index.insert(raw.at.clone(), n);
                self.elements.push(raw.at.clone());
                for &c in kids[i].iter().rev() {
                    stack.push((c, Some(n)));
                }
            }
        }
        self.last = tree.clone();
        Ok(tree)
    }

    /// The top-level window node `i` is in (an index into `windows`).
    fn window_of(&self, i: usize) -> Option<usize> {
        self.last.nodes.get(i).map(|n| n.window)
    }

    fn hit(&mut self, i: usize, x: f64, y: f64) -> Result<std::result::Result<(), String>> {
        let w = self.window_of(i).ok_or_else(|| Error::msg("hit test: no such element in the last snapshot"))?;
        let (root, origin) = self.windows[w].clone();
        let (px, py, coords) = match origin {
            Some((ox, oy)) => (x.round() as i32 - ox, y.round() as i32 - oy, 1u32),
            None => (x.round() as i32, y.round() as i32, 0u32),
        };
        let mut cur = root;
        for _ in 0..64 {
            let Ok(v) = self.bus.call(&cur.0, &cur.1, COMPONENT, "GetAccessibleAtPoint", &[Arg::I32(px), Arg::I32(py), Arg::U32(coords)]) else { break };
            match v.first().and_then(obj) {
                Some(next) if next.1 != NULL && next != cur => cur = next,
                _ => break,
            }
        }
        // A provider may hit-test to an element no child list names (GTK 4's text inside an entry):
        // its nearest ancestor the read had stands for it.
        let first = cur.1.clone();
        let mut at = self.index.get(&cur).copied();
        for _ in 0..16 {
            if at.is_some() {
                break;
            }
            let Some(up) = self.bus.get_property(&cur.0, &cur.1, ACC, "Parent").ok().as_ref().and_then(obj).filter(|p| p.1 != NULL && *p != cur) else { break };
            at = self.index.get(&up).copied();
            cur = up;
        }
        let Some(at) = at else {
            return Ok(Err(format!("an element the last read did not have ({first})")));
        };
        // The element or one inside it; or one of its ancestors, when the provider hit-tests no finer.
        if self.last.within(at, i) || self.last.within(i, at) {
            return Ok(Ok(()));
        }
        let n = &self.last.nodes[at];
        Ok(Err(format!("{} {:?}", n.role_text, n.name)))
    }

    fn act(&mut self, i: usize, action: &Action) -> Result<u64> {
        let el = self.elements.get(i).cloned().ok_or_else(|| Error::msg("act: no such element in the last snapshot"))?;
        let call = |w: &mut Worker, iface: &str, member: &str, args: &[Arg]| -> Result<u64> {
            let t = crate::host::now_ns();
            w.bus.call(&el.0, &el.1, iface, member, args).map_err(|e| Error::msg(format!("AT-SPI {member}: {e}")))?;
            Ok(t)
        };
        match action {
            Action::Invoke => self.do_action(&el, &["click", "press", "activate", "jump", "open"], "Invoke"),
            Action::Toggle => self.do_action(&el, &["toggle", "click", "press", "activate"], "Toggle"),
            Action::Expand | Action::Collapse => {
                let expandable = self.last.nodes[i].patterns & PATTERN_EXPAND != 0;
                let open = expandable && self.expanded(&el);
                if open == matches!(action, Action::Expand) {
                    return Ok(crate::host::now_ns());
                }
                self.do_action(&el, &["expand or contract", "expand", "collapse", "toggle", "open", "click", "press"], "ExpandCollapse")
            }
            Action::SetValue(v) => {
                let ifaces = self.bus.call(&el.0, &el.1, ACC, "GetInterfaces", &[]).ok().and_then(|r| r.first().cloned()).map(|a| a.fields().iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default();
                if ifaces.iter().any(|f| f == "org.a11y.atspi.EditableText") {
                    call(self, "org.a11y.atspi.EditableText", "SetTextContents", &[Arg::Str(v)])
                } else if ifaces.iter().any(|f| f == "org.a11y.atspi.Value") {
                    let d: f64 = v.trim().parse().map_err(|_| Error::usage(format!("setValue {v:?}: this element holds a number")))?;
                    call(self, PROPS, "Set", &[Arg::Str("org.a11y.atspi.Value"), Arg::Str("CurrentValue"), Arg::Var(Box::new(Arg::F64(d)))])
                } else {
                    Err(Error::usage("the element has no editable text or value in its accessibility tree"))
                }
            }
            Action::Select => {
                let parent = self.last.nodes[i].parent.and_then(|p| self.elements.get(p).cloned());
                let index = self.bus.call(&el.0, &el.1, ACC, "GetIndexInParent", &[]).ok().and_then(|v| v.first()?.as_i64());
                if let (Some(p), Some(k)) = (parent, index) {
                    let t = crate::host::now_ns();
                    if self.bus.call(&p.0, &p.1, "org.a11y.atspi.Selection", "SelectChild", &[Arg::I32(k as i32)]).is_ok() {
                        return Ok(t);
                    }
                }
                self.do_action(&el, &["select", "click", "activate"], "SelectionItem")
            }
            // ANYWHERE: the least scrolling that brings it into view.
            Action::ScrollIntoView => call(self, COMPONENT, "ScrollTo", &[Arg::U32(6)]),
            Action::Focus => call(self, COMPONENT, "GrabFocus", &[]),
            Action::Scroll(_) => Err(Error::usage("AT-SPI has no scroll action: scroll with page.scroll or page.wheel")),
        }
    }

    /// What node `i` holds now: an editable text's text, or a value's current value.
    fn value(&mut self, i: usize) -> Result<Option<String>> {
        let el = self.elements.get(i).cloned().ok_or_else(|| Error::msg("value: no such element in the last snapshot"))?;
        let ifaces = self.bus.call(&el.0, &el.1, ACC, "GetInterfaces", &[]).ok().and_then(|r| r.first().cloned()).map(|a| a.fields().iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default();
        let got = if ifaces.iter().any(|f| f == "org.a11y.atspi.EditableText") {
            self.bus.call(&el.0, &el.1, "org.a11y.atspi.Text", "GetText", &[Arg::I32(0), Arg::I32(-1)])
        } else if ifaces.iter().any(|f| f == "org.a11y.atspi.Value") {
            self.bus.call(&el.0, &el.1, PROPS, "Get", &[Arg::Str("org.a11y.atspi.Value"), Arg::Str("CurrentValue")])
        } else {
            return Ok(None);
        };
        Ok(got.ok().and_then(|v| match v.first()? {
            Value::Str(s) => Some(s.clone()),
            x => x.as_f64().map(|d| if d.fract() == 0.0 { format!("{d:.0}") } else { d.to_string() }),
        }))
    }

    fn expanded(&mut self, el: &Ref) -> bool {
        let words = self.bus.call(&el.0, &el.1, ACC, "GetState", &[]).ok().and_then(|v| v.first().cloned());
        words.and_then(|w| w.fields().first().and_then(Value::as_i64)).is_some_and(|s| s >> EXPANDED & 1 == 1)
    }

    /// Runs the element's first action named one of `names`, else its only action.
    fn do_action(&mut self, el: &Ref, names: &[&str], pattern: &str) -> Result<u64> {
        let list = self.bus.call(&el.0, &el.1, "org.a11y.atspi.Action", "GetActions", &[]).map_err(|_| Error::usage(format!("the element has no {pattern} action in its accessibility tree")))?;
        let actions: Vec<String> = list.first().map(Value::fields).unwrap_or(&[]).iter().filter_map(|a| a.fields().first().and_then(Value::as_str).map(str::to_lowercase)).collect();
        let k = names.iter().find_map(|n| actions.iter().position(|a| a == n)).or((actions.len() == 1).then_some(0)).ok_or_else(|| {
            Error::usage(format!("the element has no {pattern} action in its accessibility tree (its actions: {})", if actions.is_empty() { "none".into() } else { actions.join(", ") }))
        })?;
        let t = crate::host::now_ns();
        self.bus.call(&el.0, &el.1, "org.a11y.atspi.Action", "DoAction", &[Arg::I32(k as i32)]).map_err(|e| Error::msg(format!("AT-SPI DoAction: {e}")))?;
        Ok(t)
    }
}

fn node(r: &Raw, rect: [f64; 4], parent: Option<usize>, window: usize) -> Node {
    let role = role_id(r.role);
    let toggle = r.state(CHECKABLE) || matches!(r.role, 7 | 8 | 44 | 45 | 62 | 130);
    let mut patterns = 0;
    if r.has("org.a11y.atspi.Action") {
        patterns |= PATTERN_INVOKE;
    }
    if toggle {
        patterns |= PATTERN_TOGGLE;
    }
    if r.has("org.a11y.atspi.EditableText") || r.has("org.a11y.atspi.Value") {
        patterns |= PATTERN_VALUE;
    }
    if r.state(EXPANDABLE) {
        patterns |= PATTERN_EXPAND;
    }
    if r.state(SELECTABLE) {
        patterns |= PATTERN_SELECT;
    }
    if r.role == 49 {
        patterns |= PATTERN_SCROLL;
    }
    if r.has(COMPONENT) {
        patterns |= PATTERN_SCROLL_ITEM;
    }
    Node {
        id: r.id.clone(),
        name: r.name.clone(),
        role,
        role_text: ROLE_NAMES.get(r.role as usize).copied().unwrap_or("unknown").to_string(),
        value: r.value.clone(),
        rect,
        focused: r.state(FOCUSED),
        enabled: r.state(ENABLED) || r.state(SENSITIVE),
        offscreen: !r.state(SHOWING),
        heading: r.role == 83,
        dialog: matches!(r.role, 2 | 16 | 19),
        patterns,
        toggled: toggle.then(|| r.state(CHECKED) || r.state(PRESSED)),
        hwnd: 0,
        parent,
        window,
    }
}

/// An AT-SPI role as the UIA control type `ROLES` names it.
fn role_id(role: u32) -> i32 {
    match role {
        43 | 62 | 129 | 130 => 50000,
        5 => 50001,
        7 => 50002,
        11 => 50003,
        40 | 60 | 61 | 79 => 50004,
        88 => 50005,
        26 | 27 => 50006,
        32 => 50007,
        31 | 98 => 50008,
        33 | 41 => 50009,
        34 => 50010,
        8 | 35 | 45 | 59 => 50011,
        42 | 103 => 50012,
        44 => 50013,
        48 => 50014,
        15 | 51 => 50015,
        52 => 50016,
        54 => 50017,
        38 => 50018,
        37 => 50019,
        1 | 29 | 73 | 81 | 83 | 116 => 50020,
        63 => 50021,
        64 => 50022,
        65 | 66 => 50023,
        91 => 50024,
        85 | 87 | 99 | 109 | 110 => 50026,
        90 => 50029,
        82 | 92..=96 => 50030,
        2 | 16 | 19 | 23 | 69 => 50032,
        20 | 24 | 28 | 30 | 36 | 39 | 46 | 49 | 53 | 68 => 50033,
        55 => 50036,
        50 => 50038,
        _ => 50025,
    }
}

/// AT-SPI's role names, by role number.
const ROLE_NAMES: &[&str] = &[
    "invalid", "accelerator label", "alert", "animation", "arrow", "calendar", "canvas", "check box", "check menu item", "color chooser", "column header", "combo box", "date editor",
    "desktop icon", "desktop frame", "dial", "dialog", "directory pane", "drawing area", "file chooser", "filler", "focus traversable", "font chooser", "frame", "glass pane",
    "html container", "icon", "image", "internal frame", "label", "layered pane", "list", "list item", "menu", "menu bar", "menu item", "option pane", "page tab", "page tab list",
    "panel", "password text", "popup menu", "progress bar", "push button", "radio button", "radio menu item", "root pane", "row header", "scroll bar", "scroll pane", "separator",
    "slider", "spin button", "split pane", "status bar", "table", "table cell", "table column header", "table row header", "tearoff menu item", "terminal", "text", "toggle button",
    "tool bar", "tool tip", "tree", "tree table", "unknown", "viewport", "window", "extended", "header", "footer", "paragraph", "ruler", "application", "autocomplete", "edit bar",
    "embedded", "entry", "chart", "caption", "document frame", "heading", "page", "section", "redundant object", "form", "link", "input method window", "table row", "tree item",
    "document spreadsheet", "document presentation", "document text", "document web", "document email", "comment", "list box", "grouping", "image map", "notification", "info bar",
    "level bar", "title bar", "block quote", "audio", "video", "definition", "article", "landmark", "log", "marquee", "math", "rating", "timer", "static", "math fraction", "math root",
    "subscript", "superscript", "description list", "description term", "description value", "footnote", "content deletion", "content insertion", "mark", "suggestion",
    "push button menu", "switch",
];

impl Accessibility for Atspi {
    fn snapshot(&self, windows: &[isize]) -> Result<Tree> {
        let pids = self.signal.pids.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.with(|w| w.snapshot(windows, &pids))
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

    fn value(&self, i: usize) -> Result<Option<String>> {
        self.with(|w| w.value(i))
    }

    fn set_pids(&self, pids: &[u32]) {
        *self.signal.pids.lock().unwrap_or_else(|e| e.into_inner()) = pids.to_vec();
    }
}
