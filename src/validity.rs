//! Validators (refuse, exit 4) and analysers (warn) — SPEC §10.

use crate::host;
use crate::model::{Error, Hidden, Refusal, Result, ValidatorId, WebEngine};
use crate::target::chromium::Gpu;
use crate::target::web::WebPage as Page;
use serde::Serialize;
use serde_json::Value;

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

/// `SoftwareGpu` on what the browser said it draws with (`browser.gpu()`).
pub fn software_gpu(gpu: Gpu) -> Result<Gpu> {
    if gpu.software {
        return Err(Refusal::new(ValidatorId::SoftwareGpu, format!("the target renders on a software adapter ({}; compositing {})", gpu.devices.join(", "), gpu.compositing))
            .fix("run headed on a machine with a GPU; frame and GPU metrics from software rendering describe no real device")
            .into());
    }
    Ok(gpu)
}

/// `SoftwareGpu`'s one test, by adapter or renderer name: a software rasterizer (Microsoft Basic
/// Render Driver, WARP, llvmpipe, lavapipe, softpipe, SwiftShader, the Apple Software Renderer)
/// describes no real device.
pub fn is_software_gpu(name: &str) -> bool {
    let n = name.to_lowercase();
    ["basic render", "warp", "llvmpipe", "lavapipe", "swiftshader", "softpipe", "apple software renderer"].iter().any(|s| n.contains(s))
}

pub fn window_visible(page: &Page) -> Result<()> {
    let state = page.eval("document.visibilityState")?;
    if state.as_str() != Some("visible") {
        // macOS: a Firefox or Safari window counts as hidden behind other apps' (occlusion tracking), and
        // strobe makes it the active app only once nobody has used the Mac for a minute: a bench waits for that.
        let hidden = if cfg!(target_os = "macos") && page.cdp().is_none() { Hidden::NotActive } else { Hidden::Other };
        return Err(Refusal::window_hidden(hidden, format!("the page reports visibilityState={state}"))
            .fix("keep the window visible and unminimized while strobe measures; browsers throttle hidden windows")
            .into());
    }
    Ok(())
}

pub fn locked() -> Error {
    Refusal::new(ValidatorId::SessionLocked, "the session was locked while strobe measured: the lock screen covers the window, so nothing it drew reached the display")
        .fix("measure on an unlocked screen; bench waits for an unlock and redoes the block by itself")
        .into()
}

