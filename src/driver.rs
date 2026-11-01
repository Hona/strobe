//! The `page` driver (SPEC §9.6): inputs over CDP, event-driven waits, run and phase marks.

use crate::model::{Error, Result};
use crate::target::chromium::Page;
use serde_json::{Value, json};
use std::time::Duration;

/// Installed lazily in the page (it disappears on navigation). Observers only: nothing polls.
const HELPER: &str = r#"window.__strobe ??= (() => {
  const s = {}
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
  // `text` matches a substring of the element's text, or all of it (trimmed) when `exact`.
  const has = (el, text, exact) => text == null || (exact ? (el.textContent ?? "").trim() === text : (el.textContent ?? "").includes(text))
  s.find = (sel, text, exact) => query(document, sel.split(">>>").map((part) => part.trim())).find((el) => visible(el) && has(el, text, exact)) ?? null
  s.focused = (sel) => { const el = s.find(sel); return !!el && el.contains(document.activeElement) }
  s.box = (sel) => {
    const el = s.find(sel)
    if (!el) return null
    let r = el.getBoundingClientRect()
    if (r.bottom < 0 || r.top > innerHeight || r.right < 0 || r.left > innerWidth) { el.scrollIntoView({ block: "nearest", inline: "nearest" }); r = el.getBoundingClientRect() }
    return { x: r.left + Math.min(r.width, innerWidth) / 2, y: r.top + Math.min(r.height, innerHeight) / 2, w: r.width, h: r.height }
  }
  // Re-checked on every DOM change and every focus change: focus moves without a mutation, so a
  // `:focus` selector would otherwise never resolve.
  s.waitFor = (sel, timeout, text, exact) => new Promise((resolve, reject) => {
    if (s.find(sel, text, exact)) return resolve(true)
    const check = () => { if (s.find(sel, text, exact)) { stop(); clearTimeout(t); resolve(true) } }
    const unwatch = observe(check, { subtree: true, childList: true, attributes: true, characterData: text != null })
    document.addEventListener("focusin", check, true)
    document.addEventListener("focusout", check, true)
    const stop = () => { unwatch(); document.removeEventListener("focusin", check, true); document.removeEventListener("focusout", check, true) }
    const t = setTimeout(() => { stop(); reject(new Error("timed out waiting for " + sel + (text == null ? "" : (exact ? " with exactly " : " containing ") + JSON.stringify(text)))) }, timeout)
  })
  s.idle = (quiet, timeout) => new Promise((resolve, reject) => {
    let timer, po
    const stop = () => { unwatch(); if (po) po.disconnect(); clearTimeout(timer); clearTimeout(limit) }
    const kick = () => { clearTimeout(timer); timer = setTimeout(() => { stop(); resolve(true) }, quiet) }
    const unwatch = observe(kick, { subtree: true, childList: true, attributes: true, characterData: true })
    try { po = new PerformanceObserver(kick); po.observe({ type: "long-animation-frame" }) } catch {}
    const limit = setTimeout(() => { stop(); reject(new Error("the page never went idle (DOM kept changing for " + timeout + " ms)")) }, timeout)
    kick()
  })
  s.frame = () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))
  s.nodes = () => document.getElementsByTagName("*").length
  return s
})(), true"#;

pub struct Driver {
    pub page: Page,
    pub token: String,
    pub op_timeout: Duration,
}

impl Driver {
    pub fn new(page: Page, token: String) -> Driver {
        Driver { page, token, op_timeout: Duration::from_secs(30) }
    }

    fn helper(&self) -> Result<()> {
        self.page.eval(HELPER).map(|_| ())
    }

    fn call(&self, expr: &str) -> Result<Value> {
        self.helper()?;
        self.page.eval(expr)
    }

    /// A helper call that waits in the page for up to `timeout`: the in-page timeout fires first and names what it waited for.
    fn wait(&self, expr: &str, timeout: Duration) -> Result<Value> {
        self.helper()?;
        self.page.eval_timeout(expr, timeout + Duration::from_secs(10))
    }

