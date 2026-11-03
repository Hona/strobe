//! A native app's accessibility tree (SPEC §13.7): what any app with OS accessibility exposes, read
//! by the OS's own client API (Windows UI Automation), so strobe finds elements by the selectors bench
//! files use without the app knowing strobe.
//!
//! Selectors: an element key or a key path (`"slider/knob"`: the last segment is the element's
//! AutomationId, earlier ones the AutomationIds of its nearest ancestors that have one), `role=<x>`
//! (control type), `text=<x>` (name, substring unless exact), each with an optional `:focus` suffix.
//! Only elements a person could see match: non-empty bounds inside their clipping ancestors, not
//! offscreen. The first match in tree order wins.

#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod uia;

use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::time::Instant;

/// One element of a snapshot. Rects are screen physical pixels: x, y, width, height.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub id: String,
    pub name: String,
    /// The control type (UIA ControlType id).
    pub role: i32,
    /// The OS's own role text for it (LocalizedControlType).
    pub role_text: String,
    pub value: Option<String>,
    pub rect: [f64; 4],
    pub focused: bool,
    pub enabled: bool,
    pub offscreen: bool,
    pub heading: bool,
    pub dialog: bool,
    /// Patterns it supports (`PATTERN_*` bits).
    pub patterns: u32,
    pub toggled: Option<bool>,
    /// The element's own window, when it is one (a classic control); 0 otherwise.
    pub hwnd: isize,
    pub parent: Option<usize>,
    /// Index into `Tree::windows`.
    pub window: usize,
}

pub const PATTERN_INVOKE: u32 = 1;
pub const PATTERN_TOGGLE: u32 = 2;
pub const PATTERN_VALUE: u32 = 4;
pub const PATTERN_EXPAND: u32 = 8;
pub const PATTERN_SELECT: u32 = 16;
pub const PATTERN_SCROLL: u32 = 32;
pub const PATTERN_SCROLL_ITEM: u32 = 64;

/// The app's top-level windows' trees, in the order asked for (the main window first), each in tree
/// order (preorder).
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub windows: Vec<isize>,
    /// When the snapshot was taken (host clock).
    pub at_ns: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Base {
    Key(Vec<String>),
    Role(String),
    Text(String),
    /// `{ text }` alone: any element holding that text.
    Any,
}

/// A parsed selector: a base, then the optional `{ text, exact }` filter and `:focus`.
#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    base: Base,
    exact: bool,
    focus: bool,
    text: Option<String>,
    pub source: String,
}

impl Selector {
    /// A selector string, or `{ selector, text?, exact? }`.
    pub fn parse(v: &Value) -> Result<Selector> {
        let (s, text, exact) = match v {
            Value::String(s) => (s.clone(), None, false),
            Value::Object(o) => {
                (o.get("selector").and_then(Value::as_str).unwrap_or("").to_string(), o.get("text").and_then(Value::as_str).map(str::to_string), o.get("exact") == Some(&Value::Bool(true)))
            }
            _ => return Err(Error::usage(format!("a native selector is a string or {{ selector, text, exact }}, not {v}"))),
        };
        let source = s.clone();
        let (s, focus) = match s.strip_suffix(":focus") {
            Some(rest) => (rest.to_string(), true),
            None => (s, false),
        };
        let s = s.trim();
        if s.starts_with('[') || s.contains(">>>") || s.contains(char::is_whitespace) && !s.starts_with("text=") {
            return Err(Error::usage(format!(
                "selector {source:?}: a native app's elements are found through its accessibility tree: an element key or key path (\"nav-history\", \"slider/knob\"), role=<role>, text=<text>, with an optional :focus"
            )));
        }
        let base = if let Some(r) = s.strip_prefix("role=") {
            if role_id(r).is_none() {
                return Err(Error::usage(format!("selector {source:?}: unknown role {r:?}; roles: {}", ROLES.iter().map(|r| r.0).collect::<Vec<_>>().join(", "))));
            }
            Base::Role(r.to_ascii_lowercase())
        } else if let Some(t) = s.strip_prefix("text=") {
            Base::Text(t.to_string())
        } else if s.is_empty() {
            if text.is_none() && !focus {
                return Err(Error::usage("an empty selector matches nothing"));
            }
            Base::Any
        } else {
            Base::Key(s.split('/').map(str::to_string).collect())
        };
        Ok(Selector { base, exact, focus, text, source })
    }

