//! `strobe query <rec> waits` (SPEC §11.1): where the target's threads were off their CPUs, and who
//! woke them, from the scheduler's events a native capture kept beside its samples (attribution only).

use super::{Output, fmt};
use crate::model::{Error, Result};
use crate::probe::stacks::{Deep, WaitKind, Waits};
use crate::probe::symbols::Frame;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// OS modules, the kernel, unknown code and the language runtime: a wait is reported at the first
/// frame outside them (the code that asked to wait), its stack kept whole.
fn system(package: &str) -> bool {
    package.starts_with('(') || package.ends_with(" (windows)") || package.ends_with(" (system)") || package.ends_with(" (macos)") || matches!(package, "std" | "core" | "alloc")
}

/// One row: what waited there, for how long, and who woke it.
#[derive(Default)]
struct Row {
    ms: f64,
    n: u64,
    max: f64,
    blocked: f64,
    runnable: f64,
    /// Waker label → (wake-ups, ms of the waits they ended).
    wakers: BTreeMap<String, (u64, f64)>,
    /// (pid, stack) → ms: the heaviest full stack.
    stacks: HashMap<(u32, u32), f64>,
    frame: Option<Frame>,
}

pub fn waits(dir: &Path, by: &str, top: usize, thread: Option<&str>, phase: Option<&str>) -> Result<Output> {
    let deep = crate::symbol::read_deep(dir)?.ok_or_else(|| {
        Error::usage("this recording has no native capture: waits come with native stacks (record pid:<n> --probes deep, a native app's record --probes attribution, or a bench attribution block of a native target)")
    })?;
    let w: Waits = match (deep.waits, deep.waits_refused) {
        (Some(w), _) => w,
        (None, Some(r)) => return Err(r.into()),
        (None, None) => return Err(crate::probe::stacks::no_waits("this recording's native capture predates off-CPU waits", "record it again with this strobe").into()),
    };
    let spans = phase.map(|p| super::phase_spans(dir, p)).transpose()?;
    // Every stack a wait or a waker holds, symbolized as native samples are (cached apart).
    let mut all = Deep { modules: w.modules.clone(), stacks: w.stacks.clone(), ..Deep::default() };
    for x in &w.waits {
        all.samples.extend(x.stack.map(|s| (x.from_ns, x.pid, x.tid, 0, s)));
        all.samples.extend(x.waker.as_ref().and_then(|k| Some((x.from_ns, k.pid, k.tid, 0, k.stack?))));
    }
    let resolved = crate::symbol::resolve_cached(dir, &all, "waits-symbols.json.gz")?;
    let mut cache: HashMap<(u32, u32), Vec<Frame>> = HashMap::new();
    let mut frames_of = |pid: u32, stack: u32| -> Vec<Frame> {
        cache
            .entry((pid, stack))
            .or_insert_with(|| {
                let mut out: Vec<Frame> = Vec::new();
                for (i, &a) in w.stacks.get(stack as usize).into_iter().flatten().enumerate() {
                    for (name, file, line, package, ok) in resolved.get(&crate::symbol::addr_key(pid, a, i == 0)).into_iter().flatten() {
                        if !*ok && out.last().is_some_and(|f| f.name == *name && f.file.is_empty()) {
                            continue;
                        }
                        out.push(Frame { name: name.clone(), file: file.clone(), line: *line as i64, column: 0, url: String::new(), mapped: false, name_from: None, package: Some(package.clone()) });
                    }
                }
                out
            })
            .clone()
    };
    let named = |tid: u32| w.threads.get(&tid).cloned().unwrap_or_else(|| format!("thread {tid}"));
    let picked = |f: &[Frame]| f.iter().find(|x| !system(x.package.as_deref().unwrap_or("("))).or(f.first()).cloned();
    // The part of [a, b] inside the phase's spans (all of it without a phase).
    let clip = |a: u64, b: u64| -> f64 {
        match &spans {
            None => b.saturating_sub(a) as f64 / 1e6,
            Some(sp) => sp.iter().map(|&(s, e)| b.min(e).saturating_sub(a.max(s)) as f64 / 1e6).sum(),
        }
    };
    let mut rows: BTreeMap<(String, String), Row> = BTreeMap::new();
    let (mut total, mut blocked, mut runnable, mut n, mut woken, mut wakeable) = (0.0, 0.0, 0.0, 0u64, 0u64, 0u64);
    let mut threads = std::collections::BTreeSet::new();
    for x in &w.waits {
        if let Some(t) = thread
            && !named(x.tid).contains(t)
            && t != x.tid.to_string()
        {
            continue;
        }
        let ms = clip(x.from_ns, x.to_ns);
        if ms <= 0.0 {
            continue;
        }
        let ready = x.ready_ns.unwrap_or(x.to_ns).clamp(x.from_ns, x.to_ns);
        let (b, r) = if x.kind == WaitKind::Preempted { (0.0, ms) } else { (clip(x.from_ns, ready), clip(ready, x.to_ns)) };
        let frames = x.stack.map(|s| frames_of(x.pid, s)).unwrap_or_default();
        let frame = picked(&frames);
        let key = match (by, &frame) {
            ("package", Some(f)) => (f.package.clone().unwrap_or_default(), String::new()),
            ("file", Some(f)) => (if f.file.is_empty() { f.name.clone() } else { f.file.clone() }, String::new()),
            (_, Some(f)) => (f.name.clone(), f.location()),
            (_, None) => ("(no stack)".into(), String::new()),
        };
        let row = rows.entry(key).or_default();
        row.ms += ms;
        row.n += 1;
        row.max = row.max.max(ms);
        row.blocked += b;
        row.runnable += r;
        if row.frame.is_none() {
            row.frame = frame;
        }
        if let Some(s) = x.stack {
            *row.stacks.entry((x.pid, s)).or_default() += ms;
        }
        if x.kind != WaitKind::Preempted {
            wakeable += 1;
            if let Some(k) = &x.waker {
                woken += 1;
                let label = if k.interrupt {
                    "an interrupt (a timer or a device)".to_string()
                } else {
                    let at = k.stack.and_then(|s| picked(&frames_of(k.pid, s))).map(|f| format!(": {} {}", f.name, f.location())).unwrap_or_default();
                    format!("{}{at}", named(k.tid))
                };
                let e = row.wakers.entry(label).or_default();
                e.0 += 1;
                e.1 += ms;
            }
        }
        total += ms;
        blocked += b;
        runnable += r;
        n += 1;
        threads.insert(x.tid);
    }
    let secs = |ms: f64| if ms >= 1000.0 { format!("{:.2} s", ms / 1000.0) } else { fmt::value(ms, "ms") };
    let mut text = format!(
        "waits · {} thread{} · {n} waits · {} off CPU ({} waiting, {} runnable: preempted or woken and queued) · wakers known for {}{}\n",
        threads.len(),
        if threads.len() == 1 { "" } else { "s" },
        secs(total),
        secs(blocked),
        secs(runnable),
        if wakeable > 0 { format!("{:.0}% of waits", 100.0 * woken as f64 / wakeable as f64) } else { "no waits".into() },
        phase.map(|p| format!(" · phase {p}")).unwrap_or_default(),
    );
    text += &format!("  by {by} · where each thread waited: its innermost frame outside the OS and the language runtime\n");
    if let Some(why) = &w.no_wakers {
        text += &format!("! wakers: {why}\n");
    }
    if w.events_lost > 0 {
        text += &format!("! {} scheduler events were lost (a full buffer): some waits are missing\n", w.events_lost);
    }
    let mut sorted: Vec<((String, String), Row)> = rows.into_iter().collect();
    sorted.sort_by(|a, b| b.1.ms.total_cmp(&a.1.ms));
    let mut json_rows = Vec::new();
    for ((name, at), row) in sorted.into_iter().take(top) {
        let share = if total > 0.0 { row.ms / total } else { 0.0 };
        text += &format!("  {:>9}  {:>5.1}%  {:>5}×  max {:>9}  {}  {}\n", secs(row.ms), share * 100.0, row.n, secs(row.max), name, at);
        let mut wakers: Vec<(&String, &(u64, f64))> = row.wakers.iter().collect();
        wakers.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
        if !wakers.is_empty() {
            text += &format!("             woken by {}\n", wakers.iter().take(3).map(|(l, (c, _))| format!("{l} ({c}×)")).collect::<Vec<_>>().join(" · "));
        }
        let heavy = row.stacks.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(k, ms)| (*k, *ms));
        let stack = heavy.map(|((pid, s), _)| frames_of(pid, s)).unwrap_or_default();
        if stack.len() > 1 {
            text += "             stack (innermost first):\n";
            for f in stack.iter().take(12) {
                text += &format!("               {}  {}\n", f.name, f.location());
            }
        }
        json_rows.push(json!({
            "name": name, "where": at, "ms": row.ms, "share": share, "waits": row.n, "max_ms": row.max, "waiting_ms": row.blocked, "runnable_ms": row.runnable,
            "frame": row.frame.as_ref().map(Frame::json),
            "wakers": wakers.iter().map(|(l, (c, ms))| json!({ "waker": l, "wakeups": c, "ms": ms })).collect::<Vec<_>>(),
            "stack": stack.iter().map(Frame::json).collect::<Vec<_>>(),
        }));
    }
    let json = json!({ "by": by, "waits": n, "threads": threads.len(), "off_cpu_ms": total, "waiting_ms": blocked, "runnable_ms": runnable, "wakers_known": if wakeable > 0 { Value::from(woken as f64 / wakeable as f64) } else { Value::Null }, "events_lost": w.events_lost, "rows": json_rows });
    Ok(Output::new(text, json))
}
