//! `--video` (SPEC §8.8): one untimed block's (or `record`'s) video, kept as `video.<ext>` beside
//! `video.json`, which puts the block's runs, the inputs strobe sent and the case's phases on the
//! video's own time. Inputs come from the page calls the block served (their names, what they aimed
//! at, the edge a latency is timed from) and, when the recording has one, its action log
//! (`actions.json.gz`: each action's measured reaction). Typed text is never kept (rule 9).

use crate::host::TopWindow;
use crate::model::{Error, Result};
use crate::probe::video::{Capture, Kind, Scratch};
use crate::store;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::Path;

/// A page call during the video: when strobe began and finished it, and what it was.
struct Call {
    start_ns: u64,
    end_ns: u64,
    call: String,
    label: String,
}

/// An input strobe sent (host clock): when, when its press went out (a click), and its kind.
pub struct Sent {
    pub t_ns: u64,
    pub press_ns: Option<u64>,
}

pub struct Recorder {
    capture: Capture,
    /// The folder the capture writes into: dropped after it (field order), with the file unless kept.
    scratch: Scratch,
    calls: RefCell<Vec<Call>>,
    /// Why this video is not the kind asked for (a screen capture that failed, so the renderer's).
    note: Option<String>,
    /// The session was locked when it started.
    locked: bool,
}

/// What a call does, in a few words: the element or key it aimed at; never the text it typed.
fn label(call: &str, args: &[Value]) -> String {
    let short = |v: &Value| -> String {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if s.chars().count() > 48 { format!("{}…", s.chars().take(47).collect::<String>()) } else { s }
    };
    let first = args.first().map(short).unwrap_or_default();
    match call {
        "type" => {
            let n = args.get(1).and_then(Value::as_str).map_or(0, |t| t.chars().count());
            if first.is_empty() { format!("type {n} characters") } else { format!("type {n} characters into {first}") }
        }
        "move" => format!("move to {}, {}", args.first().and_then(Value::as_f64).unwrap_or(0.0), args.get(1).and_then(Value::as_f64).unwrap_or(0.0)),
        // A page's address without its query, which may hold a token.
        "goto" => format!("goto {}", first.split(['?', '#']).next().unwrap_or("")),
        _ if first.is_empty() => call.to_string(),
        _ => format!("{call} {first}"),
    }
}

impl Recorder {
    /// Starts the video of a target: the OS's copy of its `window` when it has one (`screen`), else, or
    /// headless, its Chromium `page`'s own frames (`renderer`). A headed page whose screen capture
    /// cannot start falls back to its renderer and says why.
    pub fn start(window: Option<&TopWindow>, page: Option<&crate::target::chromium::Page>, headless: bool) -> Result<Recorder> {
        let scratch = Scratch::new()?;
        let base = scratch.base();
        let (capture, note) = match (headless, window, page) {
            (true, _, Some(p)) => (Capture::renderer(p, &base)?, None),
            (true, _, None) => {
                return Err(Error::refused(
                    "Headless",
                    "a headless browser other than Chromium draws nothing strobe can film: no display, and stock Firefox and WebKit have no screencast (Playwright films them through screencast commands it patched into its own builds)",
                    "record the page headed, or in Chromium (drop --engine)",
                ));
            }
            (false, Some(w), p) => match (Capture::screen(w, &base), p) {
                (Ok(c), _) => (c, None),
                (Err(e), Some(p)) => (Capture::renderer(p, &base)?, Some(format!("the OS's screen capture did not start ({}), so this is the renderer's own frames", e.message))),
                (Err(e), None) => return Err(e),
            },
            (false, None, Some(p)) => (Capture::renderer(p, &base)?, Some("strobe found no window of the target to capture, so this is the renderer's own frames".into())),
            (false, None, None) => return Err(Error::refused("NotForTarget", "the target has no window strobe can film", "record a target that opens a window")),
        };
        Ok(Recorder { capture, scratch, calls: RefCell::new(Vec::new()), note, locked: crate::host::session_locked() })
    }

    pub fn kind(&self) -> Kind {
        self.capture.kind()
    }

    /// Notes a page call the block served from `start_ns` to now, when it is an input or a navigation.
    pub fn note(&self, call: &str, args: &[Value], start_ns: u64) {
        if crate::driver::os::INPUT.contains(&call) || call == "goto" {
            self.calls.borrow_mut().push(Call { start_ns, end_ns: crate::host::now_ns(), call: call.to_string(), label: label(call, args) });
        }
    }

    /// Stops the video (the file is finished, not kept yet).
    pub fn stop(self) -> Result<Stopped> {
        let locked = self.locked || crate::host::session_locked();
        let video = self.capture.stop()?;
        let mut note = self.note;
        if locked && video.kind == Kind::Screen {
            note = Some(note.map_or(String::new(), |n| format!("{n}; ")) + "the session was locked: these are the compositor's frames of the window while the display showed the lock screen");
        }
        Ok(Stopped { video, calls: self.calls.into_inner(), note, _scratch: self.scratch })
    }

    /// `stop`, then `keep`.
    pub fn finish(self, dir: &Path, runs: &[Run], sent: &[Sent], marks: &[(String, u64)]) -> Result<Value> {
        self.stop()?.keep(dir, runs, sent, marks)
    }
}

/// A run on the host clock: its key, start, end, and whether it is a measured one (not warmup).
pub type Run = (String, u64, u64, bool);

/// A finished video, not yet in its recording.
pub struct Stopped {
    video: crate::probe::video::Video,
    calls: Vec<Call>,
    note: Option<String>,
    /// Holds the file until `keep` moves it, and removes the folder after.
    _scratch: Scratch,
}

