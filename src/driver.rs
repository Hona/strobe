//! The `page` driver (SPEC §9.6): inputs over CDP, event-driven waits, run and phase marks.

pub mod native;

use crate::model::{Error, Result};
use crate::target::chromium::Page;
use serde_json::{Value, json};
use std::time::Duration;

/// Installed in every document before its scripts run: the browser's own rAF, which a page may replace (a game
/// frame pacer does) and so delay the driver's waits on its own schedule.
pub const NATIVE_RAF: &str = "window.__strobeRaf ??= window.requestAnimationFrame.bind(window)";

/// Installed lazily in the page (it disappears on navigation). Observers only: nothing polls, except a check per
/// frame while a CSS animation or transition runs, since those change what is visible without a mutation.
const HELPER: &str = r#"window.__strobe ??= (() => {
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
    let done = false
    const check = () => { if (!done && s.find(sel, text, exact)) { stop(); clearTimeout(t); resolve(true) } }
    // A fade in (opacity from 0) makes an element visible on a frame, not on a mutation: while any animation or
    // transition runs, check each frame.
    let ticking = false
    const tick = () => {
      check()
      if (!done && document.getAnimations().some((a) => a.playState === "running")) raf(tick)
      else ticking = false
    }
    const animating = () => { if (!ticking && !done) { ticking = true; raf(tick) } }
    const unwatch = observe(() => { check(); animating() }, { subtree: true, childList: true, attributes: true, characterData: text != null })
    const events = ["focusin", "focusout", "animationstart", "transitionrun"]
    for (const e of events) document.addEventListener(e, e.startsWith("focus") ? check : animating, true)
    const stop = () => {
      done = true
      unwatch()
      for (const e of events) document.removeEventListener(e, e.startsWith("focus") ? check : animating, true)
    }
    animating()
    const t = setTimeout(() => { stop(); reject(new Error("timed out waiting for " + sel + (text == null ? "" : (exact ? " with exactly " : " containing ") + JSON.stringify(text)))) }, timeout)
  })
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
      const what = mutations + " DOM mutations" + (frames ? ", " + frames + " frames over 50 ms (longest " + Math.round(longest) + " ms)" : "")
      reject(new Error("the page never went idle in " + timeout + " ms: " + what + (top ? "; most: " + top : "")))
    }, timeout)
    kick()
  })
  s.frame = () => new Promise((r) => raf(() => raf(() => r(true))))
  s.nodes = () => document.getElementsByTagName("*").length
  return s
})(), true"#;

