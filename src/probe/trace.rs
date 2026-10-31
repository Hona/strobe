//! Browser-wide Chromium tracing into a Perfetto proto file.

use super::Set;
use crate::model::{Error, Result};
use crate::target::cdp::{Cdp, wait_event};
use base64::Engine;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Categories per probe set. Timing stays small: frames, input latency, renderer tasks, marks.
pub fn categories(set: Set) -> Vec<&'static str> {
    let mut c = vec!["benchmark", "devtools.timeline", "disabled-by-default-devtools.timeline", "blink.user_timing", "__metadata"];
    match set {
        // Only what bench metrics read: PipelineReporter (frames), EventLatency (input), marks, thread
        // names. A tenth of the full set's slices, so less tracing in the target and a faster block;
        // renderer main-thread busy comes from the Performance domain instead of RunTask slices.
        Set::Lean => return vec!["disabled-by-default-devtools.timeline.frame", "input.scrolling", "blink.user_timing", "__metadata"],
        Set::Timing => {}
        // Only what attribution views read: JS stacks on layout and style events (`query layout`) and GC
        // phases (`query gc`). Invalidation tracking, `blink` and `v8.execute` recorded a million slices
        // nothing read and raised a profiled run's main-thread work by 40–50% against its timed pairs.
        Set::Attribution => c.extend(["disabled-by-default-devtools.timeline.stack", "disabled-by-default-v8.gc"]),
        Set::Memory => c.extend(["disabled-by-default-memory-infra", "disabled-by-default-v8.gc"]),
    }
    c
}

pub struct Tracing {
    cdp: Arc<Cdp>,
    events: Receiver<Value>,
}

pub fn start(cdp: &Arc<Cdp>, set: Set) -> Result<Tracing> {
    let events = cdp.events();
    cdp.send(
        "Tracing.start",
        json!({
            "transferMode": "ReturnAsStream",
            "streamFormat": "proto",
            // Attribution traces of long runs (stacks, invalidations) overflowed the default buffer.
            "traceConfig": { "recordMode": "recordAsMuchAsPossible", "includedCategories": categories(set), "traceBufferSizeInKb": if set == Set::Attribution || set == Set::Memory { 1_000_000 } else { 200_000 } },
        }),
        None,
    )?;
    Ok(Tracing { cdp: cdp.clone(), events })
}

impl Tracing {
    /// Stops tracing and streams the trace into `path`; returns its size.
    pub fn stop(self, path: &Path) -> Result<u64> {
        self.cdp.send("Tracing.end", json!({}), None)?;
        let done = wait_event(&self.events, "Tracing.tracingComplete", None, Duration::from_secs(120))?;
        if done.get("dataLossOccurred").and_then(Value::as_bool) == Some(true) {
            eprintln!("strobe: warning: the trace buffer overflowed; the recording is incomplete");
        }
        let stream = done["stream"].as_str().ok_or_else(|| Error::msg("tracing: no stream"))?;
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut total = 0u64;
        loop {
            let r = self.cdp.send("IO.read", json!({ "handle": stream, "size": 4 << 20 }), None)?;
            let data = r["data"].as_str().unwrap_or("");
            let bytes = if r["base64Encoded"].as_bool() == Some(true) {
                base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| Error::msg(format!("trace chunk: {e}")))?
            } else {
                data.as_bytes().to_vec()
            };
            total += bytes.len() as u64;
            file.write_all(&bytes)?;
            if r["eof"].as_bool() == Some(true) {
                break;
            }
        }
        file.flush()?;
        let _ = self.cdp.send("IO.close", json!({ "handle": stream }), None);
        Ok(total)
    }
}
