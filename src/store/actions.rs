//! What a block's hooks did to the target, in order (SPEC §14.1 `actions.json.gz`): each input, page
//! load, window or accessibility action, `ctx.call` and `ctx.exec`, with when it went out, what it aimed
//! at and how it ended. Kept beside a block's recording so a seeded workload (`--seed`) can be replayed
//! and compared; typed text is the user's (rule 9), so the file is owner-only and views redact it.

use crate::model::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;

/// The recording's file.
pub const FILE: &str = "actions.json.gz";

/// A block's log stops growing here (a runaway loop); the file says it was cut (`truncated`).
const MAX: usize = 100_000;

/// The hook an action ran in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hook {
    Before,
    Prepare,
    Setup,
    Run,
    Reset,
    Teardown,
}

impl Hook {
    /// The hook a runner op calls, for the ops that run a bench file's hooks.
    pub fn of_op(op: &str) -> Option<Hook> {
        Some(match op {
            "before" => Hook::Before,
            "prepare" => Hook::Prepare,
            "setup" => Hook::Setup,
            "run" => Hook::Run,
            "reset" => Hook::Reset,
            "close" => Hook::Teardown,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Hook::Before => "before",
            Hook::Prepare => "prepare",
            Hook::Setup => "setup",
            Hook::Run => "run",
            Hook::Reset => "reset",
            Hook::Teardown => "teardown",
        }
    }
}

/// The element an input landed on, as the driver's aim found it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Hit {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    /// Where the pointer went, in the window's client area (device-independent pixels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<[f64; 2]>,
    /// The element's box there: x, y, width, height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<[f64; 4]>,
    /// The driver scrolled it into view first.
    #[serde(default)]
    pub scrolled: bool,
}

/// How the call ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    /// A validator refused it (a covered element: `Covered`).
    Refusal {
        validator: String,
        summary: String,
    },
    Error {
        message: String,
    },
}

/// Which edge an input's latency sample was timed from, or why it gave none (`frames::reactions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleEdge {
    Press,
    Sent,
    Scroll,
    Unchanged,
    Busy,
    Unmatched,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub edge: SampleEdge,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
}

/// One call a hook made to the target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionRecord {
    /// Its place in the block, from 0.
    pub seq: u32,
    pub hook: Hook,
    /// The block's run (warmup included, from 0) it belongs to: a run's, and its reset's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<u32>,
    /// Sent inside a `ctx.untimed` stretch.
    #[serde(default)]
    pub untimed: bool,
    /// The innermost `ctx.phase` it ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// When strobe got the call from the runner (its request's arrival) and when it answered, on the
    /// host clock in ns (the clock of presents and marks). This is the call, not its input: the
    /// driver's own send time (`frames::Sent::t_ns`) is later by the aim and any wait before it.
    /// Where the driver reports them, `t_ns` becomes that send time, beside `press_ns`, `hit` and
    /// `sample` (not yet: SPEC §24.2).
    pub t_ns: u64,
    /// A click's press, when the driver says when it went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press_ns: Option<u64>,
    pub end_ns: u64,
    /// The method as the bench file called it: `click`, `type`, `goto`, `call`, `exec`, …
    pub call: String,
    /// What it aimed at as the bench file gave it (its `@name` resolved): a selector string, or the
    /// object form whole (`{ selector, role, text, exact, index, focus }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit: Option<Hit>,
    /// Its other arguments as given (typed text included: views redact it), each string cut at
    /// `ARG_BYTES` and each array at `ARG_ITEMS` (`args_truncated` then says so).
    #[serde(default)]
    pub args: Vec<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub args_truncated: bool,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
}

/// The file: the experiment's seed and the block's actions in order.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionLog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
    /// More than `MAX` actions: the rest were not kept.
    #[serde(default)]
    pub truncated: bool,
    pub actions: Vec<ActionRecord>,
}

/// The runner calls that act on the target: inputs, page loads, window and accessibility actions.
const DRIVER_ACTIONS: &[&str] = &["goto", "click", "hover", "move", "type", "press", "down", "up", "drag", "scroll", "wheel", "window", "act"];

/// Those whose first argument names an element.
const AIMED: &[&str] = &["click", "hover", "type", "drag", "scroll", "wheel", "act"];

