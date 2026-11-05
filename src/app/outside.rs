//! `strobe record pid:<n>|proc:<name>` (SPEC §13.6): any running process, from the outside. Timing:
//! what the OS put on the screen for its tree (present events), its CPU (cycle counters), its threads'
//! wakes and CPU, IO, GPU, and what holds its memory: the views a launched native app gets.
//! `--probes deep`: native stacks from the kernel's sampled profile (attribution, through the helper).

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::etw::{self, frames};
use crate::host;
use crate::model::{Error, Result};
use crate::probe::{self, wakes};
use crate::store;
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
        "deep" | "attribution" | "stacks-unprivileged" => true,
        other => return Err(Error::usage(format!("--probes for a process: timing, deep or stacks-unprivileged, not {other}"))),
    };
    let for_ms = match args.get("for") {
        Some(d) => duration_ms(d)?,
        None => 10_000,
    };
    let awake = host::Awake::start();
    let id = args.get("id").filter(|i| i.starts_with("rec_")).map(str::to_string).unwrap_or_else(|| store::new_id("rec"));
    let dir = store::recording_dir(&id)?;
    let draft = store::Draft::new(&dir);
    // The lock screen covers every window: nothing reaches the display and presents stop. Stacks,
    // CPU and GPU work need no display, so only frames are refused then (SPEC §10.5).
    let locked = |awake: &host::Awake| awake.locks() > 0 || host::session_locked();
    if deep {
        let cap = etw::StackCapture::start_with(root, args.get("probes") == Some("stacks-unprivileged"))?;
        progress(format!("sampling native stacks of {name} ({root}) for {}", fmt::ms_duration(for_ms)));
        std::thread::sleep(Duration::from_millis(for_ms));
        let d = cap.stop()?;
        crate::symbol::write_gz(&dir.join("deep.json.gz"), &d)?;
        progress("symbolizing (PDBs: local, then symbol servers)");
        let native = crate::symbol::native(&dir, None, None)?;
        let facts = native.as_ref().map(crate::symbol::summary).unwrap_or(Value::Null);
        let top = super::query::native_hotspots(&dir, "function", 8, None, None).map(|o| o.text).unwrap_or_default();
        let meta = json!({
            "id": id, "kind": "record", "target": target, "pid": root, "process": name, "probes": "deep", "token": "", "duration_ms": for_ms,
            "env": host::env(), "values": {}, "native": facts, "samples": d.samples.len(), "events_lost": d.events_lost, "pids": d.pids,
            "timing": "perturbed", "created": super::record::now_iso(), "valid": true, "suspend": d.suspend,
        });
        store::write_json(&dir.join("meta.json"), &meta)?;
        draft.keep();
        let text = format!(
            "{id} · {target} ({name}) · {} · deep\n{top}next     strobe query {id} hotspots --by package · strobe query {id} hotspots --thread <name> · strobe report recording {id} --open\n",
            fmt::ms_duration(for_ms)
        );
        return Ok(Output::new(text, meta));
    }
    let presents = etw::PresentCapture::start().map(|mut p| {
        p.track(&host::tree_pids(root));
        p
    });
    let via = presents.as_ref().map(|p| p.via()).unwrap_or("");
    let sampler = probe::os::Sampler::start(root);
    let cycles = || host::process_tree(root).iter().map(|p| p.cycles).sum::<u64>();
    // The tree's threads around the window: wakes and CPU per thread, as for a launched native app.
    let snap0 = wakes::Snapshot::take(&host::tree_pids(root));
    let timeline = wakes::Timeline::start(move || host::tree_pids(root));
    let procs0 = (host::now_ns(), host::all_processes());
    let (c0, tsc0, t0) = (cycles(), host::tsc(), host::now_ns());
    progress(format!("recording {name} ({root}) for {}", fmt::ms_duration(for_ms)));
    std::thread::sleep(Duration::from_millis(for_ms));
    let (c1, tsc1, t1) = (cycles(), host::tsc(), host::now_ns());
    let screen = super::bench::screen_state(&procs0, &(host::now_ns(), host::all_processes()));
    let mut timeline = timeline.stop();
    timeline.marks.push(("recording".into(), t0, t1));
    crate::symbol::write_gz(&dir.join("timeline.json.gz"), &timeline)?;
    let tree = host::process_tree(root);
    let pids: Vec<u32> = tree.iter().map(|p| p.pid).collect();
    let span = wakes::span(&snap0, &wakes::Snapshot::take(&pids));
    let os = sampler.stop().window(t0, t1);
    let presents = presents.and_then(|p| p.stop());
    let session_locked = locked(&awake);
    let ms = (t1 - t0) as f64 / 1e6;
    let mut values: BTreeMap<String, f64> = BTreeMap::new();
    if let (Some(a), Some(b)) = (tsc0, tsc1)
        && b > a
    {
        values.insert("cpu.app.total".into(), 100.0 * c1.saturating_sub(c0) as f64 / (b - a) as f64);
        values.insert("cpu.work".into(), c1.saturating_sub(c0) as f64 / (b - a) as f64 * ms);
    }
    if !span.threads.is_empty() {
        values.insert("cpu.wakes".into(), span.wakes_per_s());
        values.insert("io.read".into(), span.io_read as f64);
        values.insert("io.write".into(), span.io_write as f64);
    }
    // The UI thread: the thread that owns the tree's main window, when it has one.
    let ui_tid = host::main_window(&pids).and_then(|w| host::window_thread(w.hwnd));
    if let Some(t) = ui_tid.and_then(|tid| span.threads.iter().find(|t| t.tid == tid)) {
        if let Some(c) = t.cpu_ms {
            values.insert("ui.cpu".into(), c);
        }
        values.insert("ui.wakes".into(), t.switches as f64 / (ms / 1e3));
    }
    let names: BTreeMap<u32, String> = span.threads.iter().filter_map(|t| Some((t.tid, host::thread_name(t.tid)?))).collect();
    // The busiest threads by CPU, for the summary: per-thread CPU needs no permission on any OS.
    let mut busy: Vec<&wakes::ThreadSpan> = span.threads.iter().filter(|t| t.tid != 0 && t.cpu_ms.is_some_and(|c| c > 0.0)).collect();
    busy.sort_by(|a, b| b.cpu_ms.unwrap_or(0.0).total_cmp(&a.cpu_ms.unwrap_or(0.0)));
    let threads_top: Vec<Value> = busy.iter().take(6).map(|t| json!({ "tid": t.tid, "pid": t.pid, "name": names.get(&t.tid), "cpu_ms": t.cpu_ms })).collect();
    let spans = std::slice::from_ref(&span);
    store::write_json(&dir.join("threads.json"), &json!(wakes::by_thread(spans, &names, &wakes::modules(spans))))?;
    store::write_json(&dir.join("runs.json"), &json!([{ "run": "run:0", "ms": ms, "switches": span.switches, "wakes_per_s": span.wakes_per_s(), "io_read": span.io_read, "io_write": span.io_write }]))?;
    // What holds the tree's memory, read after the window.
    values.insert("memory.app.private".into(), tree.iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0));
    let reading = probe::memory::Reading::take(&tree, "after the recording");
    values.extend(reading.values().into_iter().map(|(k, v)| (k.to_string(), v)));
    store::write_json(&dir.join("memory.json"), &json!({ "readings": [&reading] }))?;
    if let Some(p) = os.gpu_3d_pct {
        values.insert("gpu.process.3d".into(), p);
    }
    if let Some(v) = os.vram_dedicated_mb {
        values.insert("gpu.vram".into(), v);
    }
    if let Some(v) = os.vram_shared_mb {
        values.insert("gpu.shared".into(), v);
    }
    // Presents far faster than the display refreshes, locked or not (`UnpacedSpin`).
    // The display the process shows on: its main window's, else any window's (an app that shows only
    // an overlay, such as a dictation HUD, has no main window).
    let display_of = |pids: &[u32]| host::main_window(pids).map(|w| w.hwnd).or_else(|| host::top_windows(pids).first().map(|w| w.0)).and_then(host::display_of);
    let unpaced = presents.as_ref().ok().and_then(|p| {
        let pids = host::tree_pids(root);
        let display = display_of(&pids);
        let hz = frames::refresh_ms(&p.vsyncs, t0, t1, display.as_ref()).map(|ms| 1000.0 / ms).or(display.map(|d| d.hz));
        frames::spin(&p.presents, &pids, &[(t0, t1)], hz)
    });
    let mut session = Value::Null;
    let refused = match &presents {
        _ if session_locked => Some("SessionLocked: the session was locked while strobe recorded, so nothing reached the display".to_string()),
        Ok(p) => {
            let pids = host::tree_pids(root);
            let display = display_of(&pids);
            let f = frames::native_values(p, &pids, &[(t0, t1)], display.as_ref());
            match f.refused {
                Some(why) if display.is_none() && why.contains("several displays") => {
                    Some("NoPresentSource: several displays run at different refresh rates and the process has no window to say which it is on".to_string())
                }
                Some(why) => Some(why),
                None => {
                    values.extend(f.values);
                    crate::symbol::write_gz(&dir.join("presents.json.gz"), p)?;
                    session = json!({ "via": via, "events": p.events, "events_lost": p.events_lost, "consumer_cpu_ms": p.consumer_cpu_ms, "presents": p.presents.len(), "pids": pids, "display": display, "refused_metrics": f.partial });
                    None
                }
            }
        }
        Err(e) => Some(e.message.clone()),
    };
    let meta = json!({
        "id": id, "kind": "record", "target": target, "pid": root, "process": name, "probes": "timing", "token": "", "duration_ms": for_ms,
        "env": host::env(), "values": values, "os": os, "presents": session, "presents_refused": refused, "session_locked": session_locked, "ui_thread": ui_tid,
        "display_settings_hz": host::refresh_at(0, 0), "timing": "clean", "created": super::record::now_iso(), "valid": true, "screen": screen,
        "unpaced": unpaced.map(|(fps, hz)| json!({ "fps": fps, "hz": hz })), "threads_top": threads_top,
    });
    store::write_json(&dir.join("meta.json"), &meta)?;
    draft.keep();
    Ok(Output::new(super::record::summary_text(&meta), meta))
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
    if meta["session_locked"] == true {
        s += "session  locked: CPU and GPU numbers compare only with other locked recordings\n";
    }
    if get("cpu.app.total").is_finite() {
        s += &format!("cpu      {} of one core · {} in the window, process tree\n", fmt::value(get("cpu.app.total"), "%"), fmt::value(get("cpu.work"), "ms"));
    }
    if get("cpu.wakes").is_finite() {
        s += &format!("wakes    {} context switches of its threads\n", fmt::value(get("cpu.wakes"), "/s"));
        s += &format!("io       read {} · written {}\n", fmt::bytes(get("io.read")), fmt::bytes(get("io.write")));
    }
    if get("ui.cpu").is_finite() {
        s += &format!("ui       {} CPU on the UI thread (the main window's) · {} wakes\n", fmt::value(get("ui.cpu"), "ms"), fmt::value(get("ui.wakes"), "/s"));
    }
    for (i, t) in meta["threads_top"].as_array().into_iter().flatten().enumerate() {
        s += &format!(
            "{}{} {}  {}\n",
            if i == 0 { "threads  " } else { "         " },
            fmt::pad(&fmt::value(t["cpu_ms"].as_f64().unwrap_or(f64::NAN), "ms"), 10),
            t["name"].as_str().map(str::to_string).unwrap_or_else(|| format!("thread {}", t["tid"])),
            if t["pid"] != meta["pid"] { format!("(pid {})", t["pid"]) } else { String::new() }
        );
    }
    s += &super::record::memory_lines(v);
    s += &format!(
        "next     strobe query {id} threads · strobe query {id} memory · strobe query {id} gpu · strobe record {} --probes deep (native stacks)\n",
        meta["target"].as_str().unwrap_or("")
    );
    s
}
