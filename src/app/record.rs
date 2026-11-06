//! `strobe record`: one recording of a target (SPEC §5.5).

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::driver::Driver;
use crate::host;
use crate::metrics;
use crate::model::{Error, Result, WebEngine};
use crate::probe::{self, Set};
use crate::store;
use crate::target::chromium::{self, Window};
use crate::target::{electron, isolate, web};
use crate::validity;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// `record --bench <file.bench.ts>#<case>`: one bench case on this checkout (or `--head <variant>`), recorded with
/// the attribution probes (CPU and allocation profiles, forced layouts, GC, the GPU process's work, a heap
/// snapshot). The case drives the page, so a flow behind a click or a key records as easily as a URL. It is a
/// bench `--check` with an attribution block: setup, warmup until steady, then the measured runs under the
/// profilers.
fn bench_case(args: &Args, file: &str, case: &str) -> Result<Output> {
    if args.has("gpu-cache") {
        return Err(Error::usage("--gpu-cache is for a native app's record; with --bench the bench target's gpuCache decides (set it in the bench file)"));
    }
    let mut b = Args::parse(vec!["bench".into(), file.into(), "--head".into(), args.get("head").unwrap_or(".").into(), "--check".into(), "--why".into(), "always".into()])?;
    b.set("filter", case);
    // Recordings of one executable share its GPU caches, as `record` does.
    b.set("gpu-store", "exe");
    for k in ["target", "params", "id", "pin", "power", "runs", "style", "headless", "engine"] {
        if let Some(v) = args.get(k) {
            b.set(k, v);
        }
    }
    // The recording is the case's attribution block: only `waits` adds to it (`timing` or `memory`
    // would skip it or make a leak run).
    match args.get("probes") {
        None | Some("attribution" | "deep") => {}
        Some("waits") => b.set("probes", "waits"),
        Some(p) => return Err(Error::usage(format!("record --bench records the case's attribution block; --probes {p} does not apply (waits adds off-CPU waits to a native case)"))),
    }
    let out = super::bench::run(&b)?;
    let recordings: Vec<Value> = out.json["series"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some(json!({ "case": s["label"], "recording": s["attribution"].as_str()?, "values": s["values"], "refused": s["refused"], "declared": s["declared"], "headless": s["headless"] })))
        .collect();
    if recordings.is_empty() {
        return Err(Error::msg(format!("{case}: no recording (the case failed or matched nothing):\n{}", out.text)));
    }
    let mut text = String::new();
    for r in &recordings {
        let rec = r["recording"].as_str().unwrap_or("");
        text += &format!("{rec} · {} · attribution{}\n", r["case"].as_str().unwrap_or(""), if r["headless"] == true { " · headless: frames and input latency refused" } else { "" });
        // The check run before it (one cold run, timed): its launch milestones and declared metrics.
        text += &launch_line(&r["values"]);
        let declared: Vec<String> = r["declared"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|m| !m.starts_with("launch."))
            .filter_map(|m| match r["values"][m].as_f64() {
                Some(v) => Some(format!("{m} {}", fmt::value(v, crate::model::metric(m).map_or("", |d| d.unit)))),
                None => r["refused"][m].is_string().then(|| format!("{m} refused")),
            })
            .collect();
        if !declared.is_empty() {
            text += &format!("check    {}   (one cold run before it: its own numbers, not this recording's runs)\n", declared.join(" · "));
        }
        let native = store::resolve(rec).ok().and_then(|d| store::read_json(&d.join("meta.json")).ok()).is_some_and(|m| m["native"] == true);
        text += &if native {
            format!("next     strobe query {rec} threads · strobe query {rec} hotspots --by package · strobe query {rec} gpu · strobe query {rec} summary\n")
        } else {
            format!("next     strobe query {rec} hotspots --by package · strobe query {rec} summary (per run) · strobe query {rec} style · strobe report recording {rec} --open\n")
        };
    }
    Ok(Output::new(text, json!({ "recordings": recordings })))
}

pub fn run(args: &Args) -> Result<Output> {
    if let Some(spec) = args.get("bench") {
        let (file, case) = spec.rsplit_once('#').ok_or_else(|| Error::usage("--bench <file.bench.ts>#<case> (a case glob such as show* works)"))?;
        return bench_case(args, file, case);
    }
    let target = args.pos.first().ok_or_else(|| Error::usage("record needs a target: strobe record https://… [--for 10s], or --bench <file>#<case>"))?.clone();
    if let Some(id) = args.get("id") {
        store::free_recording_id(id)?;
    }
    // A local page (`page.html` or `file:page.html`) opens in the browser as its file:// address;
    // any other `file:` is a trace to import.
    let page_file = |p: &str| {
        let ext = std::path::Path::new(p).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        matches!(ext.as_str(), "html" | "htm" | "xhtml" | "svg") && std::path::Path::new(p).is_file()
    };
    // `file:<path>`, `file:///<path>`, and on Windows `file:///C:/…`.
    let local = target.strip_prefix("file://").or_else(|| target.strip_prefix("file:")).map(|p| if cfg!(windows) && p.len() > 2 && p.as_bytes()[2] == b':' { &p[1..] } else { p });
    let target = match local {
        Some(path) if page_file(path) => super::look::address(path),
        Some(path) => return super::query::import(path),
        None if !target.contains(':') || std::path::Path::new(&target).is_file() => {
            if page_file(&target) {
                super::look::address(&target)
            } else {
                return Err(Error::usage(format!("{target}: a local target is an HTML page (page.html) or a trace to import (file:<path>)")));
            }
        }
        None => target,
    };
    // A timed recording runs on the fastest power plan, as a bench does (SPEC §9.4); attribution,
    // deep and memory recordings are not timed. A plan it switched to is said in the result, not as
    // a progress line on stderr that reads like an error.
    let power = if args.get("probes").unwrap_or("timing") == "timing" { Some(super::bench::power_quiet(args)?) } else { None };
    let mut out = recorded(args, target)?;
    // The user's plan is put back (the guard dropped) before the result says so.
    let switched = power.filter(|p| p.switched()).map(|p| p.used.clone());
    if let Some(used) = switched
        && !out.text.is_empty()
    {
        out.text += &format!("power    {used} for this recording; the user's own plan is back\n");
    }
    Ok(out)
}

