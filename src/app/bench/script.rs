//! Script targets (SPEC §9.6): a variant's Bun or Node module, loaded by strobe's host in a fresh
//! process per block and called from the bench file (`ctx.call(name, ...args)`). Runs are timed on the
//! process's own CPU (`cpu.work`) and wall clock; attribution blocks add the runtime's CPU and
//! allocation profiles (`--cpu-prof`, `--heap-prof`).

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::ChildStdin;
use std::sync::mpsc::Receiver;

/// The host process and its line protocol.
struct Host {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next: u64,
}

impl Host {
    fn spawn(target: &Value, root: &Path, cpus: Option<u64>, profiles: Option<&Path>) -> Result<Host> {
        let entry = root.join(target["entry"].as_str().ok_or_else(|| Error::usage("a script target needs `entry`, the module whose exports the cases call"))?);
        if !entry.is_file() {
            return Err(Error::usage(format!("script target entry {} does not exist in this variant", entry.display())));
        }
        let runtime = target["runtime"].as_str().unwrap_or("bun");
        let exe = match runtime {
            "bun" => crate::runner::find_bun().ok_or_else(|| Error::msg("script targets on Bun need Bun").hint("install it from https://bun.sh"))?,
            "node" => host::find_exe("node", &[]).ok_or_else(|| Error::msg("script targets on Node need Node on PATH"))?,
            other => return Err(Error::usage(format!("script runtime: bun or node, not {other}"))),
        };
        let mut cmd = Command::new(exe);
        if let Some(dir) = profiles {
            // The whole process's profiles, written on a normal exit; 200 µs samples as for pages.
            cmd.args(["--cpu-prof", &format!("--cpu-prof-interval={}", probe::cpu::INTERVAL_US), "--heap-prof"])
                .arg(format!("--cpu-prof-dir={}", dir.display()))
                .arg(format!("--heap-prof-dir={}", dir.display()));
        }
        cmd.arg(crate::runner::sdk_dir()?.join("host.mjs")).arg(&entry).current_dir(root);
        for (k, v) in target["env"].as_object().into_iter().flatten() {
            cmd.env(k, v.as_str().unwrap_or(""));
        }
        let previous = cpus.and_then(host::set_affinity);
        let spawned = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        if previous.is_some() {
            host::set_affinity(previous.unwrap_or(0));
        }
        let mut child = spawned.map_err(|e| Error::msg(format!("start the process host: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| Error::msg("host stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::msg("host stdout"))?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let host = Host { child, stdin, lines, next: 1 };
        loop {
            let line = host.lines.recv_timeout(Duration::from_secs(60)).map_err(|_| Error::msg(format!("the process host did not load {}", entry.display())))?;
            if serde_json::from_str::<Value>(&line).ok().is_some_and(|v| v["ready"] == true) {
                return Ok(host);
            }
        }
    }

    fn request(&mut self, message: Value, timeout: Duration) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let mut m = message;
        m["id"] = json!(id);
        writeln!(self.stdin, "{m}").map_err(|e| Error::msg(format!("process host: {e}")))?;
        self.stdin.flush().ok();
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).map_err(|_| Error { code: 124, message: format!("the process host did not answer in {} s", timeout.as_secs()), hint: None })?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["id"] == json!(id) {
                if let Some(e) = v["error"].as_str() {
                    return Err(Error::msg(e.to_string()));
                }
                return Ok(v["ok"].clone());
            }
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A normal exit (profiles are written), then the tree, whatever is left of it.
    fn exit(mut self) {
        let _ = self.request(json!({ "op": "exit" }), Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        electron::kill_tree(self.child.id());
    }
}

/// The host process tree's CPU so far: cycles with the time stamp counter, and clock-tick time (ns).
fn cpu(pid: u32) -> (u64, Option<u64>, u64) {
    let tree = host::process_tree(pid);
    (tree.iter().map(|p| p.cycles).sum(), host::tsc(), tree.iter().map(|p| p.cpu_ns).sum())
}

/// CPU ms between two readings over `wall_ms`. Cycles are exact; process times move in 15.6 ms ticks
/// on Windows, which made a 2 ms call read 0 ms or 15.6 ms. Ticks only where cycles are unavailable.
fn cpu_ms(a: (u64, Option<u64>, u64), b: (u64, Option<u64>, u64), wall_ms: f64) -> f64 {
    match (a.1, b.1) {
        (Some(t0), Some(t1)) if t1 > t0 && b.0 > 0 => b.0.saturating_sub(a.0) as f64 / (t1 - t0) as f64 * wall_ms,
        _ => b.2.saturating_sub(a.2) as f64 / 1e6,
    }
}

pub(super) fn block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    let target = ctx.describe["targets"][&s.target].clone();
    let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a script target runs in a variant's folder (a directory or a git ref)", v.role)))?;
    let id = store::new_id("rec");
    let profiles = (set == Set::Attribution).then(|| host::paths::run().join(format!("{id}-prof")));
    if let Some(p) = &profiles {
        std::fs::create_dir_all(p)?;
    }
    let mut steps: Vec<(String, f64)> = Vec::new();
    let launched = Instant::now();
    let host = RefCell::new(Host::spawn(&target, &root, ctx.pin, profiles.as_deref())?);
    steps.push(("launch".into(), launched.elapsed().as_secs_f64() * 1e3));
    let pid = host.borrow().pid();
    let counts: RefCell<BTreeMap<String, f64>> = RefCell::new(BTreeMap::new());
    let counting = std::cell::Cell::new(false);
    let serve = |op: &str, a: &Value| -> Result<Value> {
        match op {
            "call" => host.borrow_mut().request(json!({ "name": a["name"], "args": a["args"] }), Duration::from_secs(600)),
            "count" => {
                if counting.get() {
                    *counts.borrow_mut().entry(a["name"].as_str().unwrap_or("").to_string()).or_default() += a["n"].as_f64().unwrap_or(1.0);
                }
                Ok(Value::Bool(true))
            }
            "phase" => Ok(Value::Bool(true)),
            "driver" => Err(Error::msg("a script target has no page: call its exports with ctx.call(name, ...args)")),
            other => Err(Error::msg(format!("unknown runner request {other}"))),
        }
    };
    let t = Duration::from_secs(600);
    let result = (|| -> Result<Block> {
        let root_text = root.display().to_string();
        ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": root_text }), t, &serve)?;
        let mut lapped = Instant::now();
        ctx.runner.call("setup", json!({}), t, &serve).map_err(|e| Error::refused("RunIncomplete", format!("setup: {}", e.message), "fix the bench file's setup"))?;
        steps.push(("setup".into(), lapped.elapsed().as_secs_f64() * 1e3));
        lapped = Instant::now();
        let measured_runs = ctx.runs;
        let mut runs: Vec<(f64, f64, BTreeMap<String, f64>)> = Vec::new();
        let mut first: Option<usize> = None;
        while first.is_none_or(|f| runs.len() < f + measured_runs) {
            counts.borrow_mut().clear();
            counting.set(true);
            let (c0, t0) = (cpu(pid), Instant::now());
            ctx.runner.call("run", json!({ "case": s.case }), t, &serve).map_err(|e| Error::refused("RunIncomplete", format!("{} run: {}", s.case, e.message), "the run threw or timed out"))?;
            let wall = t0.elapsed().as_secs_f64() * 1e3;
            let used = cpu_ms(c0, cpu(pid), wall);
            counting.set(false);
            runs.push((wall, used, counts.borrow().clone()));
            if s.reset {
                ctx.runner.call("reset", json!({ "case": s.case }), t, &serve).map_err(|e| Error::refused("RunIncomplete", format!("{} reset: {}", s.case, e.message), "fix the case's reset"))?;
            }
            let i = runs.len() - 1;
            if ctx.check {
                first = Some(0);
            } else if first.is_none() {
                // Steady once two runs cost the same CPU, within 5% or 1 ms, as for pages (SPEC §9.2).
                let steady = i >= 1 && {
                    let (a, b) = (runs[i - 1].1, runs[i].1);
                    (a - b).abs() <= 1.0 || (a.max(b) > 0.0 && (a - b).abs() / a.max(b) <= 0.05)
                };
                if steady || i >= MAX_WARMUP {
                    first = Some(if steady && set != Set::Attribution { i - 1 } else { i + 1 });
                }
            }
        }
        let first = first.unwrap_or(0);
        steps.push(("runs".into(), lapped.elapsed().as_secs_f64() * 1e3));
        let measured = &runs[first..];
        let key = |j: usize| format!("run:{}", first + j);
        let mut extra: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        extra.insert("run.duration".into(), measured.iter().enumerate().map(|(j, r)| (key(j), r.0)).collect());
        extra.insert("cpu.work".into(), measured.iter().enumerate().map(|(j, r)| (key(j), r.1)).collect());
        extra.insert("cpu.app.total".into(), measured.iter().enumerate().filter(|(_, r)| r.0 > 0.0).map(|(j, r)| (key(j), 100.0 * r.1 / r.0)).collect());
        for name in s.metrics.iter().chain(std::iter::once(&s.primary)).filter(|m| m.starts_with("count.")) {
            let counter = name.trim_start_matches("count.");
            extra.insert(name.clone(), measured.iter().enumerate().map(|(j, r)| (key(j), r.2.get(counter).copied().unwrap_or(0.0))).collect());
        }
        let mut values = metrics::block_values(&metrics::BlockData::default(), "run", &extra);
        let private_mb = host::process_tree(pid).iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
        if private_mb > 0.0 {
            values.insert("memory.app.private".into(), private_mb);
        }
        let _ = ctx.runner.call("close", json!({}), t, &serve);
        Ok(Block {
            variant: v.role.to_string(),
            pair,
            values,
            run_ms: measured.iter().map(|r| r.0).collect(),
            warmup: first,
            speed_ratio: None,
            nodes: Vec::new(),
            recording: None,
            phases: BTreeMap::new(),
            frames: Vec::new(),
            intervals: Vec::new(),
            steps: steps.clone(),
            background: Vec::new(),
            leak: None,
        })
    })();
    host.into_inner().exit();
    let mut block = result?;
    if let Some(dir) = profiles.filter(|_| keep || set == Set::Attribution) {
        // The runtime names its own files: the newest of each kind is this process's.
        let newest = |ext: &str| {
            std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == ext))
                .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        };
        let rec = store::recording_dir(&id)?;
        if let Some(p) = newest("cpuprofile") {
            std::fs::copy(&p, rec.join("cpu-0.cpuprofile"))?;
        }
        if let Some(p) = newest("heapprofile") {
            std::fs::copy(&p, rec.join("alloc-0.heapprofile"))?;
        }
        let _ = std::fs::remove_dir_all(&dir);
        store::write_json(
            &rec.join("meta.json"),
            &json!({
                "id": id, "kind": "block", "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label,
                "target": target["entry"], "probes": set, "runs": block.run_ms.len(), "values": block.values,
                "created": super::super::record::now_iso(), "valid": true, "env": host::env(), "script": true,
                "note": "script profiles cover the whole host process: its start, setup, warmup and measured runs",
            }),
        )?;
        block.recording = Some(id);
    }
    Ok(block)
}
