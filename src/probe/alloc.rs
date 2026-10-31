//! Sampled allocations with full stacks (attribution blocks only): which code allocates how many bytes.
//! V8 records one sample per `INTERVAL_BYTES` allocated on average, with the allocating call stack, and
//! keeps samples of objects the GC already collected, so short-lived garbage shows too.

use crate::model::Result;
use crate::target::chromium::Page;
use serde_json::{Value, json};

/// Mean bytes between samples.
pub const INTERVAL_BYTES: u64 = 32 * 1024;

pub fn start(page: &Page) -> Result<()> {
    page.send("HeapProfiler.enable", json!({}))?;
    page.send(
        "HeapProfiler.startSampling",
        // 128 frames, V8's default and the most Chrome can return: the profile nests two levels per frame
        // and Chrome's protocol serializer stops at 300 ("CBOR: stack limit exceeded").
        json!({ "samplingInterval": INTERVAL_BYTES, "stackDepth": 128, "includeObjectsCollectedByMajorGC": true, "includeObjectsCollectedByMinorGC": true }),
    )?;
    Ok(())
}

/// Stops sampling and returns the sampling heap profile (`head` call tree with `selfSize`, `samples`).
pub fn stop(page: &Page) -> Result<Value> {
    let r = page.send("HeapProfiler.stopSampling", json!({}))?;
    Ok(r["profile"].clone())
}