fn recorded(args: &Args, target: String) -> Result<Output> {
    if target.starts_with("pid:") || target.starts_with("proc:") {
        return super::outside::record(args, &target);
    }

    // `waits`: a native app's attribution recording with its off-CPU waits.
    let set = match args.get("probes").unwrap_or("timing") {
        "waits" => Set::Attribution,
        p => Set::parse(p).ok_or_else(|| Error::usage("--probes: timing, attribution, memory or waits (a native app's attribution with off-CPU waits)"))?,
    };
    let for_ms = match args.get("for") {
        Some(d) => duration_ms(d)?,
        None => 10_000,
    };
    let attach = target.starts_with("cdp:") || target.starts_with("ws://");
    for l in isolate::reap_dead_owners() {
        progress(l);
    }
    let config = crate::config::load(&crate::config::project_root(&std::env::current_dir()?))?;
    // `exe:<path> --app <id>` borrows an app profile for another build of that app.
    let app = match (target.strip_prefix("app:"), target.strip_prefix("exe:"), args.get("app")) {
        (Some(id), _, _) => Some(isolate::app(id, &config)?),
        (_, Some(path), Some(id)) => Some(isolate::app(id, &config)?.with_exe(std::path::Path::new(path))),
        (_, Some(path), None) => Some(isolate::App::bare(std::path::Path::new(path))),
        _ => None,
    };
    // A native app (a profile of kind native, or `exe:<path> --native`): measured from outside.
    if let Some(a) = app.clone().filter(|a| a.kind == isolate::AppKind::Native || args.has("native")) {
        return native(args, &a.as_kind(isolate::AppKind::Native), &target, set, for_ms);
    }
    if args.get("probes") == Some("waits") {
        return Err(Error::usage(format!("--probes waits takes a native capture's off-CPU waits: a native app (app:<id> of kind native, exe:<path> --native) or pid:<n>, not {target}")));
    }
    // An app opens its own window: `--headless` would only skip the lock check on a headed app.
    if args.has("headless") && app.is_some() {
        return Err(Error::usage(format!("--headless runs a page in a headless browser; {target} draws its own window")));
    }
    if !attach && app.is_none() && !target.starts_with("http://") && !target.starts_with("https://") && !target.starts_with("file://") {
        return Err(Error::usage(format!("unsupported target {target}: use https://…, a local page.html, app:<id>, exe:<path>, cdp:<port> or file:<trace>")));
    }
    // `--engine`: a page in another browser engine; Chromium (the default) records below.
    match args.get("engine").map(WebEngine::parse).transpose()? {
        Some(_) if attach || app.is_some() => return Err(Error::usage("--engine picks the browser strobe launches for a page (https://…, page.html); an attached endpoint or app is the browser it is")),
        Some(WebEngine::Chromium) | None => {}
        Some(engine) => return engine_page(args, engine, &target, set, for_ms),
    }
    let mut guard = None;
    let browser = if attach {
        chromium::attach(&target)?
    } else if let Some(app) = &app {
        let app = &with_gpu_store(args, app)?;
        progress(format!("launching {} in an isolated home", app.exe.display()));
        let (b, g) = electron::launch(app, args.get("home").map(std::path::Path::new), &Window::default(), None)?;
        guard = Some(g);
        b
    } else {
        chromium::launch(&Window::default(), args.has("headless"), None, &[])?
    };
    let gpu = browser.gpu().and_then(validity::software_gpu);
    let debug = app.as_ref().and_then(|a| host::debug_runtime(&a.exe));
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
    // A launched target inherits strobe's priority; an attached one runs as its user left it.
    let qos = if attach { None } else { host::background_qos(None).or_else(|| host::background_qos(Some(browser.pid))) };
    validity::window_visible(&page.clone().into())?;
    // A headless target draws off screen, so only a headed one cares about the lock screen.
    let on_screen = !args.has("headless");
    if on_screen && host::session_locked() {
        return Err(validity::locked());
    }
    let awake = host::Awake::start();
    // `--id`: a handle chosen up front, so an MCP call that outlives its timeout can be followed.
    let (id, draft) = claim(args)?;
    let dir = draft.dir().to_path_buf();
    let token = id.trim_start_matches("rec_").to_string();
    let driver = Driver::new(page.clone().into(), token.clone());
    if set == Set::Memory {
        let meta = memory(&page, &browser, &dir, &id, &target, for_ms)?;
        if on_screen && (awake.locks() > 0 || host::session_locked()) {
            return Err(validity::locked());
        }
        draft.keep();
        if !attach {
            browser.close();
        }
        if let Some(g) = &guard {
            g.check("after the app closed")?;
        }
        return Ok(Output::new(summary_text(&meta), meta));
    }
    let style = set == Set::Attribution && args.has("style");
    let tracing = probe::trace::start_with(&browser.cdp, set, if style { &["disabled-by-default-devtools.timeline.invalidationTracking"] } else { &[] })?;
    if set == Set::Attribution {
        probe::cpu::start(&page)?;
        probe::alloc::start(&page)?;
    }
    std::thread::sleep(Duration::from_millis(300));
    let sampler = (!attach).then(|| probe::os::Sampler::start(browser.pid, browser.profile.clone()));
    // `--os-frames`: the OS's present events beside the trace's frames, a cross-check of both.
    let os_frames = (args.has("os-frames") && !attach).then(crate::probe::present::PresentCapture::start);
    // CPU in cycles over the time stamp counter, as native targets read it: process times move in
    // clock ticks (15.6 ms on Windows), so an idle page read 0.00% on one run and 0.52% on the next.
    let cycles = || -> (u64, Option<u64>) { (browser.processes().iter().map(|p| p.cycles).sum(), host::tsc()) };
    let cpu0 = cycles();
    let t0 = host::now_ns();
    driver.mark("run:0:start")?;
    progress(format!("recording {} for {}", id, fmt::ms_duration(for_ms)));
    std::thread::sleep(Duration::from_millis(for_ms));
    driver.mark("run:0:end")?;
    let t1 = host::now_ns();
    let cpu1 = cycles();
    let os = sampler.map(|s| s.stop().window(t0, t1));
    // The browser's process set (its tree, what carries its profile's stamp, its job), for the OS frames.
    let tree = browser.pids();
    let os_frames = os_frames.map(|r| r.and_then(|c| c.stop()));
    if on_screen && (awake.locks() > 0 || host::session_locked()) {
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
    let mut refused = serde_json::Map::new();
    match (cpu0.1, cpu1.1) {
        (Some(a), Some(b)) if !attach && b > a && cpu1.0 > 0 => {
            extra.entry("cpu.app.total".into()).or_default().insert("run:0".into(), 100.0 * cpu1.0.saturating_sub(cpu0.0) as f64 / (b - a) as f64);
        }
        _ if attach => {}
        _ => {
            let r = super::outside::not_on_this_os("cpu.app.total").unwrap_or_else(|| crate::model::Refusal::new(crate::model::ValidatorId::RunIncomplete, "cpu.app.total: no CPU of the browser's processes could be read"));
            refused.insert("cpu.app.total".into(), json!(r.message()));
        }
    }
    if let Some(p) = os.as_ref().and_then(|o| o.gpu_3d_pct) {
        extra.entry("gpu.process.3d".into()).or_default().insert("run:0".into(), p);
    }
    let mut values = metrics::block_values(&data, "run", &extra);
    if let Some(v) = os.as_ref().and_then(|o| o.vram_dedicated_mb) {
        values.insert("gpu.vram".into(), v);
    }
    // The trace's mark times are its own clock; the OS window is the same span on QPC (§6).
    let os_frames = match os_frames {
        Some(Ok(p)) => {
            let run = crate::probe::present::frames::from_presents(&p.presents, &tree, t0, t1);
            let display = host::main_window(&tree).and_then(|w| host::display_of(w.hwnd));
            let v = crate::probe::present::frames::values(&[run], crate::probe::present::frames::refresh_ms(&p.vsyncs, t0, t1, display.as_ref()));
            crate::symbol::write_gz(&dir.join("presents.json.gz"), &p)?;
            json!({ "values": v, "events": p.events, "events_lost": p.events_lost, "consumer_cpu_ms": p.consumer_cpu_ms, "pids": tree })
        }
        Some(Err(e)) => json!({ "refused": e.message }),
        None => Value::Null,
    };
    let mut meta = json!({
        "id": id, "kind": "record", "target": target, "probes": set, "token": token, "duration_ms": for_ms,
        "browser": version, "gpu": gpu, "env": host::env(), "trace_bytes": bytes, "values": values, "refused": refused, "os": os, "os_frames": os_frames,
        "phases": metrics::phase_frames(&data).keys().collect::<Vec<_>>(),
        // Rule 3: timing taken with profilers on describes the profiled target, not the target; an
        // app with a debug C runtime is a debug build (`DevBuild`).
        "timing": if set == Set::Attribution { "perturbed" } else if debug.is_some() { "debug build" } else { "clean" }, "debug_runtime": debug,
        "created": now_iso(), "valid": true,
    });
    refuse_background(&mut meta, qos);
    store::write_json(&dir.join("meta.json"), &meta)?;
    draft.keep();
    Ok(Output::new(summary_text(&meta), meta))
}

/// `app` with its GPU shader cache store: `--gpu-cache` (else the profile's `gpuCache`; default warm)
/// lends each launch one store per executable, so a recording after the first runs with them warm,
/// as for a person.
fn with_gpu_store(args: &Args, app: &isolate::App) -> Result<isolate::App> {
    let mut app = app.clone();
    match args.get("gpu-cache").or(app.gpu_cache_mode.as_deref()).unwrap_or("warm") {
        "warm" if !cfg!(target_os = "macos") => app.gpu_cache = Some(isolate::exe_gpu_store(&app.exe)),
        "warm" | "cold" => {}
        other => return Err(Error::usage(format!("--gpu-cache is warm or cold, not {other}"))),
    }
    Ok(app)
}

/// A page in an engine other than Chromium (`--engine firefox`) for `for_ms`: what needs no Chromium
/// trace, from outside as for a native app: its processes' CPU in cycles, GPU use and memory from the OS,
/// the DOM's size, and headed, the OS's presents of its processes (`frames.*`, SPEC §7.2). Profilers,
/// heap snapshots and the census are Chromium's DevTools: `--probes attribution|memory` refuses.
fn engine_page(args: &Args, engine: WebEngine, target: &str, set: Set, for_ms: u64) -> Result<Output> {
    if set != Set::Timing {
        return Err(Error::refused("EngineUnsupported", format!("--probes {}: profilers, heap snapshots and the memory census are Chromium's DevTools; {engine} has none in strobe yet", args.get("probes").unwrap_or("")), "record the page in Chromium (drop --engine) for attribution or memory"));
    }
    let headless = args.has("headless");
    let patched = web::patched(engine);
    if let Some(why) = &patched {
        progress(format!("! PatchedBrowser  {why}"));
    }
    let browser = web::launch(engine, &Window::default(), headless, None)?;
    let gpu = if headless { browser.gpu()? } else { validity::software_gpu(browser.gpu()?)? };
    let page = browser.page()?;
    progress(format!("loading {target} in {}", browser.version()));
    page.navigate(target, Duration::from_secs(60))?;
    let qos = host::background_qos(None).or_else(|| host::background_qos(Some(browser.pid())));
    validity::window_visible(&page)?;
    if !headless && host::session_locked() {
        return Err(validity::locked());
    }
    let awake = host::Awake::start();
    let (id, draft) = claim(args)?;
    let dir = draft.dir().to_path_buf();
    std::thread::sleep(Duration::from_millis(300));
    let sampler = probe::os::Sampler::start(browser.pid(), browser.profile());
    // The capture follows the browser's windows (macOS and X11 stream them by window).
    let presents = (!headless).then(|| {
        crate::probe::present::PresentCapture::start().map(|mut c| {
            c.track(&browser.pids());
            c
        })
    });
    let cycles = || -> (u64, Option<u64>) { (browser.processes().iter().map(|p| p.cycles).sum(), host::tsc()) };
    let cpu0 = cycles();
    let t0 = host::now_ns();
    progress(format!("recording {id} for {}", fmt::ms_duration(for_ms)));
    std::thread::sleep(Duration::from_millis(for_ms));
    let t1 = host::now_ns();
    let cpu1 = cycles();
    let os = sampler.stop().window(t0, t1);
    let tree = browser.pids();
    let presents = presents.map(|r| r.and_then(|c| c.stop()));
    if !headless && (awake.locks() > 0 || host::session_locked()) {
        return Err(validity::locked());
    }
    let mut values: BTreeMap<String, f64> = BTreeMap::new();
    let mut refused = serde_json::Map::new();
    // Where cycles are not time on this machine (`outside::not_on_this_os`), both CPU metrics are refused.
    match (cpu0.1, cpu1.1, super::outside::not_on_this_os("cpu.app.total")) {
        (Some(a), Some(b), None) if b > a && cpu1.0 > 0 => {
            let share = cpu1.0.saturating_sub(cpu0.0) as f64 / (b - a) as f64;
            values.insert("cpu.app.total".into(), 100.0 * share);
            values.insert("cpu.work".into(), share * (t1 - t0) as f64 / 1e6);
        }
        (_, _, why) => {
            let r = why.unwrap_or_else(|| crate::model::Refusal::new(crate::model::ValidatorId::RunIncomplete, "no CPU of the browser's processes could be read"));
            for m in ["cpu.app.total", "cpu.work"] {
                refused.insert(m.into(), json!(r.message()));
            }
        }
    }
    if let Some(p) = os.gpu_3d_pct.filter(|_| !headless) {
        values.insert("gpu.process.3d".into(), p);
    }
    // A headless browser that drew in software held no GPU memory: nothing to read there.
    if let Some(v) = os.vram_dedicated_mb.filter(|_| !(headless && gpu.software)) {
        values.insert("gpu.vram".into(), v);
    }
    values.insert("memory.app.private".into(), browser.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0));
    if let Some(n) = page.eval("document.getElementsByTagName('*').length").ok().and_then(|v| v.as_f64()) {
        values.insert("dom.nodes".into(), n);
    }
    let os_frames = match presents {
        Some(Ok(p)) => {
            let display = host::main_window(&tree).and_then(|w| host::display_of(w.hwnd));
            let f = crate::probe::present::frames::native_values(&p, &tree, &[(t0, t1)], display.as_ref());
            // What `query <rec> frames` reads for a recording without a trace, as a native one's.
            crate::symbol::write_gz(&dir.join("presents.json.gz"), &p)?;
            store::write_json(&dir.join("frames.json"), &json!({ "pids": tree, "runs": { "run:0": { "t0_ns": t0, "t1_ns": t1, "inputs": [], "marks": [], "changes": [] } } }))?;
            match f.refused {
                Some(why) => json!({ "refused": why }),
                None => {
                    values.extend(f.values.clone());
                    for (m, why) in &f.partial {
                        refused.insert(m.clone(), json!(why));
                    }
                    json!({ "values": f.values, "events": p.events, "events_lost": p.events_lost, "consumer_cpu_ms": p.consumer_cpu_ms, "pids": tree })
                }
            }
        }
        Some(Err(e)) => json!({ "refused": e.message }),
        None => json!({ "refused": "Headless: the page ran headless, so nothing it drew reached a display" }),
    };
    let version = browser.version().to_string();
    browser.close();
    let mut meta = json!({
        "id": id, "kind": "record", "target": target, "probes": set, "token": "", "duration_ms": for_ms, "engine": engine, "headless": headless,
        "browser": version, "gpu": gpu, "env": host::env(), "values": values, "refused": refused, "os": os, "os_frames": os_frames, "phases": [],
        "timing": "clean", "created": now_iso(), "valid": true, "patched_browser": patched,
    });
    refuse_background(&mut meta, qos);
    store::write_json(&dir.join("meta.json"), &meta)?;
    draft.keep();
    Ok(Output::new(summary_text(&meta), meta))
}

