//! The `page` driver (SPEC §9.6): inputs over CDP or WebDriver BiDi, event-driven waits, run and phase
//! marks.

pub mod native;

use crate::model::{Error, Result};
use crate::target::a11y::keys::{self, Button, Key, Mods, Named};
use crate::target::a11y::{Dialect, Selector};
use crate::target::bidi;
use crate::target::chromium;
use crate::target::web::WebPage as Page;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// Installed in every document before its scripts run: the browser's own rAF, which a page may replace (a game
/// frame pacer does) and so delay the driver's waits on its own schedule.
pub const NATIVE_RAF: &str = "window.__strobeRaf ??= window.requestAnimationFrame.bind(window)";

/// Installed lazily in the page (it disappears on navigation): the one DOM probe every page reader uses (the
/// driver's selectors and waits, `look`, a block's screen and `expect`), so they agree on what is visible, what a
/// selector matches and what the headings are. Observers only: nothing polls, except a check per frame while a CSS
/// animation or transition runs, since those change what is visible without a mutation. Its work during a run (a
/// wait's checks) is the bench's own and shows in a profile under `strobe-driver.js`, its source URL.
pub(crate) const HELPER: &str = r#"window.__strobe ??= (() => {
  const s = {}
  const raf = window.__strobeRaf ?? window.requestAnimationFrame.bind(window)
  // What a person could see: a box, and not hidden by `visibility`, `opacity: 0` or `content-visibility` on it or an ancestor.
  const visible = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && (el.checkVisibility?.({ checkOpacity: true, checkVisibilityCSS: true }) ?? true) }
  // Shadow roots (a diff or code viewer renders into one): every observer below watches them too, so `idle` and
  // `waitFor` see what changes inside, and `host >>> inner` selects across them. Roots attached before this helper
  // are found once; later ones are caught where they are created.
  const roots = new Set()
  const observers = new Set()
  const watch = (root) => {
    if (roots.has(root)) return
    roots.add(root)
    for (const o of observers) o.mo.observe(root, o.opts)
    for (const el of root.querySelectorAll("*")) if (el.shadowRoot) watch(el.shadowRoot)
  }
  for (const el of document.querySelectorAll("*")) if (el.shadowRoot) watch(el.shadowRoot)
  const attach = Element.prototype.attachShadow
  Element.prototype.attachShadow = function (init) { const root = attach.call(this, init); watch(root); return root }
  const observe = (fn, opts) => {
    const o = { mo: new MutationObserver(fn), opts }
    o.mo.observe(document, opts)
    for (const r of roots) o.mo.observe(r, opts)
    observers.add(o)
    return () => { o.mo.disconnect(); observers.delete(o) }
  }
  const query = (root, [first, ...rest]) => {
    const found = [...root.querySelectorAll(first)]
    return rest.length ? found.flatMap((host) => (host.shadowRoot ? query(host.shadowRoot, rest) : [])) : found
  }
  // An element's ARIA role: its own `role`, else the one its HTML implies.
  const implicit = (el) => {
    const type = (el.getAttribute("type") ?? "").toLowerCase()
    switch (el.localName) {
      case "a": case "area": return el.hasAttribute("href") ? "link" : ""
      case "button": case "summary": return "button"
      case "input": return { button: "button", submit: "button", reset: "button", image: "button", checkbox: "checkbox", radio: "radio", range: "slider", number: "spinbutton", search: "searchbox" }[type]
        ?? (["hidden", "file", "color", "date", "datetime-local", "month", "time", "week"].includes(type) ? "" : "textbox")
      case "textarea": return "textbox"
      case "select": return el.multiple || el.size > 1 ? "listbox" : "combobox"
      case "option": return "option"
      case "h1": case "h2": case "h3": case "h4": case "h5": case "h6": return "heading"
      case "img": return el.getAttribute("alt") === "" ? "presentation" : "img"
      case "ul": case "ol": case "menu": return "list"
      case "li": return "listitem"
      case "nav": return "navigation"
      case "main": return "main"
      case "aside": return "complementary"
      case "header": return "banner"
      case "footer": return "contentinfo"
      case "dialog": return "dialog"
      case "table": return "table"
      case "tr": return "row"
      case "td": return "cell"
      case "th": return "columnheader"
      case "progress": return "progressbar"
      case "hr": return "separator"
      case "form": return "form"
      case "article": return "article"
      case "output": return "status"
    }
    return ""
  }
  const roleOf = (el) => (el.getAttribute("role") ?? "").trim().split(/\s+/)[0].toLowerCase() || implicit(el)
  // The roles a native app's tree names otherwise.
  const alias = { image: "img", edit: "textbox" }
  const isRole = (el, want) => { const r = roleOf(el), w = alias[want] ?? want; return r === w || (w === "textbox" && r === "searchbox") }
  // `text` matches a substring of the element's text, label or value, or all of one (trimmed) when `exact`.
  const has = (el, text, exact) => text == null || [el.textContent ?? "", el.getAttribute("aria-label") ?? "", typeof el.value === "string" ? el.value : ""].some((t) => (exact ? t.trim() === text.trim() : t.includes(text)))
  const up = (el) => el.parentElement ?? el.getRootNode().host ?? null
  // The elements a text alone can match: those with a text node of their own that holds it, and those whose label or
  // value does, in tree order. A walk of the text nodes, not of every element's whole text (the page's, its body's, each
  // container's: large strings on a large page, built on its main thread on every check of a wait).
  const holders = (text, exact) => {
    const out = new Set()
    const w = document.createTreeWalker(document, NodeFilter.SHOW_TEXT)
    for (let n = w.nextNode(); n; n = w.nextNode()) if (n.parentElement && (exact ? n.data.trim() === text.trim() : n.data.includes(text))) out.add(n.parentElement)
    for (const el of document.querySelectorAll("[aria-label], input, textarea, select")) out.add(el)
    return [...out].sort((a, b) => (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1))
  }
  // A selector as strobe parsed it (`{ css, role, text, exact, index, focus }`): CSS (every element without), then
  // its filters, the cheap ones first. With a text, only the innermost visible matches count: every ancestor's text
  // holds it too, and a hidden descendant's (an icon's title, a screen-reader label) must not hide the element. It
  // runs on the page's main thread inside a run whenever a wait checks, so without a text it stops at the match it
  // needs and measures the layout of no element past it.
  s.find = (q) => {
    const bare = q.css == null && q.role == null && q.text != null
    const all = bare ? holders(q.text, q.exact) : query(document, (q.css ?? "*").split(">>>").map((p) => p.trim()))
    const found = all.filter((el) => (q.role == null || isRole(el, q.role)) && (!q.focus || el.matches(":focus")) && has(el, q.text, q.exact))
    let left = q.index ?? 0
    if (q.text == null) {
      for (const el of found) if (visible(el) && left-- === 0) return el
      return null
    }
    // An icon's text (an SVG <title>, drawn nowhere) stands for the nearest visible element around it that holds it
    // too; any other hidden holder (opacity 0, display: none) matches nothing, not an ancestor such as the body.
    const lift = (el) => {
      if (visible(el)) return el
      if (el.namespaceURI !== "http://www.w3.org/2000/svg") return null
      for (let p = up(el); p; p = up(p)) if (visible(p)) return has(p, q.text, q.exact) ? p : null
      return null
    }
    const shown = [...new Set(bare ? found.map(lift).filter(Boolean) : found.filter(visible))]
    const outer = new Set()
    for (const el of shown) for (let p = up(el); p && !outer.has(p); p = up(p)) outer.add(p)
    return shown.filter((el) => !outer.has(el))[left] ?? null
  }
  s.focused = (q) => { const el = s.find(q); return !!el && (el.contains(document.activeElement) || el.matches(":focus-within")) }
  s.box = (q) => {
    const el = s.find(q)
    if (!el) return null
    let r = el.getBoundingClientRect()
    if (r.bottom < 0 || r.top > innerHeight || r.right < 0 || r.left > innerWidth) { el.scrollIntoView({ block: "nearest", inline: "nearest" }); r = el.getBoundingClientRect() }
    return { x: r.left + Math.min(r.width, innerWidth) / 2, y: r.top + Math.min(r.height, innerHeight) / 2, w: r.width, h: r.height }
  }
  // Re-checked on every DOM change and every focus change: focus moves without a mutation, so a
  // `:focus` selector would otherwise never resolve.
  s.waitFor = (q, timeout, label) => new Promise((resolve, reject) => {
    if (s.find(q)) return resolve(true)
    let done = false
    const check = () => { if (!done && s.find(q)) { stop(); clearTimeout(t); resolve(true) } }
    // A fade in (opacity from 0) makes an element visible on a frame, not on a mutation: while any animation or
    // transition runs, check each frame.
    let ticking = false
    const tick = () => {
      check()
      if (!done && document.getAnimations().some((a) => a.playState === "running")) raf(tick)
      else ticking = false
    }
    const animating = () => { if (!ticking && !done) { ticking = true; raf(tick) } }
    const unwatch = observe(() => { check(); animating() }, { subtree: true, childList: true, attributes: true, characterData: q.text != null })
    const events = ["focusin", "focusout", "animationstart", "transitionrun"]
    for (const e of events) document.addEventListener(e, e.startsWith("focus") ? check : animating, true)
    const stop = () => {
      done = true
      unwatch()
      for (const e of events) document.removeEventListener(e, e.startsWith("focus") ? check : animating, true)
    }
    animating()
    const t = setTimeout(() => { stop(); reject(new Error("timed out waiting for " + label)) }, timeout)
  })
  // What a person sees at the top of the page: its title and address, its open dialogs (by their heading), then
  // its top headings (h1, h2, and role=heading at aria-level 1 or 2, its default), at most `max`; and its visible
  // text, at most `chars` of it (none read when 0).
  s.screen = (max, chars) => {
    const text = (e) => (e.getAttribute("aria-label") || e.innerText || e.textContent || "").trim().replace(/\s+/g, " ").slice(0, 80)
    const out = []
    for (const d of document.querySelectorAll("dialog[open], [role=dialog], [role=alertdialog], [aria-modal=true]"))
      if (visible(d)) out.push(text(d.querySelector("h1, h2, h3, [role=heading]") ?? d))
    for (const h of document.querySelectorAll("h1, h2, [role=heading]"))
      if (Number(h.getAttribute("aria-level") ?? (h.localName === "h1" ? 1 : 2)) <= 2 && visible(h)) out.push(text(h))
    const body = chars > 0 ? (document.body?.innerText ?? "").replace(/\n{3,}/g, "\n\n").trim() : ""
    return JSON.stringify({ title: document.title, url: location.href, headings: [...new Set(out.filter(Boolean))].slice(0, max), text: body.slice(0, chars), chars: body.length, ready: document.readyState })
  }
  // `tag#id.class`: a node as a person finds it in DevTools.
  const name = (n) => {
    const el = n && n.nodeType === 1 ? n : n?.parentElement
    if (!el) return '#' + (n?.nodeName ?? "?")
    const cls = typeof el.className === "string" ? el.className.trim().split(/\s+/).filter(Boolean).slice(0, 2).map((c) => "." + c).join("") : ""
    return el.tagName.toLowerCase() + (el.id ? '#' + el.id : "") + cls
  }
  s.idle = (quiet, timeout) => new Promise((resolve, reject) => {
    let timer, po
    // What kept the page busy, for the failure: mutations by node and kind, long frames by script.
    const causes = new Map()
    let mutations = 0, frames = 0, longest = 0
    const count = (key) => causes.set(key, (causes.get(key) ?? 0) + 1)
    const stop = () => { unwatch(); if (po) po.disconnect(); clearTimeout(timer); clearTimeout(limit) }
    const kick = () => { clearTimeout(timer); timer = setTimeout(() => { stop(); resolve(true) }, quiet) }
    const unwatch = observe((records) => {
      for (const r of records) { mutations++; count("mutation " + name(r.target) + (r.type === "attributes" ? " [" + r.attributeName + "]" : r.type === "childList" ? " (children)" : " (text)")) }
      kick()
    }, { subtree: true, childList: true, attributes: true, characterData: true })
    try {
      po = new PerformanceObserver((list) => {
        for (const e of list.getEntries()) {
          frames++
          longest = Math.max(longest, e.duration)
          const top = [...(e.scripts ?? [])].sort((a, b) => b.duration - a.duration)[0]
          count("long frame " + (top ? (top.invoker || "script") + (top.sourceURL ? " (" + top.sourceURL.split("/").pop() + ")" : "") : "(no script: style, layout or paint)"))
        }
        kick()
      })
      po.observe({ type: "long-animation-frame" })
    } catch {}
    const limit = setTimeout(() => {
      stop()
      const top = [...causes].sort((a, b) => b[1] - a[1]).slice(0, 3).map(([k, n]) => k + " ×" + n).join("; ")
      // Lead with what kept it busy: DOM mutations, long animation frames (a frame over 50 ms with no
      // mutation: a spinner's animation on a slow core, a script on a timer), or both.
      const muts = mutations + " DOM mutation" + (mutations === 1 ? "" : "s")
      const longs = frames + " animation frame" + (frames === 1 ? "" : "s") + " over 50 ms (longest " + Math.round(longest) + " ms)"
      const what = !frames ? "the DOM kept changing (" + muts + ", no long frames)"
        : !mutations ? "long frames kept it busy (" + longs + ", no DOM mutations)"
        : "the DOM kept changing and frames ran long (" + muts + "; " + longs + ")"
      reject(new Error("the page never went idle in " + timeout + " ms: " + what + (top ? "; most: " + top : "")))
    }, timeout)
    kick()
  })
  // What a run starts from that its input acts on (`ResetIncomplete`): the value of every visible, enabled and
  // writable text field, search box, combo box, spin button, slider and editing host (a password by its length),
  // and the state of every check box, radio button and switch, by its id, name or label (else its role), in the
  // page and its shadow roots, with the roles a native app's tree gives them.
  s.state = () => {
    const out = {}, seen = {}
    const put = (k, v) => { seen[k] = (seen[k] ?? 0) + 1; out[seen[k] === 1 ? k : k + " #" + seen[k]] = v }
    const toggles = ["checkbox", "radio", "switch", "menuitemcheckbox", "menuitemradio"]
    const values = ["textbox", "searchbox", "combobox", "spinbutton", "slider"]
    const cut = (t) => JSON.stringify(String(t ?? "").trim().slice(0, 200))
    for (const root of [document, ...roots]) for (const el of root.querySelectorAll("input, textarea, select, [role], [contenteditable]")) {
      const role = roleOf(el)
      const host = el.isContentEditable && !el.parentElement?.isContentEditable
      if (!host && !toggles.includes(role) && !values.includes(role)) continue
      if (!visible(el) || el.disabled || el.readOnly || el.getAttribute("aria-readonly") === "true" || el.closest("[inert], [aria-disabled=true]")) continue
      const label = el.getAttribute("aria-label")
      const key = el.id ? '#' + el.id : el.name ? el.localName + "[name=" + el.name + "]" : label ? (role || "editable") + " " + JSON.stringify(label) : role || "editable"
      const aria = el.getAttribute("aria-checked") ?? "false"
      if (toggles.includes(role)) put(key, el.localName === "input" ? (el.checked ? "checked" : "unchecked") : ({ true: "checked", false: "unchecked" })[aria] ?? aria)
      else if (el.type === "password") put(key, el.value.length + " characters")
      else if (typeof el.value === "string" && /^(input|textarea|select)$/.test(el.localName)) put(key, cut(el.value))
      else if (role === "slider" || role === "spinbutton") put(key, cut(el.getAttribute("aria-valuetext") ?? el.getAttribute("aria-valuenow")))
      else put(key, cut(el.innerText ?? el.textContent))
    }
    return JSON.stringify(out)
  }
  // True at the first change input could make (the DOM, an input or change event), false after `timeout` ms.
  s.change = (timeout) => new Promise((resolve) => {
    const events = ["input", "change"]
    const done = (v) => { unwatch(); for (const e of events) document.removeEventListener(e, yes, true); clearTimeout(t); resolve(v) }
    const yes = () => done(true)
    const unwatch = observe(yes, { subtree: true, childList: true, attributes: true, characterData: true })
    for (const e of events) document.addEventListener(e, yes, true)
    const t = setTimeout(() => done(false), timeout)
  })
  s.frame = () => new Promise((r) => raf(() => raf(() => r(true))))
  s.nodes = () => document.getElementsByTagName("*").length
  return s
})(), true
//# sourceURL=strobe-driver.js"#;