pub fn dev_build(page: &Page) -> Result<()> {
    let dev = page.eval(r#"!!document.querySelector('script[src*="@vite/client"]') || typeof window.__vite_plugin_react_preamble_installed__ !== "undefined""#)?;
    if dev.as_bool() == Some(true) {
        return Err(Refusal::new(ValidatorId::DevBuild, "the page is served by a dev server (Vite client present)").fix("measure a production build: set build and serve in strobe.json").into());
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
        return Err(Refusal::new(ValidatorId::Throttled, format!("the speed check ran {ratio:.2}× slower than this device's calibration ({ms:.1} ms vs {base:.1} ms)"))
            .fix("remove CPU throttling, plug in power, leave power saving, let the machine cool down")
            .into());
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

/// Busy % of each of `cpus` (all when `None`) over `ms`, from idle cycles against the TSC.
fn busy(cpus: Option<u64>, ms: u64) -> Option<Vec<f64>> {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let t0 = host::tsc()?;
    host::idle_cycles(&mut a).then_some(())?;
    std::thread::sleep(std::time::Duration::from_millis(ms));
    let t1 = host::tsc()?;
    host::idle_cycles(&mut b).then_some(())?;
    let span = t1.saturating_sub(t0) as f64;
    (span > 0.0 && a.len() == b.len()).then_some(())?;
    Some(
        a.iter()
            .zip(&b)
            .enumerate()
            .filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1))
            .map(|(_, (x, y))| (100.0 * (1.0 - y.saturating_sub(*x) as f64 / span)).clamp(0.0, 100.0))
            .collect(),
    )
}

/// Waits, up to `max`, until `cpus` are quiet: two 100 ms looks in a row with their mean busy under
/// 5% and none over 25% (an app that just ended still frees its memory and GPU objects on them).
/// Returns the last look's mean and busiest CPU, in %.
pub fn wait_quiet(cpus: Option<u64>, max: std::time::Duration) -> (f64, f64) {
    let deadline = std::time::Instant::now() + max;
    let mut calm = 0;
    loop {
        let Some(b) = busy(cpus, 100) else { return (f64::NAN, f64::NAN) };
        let mean = b.iter().sum::<f64>() / b.len().max(1) as f64;
        let top = b.iter().copied().fold(0.0, f64::max);
        calm = if mean < 5.0 && top < 25.0 { calm + 1 } else { 0 };
        if calm >= 2 || std::time::Instant::now() > deadline {
            return (mean, top);
        }
    }
}

/// The speed check's reference for one CPU mask: the median of the last checks that passed there.
const KEPT: usize = 20;

/// Other work on the target's CPUs during a check (% of one core) past which a slow check is that
/// work's doing, not the device's.
const FOREIGN_PCT: f64 = 25.0;

/// One speed check: its time, the other work on the target's CPUs meanwhile (their busy time less
/// the check's own core), and the processes outside strobe that used the most CPU (any CPU).
#[derive(Clone)]
struct Try {
    ms: f64,
    foreign_pct: f64,
    others: Vec<(String, f64)>,
}

fn timed_check(cpus: Option<u64>) -> Try {
    let me = std::process::id();
    let (p0, c0, t0) = (host::all_processes(), idle_now(), host::tsc());
    let started = std::time::Instant::now();
    let ms = native_speed(cpus);
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let (p1, c1, t1) = (host::all_processes(), idle_now(), host::tsc());
    let foreign_pct = match (c0, c1, t0, t1) {
        (Some(a), Some(b), Some(x), Some(y)) if y > x && a.len() == b.len() => {
            let span = (y - x) as f64;
            let busy: f64 = a
                .iter()
                .zip(&b)
                .enumerate()
                .filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1))
                .map(|(_, (i0, i1))| (100.0 * (1.0 - i1.saturating_sub(*i0) as f64 / span)).clamp(0.0, 100.0))
                .sum();
            (busy - 100.0).max(0.0)
        }
        _ => 0.0,
    };
    let before: std::collections::HashMap<u32, u64> = p0.iter().map(|p| (p.pid, p.cpu_ns)).collect();
    let mut others: Vec<(String, f64)> = p1
        .iter()
        .filter(|p| p.pid != me && p.pid != 0)
        .filter_map(|p| {
            let d = p.cpu_ns.saturating_sub(*before.get(&p.pid)?) as f64 / 1e6;
            let pct = 100.0 * d / wall_ms.max(1.0);
            (pct >= 5.0).then(|| (format!("{} ({})", p.name, p.pid), pct))
        })
        .collect();
    others.sort_by(|a, b| b.1.total_cmp(&a.1));
    others.truncate(3);
    Try { ms, foreign_pct, others }
}

fn idle_now() -> Option<Vec<u64>> {
    let mut v = Vec::new();
    host::idle_cycles(&mut v).then_some(v)
}