impl Stopped {
    /// Keeps the video in the recording `dir`: `video.<ext>` and `video.json` with the runs, the
    /// inputs strobe sent and the phase marks on media time. Returns what the recording's meta.json
    /// says of it.
    pub fn keep(self, dir: &Path, runs: &[Run], sent: &[Sent], marks: &[(String, u64)]) -> Result<Value> {
        let v = self.video;
        let file = format!("video.{}", v.ext);
        store::adopt(dir, &file, &v.path)?;
        let at = |t: u64| ((t as f64 - v.t0_ns as f64) / 1e9 * 1e4).round() / 1e4;
        // Each action's measured reaction (its sample's latency from the edge it was timed from), when
        // the recording keeps an action log: the action served in the call's span.
        let actions = store::actions::ActionLog::read(&dir.join(store::actions::FILE)).ok().flatten().map(|l| l.actions).unwrap_or_default();
        // A sample timed from a click's press counts from the press, else from the edge sent.
        let reaction = |from: u64, to: u64, sent_at: u64, press: Option<u64>| {
            actions.iter().filter(|a| a.t_ns + 2_000_000 >= from && a.t_ns <= to).find_map(|a| {
                let s = a.sample.as_ref()?;
                let edge = if s.edge == store::actions::SampleEdge::Press { press.or(a.press_ns)? } else { sent_at };
                Some(at(edge) + s.latency_ms? / 1e3)
            })
        };
        let mut markers: Vec<Value> = Vec::new();
        for c in &self.calls {
            // The edge a latency is timed from: the input strobe sent inside the call, else its start.
            let s = sent.iter().find(|s| s.t_ns + 1_000_000 >= c.start_ns && s.t_ns <= c.end_ns + 1_000_000);
            let t = s.map_or(c.start_ns, |s| s.t_ns);
            markers.push(json!({
                "t_s": at(t), "press_s": s.and_then(|s| s.press_ns).map(at), "end_s": at(c.end_ns), "kind": c.call, "label": c.label,
                "reaction_s": reaction(c.start_ns, c.end_ns, t, s.and_then(|s| s.press_ns)),
            }));
        }
        // Inputs no call explains (an OS input's), as they were sent.
        for s in sent.iter().filter(|s| !self.calls.iter().any(|c| s.t_ns + 1_000_000 >= c.start_ns && s.t_ns <= c.end_ns + 1_000_000)) {
            markers.push(json!({ "t_s": at(s.t_ns), "press_s": s.press_ns.map(at), "end_s": Value::Null, "kind": "input", "label": "input", "reaction_s": Value::Null }));
        }
        for (key, t) in marks {
            if let Some(name) = key.split(":phase:").nth(1).and_then(|r| r.strip_suffix(":start")) {
                let end = marks.iter().find(|(k, _)| k.split(":phase:").nth(1).and_then(|r| r.strip_suffix(":end")) == Some(name) && k.split(":phase:").next() == key.split(":phase:").next());
                markers.push(json!({ "t_s": at(*t), "press_s": Value::Null, "end_s": end.map(|e| at(e.1)), "kind": "phase", "label": name, "reaction_s": Value::Null }));
            }
            // A batch's case (`case:<i>:<name>:start|end`): one launch filmed for several cases.
            if let Some(case) = key.strip_prefix("case:").and_then(|r| r.strip_suffix(":start")) {
                let end = marks.iter().find(|(k, _)| k.strip_prefix("case:").and_then(|r| r.strip_suffix(":end")) == Some(case));
                let label = case.split_once(':').map_or(case, |(_, name)| name);
                markers.push(json!({ "t_s": at(*t), "press_s": Value::Null, "end_s": end.map(|e| at(e.1)), "kind": "case", "label": label, "reaction_s": Value::Null }));
            }
        }
        markers.sort_by(|a, b| a["t_s"].as_f64().unwrap_or(0.0).total_cmp(&b["t_s"].as_f64().unwrap_or(0.0)));
        let runs: Vec<Value> = runs.iter().map(|(k, a, b, measured)| json!({ "run": k, "start_s": at(*a), "end_s": at(*b), "measured": measured })).collect();
        let doc = json!({
            "file": file, "kind": v.kind, "about": v.kind.about(), "source": v.source, "encoder": v.encoder, "note": self.note,
            "width": v.width, "height": v.height, "t0_ns": v.t0_ns, "t1_ns": v.t1_ns, "duration_s": at(v.t1_ns),
            "fps": v.fps, "max_fps": if v.fps.is_none() { Some(60) } else { None }, "precision_ms": v.precision_ms,
            "frames": v.frames, "source_frames": v.source_frames, "cpu_ms": v.cpu_ms, "setup_cpu_ms": v.setup_cpu_ms, "encoder_cpu_ms": v.encoder_cpu_ms,
            "runs": runs, "markers": markers,
        });
        store::write_json(&dir.join("video.json"), &doc)?;
        Ok(
            json!({ "file": file, "kind": v.kind, "width": v.width, "height": v.height, "duration_s": at(v.t1_ns), "frames": v.frames, "encoder": v.encoder, "source": v.source, "cpu_ms": v.cpu_ms, "encoder_cpu_ms": v.encoder_cpu_ms, "note": self.note }),
        )
    }
}

/// The line `record` and `--check` print for a video.
pub fn line(rec: &str, v: &Value) -> String {
    format!(
        "video    {rec} · {} {}×{} · {:.1} s · {} frames · {}{} · strobe report recording {rec} --open\n",
        v["kind"].as_str().unwrap_or(""),
        v["width"],
        v["height"],
        v["duration_s"].as_f64().unwrap_or(0.0),
        v["frames"],
        v["encoder"].as_str().unwrap_or(""),
        v["note"].as_str().map(|n| format!(" ({n})")).unwrap_or_default()
    )
}