/// A native app for `for_ms` (SPEC §13.7): launched isolated, left to settle for 3 s, then one run
/// measured from outside: its processes' CPU in cycles, its threads' wakes (named in `threads.json`), IO,
/// GPU, memory, the UI thread's CPU and what reached the screen; attribution adds native stacks (the deep helper).
fn native(args: &Args, app: &isolate::App, target: &str, set: Set, for_ms: u64) -> Result<Output> {
    use crate::probe::wakes;
    use crate::target::native as nt;
    if set == Set::Memory {
        return Err(Error::usage("--probes memory follows a JS heap; a native app has none"));
    }
    // `--until-line`: start when the app prints a matching line, not after a fixed settle; `--stop-line`:
    // end at one (`--for` caps it then).
    let line = |k: &str| args.get(k).map(|r| regex_lite::Regex::new(r).map_err(|e| Error::usage(format!("--{k}: {e}")))).transpose();
    let (until, stop) = (line("until-line")?, line("stop-line")?);
    let app = &with_gpu_store(args, app)?;
    let home = isolate::prepare_home(app, args.get("home").map(std::path::Path::new), None)?;
    match home.gpu_warm {
        Some(true) => progress("GPU shader caches: warm, from earlier recordings of this executable"),
        Some(false) => progress("GPU shader caches: cold, the first recording of this executable (the next one is warm; --gpu-cache cold keeps them cold)"),
        None => {}
    }
    progress(format!("launching {} in an isolated home", app.exe.display()));
    let (mut n, guard) = match nt::launch(app, &home, &[], until.is_some() || stop.is_some(), None) {
        Ok(x) => x,
        Err(e) => {
            isolate::release_home(&home.dir);
            isolate::remove_home(&home.dir);
            return Err(e);
        }
    };
    // A profile with `window: false` (a tray app) has no window to wait for, place or check, and
    // nothing of it reaches the display: a locked session changes none of what is recorded.
    let windowless = app.windowless;
    let qos = host::background_qos(None).or_else(|| host::background_qos(Some(n.pid)));
    // Each thread's CPU and wakes from the launch on (`query <rec> threads --timeline`).
    let timeline = std::cell::RefCell::new(Some({
        let (root, dir) = (n.pid, home.dir.clone());
        wakes::Timeline::start(move || host::process_set(root, Some(&dir)).iter().map(|p| p.pid).collect())
    }));
    // The app's last output lines `wait_line` read, for a refusal when it exits.
    let seen = std::cell::RefCell::new(std::collections::VecDeque::new());
    let result = (|| -> Result<Value> {
        if !windowless {
            n.wait_window(Duration::from_secs(60), &guard)?;
            n.place(&Window::default(), false);
            guard.check("after the app opened its window")?;
        }
        // The app loads itself; give it time to settle before recording, or wait for the line that says it is ready.
        match &until {
            Some(re) => {
                let l = wait_line(&n, re, Duration::from_secs(120), &seen)?;
                progress(format!("start: {}", l.trim()));
            }
            None => std::thread::sleep(Duration::from_secs(3)),
        }
        app_gone(&mut n, "before the recording", |n| last_output(n, &seen, 5))?;
        // Its process's CPU goes with it when it exits: one that ends inside the window refuses.
        let running = n.exited().is_none();
        if !windowless {
            n.raise();
            n.check_visible()?;
            if host::session_locked() {
                return Err(validity::locked());
            }
        }
        let awake = host::Awake::start();
        let (id, draft) = claim(args)?;
        let dir = draft.dir().to_path_buf();
        let sampler = probe::os::Sampler::start(n.pid, Some(n.home.clone()));
        // What reached the screen (OS present events), or in attribution, native stacks (the helper).
        let presents = (set != Set::Attribution && !windowless).then(|| {
            crate::probe::present::PresentCapture::start().map(|mut c| {
                c.track(&n.pids());
                c
            })
        });
        let waits = args.get("probes") == Some("waits");
        let stacks = (set == Set::Attribution && crate::probe::stacks::stacks_available()).then(|| crate::probe::stacks::StackCapture::start(crate::probe::stacks::Targets::new(n.pid, Some(&n.home)), waits));
        let cycles = || -> (Cycles, Option<u64>) { (per_process(&n.processes()), host::tsc()) };
        progress(match &stop {
            Some(re) => format!("recording {id} until the app prints a line matching {re}"),
            None => format!("recording {id} for {}", fmt::ms_duration(for_ms)),
        });
        let snap0 = wakes::Snapshot::take(&n.pids());
        let procs0 = (host::now_ns(), host::all_processes());
        let c0 = cycles();
        let t0 = host::now_ns();
        match &stop {
            Some(re) => {
                let cap = if args.get("for").is_some() { Duration::from_millis(for_ms) } else { Duration::from_secs(600) };
                match wait_line(&n, re, cap, &seen) {
                    Ok(l) => progress(format!("stop: {}", l.trim())),
                    Err(e) if e.code == 124 => progress(format!("stop: no line matched in {} (the cap)", fmt::ms_duration(cap.as_millis() as u64))),
                    Err(e) => return Err(e),
                }
            }
            None => std::thread::sleep(Duration::from_millis(for_ms)),
        }
        let t1 = host::now_ns();
        let for_ms = (t1 - t0) / 1_000_000;
        let c1 = cycles();
        let span = wakes::span(&snap0, &wakes::Snapshot::take(&n.pids()));
        // A launcher that exits cleanly (code 0) and leaves the app running is not the app ending.
        if let Some(code) = n.exited().filter(|&c| running && (c != 0 || n.processes().is_empty())) {
            return Err(exited(code, "during the recording", last_output(&n, &seen, 5).as_deref()));
        }
        let screen = super::bench::screen_state(&procs0, &(host::now_ns(), host::all_processes()));
        if let Some(t) = timeline.borrow_mut().take() {
            let mut t = t.stop();
            t.marks.push(("recording".into(), t0, t1));
            crate::symbol::write_gz(&dir.join("timeline.json.gz"), &t)?;
        }
        let samples = sampler.stop();
        let gpu_missing = (!super::outside::gpu_read(&samples)).then(|| samples.gpu_unreadable.clone());
        let os = samples.window(t0, t1);
        let presents = presents.map(|r| r.and_then(|c| c.stop()));
        let stacks = stacks.map(|r| r.and_then(|c| c.stop()));
        let session_locked = awake.locks() > 0 || host::session_locked();
        if session_locked && !windowless {
            return Err(validity::locked());
        }
        if !windowless {
            n.check_visible()?;
        }
        let mut frame_values = BTreeMap::new();
        let unpaced = match &presents {
            Some(Ok(p)) => {
                let display = n.window.as_ref().and_then(|w| host::display_of(w.hwnd));
                let hz = crate::probe::present::frames::refresh_ms(&p.vsyncs, t0, t1, display.as_ref()).map(|ms| 1000.0 / ms).or(display.map(|d| d.hz));
                crate::probe::present::frames::spin(&p.presents, &n.pids(), &[(t0, t1)], hz)
            }
            _ => None,
        };
        let os_frames = match &presents {
            Some(Ok(p)) => {
                let display = n.window.as_ref().and_then(|w| host::display_of(w.hwnd));
                let f = crate::probe::present::frames::native_values(p, &n.pids(), &[(t0, t1)], display.as_ref());
                frame_values = f.values;
                crate::symbol::write_gz(&dir.join("presents.json.gz"), p)?;
                json!({ "events": p.events, "events_lost": p.events_lost, "consumer_cpu_ms": p.consumer_cpu_ms, "refused": f.refused, "refused_metrics": f.partial, "display": display })
            }
            Some(Err(e)) => json!({ "refused": e.message }),
            None => Value::Null,
        };
        // Waits asked for and not taken refuse the recording, saying why (SPEC §11.1).
        if waits {
            match &stacks {
                Some(Ok(d)) if d.waits.is_none() => return Err(d.waits_refused.clone().unwrap_or_else(|| crate::probe::stacks::no_waits("no waits were taken", "try again")).into()),
                Some(Err(e)) => return Err(crate::probe::stacks::no_waits(format!("the native capture failed: {}", e.message), "fix what it names").into()),
                None => return Err(crate::probe::stacks::no_waits(crate::probe::stacks::no_stacks(), "see strobe status").into()),
                _ => {}
            }
        }
        let stacks_note = match stacks {
            Some(Ok(d)) => {
                crate::symbol::write_gz(&dir.join("deep.json.gz"), &d)?;
                format!("native stacks: {} samples (query hotspots){}{}", d.samples.len(), crate::probe::stacks::stacks_how(&d), d.waits.as_ref().map(|w| format!("; {} off-CPU waits (query waits)", w.waits.len())).unwrap_or_default())
            }
            Some(Err(e)) => format!("native stacks failed: {}", e.message),
            None => crate::probe::stacks::no_stacks().into(),
        };
        let ms = (t1 - t0) as f64 / 1e6;
        let mut values: BTreeMap<&str, f64> = BTreeMap::new();
        values.insert("run.duration", ms);
        if let (Some(a), Some(b)) = (c0.1, c1.1)
            && c1.0.values().any(|c| *c > 0)
        {
            let cpu = cycles_used(&c0.0, &c1.0) as f64 / b.saturating_sub(a).max(1) as f64 * ms;
            values.insert("cpu.work", cpu);
            values.insert("cpu.app.total", 100.0 * cpu / ms);
        }
        if !span.threads.is_empty() {
            values.insert("cpu.wakes", span.wakes_per_s());
            values.insert("io.read", span.io_read as f64);
            values.insert("io.write", span.io_write as f64);
        }
        values.extend(wakes::counter_values(&span.counted));
        if let Some(p) = os.gpu_3d_pct {
            values.insert("gpu.process.3d", p);
        }
        if let Some(v) = os.vram_dedicated_mb {
            values.insert("gpu.vram", v);
        }
        if let Some(v) = os.vram_shared_mb {
            values.insert("gpu.shared", v);
        }
        values.insert("memory.app.private", n.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0));
        // What holds that memory (host, GPU, files, images), read after the window.
        let reading = probe::memory::Reading::take(&n.processes(), "after the recording");
        values.extend(reading.values());
        store::write_json(&dir.join("memory.json"), &json!({ "readings": [&reading] }))?;
        // The UI thread: the main window's, from its own cycle counter and context switches.
        let ui_tid = n.window.as_ref().and_then(|w| host::window_thread(w.hwnd));
        if let Some(t) = ui_tid.and_then(|tid| span.threads.iter().find(|t| t.tid == tid)) {
            if let Some(c) = t.cpu_ms {
                values.insert("ui.cpu", c);
            }
            if super::outside::not_on_this_os("ui.wakes").is_none() {
                values.insert("ui.wakes", t.switches as f64 / (ms / 1e3));
            }
        }
        let names: BTreeMap<u32, String> = span.threads.iter().filter_map(|t| Some((t.tid, host::thread_name(t.tid)?))).collect();
        let spans = std::slice::from_ref(&span);
        store::write_json(&dir.join("threads.json"), &json!(wakes::by_thread(spans, &names, &wakes::modules(spans))))?;
        store::write_json(
            &dir.join("runs.json"),
            &json!([{ "run": "run:0", "ms": ms, "switches": span.switches, "wakes_per_s": span.wakes_per_s(), "io_read": span.io_read, "io_write": span.io_write }]),
        )?;
        let debug = host::debug_runtime(&app.exe);
        let refused = super::outside::os_refusals(&|m| values.contains_key(m), gpu_missing.as_deref());
        let mut meta = json!({
            "id": id, "kind": "record", "native": true, "target": target, "exe": app.exe, "probes": set, "duration_ms": for_ms, "env": host::env(), "values": values, "refused": refused, "os": os,
            "window": n.window, "ui_thread": ui_tid, "created": now_iso(), "valid": true, "os_frames": os_frames, "session_locked": session_locked,
            "gpu_cache": home.gpu_warm.map(|w| if w { "warm" } else { "cold" }), "screen": screen,
            "unpaced": unpaced.map(|(fps, hz)| json!({ "fps": fps, "hz": hz })),
            // A debug build's times describe no build people run (`DevBuild`).
            "timing": if debug.is_some() { "debug build" } else { "clean" }, "debug_runtime": debug,
            "note": if set == Set::Attribution { format!("native attribution: OS samples; {stacks_note}") } else { String::new() },
            // macOS: the app's XPC services are missing from every number (SPEC §13.2).
            "unattributed": n.unattributed(),
        });
        for (k, x) in frame_values {
            meta["values"][k] = json!(x);
        }
        refuse_background(&mut meta, qos.clone());
        store::write_json(&dir.join("meta.json"), &meta)?;
        draft.keep();
        Ok(meta)
    })();
    n.close(true);
    guard.check("after the app closed")?;
    let meta = result?;
    Ok(Output::new(summary_text(&meta), meta))
}

