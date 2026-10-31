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
use crate::target::electron;
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
    chromium::reap_orphans();
    let config = crate::config::load(&crate::config::project_root(&std::env::current_dir()?));
    // `exe:<path> --app <id>` borrows an app profile for another build of that app.
    let app = match (target.strip_prefix("app:"), target.strip_prefix("exe:"), args.get("app")) {
        (Some(id), _, _) => Some(electron::app(id, &config)?),
        (_, Some(path), Some(id)) => Some(electron::app(id, &config)?.with_exe(std::path::Path::new(path))),
        (_, Some(path), None) => Some(electron::App::bare(std::path::Path::new(path))),
        _ => None,
    };
    if !attach && app.is_none() && !target.starts_with("http://") && !target.starts_with("https://") {
        return Err(Error::usage(format!("unsupported target {target}: use https://…, app:<id>, exe:<path>, cdp:<port> or file:<path>")));
    }
    let mut guard = None;
    let browser = if attach {
        chromium::attach(&target)?
    } else if let Some(app) = &app {
        progress(format!("launching {} in an isolated home", app.exe.display()));
        let (b, g) = electron::launch(app, args.get("home").map(std::path::Path::new), &Window::default(), None)?;
        guard = Some(g);
        b
    } else {
        chromium::launch(&Window::default(), args.has("headless"), None, &[])?
    };
    let gpu = validity::software_gpu(&browser);
    let page = if app.is_some() { electron::main_page(&browser)? } else { browser.page()? };
    if let Some(g) = &guard {
        g.check("after the app opened its window")?;
        // The app loads itself; give it time to settle before recording.
        std::thread::sleep(Duration::from_secs(3));
    } else if !attach {
        progress(format!("loading {target}"));
        page.navigate(&target, Duration::from_secs(60))?;
    }
    let gpu = gpu?;
    validity::window_visible(&page)?;
    // A headless target draws off screen, so only a headed one cares about the lock screen.
    let on_screen = !args.has("headless");
    if on_screen && host::session_locked() {
        return Err(validity::locked());
    }
    let awake = host::Awake::start();
    // `--id`: a handle chosen up front, so an MCP call that outlives its timeout can be followed.
    let id = args.get("id").filter(|i| i.starts_with("rec_")).map(str::to_string).unwrap_or_else(|| store::new_id("rec"));
    let token = id.trim_start_matches("rec_").to_string();
    let driver = Driver::new(page.clone(), token.clone());
    let dir = store::recording_dir(&id)?;
    if set == Set::Memory {
        let meta = memory(&page, &browser, &driver, &dir, &id, &target, for_ms)?;
        if on_screen && (awake.locks() > 0 || host::session_locked()) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(validity::locked());
        }
        if !attach {
            browser.close();
        }
        if let Some(g) = &guard {
            g.check("after the app closed")?;
        }
        return Ok(Output::new(summary_text(&meta), meta));
    }
    let tracing = probe::trace::start(&browser.cdp, set)?;
    if set == Set::Attribution {
        probe::cpu::start(&page)?;
        probe::alloc::start(&page)?;
    }
    std::thread::sleep(Duration::from_millis(300));
    let sampler = (!attach).then(|| probe::os::Sampler::start(browser.pid));
    let cpu0: u64 = browser.processes().iter().map(|p| p.cpu_ns).sum();
    let t0 = host::now_ns();
    driver.mark("run:0:start")?;
    progress(format!("recording {} for {}", id, fmt::ms_duration(for_ms)));
    std::thread::sleep(Duration::from_millis(for_ms));
    driver.mark("run:0:end")?;
    let t1 = host::now_ns();
    let cpu1: u64 = browser.processes().iter().map(|p| p.cpu_ns).sum();
    let os = sampler.map(|s| s.stop().window(t0, t1));
    if on_screen && (awake.locks() > 0 || host::session_locked()) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(validity::locked());
    }
    if set == Set::Attribution {
        let profile = probe::cpu::stop(&page)?;
        let allocs = probe::alloc::stop(&page)?;
        store::write_json(&dir.join("cpu-0.cpuprofile"), &profile)?;
        store::write_json(&dir.join("alloc-0.heapprofile"), &allocs)?;
        // `--served <dir>`: the folder the target's origin serves, to find original files on disk.
        let mut scripts = std::collections::BTreeSet::new();
        probe::symbols::urls(&profile, &mut scripts);
        probe::symbols::urls(&allocs, &mut scripts);
        probe::symbols::store_maps(&page, &scripts, &dir, args.get("served").map(std::path::Path::new))?;
    }
    let bytes = tracing.stop(&dir.join("trace.pftrace"))?;
    if set == Set::Attribution {
        // After tracing: a live trace subscription would copy every snapshot chunk (SPEC §8.6).
        progress("taking a heap snapshot");
        probe::heap::snapshot(&page, &dir.join("heap-0.heapsnapshot.gz"))?;
    }
    let version = browser.version.clone();
    if !attach {
        browser.close();
    }
    if let Some(g) = &guard {
        g.check("after the app closed")?;
    }
    progress("reading the trace");
    let data = metrics::read(&dir.join("trace.pftrace"), &token)?;
    let mut extra: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    if !attach && t1 > t0 && cpu1 > 0 {
        extra.entry("cpu.app.total".into()).or_default().insert("run:0".into(), 100.0 * (cpu1 - cpu0) as f64 / (t1 - t0) as f64);
    }
    if let Some(p) = os.as_ref().and_then(|o| o.gpu_3d_pct) {
        extra.entry("gpu.process.3d".into()).or_default().insert("run:0".into(), p);
    }
    let mut values = metrics::block_values(&data, "run", &extra);
    if let Some(v) = os.as_ref().and_then(|o| o.vram_dedicated_mb) {
        values.insert("gpu.vram".into(), v);
    }
    let meta = json!({
        "id": id, "kind": "record", "target": target, "probes": set, "token": token, "duration_ms": for_ms,
        "browser": version, "gpu": gpu, "env": host::env(), "trace_bytes": bytes, "values": values, "os": os,
        "phases": metrics::phase_frames(&data).keys().collect::<Vec<_>>(),
        // Rule 3: timing taken with profilers on describes the profiled target, not the target.
        "timing": if set == Set::Attribution { "perturbed" } else { "clean" },
        "created": now_iso(), "valid": true,
    });
    store::write_json(&dir.join("meta.json"), &meta)?;
    Ok(Output::new(summary_text(&meta), meta))
}

