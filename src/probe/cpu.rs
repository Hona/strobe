//! V8 CPU profiles (attribution blocks only), with line ticks.

use crate::model::Result;
use crate::target::chromium::Page;
use serde_json::{Value, json};

/// Sampling interval (µs) on macOS and Linux, and what every recording before 2026-11-07 asked for.
/// At 50 µs the profiler itself raised a JS-heavy run's main-thread work by 35–45%, enough that one
/// profiled launch could move against the timed pairs it was meant to explain; 200 µs still gives
/// 5,000 samples a second.
pub const INTERVAL_US: u64 = 200;

/// The interval strobe asks a V8 sampler for: 400 µs on Windows, else `INTERVAL_US`. Windows' timers
/// go no finer than about 0.5 ms, and V8's sampler there wakes on the timer tick after its interval:
/// Chromium on adais kept 0.52 ms when asked for 200 to 450 µs, 1.0 ms when asked for 500 µs and
/// 1.5 ms for 1 ms. 400 µs keeps the finest it can without asking for much more than it gives.
pub fn interval_us() -> u64 {
    if cfg!(windows) { 400 } else { INTERVAL_US }
}

/// A gap longer than this is no sampling interval: the thread was not sampled (an idle runtime that
/// skips ticks, a process that paused), as Firefox's `MAX_GAP_NS`.
const MAX_GAP_US: f64 = 50_000.0;

pub fn start(page: &Page) -> Result<()> {
    page.send("Profiler.enable", json!({}))?;
    page.send("Profiler.setSamplingInterval", json!({ "interval": interval_us() }))?;
    page.send("Profiler.start", json!({}))?;
    Ok(())
}

/// Stops the profiler and returns the `.cpuprofile` JSON.
pub fn stop(page: &Page) -> Result<Value> {
    let r = page.send("Profiler.stop", json!({}))?;
    let _ = page.send("Profiler.disable", json!({}));
    Ok(r["profile"].clone())
}

/// The gaps between consecutive samples of a V8 `.cpuprofile` (µs): `timeDeltas[i]` is the gap from
/// sample i − 1 to sample i.
pub fn gaps(profile: &Value) -> Vec<f64> {
    profile["timeDeltas"].as_array().map(|d| d.iter().skip(1).filter_map(Value::as_f64).collect()).unwrap_or_default()
}

/// What a JavaScript sampler was asked for and what it gave: the median gap between consecutive
/// samples of the page's main thread. A sampler need not keep the interval asked of it (Firefox 160
/// and older on Windows sample every 2 ms when asked for 1 ms; Windows' timers go no finer than about
/// 0.5 ms), and a sample weighs the gap it kept, so this is a profile's resolution.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Sampling {
    pub asked_us: f64,
    pub median_us: f64,
    /// The gaps the median is of.
    pub gaps: u64,
}

impl Sampling {
    /// The median of `gaps` (µs, each between two consecutive samples; those over 50 ms are no
    /// interval and left out); None without one.
    pub fn of(asked_us: f64, mut gaps: Vec<f64>) -> Option<Sampling> {
        gaps.retain(|g| *g > 0.0 && *g <= MAX_GAP_US);
        if gaps.is_empty() {
            return None;
        }
        gaps.sort_by(f64::total_cmp);
        let n = gaps.len();
        let median_us = if n % 2 == 1 { gaps[n / 2] } else { (gaps[n / 2 - 1] + gaps[n / 2]) / 2.0 };
        Some(Sampling { asked_us, median_us, gaps: n as u64 })
    }

    /// Coarser than asked: by more than half the interval asked (timer slack is a few per cent).
    pub fn coarse(&self) -> bool {
        self.median_us > 1.5 * self.asked_us
    }

    /// `sampled every 0.52 ms (asked 0.5 ms)`, `sampled every 2.0 ms (asked 1 ms: 2.0× coarser)`.
    pub fn text(&self) -> String {
        let got = if self.median_us >= 1000.0 { format!("{:.1} ms", self.median_us / 1000.0) } else { format!("{:.2} ms", self.median_us / 1000.0) };
        let coarser = if self.coarse() { format!(": {:.1}× coarser", self.median_us / self.asked_us) } else { String::new() };
        format!("sampled every {got} (asked {} ms{coarser})", self.asked_us / 1000.0)
    }
}