/// CPU so far in cycles by process (pid and start time).
pub type Cycles = BTreeMap<(u32, u64), u64>;

pub fn per_process(procs: &[host::ProcTimes]) -> Cycles {
    procs.iter().map(|p| ((p.pid, host::start_time(p.pid).unwrap_or(0)), p.cycles)).collect()
}

/// Cycles used between two readings, each process against its own earlier reading: one that started in
/// between counts from zero, and one that exited takes its CPU with it instead of lowering the others'
/// sum (a launcher that handed over, a utility process that ended).
pub fn cycles_used(a: &Cycles, b: &Cycles) -> u64 {
    b.iter().map(|(k, c)| c.saturating_sub(a.get(k).copied().unwrap_or(0))).sum()
}

/// A new recording's id and folder, held as a draft until its meta.json is written: `--id` (a handle
/// chosen up front, so an MCP call that outlives its timeout can be followed), else a fresh one.
pub fn claim(args: &Args) -> Result<(String, store::Draft)> {
    match args.get("id") {
        Some(id) => Ok((id.to_string(), store::claim_recording(id)?)),
        None => store::new_recording(),
    }
}

/// `RunIncomplete` once a launched native app is gone: its process exited and nothing of its process
/// set remains (a launcher that handed over to a process stamped with its home is not gone). `when`
/// says when ("before the runs"); `last` is its last output lines, when strobe read its output.
pub fn app_gone(n: &mut crate::target::native::Native, when: &str, last: impl FnOnce(&crate::target::native::Native) -> Option<Vec<String>>) -> Result<()> {
    let Some(code) = n.exited() else { return Ok(()) };
    // A process the app started just before it exited may show no stamp for a moment (a child between
    // fork and exec): one more look before refusing.
    if !n.processes().is_empty() {
        return Ok(());
    }
    std::thread::sleep(Duration::from_millis(100));
    if !n.processes().is_empty() {
        return Ok(());
    }
    Err(exited(code, when, last(n).as_deref()))
}

