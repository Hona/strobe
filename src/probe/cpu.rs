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
    /// The median of `gaps` (µs, each between two consecutive samples); None without one.
    pub fn of(asked_us: f64, mut gaps: Vec<f64>) -> Option<Sampling> {
        gaps.retain(|g| *g > 0.0);
        if gaps.is_empty() {
            return None;
        }
        gaps.sort_by(f64::total_cmp);
        let n = gaps.len();
        let median_us = if n % 2 == 1 { gaps[n / 2] } else { (gaps[n / 2 - 1] + gaps[n / 2]) / 2.0 };
        Some(Sampling { asked_us, median_us, gaps: n as u64 })
    }

    /// A V8 `.cpuprofile`'s: `timeDeltas[i]` is the gap from sample i − 1 to sample i.
    pub fn of_v8(profile: &Value) -> Option<Sampling> {
        let deltas = profile["timeDeltas"].as_array()?;
        Sampling::of(INTERVAL_US as f64, deltas.iter().skip(1).filter_map(Value::as_f64).collect())
    }

    /// Coarser than asked: by more than half the interval asked (timer slack is a few per cent).
    pub fn coarse(&self) -> bool {
        self.median_us > 1.5 * self.asked_us
    }

    /// `sampled every 2.0 ms (asked 1 ms)`.
    pub fn text(&self) -> String {
        let got = if self.median_us >= 1000.0 { format!("{:.1} ms", self.median_us / 1000.0) } else { format!("{:.2} ms", self.median_us / 1000.0) };
        format!("sampled every {got} (asked {} ms)", self.asked_us / 1000.0)
    }
}