/// `--probes memory`: does the target keep what it allocates? A census and a heap snapshot at the
/// start, the middle and the end (S0, S1, S2), no trace (SPEC §8.6). Objects allocated between S0 and
/// S1 that are still alive in S2 are the leak suspects. Each census comes before its snapshot, which
/// raises the memory of the process it serializes; OS memory is reported from the first census only.
fn memory(page: &chromium::Page, browser: &chromium::Browser, driver: &Driver, dir: &std::path::Path, id: &str, target: &str, for_ms: u64) -> Result<Value> {
    page.send("HeapProfiler.enable", json!({}))?;
    let started = std::time::Instant::now();
    progress(format!("recording {id} for {}: a heap snapshot at the start, middle and end", fmt::ms_duration(for_ms)));
    let mut census = Vec::new();
    for (i, file) in ["heap-s0", "heap-s1", "heap-0"].iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(for_ms / 2));
        }
        page.send("HeapProfiler.collectGarbage", json!({}))?;
        let heap = page.send("Runtime.getHeapUsage", json!({}))?["usedSize"].as_f64().unwrap_or(f64::NAN);
        let private = (i == 0).then(|| browser.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64);
        census.push(json!({ "at_ms": started.elapsed().as_millis() as u64, "js_heap_bytes": heap, "dom_nodes": driver.nodes().ok(), "private_bytes": private }));
        probe::heap::snapshot(page, &dir.join(format!("{file}.heapsnapshot.gz")))?;
    }
    progress("finding objects that outlived their half");
    let leaks = crate::heapsnap::leaks(
        &crate::heapsnap::ids(&dir.join("heap-s0.heapsnapshot.gz"))?,
        &crate::heapsnap::ids(&dir.join("heap-s1.heapsnapshot.gz"))?,
        &crate::heapsnap::read(&dir.join("heap-0.heapsnapshot.gz"))?,
        25,
        false,
    );
    store::write_json(&dir.join("leaks.json"), &leaks)?;
    let at = |i: usize, k: &str| census[i][k].as_f64().unwrap_or(f64::NAN);
    let minutes = (at(2, "at_ms") - at(0, "at_ms")) / 60_000.0;
    let mut values = BTreeMap::new();
    values.insert("js.heap.used", at(2, "js_heap_bytes") / (1024.0 * 1024.0));
    values.insert("js.heap.growth", (at(2, "js_heap_bytes") - at(0, "js_heap_bytes")) / (1024.0 * 1024.0) / minutes);
    values.insert("dom.nodes", at(2, "dom_nodes"));
    values.insert("dom.nodes.growth", (at(2, "dom_nodes") - at(0, "dom_nodes")) / minutes);
    values.insert("memory.app.private", at(0, "private_bytes") / (1024.0 * 1024.0));
    values.insert("leak.suspects.retained", leaks["retained_size"].as_f64().unwrap_or(0.0) / (1024.0 * 1024.0));
    let values: BTreeMap<&str, f64> = values.into_iter().filter(|(_, v)| v.is_finite()).collect();
    let meta = json!({
        "id": id, "kind": "record", "target": target, "probes": "memory", "token": "", "duration_ms": for_ms,
        "browser": browser.version, "env": host::env(), "values": values, "census": census,
        "leaks": { "count": leaks["count"], "self_size": leaks["self_size"], "retained_size": leaks["retained_size"], "classes_total": leaks["classes_total"] },
        "created": now_iso(), "valid": true,
    });
    store::write_json(&dir.join("meta.json"), &meta)?;
    Ok(meta)
}

