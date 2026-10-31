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
    if browser.is_none() {
        problems.push("no Chromium found (install Chrome or run bunx playwright install chromium)".into());
    }
    if bun.is_none() {
        problems.push("no Bun (bench files need it: https://bun.sh)".into());
    }
    if !engine::installed() {
        problems.push("trace_processor not downloaded yet (strobe up, or on first use)".into());
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
        "valid": problems.is_empty(), "problems": problems,
    });
    let mark = |ok: bool| if ok { "✓" } else { "!" };
    let mut t = String::new();
    t += &format!("  {} os         {}\n", mark(true), env.os);
    t += &format!("  {} cpu        {} · {} logical{}\n", mark(true), env.cpu, env.logical_cpus, if groups.is_empty() { String::new() } else { format!(" · {}", groups.join(", ")) });
    t += &format!("  {} power      {}{}\n", mark(!env.on_battery), env.power_plan, if env.on_battery { " · on battery" } else { "" });
    t += &format!("  {} load       {:.0}% of all CPUs busy\n", mark(busy <= 0.10), busy * 100.0);
    t += &format!("  {} browser    {}\n", mark(browser.is_some()), browser.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()));
    t += &format!("  {} bun        {}\n", mark(bun.is_some()), bun.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()));
    t += &format!("  {} engine     trace_processor {}{}\n", mark(engine::installed()), engine::VERSION, if engine::installed() { "" } else { " (not downloaded)" });
    t += &format!(
        "  {} calibrate  {}\n",
        mark(!calibration.is_null()),
        calibration.get("speed_ms").and_then(Value::as_f64).map(|s| format!("speed check {s:.1} ms · {}", calibration["at"].as_str().unwrap_or(""))).unwrap_or_else(|| "never".into())
    );
    t += &format!("    data       {} recordings, {} experiments shown · {}\n", recordings.len(), experiments.len(), super::fmt::bytes(disk as f64));
    for (id, _) in &experiments {
        t += &format!("               {id}\n");
    }
    Ok(Output::new(t, json))
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