    /// Whether node `i` of `t` matches, visibility aside.
    pub fn matches(&self, t: &Tree, i: usize) -> bool {
        let n = &t.nodes[i];
        let ok = match &self.base {
            Base::Key(path) => key_matches(t, i, path),
            Base::Role(r) => role_matches(n, r),
            Base::Text(s) => text_matches(n, s, self.exact),
            Base::Any => true,
        };
        ok && (!self.focus || n.focused) && self.text.as_ref().is_none_or(|s| text_matches(n, s, self.exact))
    }
}

/// `path`'s last segment is the node's id; earlier ones are, nearest first, the ids of its nearest
/// ancestors that have one (unkeyed ancestors skipped).
fn key_matches(t: &Tree, i: usize, path: &[String]) -> bool {
    let Some((last, up)) = path.split_last() else { return false };
    if t.nodes[i].id != *last {
        return false;
    }
    let mut at = t.nodes[i].parent;
    for want in up.iter().rev() {
        loop {
            let Some(p) = at else { return false };
            at = t.nodes[p].parent;
            if !t.nodes[p].id.is_empty() {
                if t.nodes[p].id != *want {
                    return false;
                }
                break;
            }
        }
    }
    true
}

fn text_matches(n: &Node, s: &str, exact: bool) -> bool {
    let hit = |h: &str| if exact { h.trim() == s.trim() } else { h.contains(s) };
    hit(&n.name) || n.value.as_deref().is_some_and(hit)
}

/// Role names (ARIA's where they exist) and the UIA control types they mean.
pub const ROLES: &[(&str, i32)] = &[
    ("button", 50000),
    ("calendar", 50001),
    ("checkbox", 50002),
    ("combobox", 50003),
    ("textbox", 50004),
    ("edit", 50004),
    ("link", 50005),
    ("image", 50006),
    ("listitem", 50007),
    ("list", 50008),
    ("menu", 50009),
    ("menubar", 50010),
    ("menuitem", 50011),
    ("progressbar", 50012),
    ("radio", 50013),
    ("scrollbar", 50014),
    ("slider", 50015),
    ("spinbutton", 50016),
    ("status", 50017),
    ("tablist", 50018),
    ("tab", 50019),
    ("text", 50020),
    ("toolbar", 50021),
    ("tooltip", 50022),
    ("tree", 50023),
    ("treeitem", 50024),
    ("custom", 50025),
    ("group", 50026),
    ("grid", 50028),
    ("row", 50029),
    ("document", 50030),
    ("window", 50032),
    ("pane", 50033),
    ("table", 50036),
    ("separator", 50038),
    // Roles a control type alone does not say.
    ("switch", 50000),
    ("heading", 50020),
    ("dialog", 50032),
];

pub fn role_id(name: &str) -> Option<i32> {
    let n = name.to_ascii_lowercase();
    ROLES.iter().find(|r| r.0 == n).map(|r| r.1)
}

/// The role name for a control type (the first one in `ROLES`).
pub fn role_name(id: i32) -> &'static str {
    ROLES.iter().find(|r| r.1 == id).map_or("", |r| r.0)
}

fn role_matches(n: &Node, r: &str) -> bool {
    match r {
        "switch" => n.role == 50000 && n.patterns & PATTERN_TOGGLE != 0,
        "heading" => n.heading,
        "dialog" => n.dialog,
        _ => role_id(r) == Some(n.role) || n.role_text.eq_ignore_ascii_case(r),
    }
}

