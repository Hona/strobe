//! A census: what memory the target holds at one moment, by process and layer (SPEC §8.6).

use super::trace;
use crate::engine;
use crate::host;
use crate::model::{Error, Result};
use crate::target::chromium::{Browser, Page};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// The page's live JS heap (after a forced GC, so garbage does not count) and DOM counters, and every
/// target process's OS memory with its Chromium process type. Take it before a heap snapshot of the
/// same process: serializing a snapshot raises that process's memory.
pub fn take(browser: &Browser, page: &Page) -> Value {
    let _ = page.send("HeapProfiler.enable", json!({}));
    let _ = page.send("HeapProfiler.collectGarbage", json!({}));
    let heap = page.send("Runtime.getHeapUsage", json!({})).unwrap_or(Value::Null);
    let dom = page.send("Memory.getDOMCounters", json!({})).unwrap_or(Value::Null);
    let types: BTreeMap<u64, String> = browser
        .cdp
        .send("SystemInfo.getProcessInfo", json!({}), None)
        .ok()
        .and_then(|r| r["processInfo"].as_array().map(|a| a.iter().filter_map(|p| Some((p["id"].as_u64()?, p["type"].as_str()?.to_string()))).collect()))
        .unwrap_or_default();
    let processes: Vec<Value> = browser
        .processes()
        .iter()
        .map(|p| json!({ "pid": p.pid, "name": p.name, "type": types.get(&(p.pid as u64)).map_or("other", String::as_str), "private_bytes": p.private_bytes, "working_set_bytes": p.working_set }))
        .collect();
    json!({
        "at_ns": host::now_ns(),
        "js_heap_bytes": heap["usedSize"], "js_heap_total_bytes": heap["totalSize"],
        "dom": { "documents": dom["documents"], "nodes": dom["nodes"], "listeners": dom["jsEventListeners"] },
        "processes": processes,
    })
}

/// Chromium's own accounting by allocator (the census's "Chromium allocators" layer): a detailed
/// memory-infra dump of every process, through a short trace of its own written to `path`. Sizes are
/// effective sizes, with the dump's ownership edges resolved, so allocators that overlap (cc, skia and
/// gpu inside malloc, discardable or shared memory) never count the same bytes twice. Per pid:
/// `layers` (top-level allocators) and `allocators` (their children, largest first).
pub fn allocators(browser: &Browser, path: &Path) -> Result<Value> {
    let tracing = trace::start_categories(&browser.cdp, &["disabled-by-default-memory-infra"], 64_000)?;
    let dumped = browser.cdp.send_timeout("Tracing.requestMemoryDump", json!({ "deterministic": true, "levelOfDetail": "detailed" }), None, Duration::from_secs(60));
    tracing.stop(path)?;
    if !dumped?["success"].as_bool().unwrap_or(false) {
        return Err(Error::msg("Chromium did not complete the memory dump"));
    }
    let table = engine::query(
        path,
        "select p.pid, n.path, n.size, n.effective_size
           from memory_snapshot_node n
           join process_memory_snapshot s on n.process_snapshot_id = s.id
           join process p using(upid)
          where p.pid is not null",
    )?;
    /// Per pid: top-level allocators (layers) by effective size, and their children as (path, size, effective).
    type Dump = (BTreeMap<String, f64>, Vec<(String, f64, f64)>);
    let mut by_pid: BTreeMap<String, Dump> = BTreeMap::new();
    for r in &table.rows {
        let cell = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let (pid, node) = (cell(0), cell(1));
        let (size, effective) = (cell(2).parse::<f64>().unwrap_or(0.0), cell(3).parse::<f64>().unwrap_or(0.0));
        let entry = by_pid.entry(pid).or_default();
        match node.matches('/').count() {
            0 => {
                entry.0.insert(node, effective);
            }
            1 => entry.1.push((node, size, effective)),
            _ => {}
        }
    }
    let out: serde_json::Map<String, Value> = by_pid
        .into_iter()
        .map(|(pid, (layers, mut children))| {
            children.sort_by(|a, b| b.2.total_cmp(&a.2));
            children.truncate(40);
            let children: Vec<Value> = children.into_iter().map(|(p, s, e)| json!({ "path": p, "size": s, "effective": e })).collect();
            (pid, json!({ "layers": layers, "allocators": children }))
        })
        .collect();
    Ok(Value::Object(out))
}