/// `Throttled` for targets with no page (SPEC §10.1): the native speed check on `cpus`, run once
/// they are quiet, against this device's reference for that mask (`<state>/native-speed.json`:
/// the median of its last 20 passing checks; the first check sets it). A slow check is retried
/// twice after the CPUs settle again: throttling (power, heat, a power plan) lasts, while an app
/// that just ended (freeing a large model) or a burst of other work passes. Returns the ratio.
pub fn native_speed_checked(cpus: Option<u64>) -> Result<Option<f64>> {
    let path = host::paths::state_dir().join("native-speed.json");
    let mut saved: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    let key = cpus.map_or("all".to_string(), |m| format!("{m:#x}"));
    let mut kept: Vec<f64> = saved[&key]["ms"].as_array().into_iter().flatten().filter_map(Value::as_f64).filter(|m| *m > 0.0).collect();
    let reference = (!kept.is_empty()).then(|| {
        let mut s = kept.clone();
        s.sort_by(f64::total_cmp);
        s[s.len() / 2]
    });
    // Up to 3 slow tries on quiet CPUs, and for as long as 2 minutes while other processes keep the
    // target's CPUs busy during the check: that is their work, not this device's speed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut best: Option<Try> = None;
    let mut slow = 0;
    let last = loop {
        wait_quiet(cpus, std::time::Duration::from_secs(10));
        let t = timed_check(cpus);
        if t.ms.is_finite() && best.as_ref().is_none_or(|b| t.ms < b.ms) {
            best = Some(t.clone());
        }
        if reference.is_none_or(|r| t.ms / r <= 1.5) {
            break t;
        }
        if t.foreign_pct > FOREIGN_PCT && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        slow += 1;
        if slow >= 3 || std::time::Instant::now() >= deadline {
            break t;
        }
    };
    let Some(best) = best else { return Ok(None) };
    let ms = best.ms;
    let ratio = reference.map(|r| ms / r);
    if let (Some(ratio), Some(r)) = (ratio, reference)
        && ratio > 1.5
    {
        let who = |t: &Try| if t.others.is_empty() { "nothing outside strobe".to_string() } else { t.others.iter().map(|(n, p)| format!("{n} {p:.0}%")).collect::<Vec<_>>().join(", ") };
        if last.foreign_pct > FOREIGN_PCT {
            return Err(Refusal::new(
                ValidatorId::Background,
                format!(
                    "the native speed check on the target's CPUs ran {ratio:.2}× slower than this device's reference ({ms:.1} ms vs {r:.1} ms) while other work kept those CPUs {:.0}% of a core busy for 2 minutes",
                    last.foreign_pct
                ),
            )
            .detail(format!("busiest processes (% of a core): {}", who(&last)))
            .fix("let that work finish (an antivirus scanning new files, an update, a build), or exclude it from the bench's CPUs")
            .into());
        }
        return Err(Refusal::new(
            ValidatorId::Throttled,
            format!("the native speed check ran {ratio:.2}× slower than this device's reference on these CPUs ({ms:.1} ms vs {r:.1} ms, best of 3 tries on quiet CPUs)"),
        )
        .detail(format!("other work during the last: {:.0}% of a core there; busiest processes: {}", last.foreign_pct, who(&last)))
        .fix("plug in power, leave power saving, let the machine cool down")
        .into());
    }
    kept.push(ms);
    let drop = kept.len().saturating_sub(KEPT);
    kept.drain(..drop);
    if let Some(o) = saved.as_object_mut() {
        // The single all-time best older versions kept, for every mask at once.
        o.remove("ms");
    }
    saved[&key] = serde_json::json!({ "ms": kept });
    let _ = host::paths::write_atomic(&path, saved.to_string().as_bytes());
    Ok(ratio)
}