    /// Writes a `performance.mark` that the trace records on its own clock.
    pub fn mark(&self, name: &str) -> Result<()> {
        let full = format!("strobe:{}:{}", self.token, name.replace(['"', '\\', '\n'], "_"));
        self.page.eval(&format!(r#"performance.mark("{full}"), performance.clearMarks("{full}"), true"#)).map(|_| ())
    }

    pub fn nodes(&self) -> Result<u64> {
        Ok(self.call("__strobe.nodes()")?.as_u64().unwrap_or(0))
    }

    /// Dispatches one driver call from the runner: `{ call, args }`.
    pub fn dispatch(&self, call: &str, args: &[Value]) -> Result<Value> {
        let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        let opts = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
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
                let (x, y) = self.locate(&s(0), &opts(1))?;
                for (kind, extra) in [("mouseMoved", json!({})), ("mousePressed", json!({ "button": "left", "clickCount": 1 })), ("mouseReleased", json!({ "button": "left", "clickCount": 1 }))] {
                    let mut p = json!({ "type": kind, "x": x, "y": y });
                    merge(&mut p, extra);
                    self.page.send("Input.dispatchMouseEvent", p)?;
                }
                self.until(&opts(1))
            }
            "type" => {
                let o = opts(2);
                let (x, y) = self.locate(&s(0), &o)?;
                // Click only when focus is elsewhere, so a second call keeps typing where the caret is.
                if self.page.eval(&format!("__strobe.focused({})", json!(s(0))))?.as_bool() != Some(true) {
                    for kind in ["mousePressed", "mouseReleased"] {
                        self.page.send("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
                    }
                }
                // One key event per character, like a person typing, so each keystroke is an interaction.
                let delay = Duration::from_millis(o.get("delay").and_then(Value::as_u64).unwrap_or(0));
                for (i, c) in s(1).chars().enumerate() {
                    if i > 0 && !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    match c {
                        '\n' => self.press("Enter")?,
                        '\t' => self.press("Tab")?,
                        c => self.press(&c.to_string())?,
                    }
                }
                self.until(&o)
            }
            "press" => {
                self.press(&s(0))?;
                self.until(&opts(1))
            }
            // The pointer over an element, without a click: reveals hover-only controls, or parks the pointer.
            "hover" => {
                let (x, y) = self.locate(&s(0), &opts(1))?;
                self.page.send("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))?;
                self.until(&opts(1))
            }
            "move" => {
                let at = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0);
                self.page.send("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": at(0), "y": at(1) }))?;
                self.until(&opts(2))
            }
            "scroll" => {
                let o = opts(1);
                let (x, y) = self.locate(&s(0), &opts(2))?;
                let by = o.get("by").and_then(Value::as_f64).unwrap_or(0.0);
                let by_x = o.get("byX").and_then(Value::as_f64).unwrap_or(0.0);
                let speed = o.get("speed").and_then(Value::as_f64).unwrap_or(2000.0);
                self.page
                    .send_long("Input.synthesizeScrollGesture", json!({ "x": x, "y": y, "yDistance": -by, "xDistance": -by_x, "speed": speed, "gestureSourceType": "mouse", "preventFling": true }))?;
                self.until(&opts(2))
            }
            "waitFor" => {
                let o = opts(1);
                let t = self.timeout(&o);
                let text = o.get("text").cloned().unwrap_or(Value::Null);
                let exact = o.get("exact").and_then(Value::as_bool).unwrap_or(false);
                self.wait(&format!("__strobe.waitFor({}, {}, {text}, {exact})", json!(s(0)), t.as_millis()), t)?;
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
            other => Err(Error::msg(format!("unknown page call {other}"))),
        }
    }

    fn timeout(&self, opts: &Value) -> Duration {
        opts.get("timeout").and_then(Value::as_u64).map(Duration::from_millis).unwrap_or(self.op_timeout)
    }

    /// Waits for the element, then returns the point to send input at.
    fn locate(&self, selector: &str, opts: &Value) -> Result<(f64, f64)> {
        let t = self.timeout(opts);
        let sel = json!(selector);
        self.wait(&format!("__strobe.waitFor({sel}, {})", t.as_millis()), t)?;
        let b = self.page.eval(&format!("__strobe.box({sel})"))?;
        if b.is_null() {
            return Err(Error::msg(format!("no visible element matches {selector}")));
        }
        Ok((b["x"].as_f64().unwrap_or(0.0), b["y"].as_f64().unwrap_or(0.0)))
    }

