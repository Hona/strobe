//! Native CPU samples on macOS (attribution blocks only, SPEC rule 3) through `/usr/bin/sample`, the
//! one sampler macOS lets a user run on their own processes without root or an entitlement (reading
//! another task's threads needs a task port, which only Apple-signed tools get). One `sample` per
//! process of the tree, every 1 ms, stopped with SIGINT; its call graph becomes `Deep` stacks.
//!
//! What it is, honestly: `sample` suspends the process each millisecond and walks every thread's user
//! stack (frame pointers, always kept on arm64). So a sample is a thread's stack at that moment, not
//! a thread that was running: samples whose innermost frame is a system call trap (a thread waiting,
//! or working in the kernel, which `sample` cannot see into) are left out of the stacks and counted
//! in `dropped`; a thread preempted while runnable in user code still counts. Samples carry no time
//! (`untimed`), and processes started after the capture began are not sampled.

use crate::etw::{Deep, Module};
use crate::model::{Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::process::{Child, Command, Stdio};

pub const INTERVAL_US: f64 = 1000.0;

pub struct Capture {
    runs: Vec<(u32, Child, std::path::PathBuf)>,
    t0: u64,
}

impl Capture {
    pub fn start(root: u32) -> Result<Capture> {
        let pids = crate::host::tree_pids(root);
        if pids.is_empty() {
            return Err(Error::msg(format!("process {root} is not running")));
        }
        let mut runs = Vec::new();
        for pid in pids.into_iter().take(16) {
            let out = crate::host::paths::run().join(format!("sample-{}-{pid}.txt", std::process::id()));
            let child = Command::new("/usr/bin/sample")
                .args([&pid.to_string(), "3600", "1", "-mayDie", "-fullPaths", "-file"])
                .arg(&out)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| Error::msg(format!("start /usr/bin/sample: {e}")))?;
            runs.push((pid, child, out));
        }
        Ok(Capture { runs, t0: crate::host::now_ns() })
    }

    pub fn stop(self) -> Result<Deep> {
        let t1 = crate::host::now_ns();
        let mut deep = Deep { interval_us: INTERVAL_US, t0_ns: self.t0, t1_ns: t1, untimed: true, ..Deep::default() };
        let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
        for (pid, mut child, out) in self.runs {
            unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
            let _ = child.wait();
            let Ok(text) = std::fs::read_to_string(&out) else { continue };
            let _ = std::fs::remove_file(&out);
            parse(&text, pid, self.t0, &mut deep, &mut index);
            deep.pids.push(pid);
        }
        if deep.samples.is_empty() && deep.dropped.is_empty() {
            return Err(Error::msg("sample recorded nothing (the process may be hardened or a system process, which only Apple's tools may sample)"));
        }
        Ok(deep)
    }
}

/// One call-graph line: depth (its count's column), count, function, module, address, file and line.
struct Line {
    depth: usize,
    count: u64,
    name: String,
    module: String,
    addr: Option<u64>,
    file: String,
    line: u32,
}

fn line(l: &str) -> Option<Line> {
    let start = l.find(|c: char| c.is_ascii_digit())?;
    if l[..start].chars().any(|c| !matches!(c, ' ' | '+' | '!' | ':' | '|')) {
        return None;
    }
    let rest = &l[start..];
    let (count, rest) = rest.split_once(' ')?;
    let count: u64 = count.parse().ok()?;
    let rest = rest.trim_start();
    if rest.starts_with("Thread_") {
        return Some(Line { depth: start, count, name: rest.split_whitespace().next()?.to_string(), module: String::new(), addr: None, file: String::new(), line: 0 });
    }
    let (name, after) = rest.split_once("  (in ")?;
    let (module, after) = after.split_once(')')?;
    let addr = after.split_once('[').and_then(|(_, a)| a.split([',', ']']).next()).and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok());
    let loc = after.rsplit(']').next().unwrap_or("").trim();
    let (file, ln) = loc.rsplit_once(':').map(|(f, n)| (f.to_string(), n.parse().unwrap_or(0))).unwrap_or_default();
    Some(Line { depth: start, count, name: name.trim().to_string(), module: module.to_string(), addr, file, line: ln })
}

/// A leaf in the kernel's system call library: the thread was waiting, or working inside the kernel.
fn in_syscall(module: &str) -> bool {
    module == "libsystem_kernel.dylib"
}

fn parse(text: &str, pid: u32, t0: u64, deep: &mut Deep, index: &mut HashMap<Vec<u64>, u32>) {
    let graph = text.split("Call graph:").nth(1).and_then(|g| g.split("\n\n").next()).unwrap_or("");
    let lines: Vec<Line> = graph.lines().filter_map(line).collect();
    let mut tid = 0u32;
    // The path from the thread root: (depth, address, count, module).
    let mut path: Vec<(usize, u64, u64, String)> = Vec::new();
    let mut named: BTreeMap<u64, (String, String, u32)> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        if l.addr.is_none() {
            // A thread: `Thread_282959` (its id, as Activity Monitor numbers threads).
            tid = l.name.trim_start_matches("Thread_").trim_end_matches(':').parse::<u64>().map(|t| t as u32).unwrap_or(0);
            path.clear();
            continue;
        }
        while path.last().is_some_and(|p| p.0 >= l.depth) {
            path.pop();
        }
        let addr = l.addr.unwrap_or(0);
        named.entry(addr).or_insert_with(|| (l.name.clone(), l.file.clone(), l.line));
        path.push((l.depth, addr, l.count, l.module.clone()));
        // Self count: this frame's samples minus its children's.
        let (mut children, mut child_depth) = (0u64, None);
        for c in &lines[i + 1..] {
            if c.addr.is_none() || c.depth <= l.depth {
                break;
            }
            if c.depth == *child_depth.get_or_insert(c.depth) {
                children += c.count;
            }
        }
        let own = l.count.saturating_sub(children);
        if own == 0 {
            continue;
        }
        if in_syscall(&l.module) {
            *deep.dropped.entry("thread waiting or in the kernel (system call)".into()).or_default() += own;
            continue;
        }
        let frames: Vec<u64> = path.iter().rev().map(|p| p.1).collect();
        let next = deep.stacks.len() as u32;
        let at = *index.entry(frames.clone()).or_insert_with(|| {
            deep.stacks.push(frames);
            next
        });
        for _ in 0..own {
            deep.samples.push((t0, pid, tid, 0, at));
        }
    }
    for (addr, (name, file, ln)) in named {
        deep.located.push((pid, addr, name, file, ln));
    }
    // Binary images: `0x100eb4000 - 0x100efb4bf +hotloop (0) <UUID> /path`; `sample` hides paths under
    // a home folder (`/Users/*/…`), so the process's own mapping names the file.
    if let Some(images) = text.split("Binary Images:").nth(1) {
        for l in images.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (Some(a), Some(b)) = (f.first().and_then(|x| u64::from_str_radix(x.trim_start_matches("0x"), 16).ok()), f.get(2).and_then(|x| u64::from_str_radix(x.trim_start_matches("0x"), 16).ok())) else { continue };
            let shown = l.rsplit("> ").next().unwrap_or("").trim().to_string();
            let path = if shown.contains('*') { region_file(pid, a).unwrap_or(shown) } else { shown };
            deep.modules.push(Module { pid, base: a, size: b.saturating_sub(a) + 1, path });
        }
    }
}

/// The file mapped at `addr` in `pid` (libproc; this user's processes).
fn region_file(pid: u32, addr: u64) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::proc_regionfilename(pid as i32, addr, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).to_string())
}
