//! Validators (refuse, exit 4) and analysers (warn) — SPEC §10.

use crate::host;
use crate::model::{Error, Result};
use crate::target::chromium::{Browser, Page};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize)]
pub struct Warning {
    pub id: String,
    pub level: String,
    pub text: String,
}

pub fn warn(id: &str, text: impl Into<String>) -> Warning {
    Warning { id: id.into(), level: "warn".into(), text: text.into() }
}

pub fn info(id: &str, text: impl Into<String>) -> Warning {
    Warning { id: id.into(), level: "info".into(), text: text.into() }
}

pub fn software_gpu(browser: &Browser) -> Result<crate::target::chromium::Gpu> {
    let gpu = browser.gpu()?;
    if gpu.software {
        return Err(Error::refused(
            "SoftwareGpu",
            format!("the target renders on a software adapter ({}; compositing {})", gpu.devices.join(", "), gpu.compositing),
            "run headed on a machine with a GPU; frame and GPU metrics from software rendering describe no real device",
        ));
    }
    Ok(gpu)
}

pub fn window_visible(page: &Page) -> Result<()> {
    let state = page.eval("document.visibilityState")?;
    if state.as_str() != Some("visible") {
        return Err(Error::refused(
            "WindowHidden",
            format!("the page reports visibilityState={state}"),
            "keep the window visible and unminimized while strobe measures; Chromium throttles hidden windows",
        ));
    }
    Ok(())
}

pub fn locked() -> Error {
    Error::refused(
        "SessionLocked",
        "the session was locked while strobe measured: the lock screen covers the window, so nothing it drew reached the display",
        "measure on an unlocked screen; bench waits for an unlock and redoes the block by itself",
    )
}

pub fn dev_build(page: &Page) -> Result<()> {
    let dev = page.eval(r#"!!document.querySelector('script[src*="@vite/client"]') || typeof window.__vite_plugin_react_preamble_installed__ !== "undefined""#)?;
    if dev.as_bool() == Some(true) {
        return Err(Error::refused("DevBuild", "the page is served by a dev server (Vite client present)", "measure a production build: set build and serve in strobe.json"));
    }
    Ok(())
}

/// The fixed speed-check workload; returns its time in ms (median of 3).
pub fn speed_check(page: &Page) -> Result<f64> {
    let v = page.eval(
        r#"(() => { const t = []; for (let r = 0; r < 3; r++) { const a = new Float64Array(1 << 16); const s = performance.now(); let x = 0; for (let i = 0; i < 6e6; i++) { const j = (i * 2654435761) & 65535; a[j] += i; x ^= a[(j + 7) & 65535] | 0 } t.push(performance.now() - s + (x & 0)) } t.sort((p, q) => p - q); return t[1] })()"#,
    )?;
    Ok(v.as_f64().unwrap_or(f64::NAN))
}

pub fn throttled(ms: f64, baseline: Option<f64>) -> Result<Option<f64>> {
    let Some(base) = baseline.filter(|b| *b > 0.0) else { return Ok(None) };
    let ratio = ms / base;
    if ratio > 1.5 {
        return Err(Error::refused(
            "Throttled",
            format!("the speed check ran {ratio:.2}× slower than this device's calibration ({ms:.1} ms vs {base:.1} ms)"),
            "remove CPU throttling, plug in power, leave power saving, let the machine cool down",
        ));
    }
    Ok(Some(ratio))
}

/// The native speed check (`Throttled` for targets with no page to run the JS check in): a fixed
/// integer workload on a strobe thread on `cpus` (the target's CPUs), median of 3, in ms. strobe's own
/// mask (the runner's CPUs) is widened to them for the check and comes back after it.
pub fn native_speed(cpus: Option<u64>) -> f64 {
    let previous = cpus.and_then(host::set_affinity);
    let ms = std::thread::spawn(move || {
        if let Some(m) = cpus {
            host::set_thread_affinity(m);
        }
        let mut t: Vec<f64> = (0..3)
            .map(|_| {
                let mut a = vec![0f64; 1 << 16];
                let started = std::time::Instant::now();
                let mut x = 0u64;
                for i in 0..24_000_000u64 {
                    let j = (i.wrapping_mul(2_654_435_761) & 65535) as usize;
                    a[j] += i as f64;
                    x ^= a[(j + 7) & 65535] as u64;
                }
                std::hint::black_box(x);
                started.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        t.sort_by(f64::total_cmp);
        t[1]
    })
    .join()
    .unwrap_or(f64::NAN);
    if let Some(p) = previous {
        host::set_affinity(p);
    }
    ms
}

/// `Throttled` for the native speed check, against the fastest this device has run it (kept in
/// `<state>/native-speed.json`; the first check sets it). Returns the ratio.
pub fn native_throttled(ms: f64) -> Result<Option<f64>> {
    let path = host::paths::state_dir().join("native-speed.json");
    let best = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["ms"].as_f64()).filter(|b| *b > 0.0);
    if ms.is_finite() && best.is_none_or(|b| ms < b) {
        let _ = std::fs::write(&path, serde_json::json!({ "ms": ms }).to_string());
    }
    let Some(base) = best else { return Ok(None) };
    let ratio = ms / base;
    if ratio > 1.5 {
        return Err(Error::refused(
            "Throttled",
            format!("the native speed check ran {ratio:.2}× slower than the fastest on this device ({ms:.1} ms vs {base:.1} ms)"),
            "plug in power, leave power saving, let the machine cool down",
        ));
    }
    Ok(Some(ratio))
}

/// Holds the per-device bench lock while alive.
pub struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn lock() -> Result<Lock> {
    let path = host::paths::run().join("bench.lock");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(pid) = text.trim().parse::<u32>()
        && pid != std::process::id()
        // A lock a killed bench left behind can name a pid the OS gave to something else since.
        && host::process_tree(pid).first().is_some_and(|p| p.name.to_lowercase().starts_with("strobe"))
    {
        return Err(Error::refused("ConcurrentBench", format!("another bench (pid {pid}) is running on this device"), "wait for it to finish; two benches on one device disturb each other"));
    }
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(Lock(path))
}

/// The speed baseline from calibration, if any.
pub fn baseline() -> Option<f64> {
    calibration()?["speed_ms"].as_f64()
}

/// The device's last calibration (`at`, `ok`, `speed_ms`, checks), if any.
pub fn calibration() -> Option<Value> {
    let state = std::fs::read_to_string(host::paths::state_dir().join("state.json")).ok()?;
    let v: Value = serde_json::from_str(&state).ok()?;
    v.get("last_calibration").filter(|c| c.is_object()).cloned()
}

/// Spearman's rank correlation, for drift across pairs.
pub fn spearman(values: &[f64]) -> f64 {
    let n = values.len();
    if n < 3 {
        return 0.0;
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut rank = vec![0.0; n];
    for (r, &i) in idx.iter().enumerate() {
        rank[i] = r as f64;
    }
    let mean = (n - 1) as f64 / 2.0;
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (i, r) in rank.iter().enumerate() {
        let a = i as f64 - mean;
        let b = r - mean;
        num += a * b;
        da += a * a;
        db += b * b;
    }
    if da == 0.0 || db == 0.0 { 0.0 } else { num / (da * db).sqrt() }
}