    fn until(&self, opts: &Value) -> Result<Value> {
        let t = self.timeout(opts);
        // `{ selector, text?, exact? }`: a selector whose element holds that text.
        if let Some(u) = opts.get("until").filter(|u| u.is_object()) {
            let text = u.get("text").cloned().unwrap_or(Value::Null);
            let exact = u.get("exact").and_then(Value::as_bool).unwrap_or(false);
            let sel = json!(u["selector"].as_str().unwrap_or(""));
            return self.wait(&format!("__strobe.waitFor({sel}, {}, {text}, {exact})", t.as_millis()), t);
        }
        let Some(until) = opts.get("until").and_then(Value::as_str) else { return Ok(Value::Bool(true)) };
        match until {
            "idle" => self.wait(&format!("__strobe.idle(300, {})", t.as_millis()), t),
            "frame" => self.call("__strobe.frame()"),
            sel => self.wait(&format!("__strobe.waitFor({}, {})", json!(sel), t.as_millis()), t),
        }
    }

    fn press(&self, chord: &str) -> Result<()> {
        // "+" alone, or as the last key of a chord ("Control++"), is the plus key.
        let (mods, key) = match chord.strip_suffix("++") {
            Some(rest) => (rest, "+"),
            None if chord == "+" => ("", "+"),
            None => chord.rsplit_once('+').unwrap_or(("", chord)),
        };
        let mut modifiers = 0;
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "alt" => 1,
                "control" | "ctrl" => 2,
                "meta" | "cmd" => 4,
                "shift" => 8,
                "controlormeta" | "mod" => {
                    if cfg!(target_os = "macos") {
                        4
                    } else {
                        2
                    }
                }
                _ => 0,
            };
        }
        let (code, key_code, text) = key_info(key);
        let down_text = if modifiers & 0b0111 == 0 { text.clone() } else { None };
        let mut down = json!({ "type": if down_text.is_some() { "keyDown" } else { "rawKeyDown" }, "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": modifiers });
        if let Some(t) = &down_text {
            down["text"] = json!(t);
        }
        self.page.send("Input.dispatchKeyEvent", down)?;
        self.page.send("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": modifiers }))?;
        Ok(())
    }
}

fn merge(a: &mut Value, b: Value) {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
}

fn key_info(key: &str) -> (String, u32, Option<String>) {
    let named: &[(&str, &str, u32, Option<&str>)] = &[
        ("Enter", "Enter", 13, Some("\r")),
        ("Escape", "Escape", 27, None),
        ("Tab", "Tab", 9, None),
        ("Backspace", "Backspace", 8, None),
        ("Delete", "Delete", 46, None),
        ("ArrowUp", "ArrowUp", 38, None),
        ("ArrowDown", "ArrowDown", 40, None),
        ("ArrowLeft", "ArrowLeft", 37, None),
        ("ArrowRight", "ArrowRight", 39, None),
        ("PageUp", "PageUp", 33, None),
        ("PageDown", "PageDown", 34, None),
        ("Home", "Home", 36, None),
        ("End", "End", 35, None),
        ("Space", "Space", 32, Some(" ")),
        (" ", "Space", 32, Some(" ")),
    ];
    if let Some(n) = named.iter().find(|n| n.0 == key) {
        return (n.1.to_string(), n.2, n.3.map(str::to_string));
    }
    let c = key.chars().next().unwrap_or(' ');
    if c.is_ascii_alphabetic() {
        let up = c.to_ascii_uppercase();
        return (format!("Key{up}"), up as u32, Some(key.to_string()));
    }
    if c.is_ascii_digit() {
        return (format!("Digit{c}"), c as u32, Some(key.to_string()));
    }
    (String::new(), 0, Some(key.to_string()))
}

impl Page {
    /// Like `send`, for commands that take as long as the gesture they perform.
    pub fn send_long(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.send_timeout(method, params, Some(&self.session), Duration::from_secs(120))
    }
}