/// The refusal for an app that exited with `code` `when`, with its last output lines (None: strobe
/// did not read its output).
pub fn exited(code: i32, when: &str, last: Option<&[String]>) -> Error {
    let r = crate::model::Refusal::new(crate::model::ValidatorId::RunIncomplete, format!("the app exited (code {code}) {when}, so there was no app left to measure"));
    match last {
        Some([]) => r.detail("it printed nothing").fix("run the app's command line by hand to see why it exits"),
        Some(lines) => r.detail(format!("its last output: {}", lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" | "))).fix("fix what made the app exit (its output above)"),
        None => r.fix("run the app's command line by hand to see why it exits; output: true on the target keeps its last lines here"),
    }
    .into()
}

/// Up to the last `keep` lines of the app's output: those `wait_line` read (`seen`), then what its
/// pipe still holds, read until the pipe closes or for half a second (a crash's last stderr lines come
/// after its exit). None when its output is not read.
fn last_output(n: &crate::target::native::Native, seen: &std::cell::RefCell<std::collections::VecDeque<String>>, keep: usize) -> Option<Vec<String>> {
    let rx = n.output.as_ref()?;
    let mut lines: Vec<String> = seen.borrow().iter().cloned().collect();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok((_, _, l)) => lines.push(l),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Some(lines[lines.len().saturating_sub(keep)..].to_vec())
}

