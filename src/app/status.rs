//! `strobe status`: environment, validity preflight, attachable targets (SPEC §5.4).

use super::Output;
use crate::cli::Args;
use crate::engine;
use crate::host;
use crate::model::Result;
use crate::runner;
use crate::store;
use crate::target::chromium;
use serde_json::{Value, json};

pub fn run(args: &Args) -> Result<Output> {
    let env = host::env();
    let topo = host::topology();
    let busy = host::system_busy(if args.has("fast") { 300 } else { 3000 });
    let browser = chromium::find();
    let bun = runner::find_bun();
    let state: Value = std::fs::read_to_string(host::paths::state_dir().join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let recordings = store::list("rec", 5);
    let experiments = store::list("exp", 5);
    let disk: u64 = dir_size(&host::paths::recordings()) + dir_size(&host::paths::experiments());
    let vcache = topo.vcache().map(|g| format!("{} MB L3 on {} CPUs", g.l3_mb, g.cpus));
    let groups: Vec<String> = topo.groups.iter().enumerate().map(|(i, g)| format!("CCD{i} {} MB L3 ({} CPUs)", g.l3_mb, g.cpus)).collect();
    let mut problems: Vec<String> = Vec::new();
    if busy > 0.10 {
        problems.push(format!("background load {:.0}% of all CPUs", busy * 100.0));
    }
    if env.on_battery {
        problems.push("on battery".into());
    }
    let qos = host::background_qos(None);
    if let Some(why) = &qos {
        problems.push(format!("strobe runs at {why}: what it launches inherits it, and frame and input-latency metrics are refused (BackgroundPriority)"));
    }
    let locked = host::session_locked();
    if locked {
        problems.push("the session is locked (benches wait for an unlock)".into());
    }
    if browser.is_none() {
        problems.push("no Chromium found (install Chrome or run bunx playwright install chromium)".into());
    }
    if bun.is_none() {
        problems.push("no Bun (bench files need it: https://bun.sh)".into());
    }
    if !engine::installed() {
        problems.push("trace_processor not downloaded yet (strobe up, or on first use)".into());
    }
    // Benches on this device and what each does: running a block, queued for the slot, waiting for an
    // unlock, building.
    let benches = super::bench::slot::list();
    let daemon = super::mcp::running();
    let skill = super::up::skill_state();
    let deep = crate::etw::access();
    #[cfg(windows)]
    let deep_text = super::helper::describe(&deep);
    #[cfg(target_os = "linux")]
    let deep_text = match crate::probe::perf::access() {
        (true, true) => "native stacks: perf events, kernel frames included".to_string(),
        (true, false) => "native stacks: perf events, user frames (kernel frames need perf_event_paranoid 1 or CAP_PERFMON)".to_string(),
        _ => format!("native stacks off: {}", crate::probe::perf::refusal().unwrap_or_default()),
    };
    #[cfg(target_os = "macos")]
    let deep_text = "native stacks: /usr/bin/sample (every thread's stack each 1 ms, untimed)".to_string();
    if !deep.presents() && cfg!(windows) {
        problems.push("no OS present events (native frame metrics are refused): strobe up".into());
    }
    let (native_ok, native_text) = native_drive();
    // A headless Linux machine has no app to drive: only a Mac without its permissions is a problem.
    if !native_ok && cfg!(target_os = "macos") {
        problems.push(format!("native apps: {native_text}"));
    }
    let calibration = if args.has("calibrate") { super::up::calibrate(true)? } else { state.get("last_calibration").cloned().unwrap_or(Value::Null) };
    if calibration.is_null() {
        problems.push("no calibration yet (strobe up)".into());
    }
    let json = json!({
        "env": env, "topology": { "groups": groups, "vcache": vcache }, "busy": busy,
        "browser": browser, "bun": bun, "engine": { "installed": engine::installed(), "version": engine::VERSION, "path": engine::path() },
        "calibration": calibration, "recordings": recordings.iter().map(|r| &r.0).collect::<Vec<_>>(),
        "experiments": experiments.iter().map(|r| &r.0).collect::<Vec<_>>(), "disk_bytes": disk,
        "session_locked": locked, "background_priority": qos, "valid": problems.is_empty(), "problems": problems,
        "daemon": { "running": daemon, "port": super::mcp::port() }, "skill": skill, "benches": benches,
        "deep": { "access": deep, "presents": deep.presents(), "detail": deep_text },
        "native": { "ready": native_ok, "detail": native_text },
    });
    let mark = |ok: bool| if ok { "✓" } else { "!" };
    let mut t = String::new();
    t += &format!("  {} os         {}\n", mark(true), env.os);
    t += &format!("  {} cpu        {} · {} logical{}\n", mark(true), env.cpu, env.logical_cpus, if groups.is_empty() { String::new() } else { format!(" · {}", groups.join(", ")) });
    t += &format!("  {} power      {}{}\n", mark(!env.on_battery), env.power_plan, if env.on_battery { " · on battery" } else { "" });
    t += &format!("  {} load       {:.0}% of all CPUs busy\n", mark(busy <= 0.10), busy * 100.0);
    t += &format!("  {} priority   {}\n", mark(qos.is_none()), qos.as_ref().map(|w| format!("{w}: frame and input-latency metrics are refused (run strobe at normal priority)")).unwrap_or_else(|| "normal".into()));
    t += &format!("  {} screen     {}\n", mark(!locked), if locked { "locked: benches wait for an unlock" } else { "unlocked" });
    t += &format!("  {} browser    {}\n", mark(browser.is_some()), browser.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()));
    t += &format!("  {} bun        {}\n", mark(bun.is_some()), bun.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()));
    t += &format!("  {} deep       {}\n", mark(deep.presents() && deep.helper), deep_text);
    t += &format!("  {} native     {}\n", mark(native_ok), native_text);
    t += &format!("  {} engine     trace_processor {}{}\n", mark(engine::installed()), engine::VERSION, if engine::installed() { "" } else { " (not downloaded)" });
    t += &format!(
        "  {} calibrate  {}\n",
        mark(!calibration.is_null()),
        calibration.get("speed_ms").and_then(Value::as_f64).map(|s| format!("speed check {s:.1} ms · {}", calibration["at"].as_str().unwrap_or(""))).unwrap_or_else(|| "never".into())
    );
    t += &format!(
        "  {} daemon     {}\n",
        mark(daemon),
        if daemon { format!("running on 127.0.0.1:{} (MCP)", super::mcp::port()) } else { "not running: strobe up starts it (MCP calls need it)".into() }
    );
    t += &format!(
        "  {} skill      {}\n",
        mark(skill == "current"),
        match skill {
            "current" => "installed, this build's",
            "customised" => "customised by hand: strobe up leaves it as is",
            "outdated" => "from another build: strobe up refreshes it",
            _ => "not installed: strobe up installs it",
        }
    );
    if benches.is_empty() {
        t += "    benches    none running\n";
    }
    for (i, b) in benches.iter().enumerate() {
        let file = b["file"].as_str().unwrap_or("").trim_start_matches(r"\\?\");
        let file = std::path::Path::new(file).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        let why = b["why"].as_str().filter(|w| !w.is_empty()).map(|w| format!(": {w}")).unwrap_or_default();
        t += &format!(
            "    {}  {} {file} (pid {}) · {} since {}{why}\n",
            if i == 0 { "benches  " } else { "         " },
            b["exp"].as_str().unwrap_or(""),
            b["pid"],
            b["state"].as_str().unwrap_or(""),
            b["since"].as_str().unwrap_or("")
        );
    }
    t += &format!("    data       {} recordings, {} experiments shown · {}\n", recordings.len(), experiments.len(), super::fmt::bytes(disk as f64));
    for (id, _) in &experiments {
        t += &format!("               {id}\n");
    }
    Ok(Output::new(t, json))
}

