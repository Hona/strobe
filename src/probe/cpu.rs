//! V8 CPU profiles (attribution blocks only): 50 µs sampling with line ticks.

use crate::model::Result;
use crate::target::chromium::Page;
use serde_json::{Value, json};

pub fn start(page: &Page) -> Result<()> {
    page.send("Profiler.enable", json!({}))?;
    page.send("Profiler.setSamplingInterval", json!({ "interval": 50 }))?;
    page.send("Profiler.start", json!({}))?;
    Ok(())
}

/// Stops the profiler and returns the `.cpuprofile` JSON.
pub fn stop(page: &Page) -> Result<Value> {
    let r = page.send("Profiler.stop", json!({}))?;
    let _ = page.send("Profiler.disable", json!({}));
    Ok(r["profile"].clone())
}
