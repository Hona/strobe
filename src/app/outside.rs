//! `strobe record pid:<n>|proc:<name>` (SPEC §13.6): any running process, from the outside. Timing:
//! what the OS put on the screen for its tree (present events), its CPU (cycle counters) and GPU.
//! `--probes deep`: native stacks from the kernel's sampled profile (attribution, through the helper).

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::etw::{self, frames};
use crate::host;
use crate::model::{Error, Result};
use crate::probe;
use crate::store;
use crate::validity;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// `pid:<n>`, or `proc:<name>`: the one process of that name whose parent is not of that name.
pub fn resolve(target: &str) -> Result<u32> {
    if let Some(p) = target.strip_prefix("pid:") {
        let pid: u32 = p.parse().map_err(|_| Error::usage(format!("not a pid: {p}")))?;
        if !host::all_processes().iter().any(|x| x.pid == pid) {
            return Err(Error::usage(format!("no process {pid}")));
        }
        return Ok(pid);
    }
    let name = target.strip_prefix("proc:").unwrap_or(target).to_ascii_lowercase();
    let all = host::all_processes();
    let is = |n: &str| {
        let n = n.to_ascii_lowercase();
        n == name || n.strip_suffix(".exe") == Some(name.as_str())
    };
    let matching: Vec<&host::ProcInfo> = all.iter().filter(|p| is(&p.name)).collect();
    let roots: Vec<&&host::ProcInfo> = matching.iter().filter(|p| !matching.iter().any(|q| q.pid == p.parent)).collect();
    match roots.as_slice() {
        [one] => Ok(one.pid),
        [] => Err(Error::usage(format!("no process named {name}"))),
        many => Err(Error::usage(format!("{} processes named {name}: {}", many.len(), many.iter().map(|p| format!("pid:{}", p.pid)).collect::<Vec<_>>().join(", ")))),
    }
}

pub fn record(args: &Args, target: &str) -> Result<Output> {
    let root = resolve(target)?;
    let name = host::all_processes().into_iter().find(|p| p.pid == root).map(|p| p.name).unwrap_or_default();
    let deep = match args.get("probes").unwrap_or("timing") {
        "timing" => false,
        "deep" | "attribution" => true,
        other => return Err(Error::usage(format!("--probes for a process: timing or deep, not {other}"))),
    };
    let for_ms = match args.get("for") {
        Some(d) => duration_ms(d)?,
        None => 10_000,
    };
    // The lock screen covers every window: nothing reaches the display and presents stop.
    if host::session_locked() {
        return Err(validity::locked());
    }
    let awake = host::Awake::start();
    let id = args.get("id").filter(|i| i.starts_with("rec_")).map(str::to_string).unwrap_or_else(|| store::new_id("rec"));
    let dir = store::recording_dir(&id)?;
    let locked = |awake: &host::Awake| awake.locks() > 0 || host::session_locked();
    if deep {
        let cap = etw::StackCapture::start(root)?;
        progress(format!("sampling native stacks of {name} ({root}) for {}", fmt::ms_duration(for_ms)));
        std::thread::sleep(Duration::from_millis(for_ms));
        let d = cap.stop()?;
        crate::symbol::write_gz(&dir.join("deep.json.gz"), &d)?;
        progress("symbolizing (PDBs: local, then symbol servers)");
        let native = crate::symbol::native(&dir, None)?;
        let facts = native.as_ref().map(crate::symbol::summary).unwrap_or(Value::Null);
        let top = super::query::native_hotspots(&dir, "function", 8, None).map(|o| o.text).unwrap_or_default();
        let meta = json!({
            "id": id, "kind": "record", "target": target, "pid": root, "process": name, "probes": "deep", "token": "", "duration_ms": for_ms,
            "env": host::env(), "values": {}, "native": facts, "samples": d.samples.len(), "events_lost": d.events_lost, "pids": d.pids,
            "timing": "perturbed", "created": super::record::now_iso(), "valid": true,
        });
        store::write_json(&dir.join("meta.json"), &meta)?;
        let text = format!(
            "{id} · {target} ({name}) · {} · deep\n{top}next     strobe query {id} hotspots --by package · strobe query {id} hotspots --thread <name> · strobe report recording {id} --open\n",
            fmt::ms_duration(for_ms)
        );
        return Ok(Output::new(text, meta));
    }
    let presents = etw::PresentCapture::start();
    let via = presents.as_ref().map(|p| p.via()).unwrap_or("");
    let sampler = probe::os::Sampler::start(root);
    let cycles = || host::process_tree(root).iter().map(|p| p.cycles).sum::<u64>();
    let (c0, tsc0, t0) = (cycles(), host::tsc(), host::now_ns());
    progress(format!("recording {name} ({root}) for {}", fmt::ms_duration(for_ms)));
    std::thread::sleep(Duration::from_millis(for_ms));
    let (c1, tsc1, t1) = (cycles(), host::tsc(), host::now_ns());
    let os = sampler.stop().window(t0, t1);
    let presents = presents.and_then(|p| p.stop());
    if locked(&awake) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(validity::locked());
    }
    let mut values: BTreeMap<String, f64> = BTreeMap::new();
    if let (Some(a), Some(b)) = (tsc0, tsc1)
        && b > a
    {
        values.insert("cpu.app.total".into(), 100.0 * c1.saturating_sub(c0) as f64 / (b - a) as f64);
    }
    if let Some(p) = os.gpu_3d_pct {
        values.insert("gpu.process.3d".into(), p);
    }
    if let Some(v) = os.vram_dedicated_mb {
        values.insert("gpu.vram".into(), v);
    }
    let mut session = Value::Null;
    let refused = match &presents {
        Ok(p) => {
            let pids = host::tree_pids(root);
            let run = frames::from_presents(&p.presents, &pids, t0, t1);
            values.extend(frames::values(&[run], frames::refresh_ms(&p.vsyncs, t0, t1)));
            crate::symbol::write_gz(&dir.join("presents.json.gz"), p)?;
            session = json!({ "via": via, "events": p.events, "events_lost": p.events_lost, "consumer_cpu_ms": p.consumer_cpu_ms, "presents": p.presents.len(), "pids": pids });
            None
        }
        Err(e) => Some(e.message.clone()),
    };
    let meta = json!({
        "id": id, "kind": "record", "target": target, "pid": root, "process": name, "probes": "timing", "token": "", "duration_ms": for_ms,
        "env": host::env(), "values": values, "os": os, "presents": session, "presents_refused": refused,
        "display_settings_hz": host::refresh_at(0, 0), "timing": "clean", "created": super::record::now_iso(), "valid": true,
    });
    store::write_json(&dir.join("meta.json"), &meta)?;
    Ok(Output::new(text(&meta), meta))
}

