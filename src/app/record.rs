//! `strobe record`: one recording of a target (SPEC §5.5).

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::driver::Driver;
use crate::host;
use crate::metrics;
use crate::model::{Error, Result};
use crate::probe::{self, Set};
use crate::store;
use crate::target::chromium::{self, Window};
use crate::validity;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

pub fn run(args: &Args) -> Result<Output> {
    let target = args.pos.first().ok_or_else(|| Error::usage("record needs a target: strobe record https://… [--for 10s]"))?.clone();
    if let Some(path) = target.strip_prefix("file:") {
        return super::query::import(path);
    }
    let set = Set::parse(args.get("probes").unwrap_or("timing")).ok_or_else(|| Error::usage("--probes: timing, attribution or memory"))?;
    let for_ms = match args.get("for") {
        Some(d) => duration_ms(d)?,
        None => 10_000,
    };
    let attach = target.starts_with("cdp:") || target.starts_with("ws://");
    if !attach && !target.starts_with("http://") && !target.starts_with("https://") {
        return Err(Error::usage(format!("unsupported target {target}: use https://…, http://…, cdp:<port> or file:<path>")));
    }
    let browser = if attach { chromium::attach(&target)? } else { chromium::launch(&Window::default(), args.has("headless"), None, &[])? };
    let gpu = validity::software_gpu(&browser);
    let page = browser.page()?;
    if !attach {
        progress(format!("loading {target}"));
        page.navigate(&target, Duration::from_secs(60))?;
    }
    let gpu = gpu?;
    validity::window_visible(&page)?;
    let id = store::new_id("rec");
    let token = id.trim_start_matches("rec_").to_string();
    let driver = Driver::new(page.clone(), token.clone());
    let dir = store::recording_dir(&id)?;
    let tracing = probe::trace::start(&browser.cdp, set)?;
    if set == Set::Attribution {
        probe::cpu::start(&page)?;
    }
    std::thread::sleep(Duration::from_millis(300));
    let cpu0: u64 = host::process_tree(browser.pid).iter().map(|p| p.cpu_ns).sum();
    let t0 = host::now_ns();
    driver.mark("run:0:start")?;
    progress(format!("recording {} for {}", id, fmt::ms_duration(for_ms)));
    std::thread::sleep(Duration::from_millis(for_ms));
    driver.mark("run:0:end")?;
    let t1 = host::now_ns();
    let cpu1: u64 = host::process_tree(browser.pid).iter().map(|p| p.cpu_ns).sum();
    if set == Set::Attribution {
        let profile = probe::cpu::stop(&page)?;
        store::write_json(&dir.join("cpu-0.cpuprofile"), &profile)?;
    }
    let bytes = tracing.stop(&dir.join("trace.pftrace"))?;
    let version = browser.version.clone();
    if !attach {
        browser.close();
    }
    progress("reading the trace");
    let data = metrics::read(&dir.join("trace.pftrace"), &token)?;
    let mut extra: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    if !attach && t1 > t0 {
        extra.entry("cpu.app.total".into()).or_default().insert("run:0".into(), 100.0 * (cpu1 - cpu0) as f64 / (t1 - t0) as f64);
    }
    let values = metrics::block_values(&data, "run", &extra);
    let meta = json!({
        "id": id, "kind": "record", "target": target, "probes": set, "token": token, "duration_ms": for_ms,
        "browser": version, "gpu": gpu, "env": host::env(), "trace_bytes": bytes, "values": values,
        "phases": metrics::phase_frames(&data).keys().collect::<Vec<_>>(),
        "created": now_iso(), "valid": true,
    });
    store::write_json(&dir.join("meta.json"), &meta)?;
    Ok(Output::new(summary_text(&meta), meta))
}

pub fn summary_text(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let mut s = format!(
        "{} · {} · {} · {}\n",
        meta["id"].as_str().unwrap_or(""),
        meta["target"].as_str().unwrap_or(""),
        fmt::ms_duration(meta["duration_ms"].as_u64().unwrap_or(0)),
        meta["probes"].as_str().unwrap_or("")
    );
    s += &format!(
        "frames   {} presented · {} of vsyncs dropped · display {}\n",
        fmt::value(get("frames.n"), "count"),
        fmt::value(get("frames.dropped.pct"), "%"),
        if get("display.hz").is_finite() { format!("{:.0} Hz", get("display.hz")) } else { "?".into() },
    );
    s += &format!(
        "interval p50 {} · p95 {} · max {}   (gap between presented frames)\n",
        fmt::value(get("frames.interval.p50"), "ms"),
        fmt::value(get("frames.interval.p95"), "ms"),
        fmt::value(get("frames.interval.max"), "ms"),
    );
    s += &format!(
        "latency  p50 {} · p95 {} · max {}   (frame begin to presentation)\n",
        fmt::value(get("frames.presented.p50"), "ms"),
        fmt::value(get("frames.presented.p95"), "ms"),
        fmt::value(get("frames.presented.max"), "ms"),
    );
    s += &format!("main     {} busy · {} long tasks\n", fmt::value(get("main.busy"), "%"), fmt::value(get("main.tasks.long"), "count"));
    if get("cpu.app.total").is_finite() {
        s += &format!("cpu      {} of one core, all processes\n", fmt::value(get("cpu.app.total"), "%"));
    }
    if get("interaction.p95").is_finite() {
        s += &format!("input    p95 {} input → presentation\n", fmt::value(get("interaction.p95"), "ms"));
    }
    s += &format!("next     strobe query {} frames · strobe report recording {} --open\n", meta["id"].as_str().unwrap_or(""), meta["id"].as_str().unwrap_or(""));
    s
}

pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    let (y, m, d) = civil(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
