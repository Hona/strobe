//! Native memory by what holds it (SPEC §8.6): per process of a target's tree, the host memory it
//! allocated, GPU memory mapped into it, shared sections, mapped files and images. A reading walks
//! each process's address space, which holds the process's memory lock for a moment: strobe takes
//! readings between runs, never inside one.

use crate::host::{self, MemoryMap, ProcTimes};
use serde::Serialize;
use serde_json::{Value, json};

const MB: f64 = 1024.0 * 1024.0;

/// What each part is, in `query memory`'s order.
const PARTS: [(&str, &str); 6] = [
    ("host", "memory.app.host: what the processes allocated in their own address space (heap, stacks, anonymous memory)"),
    ("gpu_mapped", "GPU memory mapped for the CPU (write-combined or uncached regions; Linux: GPU device mappings)"),
    ("unmapped", "commit charged with no private mapping in the processes (page tables, commit a driver holds for them)"),
    ("shared", "shared memory sections"),
    ("files", "mapped data files"),
    ("image", "executables and libraries"),
];

/// One reading of a tree.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Reading {
    pub at_ns: u64,
    /// Where in the recording it was taken ("after the runs", "after run 3").
    pub label: String,
    pub processes: Vec<MemoryMap>,
    /// Processes that could not be read: totals leave them out, so they give no metric.
    pub unreadable: Vec<u32>,
}

impl Reading {
    pub fn take(procs: &[ProcTimes], label: impl Into<String>) -> Reading {
        let mut r = Reading { at_ns: host::now_ns(), label: label.into(), ..Reading::default() };
        for p in procs {
            match host::memory_map(p.pid) {
                Some(m) => r.processes.push(MemoryMap { name: p.name.clone(), ..m }),
                None => r.unreadable.push(p.pid),
            }
        }
        r
    }

    /// Every process of the tree was read.
    pub fn complete(&self) -> bool {
        !self.processes.is_empty() && self.unreadable.is_empty()
    }

    pub fn host(&self) -> u64 {
        self.processes.iter().map(|m| m.host).sum()
    }

    /// `memory.app.host` and `memory.app.private` in MB, from a complete reading only.
    pub fn values(&self) -> Vec<(&'static str, f64)> {
        if !self.complete() {
            return Vec::new();
        }
        vec![("memory.app.host", self.host() as f64 / MB), ("memory.app.private", self.processes.iter().map(|m| m.private).sum::<u64>() as f64 / MB)]
    }
}

/// Why a reading has nothing, for the view: the OS gives no regions (macOS), or the processes could
/// not be opened.
pub fn why_empty() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS gives another process's memory regions only to a debugger (its task port); memory.app.private is the physical footprint, which includes the process's GPU allocations"
    } else {
        "strobe could not open the target's processes (another user's, or protected)"
    }
}

/// A part's total over a stored reading's processes; None when no process has it (`unmapped` off Windows).
fn part(r: &Value, key: &str) -> Option<f64> {
    let ps = r["processes"].as_array()?;
    ps.iter().any(|p| p[key].is_number()).then(|| ps.iter().filter_map(|p| p[key].as_f64()).sum())
}

/// Totals by part of a stored reading, for `a..b memory`.
pub fn totals(r: &Value) -> Vec<(String, f64)> {
    std::iter::once("private").chain(PARTS.iter().map(|p| p.0)).chain(["working_set"]).filter_map(|k| Some((k.to_string(), part(r, k)?))).collect()
}

/// `query <rec> memory` of a native recording: one stored reading by process (default) or by part,
/// with the GPU memory the sampler read (`gpu.vram`, `gpu.shared`) beside it.
pub fn view(r: &Value, label: &str, by: &str, top: usize, values: &Value) -> (String, Value) {
    let b = crate::fmt::bytes;
    let mut text = format!("memory by {by} · {label}{}\n", r["label"].as_str().map(|l| format!(" · {l}")).unwrap_or_default());
    let procs = r["processes"].as_array().cloned().unwrap_or_default();
    if procs.is_empty() {
        text += &format!("no reading: {}\n", why_empty());
        return (text, json!({ "reading": r }));
    }
    let windows = procs.iter().any(|p| p["unmapped"].is_number());
    if by == "layer" {
        let private = part(r, "private").unwrap_or(f64::NAN);
        text += &format!("  {:>10}  private: memory.app.private ({})\n", b(private), if windows { "the commit charge" } else { "private resident and swapped pages" });
        let mut rows: Vec<(&str, &str, f64)> = PARTS.iter().filter_map(|(k, about)| Some((*k, *about, part(r, k)?))).collect();
        rows.sort_by(|a, b| b.2.total_cmp(&a.2));
        for (_, about, v) in rows.iter().take(top) {
            text += &format!("  {:>10}  {about}\n", b(*v));
        }
        text += &format!("  {:>10}  working set ({} private)\n", b(part(r, "working_set").unwrap_or(f64::NAN)), b(part(r, "private_working_set").unwrap_or(f64::NAN)));
    } else {
        text += &format!(
            "  {:<18} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}  working set (private)\n",
            "process", "pid", "private", "host", "gpu mapped", "unmapped", "shared", "files", "image"
        );
        for p in procs.iter().take(top) {
            let g = |k: &str| p[k].as_f64().map(b).unwrap_or_else(|| "—".into());
            text += &format!(
                "  {:<18} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}  {} ({})\n",
                p["name"].as_str().unwrap_or("?"),
                p["pid"].as_u64().unwrap_or(0),
                g("private"),
                g("host"),
                g("gpu_mapped"),
                g("unmapped"),
                g("shared"),
                g("files"),
                g("image"),
                g("working_set"),
                g("private_working_set")
            );
        }
        if procs.len() > top {
            text += &format!("  … {} more (--top)\n", procs.len() - top);
        }
    }
    if let Some(u) = r["unreadable"].as_array().filter(|u| !u.is_empty()) {
        text += &format!("unreadable: pids {} (left out of every total, so no memory.app.host)\n", u.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
    }
    let gpu = |k: &str| values[k].as_f64().map(|mb| b(mb * MB));
    if let (Some(v), Some(s)) = (gpu("gpu.vram"), gpu("gpu.shared")) {
        text += &format!("gpu       {v} dedicated (gpu.vram) · {s} in system memory (gpu.shared): the GPU allocations themselves, from the GPU's statistics\n");
    }
    text += if windows {
        "private is the commit charge (memory.app.private): it includes the commit Windows charges for the processes' GPU allocations, mapped into them (gpu mapped: a GPU driver maps VRAM write-combined) or not (unmapped), which is no host memory; host is committed private regions without write-combined or uncached ones (memory.app.host); shared, files and image are committed views, not resident memory\n"
    } else {
        "private is private resident and swapped pages (memory.app.private), GPU device mappings included; host is anonymous private pages (memory.app.host); shared, files and image are resident pages\n"
    };
    (text, json!({ "reading": r, "label": label, "by": by }))
}
