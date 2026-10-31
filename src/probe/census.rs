//! A census: what memory the target holds at one moment, by process and layer (SPEC §8.6).

use crate::host;
use crate::target::chromium::{Browser, Page};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