pub fn summary_text(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    if meta["probes"] == "memory" {
        let id = meta["id"].as_str().unwrap_or("");
        let l = &meta["leaks"];
        let growth = format!("{}{}", if get("js.heap.growth") < 0.0 { "−" } else { "+" }, fmt::bytes(get("js.heap.growth").abs() * 1048576.0));
        return format!(
            "{id} · {} · {} · memory\njs heap  {} after GC · {} per minute\ndom      {} nodes · {} per minute\nsuspects {} objects allocated in the first half and alive at the end · {} retained\nnext     strobe query {id} leaks · strobe query {id} heap\n",
            meta["target"].as_str().unwrap_or(""),
            fmt::ms_duration(meta["duration_ms"].as_u64().unwrap_or(0)),
            fmt::bytes(get("js.heap.used") * 1048576.0),
            growth,
            fmt::value(get("dom.nodes"), "count"),
            fmt::signed(get("dom.nodes.growth"), "count"),
            l["count"].as_u64().unwrap_or(0),
            fmt::bytes(l["retained_size"].as_f64().unwrap_or(0.0)),
        );
    }
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
    if meta["timing"] == "perturbed" {
        s += "timing   perturbed: profilers ran during this recording, so its times are not the target's; take times with --probes timing\n";
    }
    s += &format!("next     strobe query {} frames · strobe report recording {} --open\n", meta["id"].as_str().unwrap_or(""), meta["id"].as_str().unwrap_or(""));
    s
}

pub fn now_iso() -> String {
    iso(now_secs())
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`, which sorts like time.
pub fn iso(secs: u64) -> String {
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