/// Evaluates a helper call (`__strobe.…`), installing the helper first only when this document lacks
/// it (a navigation drops it). Page calls run inside measured runs: sending the whole helper with
/// each one made the page parse it on its main thread every call.
fn helped(page: &Page, expr: &str, timeout: Duration) -> Result<Value> {
    match page.eval_timeout(expr, timeout) {
        // V8 and SpiderMonkey say "is not defined", JavaScriptCore "Can't find variable".
        Err(e) if e.message.contains("__strobe is not defined") || e.message.contains("Can't find variable: __strobe") => {
            page.eval_timeout(HELPER, timeout)?;
            page.eval_timeout(expr, timeout)
        }
        r => r,
    }
}

/// What the page shows (`__strobe.screen`): its title, address, open dialogs and top headings (at most
/// `max`), and its visible text (at most `chars` of it).
pub fn screen(page: &Page, max: usize, chars: usize, timeout: Duration) -> Result<Value> {
    let v = helped(page, &format!("__strobe.screen({max}, {chars})"), timeout)?;
    v.as_str().and_then(|s| serde_json::from_str(s).ok()).ok_or_else(|| Error::msg("the page's screen did not read"))
}

/// What a run would start from (`__strobe.state`): each editable control's value and each toggle's
/// state, by control.
pub fn state(page: &Page, timeout: Duration) -> Result<std::collections::BTreeMap<String, String>> {
    let v = helped(page, "__strobe.state()", timeout)?;
    v.as_str().and_then(|s| serde_json::from_str(s).ok()).ok_or_else(|| Error::msg("the page's state did not read"))
}