pub struct Driver {
    pub page: Page,
    pub token: String,
    pub op_timeout: Duration,
    /// A pixels block's folder: `snapshot(name)` writes `<name>.png` there. Elsewhere a snapshot does nothing.
    pub pixels: Option<std::path::PathBuf>,
    /// Modifiers held down by `down` (CDP's bits: Alt 1, Control 2, Meta 4, Shift 8).
    held: std::cell::Cell<i64>,
    /// Where the pointer is (CSS pixels), and the mouse buttons held (`buttons` bits: left 1, right 2,
    /// middle 4) with the name of the last one pressed.
    pointer: std::cell::Cell<(f64, f64)>,
    buttons: std::cell::Cell<(i64, &'static str)>,
    /// The display's frame period as the page's rAF sees it (ms), measured at the first drag.
    frame_ms: std::cell::Cell<Option<f64>>,
    /// Each drag's move times on the host clock (a button held by `down` or `drag`), for
    /// `input.rate` and `input.gap.p95`; the block takes them per run.
    pub drags: std::cell::RefCell<Vec<Vec<u64>>>,
}

impl Driver {
    pub fn new(page: Page, token: String) -> Driver {
        Driver {
            page,
            token,
            op_timeout: Duration::from_secs(30),
            pixels: None,
            held: std::cell::Cell::new(0),
            pointer: std::cell::Cell::new((0.0, 0.0)),
            buttons: std::cell::Cell::new((0, "none")),
            frame_ms: std::cell::Cell::new(None),
            drags: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// A pointer move to (x, y) carrying the held buttons. While one is held it is a drag's move: sent
    /// without waiting for the renderer to handle it (`paced`), and its time is kept for `input.rate`.
    fn pointer_move(&self, x: f64, y: f64, paced: bool) -> Result<()> {
        let (mask, name) = self.buttons.get();
        self.pointer.set((x, y));
        let p = json!({ "type": "mouseMoved", "x": x, "y": y, "button": name, "buttons": mask, "modifiers": self.held.get() });
        if mask != 0
            && let Some(d) = self.drags.borrow_mut().last_mut()
        {
            d.push(crate::host::now_ns());
        }
        if paced { self.page.post("Input.dispatchMouseEvent", p) } else { self.page.send("Input.dispatchMouseEvent", p).map(|_| ()) }
    }

    /// A mouse button going down or up where the pointer is.
    fn pointer_button(&self, name: &'static str, bit: i64, down: bool) -> Result<()> {
        let (x, y) = self.pointer.get();
        let mask = if down { self.buttons.get().0 | bit } else { self.buttons.get().0 & !bit };
        self.buttons.set((mask, if mask == 0 { "none" } else { name }));
        if down {
            self.drags.borrow_mut().push(vec![crate::host::now_ns()]);
        }
        let kind = if down { "mousePressed" } else { "mouseReleased" };
        self.page.send("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": name, "buttons": mask, "clickCount": 1, "modifiers": self.held.get() })).map(|_| ())
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
    fn drag(&self, sel: &str, path: &Value, o: &Value) -> Result<Value> {
        use crate::target::a11y::keys::{parse_path, path_at};
        let path = parse_path(path)?;
        let (x0, y0) = if sel.is_empty() { self.pointer.get() } else { self.locate(sel, o)? };
        let (name, bit) = cdp_button(match o.get("button").and_then(Value::as_str).unwrap_or("left") {
            "right" => "MouseRight",
            "middle" => "MouseMiddle",
            _ => "Mouse",
        })
        .unwrap_or(("left", 1));
        let duration = o.get("duration").and_then(Value::as_f64).unwrap_or(1000.0).max(1.0);
        let frame = self.frame_period()?;
        let (dx, dy) = path_at(&path, 0.0);
        self.pointer_move(x0 + dx, y0 + dy, false)?;
        let held = self.buttons.get().0 & bit != 0;
        if held {
            self.drags.borrow_mut().push(vec![crate::host::now_ns()]);
        } else {
            self.pointer_button(name, bit, true)?;
        }
        let steps = (duration / frame).ceil().max(1.0) as u32;
        let start = std::time::Instant::now();
        for k in 1..=steps {
            let due = start + Duration::from_secs_f64(frame * k as f64 / 1e3);
            if let Some(w) = due.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(w);
            }
            let (dx, dy) = path_at(&path, k as f64 / steps as f64);
            self.pointer_move(x0 + dx, y0 + dy, true)?;
        }
        if !held {
            self.pointer_button(name, bit, false)?;
        }
        self.until(o)
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
        // A wait ends with the document it runs in: CDP reports a reload or navigation as a collected promise or
        // a destroyed context, which says nothing about what happened to the page.
        self.page.eval_timeout(expr, timeout + Duration::from_secs(10)).map_err(|e| {
            if e.message.contains("Promise was collected") || e.message.contains("context was destroyed") {
                Error::msg(format!("the page reloaded or navigated away while strobe waited ({expr}): the app replaced the document on its own"))
            } else {
                e
            }
        })
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
                self.pointer.set((x, y));
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
                self.pointer_move(x, y, false)?;
                self.until(&opts(1))
            }
            "move" => {
                let at = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0);
                self.pointer_move(at(0), at(1), false)?;
                self.until(&opts(2))
            }
            "drag" => self.drag(&s(0), &opts(1), &opts(2)),
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
            // One wheel event at the element's centre (the viewport's without a selector), as a mouse wheel sends.
            "wheel" => {
                let d = opts(1);
                let (x, y) = if s(0).is_empty() {
                    let c = self.page.eval("[innerWidth / 2, innerHeight / 2]")?;
                    (c[0].as_f64().unwrap_or(0.0), c[1].as_f64().unwrap_or(0.0))
                } else {
                    self.locate(&s(0), &opts(2))?
                };
                let delta = |k: &str| d.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                self.page.send("Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": delta("dx"), "deltaY": delta("dy") }))?;
                self.until(&opts(2))
            }
            "window" => Err(Error::usage("page.window() shows, hides, focuses or closes a native app's windows; a page has none of its own")),
            "act" | "query" => Err(Error::usage(format!("page.{call}() reads a native app's accessibility tree; on a page use eval or Playwright in prepare"))),
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
            // A key held across other calls (a game's WASD, a held shortcut): `down` presses it, `up` lets go.
            "down" | "up" => {
                let o = opts(1);
                if let Some((name, bit)) = cdp_button(&s(0)) {
                    // `at`: a selector or [x, y] to move to first, as a person moves before pressing.
                    match o.get("at") {
                        Some(Value::String(sel)) => {
                            let (x, y) = self.locate(sel, &o)?;
                            self.pointer_move(x, y, false)?;
                        }
                        Some(Value::Array(p)) => self.pointer_move(p.first().and_then(Value::as_f64).unwrap_or(0.0), p.get(1).and_then(Value::as_f64).unwrap_or(0.0), false)?,
                        _ => {}
                    }
                    self.pointer_button(name, bit, call == "down")?;
                } else {
                    self.key(&s(0), call == "down")?;
                }
                self.until(&o)
            }
            // The page's pixels for `--pixels`: only a pixels block captures, so timed runs never pay for it.
            "snapshot" => {
                let Some(dir) = &self.pixels else { return Ok(Value::Bool(false)) };
                let name: String = s(0).chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
                let shot = self.page.send_long("Page.captureScreenshot", json!({ "format": "png" }))?;
                let png = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, shot["data"].as_str().unwrap_or("")).map_err(|e| Error::msg(format!("snapshot {name}: {e}")))?;
                std::fs::create_dir_all(dir)?;
                std::fs::write(dir.join(format!("{name}.png")), png)?;
                Ok(Value::Bool(true))
            }
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
        let modifiers = modifiers | self.held.get();
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

    /// One key, pressed (`down`) or let go: a held key stays down across other calls until its `up`. A held
    /// modifier (Shift, Control, Alt, Meta) also marks the key events sent while it is down. Chromium sends no
    /// auto-repeat, as a game reading key state does not need.
    fn key(&self, key: &str, down: bool) -> Result<()> {
        let bit = match key {
            "Alt" => 1,
            "Control" => 2,
            "Meta" => 4,
            "Shift" => 8,
            _ => 0,
        };
        let held = if down { self.held.get() | bit } else { self.held.get() & !bit };
        self.held.set(held);
        let (code, key_code, text) = key_info(key);
        let mut e = json!({ "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": held });
        if down {
            let text = text.filter(|_| held & 0b0111 == 0);
            e["type"] = json!(if text.is_some() { "keyDown" } else { "rawKeyDown" });
            if let Some(t) = text {
                e["text"] = json!(t);
            }
        } else {
            e["type"] = json!("keyUp");
        }
        self.page.send("Input.dispatchKeyEvent", e)?;
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
        ("Shift", "ShiftLeft", 16, None),
        ("Control", "ControlLeft", 17, None),
        ("Alt", "AltLeft", 18, None),
        ("Meta", "MetaLeft", 91, None),
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

    /// A command whose reply nobody waits for (`Cdp::post`).
    pub fn post(&self, method: &str, params: Value) -> Result<()> {
        self.cdp.post(method, params, Some(&self.session))
    }
}

/// CDP's name and `buttons` bit for a mouse button `page.down` names (`Mouse`, `MouseRight`, `MouseMiddle`).
fn cdp_button(name: &str) -> Option<(&'static str, i64)> {
    use crate::target::a11y::keys::{Button, mouse_button};
    mouse_button(name).map(|b| match b {
        Button::Left => ("left", 1),
        Button::Right => ("right", 2),
        Button::Middle => ("middle", 4),
    })
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
