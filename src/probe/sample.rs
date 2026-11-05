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

use crate::probe::stacks::{Deep, Module};
use crate::model::{Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::process::{Child, Command, Stdio};

pub const INTERVAL_US: f64 = 1000.0;
/// The longest a capture samples, whatever happens to strobe: a block's runs end long before.
const MAX_S: u32 = 600;
/// How long `sample` may take to write its report once asked to stop.
const REPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a capture waits for `sample` to read the app's symbols and begin.
const START_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

pub struct Capture {
    runs: Vec<(u32, Child, std::path::PathBuf)>,
    t0: u64,
}

impl Capture {
    pub fn start(root: u32) -> Result<Capture> {
        use std::os::unix::process::CommandExt;
        let pids = crate::host::tree_pids(root);
        if pids.is_empty() {
            return Err(Error::msg(format!("process {root} is not running")));
        }
        let mut runs = Vec::new();
        for pid in pids.into_iter().take(16) {
            let out = crate::host::paths::run().join(format!("sample-{}-{pid}.txt", std::process::id()));
            let said = std::fs::File::create(said_path(&out))?;
            let mut cmd = Command::new("/usr/bin/sample");
            cmd.args([&pid.to_string(), &MAX_S.to_string(), "1", "-mayDie", "-fullPaths", "-file"]).arg(&out).stdin(Stdio::null()).stdout(said.try_clone()?).stderr(said);
            // `sample` stops and writes its report on SIGINT, but a process started from a background
            // job (a remote shell, `nohup`, a daemon) inherits SIGINT ignored, and then `sample` ran for
            // its whole duration: give it the default disposition and an empty signal mask.
            unsafe {
                cmd.pre_exec(|| {
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                    Ok(())
                });
            }
            let child = cmd.spawn().map_err(|e| Error::msg(format!("start /usr/bin/sample: {e}")))?;
            runs.push((pid, child, out));
        }
        // `sample` first reads the app's symbols (`-mayDie`; seconds for a large binary) and takes no
        // SIGINT until it samples: a block whose runs ended sooner stopped it too early, and it then
        // sampled its whole duration. The capture begins once each one samples.
        let deadline = std::time::Instant::now() + START_WAIT;
        for (_, child, out) in &mut runs {
            while !sampling(out) && child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        Ok(Capture { runs, t0: crate::host::now_ns() })
    }

    pub fn stop(mut self) -> Result<Deep> {
        let t1 = crate::host::now_ns();
        let mut deep = Deep { interval_us: INTERVAL_US, t0_ns: self.t0, t1_ns: t1, untimed: true, ..Deep::default() };
        let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
        let deadline = std::time::Instant::now() + REPORT_WAIT;
        for (_, child, out) in &mut self.runs {
            // One still reading symbols would drop the signal: wait until it samples.
            while !sampling(out) && child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        }
        let mut late = 0;
        for (pid, mut child, out) in std::mem::take(&mut self.runs) {
            let _ = std::fs::remove_file(said_path(&out));
            if !wait_until(&mut child, deadline) {
                // It never wrote its report: what it sampled is lost, but the block goes on.
                let _ = child.kill();
                let _ = child.wait();
                late += 1;
            }
            let Ok(text) = std::fs::read_to_string(&out) else { continue };
            let _ = std::fs::remove_file(&out);
            parse(&text, pid, self.t0, &mut deep, &mut index);
            deep.pids.push(pid);
        }
        if deep.samples.is_empty() && deep.dropped.is_empty() {
            if late > 0 {
                return Err(Error::msg(format!("/usr/bin/sample did not write its report within {} s of being stopped, so it was ended", REPORT_WAIT.as_secs())));
            }
            return Err(Error::msg("sample recorded nothing (the process may be hardened or a system process, which only Apple's tools may sample)"));
        }
        Ok(deep)
    }
}

/// A capture dropped without `stop` (a failed block, strobe stopping): its samplers end with it.
impl Drop for Capture {
    fn drop(&mut self) {
        for (_, child, out) in &mut self.runs {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&*out);
            let _ = std::fs::remove_file(said_path(out));
        }
    }
}

/// Where a `sample` prints its progress, beside its report.
fn said_path(out: &std::path::Path) -> std::path::PathBuf {
    out.with_extension("said")
}

/// Whether the `sample` writing `out` has begun sampling (it says `Sampling process …`).
fn sampling(out: &std::path::Path) -> bool {
    std::fs::read_to_string(said_path(out)).is_ok_and(|s| s.contains("Sampling process"))
}

/// Whether `child` exited by `deadline`.
fn wait_until(child: &mut Child, deadline: std::time::Instant) -> bool {
    loop {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
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