/// Waits until the page changes (`__strobe.change`) or `t` passes.
pub fn wait_change(page: &Page, t: Duration) -> Result<bool> {
    Ok(helped(page, &format!("__strobe.change({})", t.as_millis()), t + Duration::from_secs(2))? == Value::Bool(true))
}

/// Whether a visible element matches `sel` within `timeout`, re-checked on each DOM and focus change
/// as `waitFor` does.
pub fn shows(page: &Page, sel: &Selector, timeout: Duration) -> Result<bool> {
    let expr = format!("__strobe.waitFor({}, {}, {}).then(() => true, () => false)", sel.page_spec(), timeout.as_millis(), json!(sel.source));
    Ok(helped(page, &expr, timeout + Duration::from_secs(5))? == Value::Bool(true))
}

/// The first visible element `sel` matches, as its box on the document in CSS pixels (x, y, width,
/// height), by the driver's own probe; None when nothing matches.
pub fn element_box(page: &Page, sel: &Selector, timeout: Duration) -> Result<Option<[f64; 4]>> {
    let expr = format!(
        "(() => {{ const el = __strobe.find({}); if (!el) return null; const r = el.getBoundingClientRect(); return JSON.stringify([r.left + scrollX, r.top + scrollY, r.width, r.height]) }})()",
        sel.page_spec()
    );
    let v = helped(page, &expr, timeout)?;
    Ok(v.as_str().and_then(|s| serde_json::from_str(s).ok()))
}

/// A selector argument with a call's `{ text, exact }` options added (`waitFor(sel, { text })`).
pub(crate) fn with_text(sel: &Value, o: &Value) -> Value {
    if o.get("text").is_none() && o.get("exact").is_none() {
        return sel.clone();
    }
    let mut sel = match sel {
        Value::Object(_) => sel.clone(),
        s => json!({ "selector": s.as_str().unwrap_or("") }),
    };
    for k in ["text", "exact"] {
        if let Some(v) = o.get(k) {
            sel[k] = v.clone();
        }
    }
    sel
}

/// CDP's modifier bits (Alt 1, Control 2, Meta 4, Shift 8).
fn cdp_mods(m: Mods) -> i64 {
    m.alt as i64 | (m.ctrl as i64) << 1 | (m.meta as i64) << 2 | (m.shift as i64) << 3
}

