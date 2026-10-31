//! V8 CPU profiles (attribution blocks only), with line ticks.

use crate::model::Result;
use crate::target::chromium::Page;
use serde_json::{Value, json};

/// Sampling interval (µs). At 50 µs the profiler itself raised a JS-heavy run's main-thread work by
/// 35–45%, enough that one profiled launch could move against the timed pairs it was meant to explain;
/// 200 µs still gives 5,000 samples a second.
pub const INTERVAL_US: u64 = 200;

pub fn start(page: &Page) -> Result<()> {
    page.send("Profiler.enable", json!({}))?;
    page.send("Profiler.setSamplingInterval", json!({ "interval": INTERVAL_US }))?;
    page.send("Profiler.start", json!({}))?;
    Ok(())
}

/// Stops the profiler and returns the `.cpuprofile` JSON.
pub fn stop(page: &Page) -> Result<Value> {
    let r = page.send("Profiler.stop", json!({}))?;
    let _ = page.send("Profiler.disable", json!({}));
    Ok(r["profile"].clone())
}