/// `Throttled` for a page in an engine other than Chromium (whose calibration is Chromium's): the
/// in-page speed check against that engine's own reference on these CPUs, the median of its last 20
/// passing checks (`<state>/engine-speed.json`, by engine and CPU mask; the first check sets it). JS
/// engines differ in speed: a check is never compared across engines.
/// `keep` false (other work kept the target's CPUs busy during the check) compares without adding the
/// check to the reference.
pub fn engine_throttled(engine: WebEngine, cpus: Option<u64>, ms: f64, keep: bool) -> Result<Option<f64>> {
    if !ms.is_finite() || ms <= 0.0 {
        return Ok(None);
    }
    let path = host::paths::state_dir().join("engine-speed.json");
    let mut saved: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    let key = format!("{engine}:{}", cpus.map_or("all".to_string(), |m| format!("{m:#x}")));
    let mut kept: Vec<f64> = saved[&key]["ms"].as_array().into_iter().flatten().filter_map(Value::as_f64).filter(|m| *m > 0.0).collect();
    let reference = (!kept.is_empty()).then(|| {
        let mut s = kept.clone();
        s.sort_by(f64::total_cmp);
        s[s.len() / 2]
    });
    let ratio = reference.map(|r| ms / r);
    if let (Some(ratio), Some(r)) = (ratio, reference)
        && ratio > 1.5
    {
        return Err(Refusal::new(ValidatorId::Throttled, format!("the page's speed check ran {ratio:.2}× slower than this device's reference for {engine} on these CPUs ({ms:.1} ms vs {r:.1} ms)"))
            .fix(format!(
                "remove CPU throttling, plug in power, leave power saving, let the machine cool down; a reference set by an unusually fast check goes with its entry {key:?} in {}",
                path.display()
            ))
            .into());
    }
    if !keep {
        return Ok(ratio);
    }
    kept.push(ms);
    let drop = kept.len().saturating_sub(KEPT);
    kept.drain(..drop);
    saved[&key] = serde_json::json!({ "ms": kept });
    let _ = host::paths::write_atomic(&path, saved.to_string().as_bytes());
    Ok(ratio)
}

/// What a page engine has no way to measure (`EngineUnsupported`), with the closest metric it has.
/// Firefox: the renderer main thread's task time and its parts, layouts and style counts, long tasks,
/// layout shifts, GC and GPU compiles (Chromium's Performance domain and trace), the JS heap, the page's
/// spans, and a frame's begin-to-screen cost (only a toolkit knows when it began a frame: Firefox's
/// frames come from the OS's presents, §7.2). WebKit (WebDriver Classic) also has no network events
/// (`net.*`) and runs an input action list as one batch in its UI process (`input.rate`, `input.gap.p95`
/// would time strobe's requests, not the page's input).
pub fn engine_refusal(engine: WebEngine, metric: &str) -> Option<Refusal> {
    match engine {
        WebEngine::Chromium => return None,
        // Neither has Chromium's Performance domain or trace.
        WebEngine::Firefox | WebEngine::Webkit => {}
    }
    if engine == WebEngine::Webkit {
        let classic = match metric {
            m if m.starts_with("net.") => Some(("WebDriver Classic reports no network requests", "measure it in Chromium or Firefox")),
            "input.rate" | "input.gap.p95" => Some(("WebDriver Classic delivers input actions in batches inside the browser, so strobe's send times are not the page's input times", "measure it in Chromium or Firefox")),
            _ => None,
        };
        if let Some((why, fix)) = classic {
            return Some(Refusal::new(ValidatorId::EngineUnsupported, format!("{metric}: {why}")).fix(fix));
        }
    }
    let instead = match metric {
        m if m.starts_with("main.") || matches!(m, "layout.count" | "style.count") => "cpu.work (the browser's processes' CPU per run) or cpu.renderer",
        m if m.starts_with("frames.presented.") && m != "frames.presented.count" => "frames.interval.p95 (headed) or cpu.work",
        "frames.partial" => "frames.dropped (headed)",
        m if crate::metrics::split_metric(m) => "frames.interval.p95 (headed) or cpu.gpu",
        m if m.starts_with("js.heap.") => "memory.app.private",
        "gpu.compile" => "gpu.process.3d",
        "layout.shift" => "a count.<name> the page keeps",
        m if m.starts_with("span.") => "phase.<name> around the same work in the case",
        _ => return None,
    };
    Some(Refusal::new(ValidatorId::EngineUnsupported, format!("{metric}: {engine} gives strobe no way to read it (it comes from Chromium's Performance domain or trace)")).fix(format!("measure it in Chromium, or use {instead}")))
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
    // Ties share their mean rank: with order-of-arrival ranks, a constant series read as a perfect trend.
    let mut rank = vec![0.0; n];
    let mut start = 0;
    while start < n {
        let mut end = start + 1;
        while end < n && values[idx[end]] == values[idx[start]] {
            end += 1;
        }
        for &i in &idx[start..end] {
            rank[i] = (start + end - 1) as f64 / 2.0;
        }
        start = end;
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