/// CDP's name and `buttons` bit for a mouse button.
fn cdp_button(b: Button) -> (&'static str, i64) {
    match b {
        Button::Left => ("left", 1),
        Button::Right => ("right", 2),
        Button::Middle => ("middle", 4),
    }
}

/// WebDriver's button number.
fn bidi_button(b: Button) -> u8 {
    match b {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
    }
}

/// The modifier keys of CDP modifier bits, in the order they go down.
fn mod_keys(bits: i64) -> Vec<Key> {
    [(2, Named::Control), (8, Named::Shift), (1, Named::Alt), (4, Named::Meta)].into_iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| Key::Named(n)).collect()
}

/// A key source's down or up action for WebDriver input.
fn key_action(k: Key, down: bool) -> Result<Value> {
    Ok(json!({ "type": if down { "keyDown" } else { "keyUp" }, "value": bidi::key_value(k)? }))
}

/// One BiDi `performActions` list: `pointer` actions with the modifier keys `mods` (CDP bits) pressed
/// around them, as ticks of a key source beside the pointer source.
fn with_mods(pointer: Vec<Value>, mods: i64) -> Result<Value> {
    let keys = mod_keys(mods);
    if keys.is_empty() {
        return Ok(json!([{ "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" }, "actions": pointer }]));
    }
    let pause = || json!({ "type": "pause" });
    let mut key_src: Vec<Value> = keys.iter().map(|k| key_action(*k, true)).collect::<Result<_>>()?;
    key_src.extend((0..pointer.len()).map(|_| pause()));
    key_src.extend(keys.iter().rev().map(|k| key_action(*k, false)).collect::<Result<Vec<_>>>()?);
    let mut ptr: Vec<Value> = (0..keys.len()).map(|_| pause()).collect();
    ptr.extend(pointer);
    ptr.extend((0..keys.len()).map(|_| pause()));
    Ok(json!([{ "type": "key", "id": "keyboard", "actions": key_src }, { "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" }, "actions": ptr }]))
}

/// No selector given: `""`, or none at all.
fn no_selector(sel: &Value) -> bool {
    sel.is_null() || sel.as_str() == Some("")
}

pub struct Driver {
    pub page: Page,
    pub token: String,
    pub op_timeout: Duration,
    /// A pixels block's folder: `snapshot(name)` writes `<name>.png` there. Elsewhere a snapshot does nothing.
    pub pixels: Option<std::path::PathBuf>,
    /// Keys held down by `down`, and the modifier bits they set (CDP's).
    held: RefCell<Vec<Key>>,
    mods: Cell<i64>,
    /// Where the pointer is (CSS pixels), and the mouse buttons held (`buttons` bits: left 1, right 2,
    /// middle 4) with the last one pressed.
    pointer: Cell<(f64, f64)>,
    buttons: Cell<(i64, Option<Button>)>,
    /// The display's frame period as the page's rAF sees it (ms), measured at the first drag.
    frame_ms: Cell<Option<f64>>,
    pacer: Pacer,
    /// Each drag's move times on the host clock (a button held by `down` or `drag`), for
    /// `input.rate` and `input.gap.p95`; the block takes them per run.
    pub drags: RefCell<Vec<Vec<u64>>>,
    /// The inputs a person's interaction latency is measured from (a click's release, each key press,
    /// a scroll's first wheel step), on the host clock as strobe sent them, with whether it was a
    /// scroll: a page without the browser's own input latencies (Firefox) joins them to the frames
    /// that showed them.
    pub inputs: RefCell<Vec<(u64, bool)>>,
}

impl Driver {
    pub fn new(page: Page, token: String) -> Driver {
        Driver {
            page,
            token,
            op_timeout: Duration::from_secs(30),
            pixels: None,
            held: RefCell::new(Vec::new()),
            mods: Cell::new(0),
            pointer: Cell::new((0.0, 0.0)),
            buttons: Cell::new((0, None)),
            frame_ms: Cell::new(None),
            pacer: Pacer::default(),
            drags: RefCell::new(Vec::new()),
            inputs: RefCell::new(Vec::new()),
        }
    }

    fn sel(v: &Value) -> Result<Selector> {
        Selector::parse(v, Dialect::Page)
    }

    /// A mouse event at (x, y) carrying the held buttons and modifiers. Over BiDi the held buttons and
    /// keys are the session's own input state; `extra` modifiers go down around the event.
    fn mouse(&self, kind: &str, (x, y): (f64, f64), button: Option<Button>, mask: i64, count: u64, extra: i64) -> Result<()> {
        match &self.page {
            Page::Chromium(p) => {
                let name = button.map_or("none", |b| cdp_button(b).0);
                p.send("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": name, "buttons": mask, "clickCount": count, "modifiers": self.mods.get() | extra })).map(|_| ())
            }
            Page::Firefox(_) | Page::Webkit(_) => {
                let at = json!({ "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "origin": "viewport" });
                let b = button.map_or(0, bidi_button);
                let pointer = match kind {
                    "mouseMoved" => vec![at],
                    "mousePressed" => vec![at, json!({ "type": "pointerDown", "button": b })],
                    _ => vec![json!({ "type": "pointerUp", "button": b })],
                };
                self.page.actions(with_mods(pointer, extra & !self.mods.get())?)
            }
        }
    }

