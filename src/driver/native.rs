//! The `page` driver for native targets (SPEC §9.6, §13.7): the bench file's page calls mapped onto the
//! app's bridge ops, so one bench file drives a page and a native app alike. What the bridge does not
//! do fails with the call's name.

use crate::model::{Error, Result};
use crate::target::native_bridge::Bridge;
use serde_json::{Map, Value, json};
use std::time::Duration;

pub struct NativeDriver<'a> {
    pub bridge: &'a Bridge,
    pub op_timeout: Duration,
    /// A pixels block's folder: `snapshot(name)` writes `<name>.png` there. Elsewhere a snapshot does nothing.
    pub pixels: Option<std::path::PathBuf>,
}

impl NativeDriver<'_> {
    /// One page call from the runner: `{ call, args }`.
    pub fn dispatch(&self, call: &str, args: &[Value]) -> Result<Value> {
        let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        let opts = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        match call {
            "click" | "hover" => self.op(call, json!({ "selector": s(0) }), &opts(1), 0.0),
            "move" => {
                let at = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0);
                self.op("move", json!({ "x": at(0), "y": at(1) }), &opts(2), 0.0)
            }
            "type" => {
                let o = opts(2);
                let delay = o.get("delay").and_then(Value::as_f64).unwrap_or(0.0);
                let text = s(1);
                let mut a = json!({ "selector": s(0), "text": text });
                if delay > 0.0 {
                    a["delay_ms"] = json!(delay);
                }
                self.op("type", a, &o, delay * text.chars().count() as f64 / 1e3)
            }
            "press" => self.op("press", json!({ "chord": s(0) }), &opts(1), 0.0),
            "scroll" => {
                let d = opts(1);
                if d.get("byX").and_then(Value::as_f64).is_some_and(|x| x != 0.0) {
                    return Err(Error::usage("page.scroll() with byX: a native app's bridge scrolls vertically; use page.wheel() for a horizontal delta"));
                }
                let by = d.get("by").and_then(Value::as_f64).unwrap_or(0.0);
                let speed = d.get("speed").and_then(Value::as_f64).unwrap_or(3000.0);
                self.op("scroll", json!({ "selector": s(0), "by": by, "speed": speed }), &opts(2), by.abs() / speed.max(1.0))
            }
            "wheel" => {
                let d = opts(1);
                let mut a = json!({ "dx": d.get("dx").and_then(Value::as_f64).unwrap_or(0.0), "dy": d.get("dy").and_then(Value::as_f64).unwrap_or(0.0) });
                if !s(0).is_empty() {
                    a["selector"] = json!(s(0));
                }
                self.op("wheel", a, &opts(2), 0.0)
            }
            "waitFor" => {
                let o = opts(1);
                let mut until = json!({ "selector": s(0) });
                for k in ["text", "exact"] {
                    if let Some(v) = o.get(k) {
                        until[k] = v.clone();
                    }
                }
                self.wait(until, &o)
            }
            "idle" => self.wait(json!("idle"), &opts(0)),
            "frame" => self.wait(json!("frame"), &opts(0)),
            "wait" => {
                let ms = args.first().and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
                std::thread::sleep(Duration::from_millis(ms as u64));
                Ok(Value::Bool(true))
            }
            "window" => {
                let o = opts(1);
                let mut a = json!({ "action": s(0) });
                if let Some(w) = o.get("window") {
                    a["window"] = w.clone();
                }
                self.bridge.request("window", a, self.timeout(&o) + Duration::from_secs(10))?;
                Ok(Value::Bool(true))
            }
            "snapshot" => {
                let Some(dir) = &self.pixels else { return Ok(Value::Bool(false)) };
                let name: String = s(0).chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
                std::fs::create_dir_all(dir)?;
                let mut a = json!({ "path": dir.join(format!("{name}.png")) });
                if let Some(w) = opts(1).get("window") {
                    a["window"] = w.clone();
                }
                self.bridge.request("snapshot", a, Duration::from_secs(60))?;
                Ok(Value::Bool(true))
            }
            "goto" | "eval" | "down" | "up" => Err(Error::usage(format!(
                "page.{call}() is not available on a native target: its bridge drives elements, the pointer, keys, the wheel and windows (click, hover, move, type, press, scroll, wheel, waitFor, idle, wait, window, snapshot)"
            ))),
            other => Err(Error::msg(format!("unknown page call {other}"))),
        }
    }

    fn timeout(&self, opts: &Value) -> Duration {
        opts.get("timeout").and_then(Value::as_u64).map(Duration::from_millis).unwrap_or(self.op_timeout)
    }

    /// An input op with the call's options: `until`, `timeout` (as `timeout_ms`), `window`, and the
    /// pointer's `button`, `count` and `modifiers`. `busy_s` is how long the input itself takes.
    fn op(&self, op: &str, mut args: Value, opts: &Value, busy_s: f64) -> Result<Value> {
        let t = self.timeout(opts);
        if let (Some(a), Some(o)) = (args.as_object_mut(), opts.as_object()) {
            copy(o, a, &["until", "window", "button", "count", "modifiers"]);
            a.insert("timeout_ms".into(), json!(t.as_millis() as u64));
        }
        self.bridge.request(op, args, t + Duration::from_secs_f64(busy_s) + Duration::from_secs(10))
    }

    /// `wait` until a selector, `"idle"` or `"frame"`.
    fn wait(&self, until: Value, opts: &Value) -> Result<Value> {
        let t = self.timeout(opts);
        let mut a = json!({ "until": until, "timeout_ms": t.as_millis() as u64 });
        if let Some(w) = opts.get("window") {
            a["window"] = w.clone();
        }
        self.bridge.request("wait", a, t + Duration::from_secs(10))?;
        Ok(Value::Bool(true))
    }
}

fn copy(from: &Map<String, Value>, to: &mut Map<String, Value>, keys: &[&str]) {
    for k in keys {
        if let Some(v) = from.get(*k).filter(|v| !v.is_null()) {
            to.insert(k.to_string(), v.clone());
        }
    }
}