impl Tree {
    /// The part of node `i` a person could see: its rect clipped by every ancestor with a rect;
    /// None when empty or offscreen.
    pub fn visible_rect(&self, i: usize) -> Option<[f64; 4]> {
        let n = &self.nodes[i];
        if n.offscreen || n.rect[2] <= 0.0 || n.rect[3] <= 0.0 {
            return None;
        }
        let [mut x0, mut y0] = [n.rect[0], n.rect[1]];
        let [mut x1, mut y1] = [n.rect[0] + n.rect[2], n.rect[1] + n.rect[3]];
        let mut at = n.parent;
        while let Some(p) = at {
            let r = self.nodes[p].rect;
            if r[2] > 0.0 && r[3] > 0.0 {
                x0 = x0.max(r[0]);
                y0 = y0.max(r[1]);
                x1 = x1.min(r[0] + r[2]);
                y1 = y1.min(r[1] + r[3]);
            }
            at = self.nodes[p].parent;
        }
        (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then_some([x0, y0, x1 - x0, y1 - y0])
    }

    /// Visible matches in tree order.
    pub fn find(&self, sel: &Selector) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| sel.matches(self, i) && self.visible_rect(i).is_some()).collect()
    }

    /// The node's key path: the ids of it and its keyed ancestors, root first.
    pub fn key_path(&self, i: usize) -> String {
        let mut parts = Vec::new();
        let mut at = Some(i);
        while let Some(p) = at {
            if !self.nodes[p].id.is_empty() {
                parts.push(self.nodes[p].id.as_str());
            }
            at = self.nodes[p].parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Whether `inner` is `outer` or inside it.
    pub fn within(&self, inner: usize, outer: usize) -> bool {
        let mut at = Some(inner);
        while let Some(p) = at {
            if p == outer {
                return true;
            }
            at = self.nodes[p].parent;
        }
        false
    }

    /// The node and its descendants hold keyboard focus.
    pub fn focus_within(&self, i: usize) -> bool {
        (i..self.nodes.len()).take_while(|&j| j == i || self.within(j, i)).any(|j| self.nodes[j].focused)
    }

    /// What `query` returns for node `i`.
    pub fn describe(&self, i: usize) -> Value {
        let n = &self.nodes[i];
        json!({
            "key": n.id, "path": self.key_path(i), "role": if role_name(n.role).is_empty() { n.role_text.as_str() } else { role_name(n.role) }, "name": n.name, "value": n.value,
            "rect": self.visible_rect(i).unwrap_or(n.rect), "focused": n.focused, "enabled": n.enabled, "window": self.windows.get(n.window),
        })
    }
}

/// Changes the app's accessibility tree announced (structure, property and focus events): how many
/// so far, and when the last arrived (host clock).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Changes {
    pub count: u64,
    pub last_ns: u64,
}

/// What a pattern does instead of pointer or keyboard input (SPEC §13.7: it skips hit testing and
/// pointer handling, so its latency is never an interaction).
#[derive(Clone, Debug)]
pub enum Action {
    Invoke,
    Toggle,
    SetValue(String),
    Expand,
    Collapse,
    Select,
    ScrollIntoView,
    /// Vertical scroll by small (1) or large (2) steps; negative is up.
    Scroll(i32),
    Focus,
}

impl Action {
    pub fn parse(name: &str, value: Option<&str>) -> Result<Action> {
        Ok(match name {
            "invoke" => Action::Invoke,
            "toggle" => Action::Toggle,
            "setValue" | "value" => Action::SetValue(value.unwrap_or("").to_string()),
            "expand" => Action::Expand,
            "collapse" => Action::Collapse,
            "select" => Action::Select,
            "scrollIntoView" => Action::ScrollIntoView,
            "focus" => Action::Focus,
            other => return Err(Error::usage(format!("page.act({other}): actions are invoke, toggle, setValue, expand, collapse, select, scrollIntoView, focus"))),
        })
    }
}

/// An OS accessibility client: Windows UI Automation now; macOS AX and Linux AT-SPI refuse.
pub trait Accessibility {
    /// The trees of these top-level windows (main first). It registers for their change events once.
    fn snapshot(&self, windows: &[isize]) -> Result<Tree>;
    /// Changes announced so far.
    fn changes(&self) -> Changes;
    /// When each change arrived (host clock), the newest 65,536.
    fn change_times(&self) -> Vec<u64>;
    /// Waits until more than `seen` changes were announced or `deadline` passes.
    fn wait_change(&self, seen: u64, deadline: Instant) -> Changes;
    /// Whether the element the OS hit-tests at a screen point is node `i` of the last snapshot or
    /// inside it; else the name of what is there.
    fn hit(&self, i: usize, x: f64, y: f64) -> Result<std::result::Result<(), String>>;
    /// Does `action` on node `i` of the last snapshot through its pattern; returns when (host clock).
    fn act(&self, i: usize, action: &Action) -> Result<u64>;
    /// The app's processes now (focus events come from the whole desktop; children start late).
    fn set_pids(&self, pids: &[u32]);
}

/// The OS's accessibility client for an app's processes.
pub fn open(pids: &[u32]) -> Result<Box<dyn Accessibility>> {
    #[cfg(windows)]
    {
        Ok(Box::new(uia::Uia::start(pids)?))
    }
    #[cfg(not(windows))]
    {
        let _ = pids;
        Err(Error::usage(if cfg!(target_os = "macos") {
            "driving a native app through accessibility (AX) is not built on macOS yet: strobe measures it from outside only"
        } else {
            "driving a native app through accessibility (AT-SPI) is not built on Linux yet: strobe measures it from outside only"
        }))
    }
}
