//! The `page` driver (SPEC §9.6): inputs over CDP, event-driven waits, run and phase marks.

use crate::model::{Error, Result};
use crate::target::chromium::Page;
use serde_json::{Value, json};
use std::time::Duration;

/// Installed lazily in the page (it disappears on navigation). Observers only: nothing polls.
const HELPER: &str = r#"window.__strobe ??= (() => {
  const s = {}
  const visible = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 }
  s.find = (sel) => { const el = document.querySelector(sel); return el && visible(el) ? el : null }
  s.box = (sel) => {
    const el = s.find(sel)
    if (!el) return null
    let r = el.getBoundingClientRect()
    if (r.bottom < 0 || r.top > innerHeight || r.right < 0 || r.left > innerWidth) { el.scrollIntoView({ block: "nearest", inline: "nearest" }); r = el.getBoundingClientRect() }
    return { x: r.left + Math.min(r.width, innerWidth) / 2, y: r.top + Math.min(r.height, innerHeight) / 2, w: r.width, h: r.height }
  }
  s.waitFor = (sel, timeout) => new Promise((resolve, reject) => {
    if (s.find(sel)) return resolve(true)
    const mo = new MutationObserver(() => { if (s.find(sel)) { mo.disconnect(); clearTimeout(t); resolve(true) } })
    mo.observe(document, { subtree: true, childList: true, attributes: true })
    const t = setTimeout(() => { mo.disconnect(); reject(new Error("timed out waiting for " + sel)) }, timeout)
  })
  s.idle = (quiet, timeout) => new Promise((resolve, reject) => {
    let timer, po
    const stop = () => { mo.disconnect(); if (po) po.disconnect(); clearTimeout(timer); clearTimeout(limit) }
    const kick = () => { clearTimeout(timer); timer = setTimeout(() => { stop(); resolve(true) }, quiet) }
    const mo = new MutationObserver(kick)
    mo.observe(document, { subtree: true, childList: true, attributes: true, characterData: true })
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
                self.page.navigate(&s(0), self.timeout(&opts(1)))?;
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
                let (x, y) = self.locate(&s(0), &opts(2))?;
                for kind in ["mousePressed", "mouseReleased"] {
                    self.page.send("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
                }
                self.page.send("Input.insertText", json!({ "text": s(1) }))?;
                self.until(&opts(2))
            }
            "press" => {
                self.press(&s(0))?;
                self.until(&opts(1))
            }
            "scroll" => {
                let o = opts(1);
                let (x, y) = self.locate(&s(0), &opts(2))?;
                let by = o.get("by").and_then(Value::as_f64).unwrap_or(0.0);
                let by_x = o.get("byX").and_then(Value::as_f64).unwrap_or(0.0);
                let speed = o.get("speed").and_then(Value::as_f64).unwrap_or(2000.0);
                self.page.send_long(
                    "Input.synthesizeScrollGesture",
                    json!({ "x": x, "y": y, "yDistance": -by, "xDistance": -by_x, "speed": speed, "gestureSourceType": "mouse", "preventFling": true }),
                )?;
                self.until(&opts(2))
            }
            "waitFor" => {
                let t = self.timeout(&opts(1)).as_millis();
                self.call(&format!("__strobe.waitFor({}, {t})", json!(s(0))))?;
                Ok(Value::Bool(true))
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
        let t = self.timeout(opts).as_millis();
        let sel = json!(selector);
        self.call(&format!("__strobe.waitFor({sel}, {t})"))?;
        let b = self.page.eval(&format!("__strobe.box({sel})"))?;
        if b.is_null() {
            return Err(Error::msg(format!("no visible element matches {selector}")));
        }
        Ok((b["x"].as_f64().unwrap_or(0.0), b["y"].as_f64().unwrap_or(0.0)))
    }

    fn until(&self, opts: &Value) -> Result<Value> {
        let Some(until) = opts.get("until").and_then(Value::as_str) else { return Ok(Value::Bool(true)) };
        let t = self.timeout(opts).as_millis();
        match until {
            "idle" => self.call(&format!("__strobe.idle(300, {t})")),
            "frame" => self.call("__strobe.frame()"),
            sel => self.call(&format!("__strobe.waitFor({}, {t})", json!(sel))),
        }
    }

    fn press(&self, chord: &str) -> Result<()> {
        let parts: Vec<&str> = chord.split('+').collect();
        let key = parts.last().copied().unwrap_or("");
        let mut modifiers = 0;
        for m in &parts[..parts.len().saturating_sub(1)] {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "alt" => 1,
                "control" | "ctrl" => 2,
                "meta" | "cmd" => 4,
                "shift" => 8,
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