/// The first line of the app's output (stdout and stderr) matching `re`, within `timeout`.
fn wait_line(n: &crate::target::native::Native, re: &regex_lite::Regex, timeout: Duration, seen: &std::cell::RefCell<std::collections::VecDeque<String>>) -> Result<String> {
    let rx = n.output.as_ref().ok_or_else(|| Error::msg("the app's output is not being read"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(Error::timeout(format!("no line of the app's output matched {re} in {} s", timeout.as_secs())).hint("check the pattern against what the app prints (stdout and stderr)"));
        }
        match rx.recv_timeout(left) {
            Ok((_, _, line)) => {
                let mut s = seen.borrow_mut();
                if s.len() == 5 {
                    s.pop_front();
                }
                s.push_back(line.clone());
                if re.is_match(&line) {
                    return Ok(line);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(Error::msg(format!("the app's output ended before a line matched {re}"))),
        }
    }
}

/// `--probes memory`: does the target keep what it allocates? A census and a heap snapshot at the
/// start, the middle and the end (S0, S1, S2), no trace (SPEC §8.6). Objects allocated between S0 and
/// S1 that are still alive in S2 are the leak suspects. Each census comes before its snapshot, which
/// raises the memory of the process it serializes; OS memory is reported from the first census only.
fn memory(page: &chromium::Page, browser: &chromium::Browser, dir: &std::path::Path, id: &str, target: &str, for_ms: u64) -> Result<Value> {
    page.send("HeapProfiler.enable", json!({}))?;
    let started = std::time::Instant::now();
    progress(format!("recording {id} for {}: a heap snapshot at the start, middle and end", fmt::ms_duration(for_ms)));
    let mut census = Vec::new();
    for (i, file) in ["heap-s0", "heap-s1", "heap-0"].iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(for_ms / 2));
        }
        let mut c = probe::census::take(browser, page);
        c["at_ms"] = json!(started.elapsed().as_millis() as u64);
        c["dom_nodes"] = c["dom"]["nodes"].clone();
        // OS memory of the snapshotted process is inflated after its first snapshot (CensusContaminated).
        c["private_bytes"] = if i == 0 { json!(browser.processes().iter().map(|p| p.private_bytes).sum::<u64>()) } else { Value::Null };
        c["os_after_snapshot"] = json!(i > 0);
        c["allocators"] = probe::census::allocators(browser, &dir.join(format!("memdump-{i}.pftrace"))).unwrap_or_else(|e| json!({ "error": e.message }));
        census.push(c);
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

/// `BackgroundPriority` (SPEC §10.1): a recording whose target ran at background priority keeps no
/// frame or input-latency values; its summary says why.
fn refuse_background(meta: &mut Value, why: Option<String>) {
    let Some(why) = why else { return };
    for path in ["/values", "/os_frames/values"] {
        if let Some(o) = meta.pointer_mut(path).and_then(Value::as_object_mut) {
            o.retain(|k, _| !super::bench::foreground_metric(k));
        }
    }
    meta["background_priority"] = json!(why);
}

fn background_line(meta: &Value) -> Option<String> {
    meta["background_priority"].as_str().map(|why| format!("frames   refused (BackgroundPriority): the target ran at {why}; frames and input latencies there describe no person's machine (CPU is kept)\n"))
}

/// A page recorded in another engine than Chromium (`engine_page`): what it measured from outside.
fn engine_summary(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let id = meta["id"].as_str().unwrap_or("");
    let mut s = format!(
        "{id} · {} · {} · {}{}\n",
        meta["target"].as_str().unwrap_or(""),
        fmt::ms_duration(meta["duration_ms"].as_u64().unwrap_or(0)),
        meta["browser"].as_str().unwrap_or(""),
        if meta["headless"] == true { " · headless" } else { "" }
    );
    s += &format!("cpu      {} of one core (cpu.app.total) · {} in the window (cpu.work), all processes\n", fmt::value(get("cpu.app.total"), "%"), fmt::value(get("cpu.work"), "ms"));
    s += &format!("memory   {} private · {} DOM nodes\n", fmt::value(get("memory.app.private"), "MB"), fmt::value(get("dom.nodes"), "count"));
    match (background_line(meta), meta["os_frames"]["refused"].as_str()) {
        (Some(line), _) => s += &line,
        (None, Some(why)) => s += &format!("frames   refused: {why}\n"),
        (None, None) => {
            s += &format!(
                "frames   {} on screen (OS presents) · {} of refreshes dropped · display {}\n",
                fmt::value(get("frames.presented.count"), "count"),
                fmt::value(get("frames.dropped.pct"), "%"),
                if get("display.hz").is_finite() { format!("{:.0} Hz", get("display.hz")) } else { "?".into() }
            );
            if get("gpu.process.3d").is_finite() {
                s += &format!("gpu      {} of the 3D engine\n", fmt::value(get("gpu.process.3d"), "%"));
            }
        }
    }
    s += &format!("next     strobe query {id} summary\n");
    s
}

pub fn summary_text(meta: &Value) -> String {
    let mut s = summary_body(meta);
    s += &context_lines(meta);
    s
}

/// What a recording ran and in what state, where it kept them: the variant's commit, the screen
/// (display, session, compositor), the GPU shader caches, and presents that never waited for the
/// display.
fn context_lines(meta: &Value) -> String {
    let mut s = String::new();
    let g = &meta["variant_git"];
    if let Some(commit) = g["commit"].as_str() {
        let changes = match (g["dirty"] == true, g["changes"].as_str()) {
            (true, Some(h)) => format!(" with uncommitted changes (diff {h})"),
            (true, None) => " with uncommitted changes".into(),
            _ => String::new(),
        };
        s += &format!("variant  {} · {commit}{changes}\n", meta["variant"].as_str().unwrap_or(""));
    }
    if let Some(line) = super::bench::screen_line(&meta["screen"]) {
        s += &format!("screen   {line}\n");
    }
    if let Some(c) = meta["gpu_cache"].as_str() {
        s += &format!("gpu      shader caches {c} at the launch{}\n", if c == "cold" { " (the driver compiled them during it)" } else { "" });
    }
    if let (Some(fps), Some(hz)) = (meta["unpaced"]["fps"].as_f64(), meta["unpaced"]["hz"].as_f64()) {
        s += &format!("! UnpacedSpin  the app presented {fps:.0} frames a second on a {hz:.0} Hz display: its presents finish without waiting for the display, so it draws as fast as it can\n");
    }
    if let Some(why) = meta["unattributed"].as_str() {
        s += &format!("! Unattributed  {why}: cpu, memory, wakes and GPU cover the app's own processes only\n");
    }
    s
}

fn summary_body(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    if meta["pid"].is_u64() && meta["probes"] == "timing" {
        return super::outside::text(meta);
    }
    if meta["probes"] == "deep" {
        let id = meta["id"].as_str().unwrap_or("");
        return format!(
            "{id} · {} · deep · {} native samples · {:.1}% resolved\nnext     strobe query {id} hotspots · strobe query {id} hotspots --by package\n",
            meta["target"].as_str().unwrap_or(""),
            meta["samples"],
            100.0 * meta["native"]["resolved_share"].as_f64().unwrap_or(0.0)
        );
    }
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
    if meta["native"] == true {
        return native_summary(meta);
    }
    if meta["engine"].as_str().is_some_and(|e| e != "chromium") {
        return engine_summary(meta);
    }
    let mut s = format!(
        "{} · {} · {} · {}\n",
        meta["id"].as_str().unwrap_or(""),
        meta["target"].as_str().unwrap_or(""),
        fmt::ms_duration(meta["duration_ms"].as_u64().unwrap_or(0)),
        meta["probes"].as_str().unwrap_or("")
    );
    if let Some(line) = background_line(meta) {
        s += &line;
    } else {
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
    }
    s += &format!("main     {} busy · {} long tasks\n", fmt::value(get("main.busy"), "%"), fmt::value(get("main.tasks.long"), "count"));
    if get("cpu.app.total").is_finite() {
        s += &format!("cpu      {} of one core, all processes\n", fmt::value(get("cpu.app.total"), "%"));
    }
    if get("interaction.p95").is_finite() {
        s += &format!("input    p95 {} input → presentation\n", fmt::value(get("interaction.p95"), "ms"));
    }
    let of = &meta["os_frames"];
    if let Some(why) = of["refused"].as_str() {
        s += &format!("os       refused: {why}\n");
    } else if of["values"].is_object() {
        let g = |k: &str| of["values"][k].as_f64().unwrap_or(f64::NAN);
        s += &format!(
            "os       {} displayed · interval p50 {} · p95 {} · {} dropped ({}) · display {:.1} Hz   (OS present events)\n",
            fmt::value(g("frames.n"), "count"),
            fmt::value(g("frames.interval.p50"), "ms"),
            fmt::value(g("frames.interval.p95"), "ms"),
            fmt::value(g("frames.dropped"), "count"),
            fmt::value(g("frames.dropped.pct"), "%"),
            g("display.hz")
        );
    }
    s += &refused_lines(meta);
    if meta["timing"] == "perturbed" {
        s += "timing   perturbed: profilers ran during this recording, so its times are not the target's; take times with --probes timing\n";
    }
    s += &format!("next     strobe query {} frames · strobe report recording {} --open\n", meta["id"].as_str().unwrap_or(""), meta["id"].as_str().unwrap_or(""));
    s
}

/// A native recording or block: what the OS saw of the app.
fn native_summary(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let id = meta["id"].as_str().unwrap_or("");
    let target = meta["target"].as_str().map(str::to_string).unwrap_or_else(|| meta["target"].to_string());
    let mut s = format!("{id} · {target} · {} · {} · native\n", fmt::ms_duration(meta["duration_ms"].as_u64().unwrap_or(0)), meta["probes"].as_str().unwrap_or(""));
    if meta["launch"] == true {
        s += &launch_line(v);
        s += &format!("cpu      {} from the spawn until ready, all processes\n", fmt::value(get("cpu.work"), "ms"));
    } else {
        s += &format!("cpu      {} of one core (cpu.app.total) · {} in the run (cpu.work), all processes\n", fmt::value(get("cpu.app.total"), "%"), fmt::value(get("cpu.work"), "ms"));
    }
    if get("cpu.wakes").is_finite() {
        s += &format!("wakes    {} context switches of its threads\n", fmt::value(get("cpu.wakes"), "/s"));
    }
    if get("io.read").is_finite() {
        s += &format!("io       read {} · written {}\n", fmt::bytes(get("io.read")), fmt::bytes(get("io.write")));
    }
    s += &fmt::counters(get);
    s += &memory_lines(meta);
    if get("ui.cpu").is_finite() {
        s += &ui_line(meta);
    }
    if get("frames.n").is_finite() {
        s += &format!(
            "frames   {} on screen · interval p95 {} · {} dropped (OS present events)\n",
            fmt::value(get("frames.n"), "count"),
            fmt::value(get("frames.interval.p95"), "ms"),
            fmt::value(get("frames.dropped"), "count")
        );
    }
    if let Some(line) = background_line(meta) {
        s += &line;
    }
    s += &refused_lines(meta);
    if meta["timing"] == "perturbed" {
        s += "timing   perturbed: take times with --probes timing\n";
    }
    s += &if meta["launch"] == true {
        format!("next     strobe query {id} threads (spawn until ready) · strobe query {id} hotspots\n")
    } else {
        format!("next     strobe query {id} threads · strobe query {id} memory · strobe query {id} gpu · strobe query {id} frames\n")
    };
    s
}

/// A launch's milestones from the spawn, ready first: `launch   ready 412 ms · up 380 ms · painted 300 ms`.
pub fn launch_line(v: &Value) -> String {
    let mut ms: Vec<(&str, f64)> = v.as_object().into_iter().flatten().filter_map(|(k, x)| Some((k.strip_prefix("launch.")?, x.as_f64()?))).collect();
    if ms.is_empty() {
        return String::new();
    }
    ms.sort_by(|a, b| (a.0 != "ready").cmp(&(b.0 != "ready")).then(a.1.total_cmp(&b.1)));
    format!("launch   {}   (from the spawn)\n", ms.iter().map(|(k, x)| format!("{k} {}", fmt::value(*x, "ms"))).collect::<Vec<_>>().join(" · "))
}

/// A native target's GPU use and memory: what the GPU's statistics say and what holds the processes'
/// memory (`memory.app.host` beside `memory.app.private`).
pub fn memory_lines(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    // A value, or `refused` when the recording says why there is none (`refused_lines` says it).
    let shown = |k: &str, unit: &str| if meta["refused"][k].is_string() { "refused".to_string() } else { fmt::value(get(k), unit) };
    let mut s = String::new();
    if get("gpu.process.3d").is_finite() || get("gpu.vram").is_finite() {
        s += &format!("gpu      3D {} · VRAM {} · shared {}\n", shown("gpu.process.3d", "%"), shown("gpu.vram", "MB"), shown("gpu.shared", "MB"));
    }
    if get("memory.app.private").is_finite() {
        s += &format!("memory   {} private", fmt::value(get("memory.app.private"), "MB"));
        if get("memory.app.host").is_finite() {
            s += &format!(" · {} host (what the processes allocated; private also counts GPU memory mapped into or charged to them)", fmt::value(get("memory.app.host"), "MB"));
        }
        s += "\n";
    }
    s
}

/// The UI thread's CPU and wakes (`refused` where the OS counts no wakes per thread).
pub fn ui_line(meta: &Value) -> String {
    let get = |k: &str| meta["values"].get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let wakes = if meta["refused"]["ui.wakes"].is_string() { "wakes refused".to_string() } else { format!("{} wakes", fmt::value(get("ui.wakes"), "/s")) };
    format!("ui       {} CPU on the UI thread (the main window's) · {wakes}\n", fmt::value(get("ui.cpu"), "ms"))
}

/// The recording's refused metrics, once per reason: `refused  gpu.vram, gpu.shared: macOS: …`.
pub fn refused_lines(meta: &Value) -> String {
    let mut reasons: Vec<(String, Vec<&str>)> = Vec::new();
    for (m, why) in meta["refused"].as_object().into_iter().flatten() {
        let line = why.as_str().unwrap_or("");
        // `<validator>: <metric>: <why>` reads as `<validator>: <why>` under its metrics.
        let why = match line.split_once(": ") {
            Some((id, rest)) => format!("{id}: {}", rest.strip_prefix(&format!("{m}: ")).unwrap_or(rest)),
            None => line.to_string(),
        };
        match reasons.iter_mut().find(|r| r.0 == why) {
            Some(r) => r.1.push(m),
            None => reasons.push((why, vec![m])),
        }
    }
    reasons.iter().map(|(why, ms)| format!("refused  {}: {why}\n", ms.join(", "))).collect()
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