pub fn text(meta: &Value) -> String {
    let v = &meta["values"];
    let get = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let id = meta["id"].as_str().unwrap_or("");
    let mut s = format!("{id} · {} ({}) · {} · timing\n", meta["target"].as_str().unwrap_or(""), meta["process"].as_str().unwrap_or(""), fmt::ms_duration(meta["duration_ms"].as_u64().unwrap_or(0)));
    match meta["presents_refused"].as_str() {
        Some(why) => s += &format!("frames   refused: {why}\n"),
        None => {
            s += &format!(
                "frames   {} displayed · {} dropped per run ({} of frames) · {} discarded · display {}\n",
                fmt::value(get("frames.n"), "count"),
                fmt::value(get("frames.dropped"), "count"),
                fmt::value(get("frames.dropped.pct"), "%"),
                fmt::value(get("frames.discarded"), "count"),
                if get("display.hz").is_finite() { format!("{:.1} Hz", get("display.hz")) } else { "?".into() },
            );
            s += &format!(
                "interval p50 {} · p95 {} · max {}   (gap between frames on screen while drawing)\n",
                fmt::value(get("frames.interval.p50"), "ms"),
                fmt::value(get("frames.interval.p95"), "ms"),
                fmt::value(get("frames.interval.max"), "ms"),
            );
            let p = &meta["presents"];
            let cpu = p["consumer_cpu_ms"].as_f64();
            let dur = meta["duration_ms"].as_f64().unwrap_or(1.0).max(1.0);
            s += &format!(
                "presents {} events · {} lost · {} · consumer {}\n",
                p["events"],
                p["events_lost"],
                p["via"].as_str().unwrap_or(""),
                cpu.map(|c| format!("{c:.1} ms CPU ({:.3}% of a core)", 100.0 * c / dur)).unwrap_or_else(|| "?".into())
            );
        }
    }
    if get("cpu.app.total").is_finite() {
        s += &format!("cpu      {} of one core, process tree", fmt::value(get("cpu.app.total"), "%"));
        if get("gpu.process.3d").is_finite() {
            s += &format!(" · gpu 3D {}", fmt::value(get("gpu.process.3d"), "%"));
        }
        s += "\n";
    }
    s += &format!("next     strobe query {id} summary · strobe record {} --probes deep (native stacks)\n", meta["target"].as_str().unwrap_or(""));
    s
}