impl ActionLog {
    /// A new block: nothing logged yet.
    pub fn clear(&mut self, seed: Option<u32>) {
        *self = ActionLog { seed, ..ActionLog::default() };
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Adds an action; past `MAX` the log is marked cut instead.
    pub fn push(&mut self, mut a: ActionRecord) {
        if self.actions.len() >= MAX {
            self.truncated = true;
            return;
        }
        a.seq = self.actions.len() as u32;
        self.actions.push(a);
    }

    /// The action a runner request is, if it is one: a driver input or load, `ctx.call` or `ctx.exec`,
    /// stamped `t_ns` and not yet answered.
    pub fn of_request(hook: Hook, run: Option<u32>, untimed: bool, phase: Option<String>, op: &str, args: &Value, t_ns: u64) -> Option<ActionRecord> {
        let (call, selector, rest): (String, Option<Value>, Vec<Value>) = match op {
            "driver" => {
                let call = args["call"].as_str().filter(|c| DRIVER_ACTIONS.contains(c))?;
                let list = args["args"].as_array().map(Vec::as_slice).unwrap_or_default();
                let (selector, rest) = match list.split_first() {
                    Some((s, rest)) if AIMED.contains(&call) => (Some(s.clone()), rest),
                    _ => (None, list),
                };
                // `""` (type where focus is) and `{}` aim at nothing.
                let selector = selector.filter(|s| !(s.as_str() == Some("") || s.as_object().is_some_and(|o| o.is_empty()) || s.is_null()));
                (call.to_string(), selector, rest.to_vec())
            }
            "call" => {
                let mut list = vec![args["name"].clone()];
                list.extend(args["args"].as_array().cloned().unwrap_or_default());
                ("call".into(), None, list)
            }
            "exec" => ("exec".into(), None, args["line"].as_str().map(|l| vec![Value::String(l.into())]).unwrap_or_default()),
            _ => return None,
        };
        let mut rest = rest;
        let args_truncated = rest.iter_mut().fold(false, |cut, v| bound(v) | cut);
        Some(ActionRecord { seq: 0, hook, run, untimed, phase, t_ns, press_ns: None, end_ns: t_ns, call, selector, hit: None, args: rest, args_truncated, outcome: Outcome::Ok, sample: None })
    }

    /// Writes the log gzipped at `path` (owner-only): a recording's `actions.json.gz`, or beside a
    /// `--check` result for a case without a recording.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&serde_json::to_vec(self)?)?;
        crate::host::paths::write_owner_only(path, &gz.finish()?).map_err(|e| Error::msg(format!("write {}: {e}", path.display())))
    }

    /// A log written by `write` (or plain JSON); None when there is none.
    pub fn read(path: &Path) -> Result<Option<ActionLog>> {
        if !path.is_file() {
            return Ok(None);
        }
        let raw = std::fs::read(path)?;
        let text = if raw.starts_with(&[0x1f, 0x8b]) {
            let mut t = Vec::new();
            flate2::read::GzDecoder::new(raw.as_slice()).read_to_end(&mut t)?;
            t
        } else {
            raw
        };
        Ok(Some(serde_json::from_slice(&text)?))
    }
}

/// A string argument longer than this keeps its first part (a pasted document typed in one call).
const ARG_BYTES: usize = 4096;

/// An array argument longer than this keeps its first items (a drag's path).
const ARG_ITEMS: usize = 32;

/// Cuts `v`'s strings and arrays to the bounds, at every depth; true when it cut something.
fn bound(v: &mut Value) -> bool {
    match v {
        Value::String(s) if s.len() > ARG_BYTES => {
            let mut end = ARG_BYTES;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            true
        }
        Value::Array(a) => {
            let cut = a.len() > ARG_ITEMS;
            a.truncate(ARG_ITEMS);
            a.iter_mut().fold(cut, |c, x| bound(x) | c)
        }
        Value::Object(o) => o.values_mut().fold(false, |c, x| bound(x) | c),
        _ => false,
    }
}

impl Outcome {
    /// How a served request ended.
    pub fn of(r: &Result<Value>) -> Outcome {
        match r {
            Ok(_) => Outcome::Ok,
            Err(e) => match &e.refusal {
                Some(r) => Outcome::Refusal { validator: r.validator.to_string(), summary: r.summary.clone() },
                None => Outcome::Error { message: e.message.clone() },
            },
        }
    }
}