/// What drives native apps here and what shows their frames, and whether both work (SPEC §13.7).
fn native_drive() -> (bool, String) {
    #[cfg(windows)]
    {
        let p = crate::etw::access();
        (true, format!("UI Automation, posted window messages · frames: {}", if p.presents() { "OS present events" } else { "none (strobe up)" }))
    }
    #[cfg(target_os = "macos")]
    {
        let app = host::macos::responsible_app();
        let own = host::macos::own_exe();
        let (ax, rec) = (host::macos::accessibility_trusted(), host::macos::screen_capture_allowed());
        // Whose grant each permission is: one strobe inherits from the app or daemon it runs under,
        // or strobe's own. macOS 27 passes Device Control on to a daemon's child, not Screen Recording.
        let whose = if app == own { "strobe's own grant" } else { "inherited" };
        let mut grant = Vec::new();
        if !ax {
            grant.push(format!("accessibility to {app}: {}", host::macos::accessibility_pane()));
        }
        if !rec {
            grant.push(format!("Screen Recording to {}: {}", host::macos::screen_recording_grantee(), host::macos::screen_recording_pane()));
        }
        let split = host::macos::tcc_split().is_some();
        let text = format!(
            "accessibility API, Quartz events to the app · accessibility {} ({whose}: {app}) · frames: ScreenCaptureKit, Screen Recording {}{}{}",
            if ax { "granted" } else { "missing" },
            if rec { "granted" } else { "missing" },
            if split { format!(" (not inherited: macOS checks it against strobe itself, {own})") } else { String::new() },
            if grant.is_empty() { String::new() } else { format!(" (grant {})", grant.join("; ")) }
        );
        (ax && rec, text)
    }
    #[cfg(target_os = "linux")]
    {
        let bus = host::dbus::Conn::session().ok().and_then(|mut s| s.call("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", &[]).ok()).is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        let frames = match crate::etw::PresentCapture::start() {
            Ok(_) => "X Present".to_string(),
            Err(e) => format!("none ({})", e.message),
        };
        let text = format!(
            "AT-SPI {} · XTest {} · frames: {frames}",
            if bus { "(accessibility bus found)" } else { "missing (no accessibility bus: at-spi2-core)" },
            if x11 { "on the X11 display" } else { "missing (no X11 display; Wayland windows take none)" }
        );
        (bus && x11, text)
    }
}

fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() { dir_size(&path) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }
        })
        .sum()
}