    /// A click over WebDriver (BiDi or Classic) as a person clicks, as native targets are clicked: `mods` go down once and
    /// stay down over the move, every press and release; each press is held (60 ms for one click, a
    /// frame between a double click's presses), and the input time is taken just before the last
    /// release goes out. The session's input state keeps keys and buttons between the action lists.
    fn webdriver_click(&self, (x, y): (f64, f64), b: Button, mods: i64, count: u64) -> Result<()> {
        let p = &self.page;
        let keys = mod_keys(mods);
        let pointer = |actions: Vec<Value>| json!({ "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" }, "actions": actions });
        let key_list = |down: bool| -> Result<Value> { Ok(json!({ "type": "key", "id": "keyboard", "actions": keys.iter().map(|k| key_action(*k, down)).collect::<Result<Vec<_>>>()? })) };
        let at = json!({ "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "origin": "viewport" });
        let n = bidi_button(b);
        if keys.is_empty() {
            p.actions(json!([pointer(vec![at])]))?;
        } else {
            // Keys first, then the move: the key source's ticks, then the pointer's after them.
            let pause = keys.iter().map(|_| json!({ "type": "pause" })).chain(std::iter::once(at)).collect();
            p.actions(json!([key_list(true)?, pointer(pause)]))?;
        }
        let gap = Duration::from_millis(if count > 1 { 16 } else { 60 });
        let result = (|| -> Result<()> {
            for k in 1..=count {
                p.actions(json!([pointer(vec![json!({ "type": "pointerDown", "button": n })])]))?;
                std::thread::sleep(gap);
                if k == count {
                    self.input_at(false);
                }
                p.actions(json!([pointer(vec![json!({ "type": "pointerUp", "button": n })])]))?;
            }
            Ok(())
        })();
        if !keys.is_empty() {
            let mut up = key_list(false)?;
            if let Some(a) = up["actions"].as_array_mut() {
                a.reverse();
            }
            p.actions(json!([up]))?;
        }
        result
    }

    /// The pointer's input time for `interaction.*` (a click's release).
    fn input_at(&self, scroll: bool) {
        self.inputs.borrow_mut().push((crate::host::now_ns(), scroll));
    }

    /// A pointer move to (x, y) carrying the held buttons. While one is held it is a drag's move: sent
    /// without waiting for the renderer to handle it (`paced`), and its time is kept for `input.rate`.
    fn pointer_move(&self, x: f64, y: f64, paced: bool) -> Result<()> {
        let (mask, last) = self.buttons.get();
        self.pointer.set((x, y));
        if mask != 0
            && let Some(d) = self.drags.borrow_mut().last_mut()
        {
            d.push(crate::host::now_ns());
        }
        if !paced {
            return self.mouse("mouseMoved", (x, y), last.filter(|_| mask != 0), mask, 0, 0);
        }
        match &self.page {
            Page::Chromium(p) => {
                let e = json!({ "type": "mouseMoved", "x": x, "y": y, "button": last.filter(|_| mask != 0).map_or("none", |b| cdp_button(b).0), "buttons": mask, "modifiers": self.mods.get() });
                p.post("Input.dispatchMouseEvent", e)
            }
            Page::Firefox(_) | Page::Webkit(_) => self.page.post_actions(with_mods(vec![json!({ "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "origin": "viewport" })], 0)?),
        }
    }

    /// A mouse button going down or up where the pointer is.
    fn pointer_button(&self, b: Button, down: bool) -> Result<()> {
        let (x, y) = self.pointer.get();
        let bit = cdp_button(b).1;
        let mask = if down { self.buttons.get().0 | bit } else { self.buttons.get().0 & !bit };
        self.buttons.set((mask, Some(b)));
        if down {
            self.drags.borrow_mut().push(vec![crate::host::now_ns()]);
        }
        self.mouse(if down { "mousePressed" } else { "mouseReleased" }, (x, y), Some(b), mask, 1, 0)
    }

    /// The display's frame period (ms) from ten of the page's animation frames, once per block.
    fn frame_period(&self) -> Result<f64> {
        if let Some(ms) = self.frame_ms.get() {
            return Ok(ms);
        }
        let t = self.wait("new Promise((r) => { const raf = window.__strobeRaf ?? requestAnimationFrame.bind(window); const t = []; const f = (ts) => { t.push(ts); if (t.length < 11) raf(f); else r(t) }; raf(f) })", Duration::from_secs(5))?;
        let ts: Vec<f64> = t.as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
        let mut d: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.0).collect();
        d.sort_by(f64::total_cmp);
        let ms = d.get(d.len() / 2).copied().unwrap_or(1000.0 / 60.0).clamp(4.0, 50.0);
        self.frame_ms.set(Some(ms));
        Ok(ms)
    }

    /// `drag(selector, path, { duration, button })`: presses at the element's centre (or where the pointer
    /// is, with `""`), moves along `path` (offsets in CSS pixels, spread evenly over `duration`) one move
    /// per display frame on strobe's clock without waiting for each to be handled, and releases. A button
    /// already held by `down` stays held.
    fn drag(&self, sel: &Value, path: &Value, o: &Value) -> Result<Value> {
        let path = keys::parse_path(path)?;
        let (x0, y0) = if no_selector(sel) { self.pointer.get() } else { self.locate(&Self::sel(sel)?, o)? };
        let b = Button::parse(o["button"].as_str())?;
        let duration = o.get("duration").and_then(Value::as_f64).unwrap_or(1000.0).max(1.0);
        let frame = self.frame_period()?;
        let (dx, dy) = keys::path_at(&path, 0.0);
        self.pointer_move(x0 + dx, y0 + dy, false)?;
        let held = self.buttons.get().0 & cdp_button(b).1 != 0;
        if held {
            self.drags.borrow_mut().push(vec![crate::host::now_ns()]);
        } else {
            self.pointer_button(b, true)?;
        }
        let steps = (duration / frame).ceil().max(1.0) as u32;
        let start = Instant::now();
        for k in 1..=steps {
            self.pacer.wait(start + Duration::from_secs_f64(frame * k as f64 / 1e3), Duration::from_secs_f64(frame / 1e3));
            let (dx, dy) = keys::path_at(&path, k as f64 / steps as f64);
            self.pointer_move(x0 + dx, y0 + dy, true)?;
        }
        if !held {
            self.pointer_button(b, false)?;
        }
        self.until(o)
    }

    fn call(&self, expr: &str) -> Result<Value> {
        helped(&self.page, expr, Duration::from_secs(60))
    }

    /// A helper call that waits in the page for up to `timeout`: the in-page timeout fires first and names what it waited for.
    fn wait(&self, expr: &str, timeout: Duration) -> Result<Value> {
        // A wait ends with the document it runs in: CDP reports a reload or navigation as a collected promise or
        // a destroyed context, which says nothing about what happened to the page.
        helped(&self.page, expr, timeout + Duration::from_secs(10)).map_err(|e| {
            if ["Promise was collected", "context was destroyed", "no such frame", "was discarded"].iter().any(|m| e.message.contains(m)) {
                Error::msg(format!("the page reloaded or navigated away while strobe waited ({expr}): the app replaced the document on its own"))
            } else {
                e
            }
        })
    }

    /// Waits for a visible element `sel` matches.
    fn wait_for(&self, sel: &Selector, t: Duration) -> Result<Value> {
        self.wait(&format!("__strobe.waitFor({}, {}, {})", sel.page_spec(), t.as_millis(), json!(sel.source)), t)
    }

    /// Writes a `performance.mark` that the trace records on its own clock.
    /// A page without such a trace (Firefox) takes none: its runs and phases are strobe's own spans on
    /// the host clock, as a native app's.
    pub fn mark(&self, name: &str) -> Result<()> {
        if self.page.cdp().is_none() {
            return Ok(());
        }
        let full = format!("strobe:{}:{}", self.token, name.replace(['"', '\\', '\n'], "_"));
        self.page.eval(&format!(r#"performance.mark("{full}"), performance.clearMarks("{full}"), true"#)).map(|_| ())
    }

    pub fn nodes(&self) -> Result<u64> {
        Ok(self.call("__strobe.nodes()")?.as_u64().unwrap_or(0))
    }

    /// Lets go of every key and mouse button `down` (or a drag) left held, so the next run starts with none.
    pub fn release_all(&self) -> Result<()> {
        let (mask, _) = self.buttons.get();
        for b in [Button::Left, Button::Right, Button::Middle] {
            if mask & cdp_button(b).1 != 0 {
                self.pointer_button(b, false)?;
            }
        }
        let held: Vec<Key> = self.held.borrow().clone();
        for k in held.into_iter().rev() {
            self.key(k, false)?;
        }
        Ok(())
    }

    /// Clicks into the element unless focus is already in it, so a second call keeps typing where the caret is.
    fn focus(&self, sel: &Value, o: &Value) -> Result<()> {
        if no_selector(sel) {
            return Ok(());
        }
        let sel = Self::sel(sel)?;
        let (x, y) = self.locate(&sel, o)?;
        if self.call(&format!("__strobe.focused({})", sel.page_spec()))?.as_bool() != Some(true) {
            let held = self.buttons.get().0;
            self.pointer.set((x, y));
            self.mouse("mouseMoved", (x, y), None, held, 0, 0)?;
            self.mouse("mousePressed", (x, y), Some(Button::Left), held | 1, 1, 0)?;
            self.mouse("mouseReleased", (x, y), Some(Button::Left), held, 1, 0)?;
        }
        Ok(())
    }

    /// Dispatches one driver call from the runner: `{ call, args }`.
    pub fn dispatch(&self, call: &str, args: &[Value]) -> Result<Value> {
        let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        let opts = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        // A selector as the bench gave it: a string, or `{ selector, role, text, exact, index, focus }`.
        let sel = |i: usize| args.get(i).cloned().unwrap_or(Value::String(String::new()));
        match call {
            "goto" => {
                let to = s(0);
                // Relative paths resolve against the page, like a link would.
                let url =
                    if to.contains("://") || to.starts_with("about:") { to } else { self.page.eval(&format!("new URL({}, location.href).href", json!(to)))?.as_str().unwrap_or_default().to_string() };
                self.page.navigate(&url, self.timeout(&opts(1)))?;
                self.until(&opts(1))
            }
            "click" => {
                let o = opts(1);
                let (x, y) = self.locate(&Self::sel(&sel(0))?, &o)?;
                let b = Button::parse(o["button"].as_str())?;
                let mods = cdp_mods(Mods::parse_list(&o["modifiers"])?);
                let count = o["count"].as_u64().unwrap_or(1).clamp(1, 3);
                let (held, bit) = (self.buttons.get().0, cdp_button(b).1);
                self.pointer.set((x, y));
                match &self.page {
                    Page::Firefox(_) | Page::Webkit(_) => {
                        self.webdriver_click((x, y), b, mods & !self.mods.get(), count)?;
                        return self.until(&o);
                    }
                    Page::Chromium(_) => {}
                }
                self.mouse("mouseMoved", (x, y), None, held, 0, mods)?;
                // Each press of a double or triple click carries its count, as a mouse's do.
                for k in 1..=count {
                    self.mouse("mousePressed", (x, y), Some(b), held | bit, k, mods)?;
                    if k == count {
                        self.input_at(false);
                    }
                    self.mouse("mouseReleased", (x, y), Some(b), held, k, mods)?;
                }
                self.until(&o)
            }
            "type" => {
                let o = opts(2);
                self.focus(&sel(0), &o)?;
                // One key event per character, like a person typing, so each keystroke is an interaction.
                let delay = Duration::from_millis(o.get("delay").and_then(Value::as_u64).unwrap_or(0));
                for (i, c) in s(1).chars().enumerate() {
                    if i > 0 && !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    let k = match c {
                        '\n' => Key::Named(Named::Enter),
                        '\t' => Key::Named(Named::Tab),
                        c => Key::Char(c),
                    };
                    self.stroke(k, 0)?;
                }
                self.until(&o)
            }
            "press" => {
                let o = opts(1);
                self.focus(&o["selector"], &o)?;
                let (mods, key) = keys::parse_chord(&s(0))?;
                self.stroke(key, cdp_mods(mods))?;
                self.until(&o)
            }
            // The pointer over an element, without a click: reveals hover-only controls, or parks the pointer.
            "hover" => {
                let (x, y) = self.locate(&Self::sel(&sel(0))?, &opts(1))?;
                self.pointer_move(x, y, false)?;
                self.until(&opts(1))
            }
            "move" => {
                let at = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0);
                self.pointer_move(at(0), at(1), false)?;
                self.until(&opts(2))
            }
            "drag" => self.drag(&sel(0), &opts(1), &opts(2)),
            "scroll" => {
                let o = opts(1);
                let (x, y) = self.locate(&Self::sel(&sel(0))?, &opts(2))?;
                let by = o.get("by").and_then(Value::as_f64).unwrap_or(0.0);
                let by_x = o.get("byX").and_then(Value::as_f64).unwrap_or(0.0);
                let speed = o.get("speed").and_then(Value::as_f64).unwrap_or(2000.0);
                match &self.page {
                    Page::Chromium(p) => {
                        p.send_long("Input.synthesizeScrollGesture", json!({ "x": x, "y": y, "yDistance": -by, "xDistance": -by_x, "speed": speed, "gestureSourceType": "mouse", "preventFling": true }))?;
                    }
                    Page::Firefox(_) | Page::Webkit(_) => self.paced_wheel((x, y), (by_x, by), speed)?,
                }
                self.until(&opts(2))
            }
            // One wheel event at the element's centre (the viewport's without a selector), as a mouse wheel
            // sends, with the keys and buttons `down` holds (Control and the wheel zoom).
            "wheel" => {
                let d = opts(1);
                let (x, y) = if no_selector(&sel(0)) {
                    let c = self.page.eval("[innerWidth / 2, innerHeight / 2]")?;
                    (c[0].as_f64().unwrap_or(0.0), c[1].as_f64().unwrap_or(0.0))
                } else {
                    self.locate(&Self::sel(&sel(0))?, &opts(2))?
                };
                let delta = |k: &str| d.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                let (mask, last) = self.buttons.get();
                self.input_at(true);
                match &self.page {
                    Page::Chromium(p) => {
                        p.send(
                            "Input.dispatchMouseEvent",
                            json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": delta("dx"), "deltaY": delta("dy"), "button": last.filter(|_| mask != 0).map_or("none", |b| cdp_button(b).0), "buttons": mask, "modifiers": self.mods.get() }),
                        )?;
                    }
                    Page::Firefox(_) | Page::Webkit(_) => self.page.actions(json!([{ "type": "wheel", "id": "wheel", "actions": [wheel_step((x, y), (delta("dx"), delta("dy")))] }]))?,
                }
                self.until(&opts(2))
            }
            "window" => Err(Error::usage("page.window() shows, hides, focuses or closes a native app's windows; a page has none of its own")),
            "act" | "query" => Err(Error::usage(format!("page.{call}() reads a native app's accessibility tree; on a page use eval or Playwright in prepare"))),
            "waitFor" => {
                let o = opts(1);
                self.wait_for(&Self::sel(&with_text(&sel(0), &o))?, self.timeout(&o))?;
                Ok(Value::Bool(true))
            }
            "idle" => {
                let t = self.timeout(&opts(0));
                self.wait(&format!("__strobe.idle(300, {})", t.as_millis()), t)
            }
            "wait" => {
                let ms = args.first().and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
                std::thread::sleep(Duration::from_millis(ms as u64));
                Ok(Value::Bool(true))
            }
            "eval" => {
                let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
                self.page.eval(&format!("({})(...{})", s(0), Value::Array(rest)))
            }
            "frame" => self.call("__strobe.frame()"),
            // A key held across other calls (a game's WASD, a held shortcut): `down` presses it, `up` lets go.
            "down" | "up" => {
                let o = opts(1);
                if let Some(b) = keys::mouse_button(&s(0)) {
                    // `at`: a selector or [x, y] to move to first, as a person moves before pressing.
                    match o.get("at") {
                        Some(at @ (Value::String(_) | Value::Object(_))) => {
                            let (x, y) = self.locate(&Self::sel(at)?, &o)?;
                            self.pointer_move(x, y, false)?;
                        }
                        Some(Value::Array(p)) => self.pointer_move(p.first().and_then(Value::as_f64).unwrap_or(0.0), p.get(1).and_then(Value::as_f64).unwrap_or(0.0), false)?,
                        _ => {}
                    }
                    self.pointer_button(b, call == "down")?;
                } else {
                    let (mods, key) = keys::parse_chord(&s(0))?;
                    if mods.any() {
                        return Err(Error::usage(format!("page.{call}({:?}): one key, not a chord (hold each with its own down)", s(0))));
                    }
                    self.key(key, call == "down")?;
                }
                self.until(&o)
            }
            // The page's pixels for `--pixels`: only a pixels block captures, so timed runs never pay for it.
            "snapshot" => {
                let Some(dir) = &self.pixels else { return Ok(Value::Bool(false)) };
                let name: String = s(0).chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
                let png = self.page.screenshot(Duration::from_secs(120)).map_err(|e| Error::msg(format!("snapshot {name}: {}", e.message)))?;
                std::fs::create_dir_all(dir)?;
                crate::store::write_atomic(&dir.join(format!("{name}.png")), &png)?;
                Ok(Value::Bool(true))
            }
            other => Err(Error::msg(format!("unknown page call {other}"))),
        }
    }

    fn timeout(&self, opts: &Value) -> Duration {
        opts.get("timeout").and_then(Value::as_u64).map(Duration::from_millis).unwrap_or(self.op_timeout)
    }

    /// Waits for the element, then returns the point to send input at.
    fn locate(&self, sel: &Selector, opts: &Value) -> Result<(f64, f64)> {
        self.wait_for(sel, self.timeout(opts))?;
        let b = self.call(&format!("__strobe.box({})", sel.page_spec()))?;
        if b.is_null() {
            return Err(Error::msg(format!("no visible element matches {}", sel.source)));
        }
        Ok((b["x"].as_f64().unwrap_or(0.0), b["y"].as_f64().unwrap_or(0.0)))
    }

    fn until(&self, opts: &Value) -> Result<Value> {
        let t = self.timeout(opts);
        match opts.get("until") {
            None | Some(Value::Null) => Ok(Value::Bool(true)),
            Some(Value::String(u)) if u == "idle" => self.wait(&format!("__strobe.idle(300, {})", t.as_millis()), t),
            Some(Value::String(u)) if u == "frame" => self.call("__strobe.frame()"),
            Some(u) => self.wait_for(&Self::sel(u)?, t),
        }
    }

    /// A key pressed and released with `extra` modifiers on top of the held ones. Modifiers mark the
    /// events, as CDP takes them; they are not keys of their own.
    /// Over WebDriver (BiDi or Classic) a chord's modifiers (and Shift for a character a US keyboard types
    /// with it) go down as keys around it, in one action list.
    fn stroke(&self, key: Key, extra: i64) -> Result<()> {
        let m = self.mods.get() | extra;
        self.input_at(false);
        match &self.page {
            Page::Chromium(_) => {
                self.key_event(key, true, m)?;
                return self.key_event(key, false, m);
            }
            Page::Firefox(_) | Page::Webkit(_) => {}
        }
        let shift = if matches!(key, Key::Char(c) if keys::us_key(c).is_some_and(|k| k.1)) { 8 } else { 0 };
        let press = (extra | shift) & !self.mods.get();
        let key = match key {
            // Safari types a symbol with Shift only from its key's own character.
            Key::Char(c) if (m | shift) & 8 != 0 && !c.is_ascii_alphabetic() && self.page.shift_takes_base_key() => Key::Char(keys::us_key(c).map_or(c, |k| k.0)),
            Key::Char(c) if (m | shift) & 8 != 0 && c.is_ascii_graphic() => Key::Char(keys::us_shifted(c)),
            k => k,
        };
        let down: Vec<Value> = mod_keys(press).into_iter().map(|k| key_action(k, true)).collect::<Result<_>>()?;
        let up: Vec<Value> = mod_keys(press).into_iter().rev().map(|k| key_action(k, false)).collect::<Result<_>>()?;
        let list = |actions: Vec<Value>| json!([{ "type": "key", "id": "keyboard", "actions": actions }]);
        // Safari sometimes drops a key that comes in the same action list as the Shift its window has not
        // taken yet ("hello orld!" once in three cold runs): there the modifiers go down in a list of their
        // own, and the key's input time is taken again right before it.
        if self.page.shift_takes_base_key() && !down.is_empty() {
            self.page.actions(list(down))?;
            if let Some(last) = self.inputs.borrow_mut().last_mut() {
                last.0 = crate::host::now_ns();
            }
            self.page.actions(list(vec![key_action(key, true)?, key_action(key, false)?]))?;
            return self.page.actions(list(up));
        }
        let mut actions = down;
        actions.push(key_action(key, true)?);
        actions.push(key_action(key, false)?);
        actions.extend(up);
        self.page.actions(list(actions))
    }

    /// `scroll` over WebDriver, which has no scroll gesture: wheel steps of the distance at `speed` (CSS
    /// px a second), one per display frame on strobe's clock, sent without waiting for each to be handled
    /// (WebDriver Classic: each waited for), then one empty action list that returns once the page has
    /// handled them all.
    fn paced_wheel(&self, at: (f64, f64), (dx, dy): (f64, f64), speed: f64) -> Result<()> {
        let p = &self.page;
        let frame = self.frame_period()?;
        let dist = dx.hypot(dy);
        let steps = ((dist / speed.max(1.0)) * 1e3 / frame).ceil().max(1.0) as u32;
        let start = Instant::now();
        let mut sent = (0.0, 0.0);
        for k in 1..=steps {
            if k > 1 {
                self.pacer.wait(start + Duration::from_secs_f64(frame * (k - 1) as f64 / 1e3), Duration::from_secs_f64(frame / 1e3));
            } else {
                self.input_at(true);
            }
            let f = k as f64 / steps as f64;
            let to = ((dx * f).round(), (dy * f).round());
            p.post_actions(json!([{ "type": "wheel", "id": "wheel", "actions": [wheel_step(at, (to.0 - sent.0, to.1 - sent.1))] }]))?;
            sent = to;
        }
        p.actions(json!([{ "type": "none", "id": "sync", "actions": [{ "type": "pause", "duration": 0 }] }]))
    }

    /// One key going down or up, from the key table: its `key`, `code` and virtual key, and its text only
    /// while no Control, Alt or Meta is down (Shift types what a US keyboard's key types with it: `A`, `!`).
    fn key_event(&self, key: Key, down: bool, modifiers: i64) -> Result<()> {
        let mut d = match key {
            Key::Named(n) => keys::dom(n),
            Key::Char(c) => keys::dom_char(c),
        };
        // A character a US keyboard types with Shift (`A`, `+`) holds Shift, as the native posters press it.
        let modifiers = modifiers | if matches!(key, Key::Char(c) if keys::us_key(c).is_some_and(|k| k.1)) { 8 } else { 0 };
        if modifiers & 8 != 0
            && let Some(c) = d.text.as_deref().and_then(|t| t.chars().next()).filter(|c| d.key.chars().count() == 1 && c.is_ascii_graphic())
        {
            d.key = keys::us_shifted(c).to_string();
            d.text = Some(d.key.clone());
        }
        let text = d.text.filter(|_| down && modifiers & 0b0111 == 0);
        let kind = match (down, &text) {
            (false, _) => "keyUp",
            (true, Some(_)) => "keyDown",
            (true, None) => "rawKeyDown",
        };
        let mut e = json!({ "type": kind, "key": d.key, "code": d.code, "windowsVirtualKeyCode": d.vk, "modifiers": modifiers });
        if let Some(t) = text {
            e["text"] = json!(t);
        }
        match &self.page {
            Page::Chromium(p) => p.send("Input.dispatchKeyEvent", e).map(|_| ()),
            // The session's input state holds the modifiers already down.
            Page::Firefox(_) | Page::Webkit(_) => {
                // Safari types a symbol with Shift held only from its key's own character (`stroke`).
                let key = match key {
                    Key::Char(c) if self.mods.get() & 8 != 0 && !c.is_ascii_alphabetic() && self.page.shift_takes_base_key() => Key::Char(keys::us_key(c).map_or(c, |k| k.0)),
                    k => k,
                };
                self.page.actions(json!([{ "type": "key", "id": "keyboard", "actions": [key_action(key, down)?] }]))
            }
        }
    }

    /// One key, pressed (`down`) or let go: a held key stays down across other calls until its `up`. A held
    /// modifier (Shift, Control, Alt, Meta) also marks the key events and mouse events sent while it is down.
    /// Chromium sends no auto-repeat, as a game reading key state does not need.
    fn key(&self, key: Key, down: bool) -> Result<()> {
        let bit = match key {
            Key::Named(Named::Alt) => 1,
            Key::Named(Named::Control) => 2,
            Key::Named(Named::Meta) => 4,
            Key::Named(Named::Shift) => 8,
            _ => 0,
        };
        {
            let mut held = self.held.borrow_mut();
            let at = held.iter().position(|k| *k == key);
            match (down, at) {
                (true, None) => held.push(key),
                (false, Some(i)) => {
                    held.remove(i);
                }
                // Already down, or not held: nothing to send.
                _ => return Ok(()),
            }
        }
        self.mods.set(if down { self.mods.get() | bit } else { self.mods.get() & !bit });
        if down && bit == 0 {
            self.input_at(false);
        }
        self.key_event(key, down, self.mods.get())
    }
}

/// One wheel step at a viewport point, for WebDriver input (whole pixels).
fn wheel_step((x, y): (f64, f64), (dx, dy): (f64, f64)) -> Value {
    json!({ "type": "scroll", "x": x.round() as i64, "y": y.round() as i64, "deltaX": dx.round() as i64, "deltaY": dy.round() as i64, "origin": "viewport" })
}

impl chromium::Page {
    /// Like `send`, for commands that take as long as the gesture they perform.
    pub fn send_long(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.send_timeout(method, params, Some(&self.session), Duration::from_secs(120))
    }

    /// A command whose reply nobody waits for (`Cdp::post`).
    pub fn post(&self, method: &str, params: Value) -> Result<()> {
        self.cdp.post(method, params, Some(&self.session))
    }
}

/// Paces a drag's moves on strobe's clock: sleeps while the OS's timers wake strobe in time and
/// spins the rest. Each driver keeps its own: how late its sleeps woke is the lead it spins before
/// each move, which grows to the latest wake, shrinks by an eighth with each wake in time and a
/// sixteenth with each frame it spun whole, and never passes a frame (then the whole frame is spun).
/// A process whose timers macOS coalesces (a launchd job of ProcessType Standard, such as a remote
/// shell's daemon, and what it starts) woke 20 to 60 ms late from a 16 ms sleep, so its moves came in
/// bursts; one late wake no longer makes every later drag of the process spin.
#[derive(Default)]
pub(crate) struct Pacer {
    late: Cell<Duration>,
}

impl Pacer {
    /// Waits until `due`, the next move of a drag whose frames are `frame` long.
    pub fn wait(&self, due: Instant, frame: Duration) {
        let late = self.late.get();
        let lead = (late + Duration::from_millis(1)).min(frame);
        match due.checked_duration_since(Instant::now() + lead) {
            Some(w) => {
                let asked = Instant::now() + w;
                std::thread::sleep(w);
                let woke = Instant::now().saturating_duration_since(asked);
                self.late.set(if woke >= late { woke } else { late - (late - woke) / 8 });
            }
            None => self.late.set(late - late / 16),
        }
        while Instant::now() < due {
            std::thread::yield_now();
        }
    }
}

/// `input.rate` and `input.gap.p95` (SPEC §9.6): pointer moves strobe sent per second while a button was
/// held, over the time each drag lasted, and the 95th percentile gap between consecutive moves (ms).
/// Input that starved (a slow driver, a busy machine) shows here rather than as dropped frames.
pub fn input_pacing(drags: &[Vec<u64>]) -> Option<(f64, f64)> {
    let mut gaps: Vec<f64> = Vec::new();
    let mut span = 0u64;
    for d in drags.iter().filter(|d| d.len() >= 2) {
        span += d[d.len() - 1].saturating_sub(d[0]);
        gaps.extend(d.windows(2).map(|w| w[1].saturating_sub(w[0]) as f64 / 1e6));
    }
    if gaps.is_empty() || span == 0 {
        return None;
    }
    let rate = gaps.len() as f64 / (span as f64 / 1e9);
    gaps.sort_by(f64::total_cmp);
    let p95 = gaps[((gaps.len() as f64 * 0.95).ceil() as usize).clamp(1, gaps.len()) - 1];
    Some((rate, p95))
}
