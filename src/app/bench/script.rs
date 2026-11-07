//! Script targets (SPEC §9.6): a variant's Bun or Node module, loaded by strobe's host in a fresh
//! process per block and called from the bench file (`ctx.call(name, ...args)`). Runs are timed on the
//! process's own CPU (`cpu.work`) and wall clock; attribution blocks add the runtime's CPU and
//! allocation profiles (`--cpu-prof`, `--heap-prof`).

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::Receiver;

/// The host process and its line protocol, over a loopback connection.
struct Host {
    /// The host with its process group: dropped, the host and what it started end.
    child: host::spawn::Owned,
    conn: TcpStream,
    lines: Receiver<String>,
    next: u64,
}

/// The harness module for `root`. The entry resolves from the bench file, like everything else in it,
/// so both variants run the same harness; in a worktree strobe made for a git ref the file is placed
/// at the same path, so its relative imports resolve to that variant's code. A directory variant is
/// the user's own checkout: strobe never writes there, and the file must already exist.
fn place(bench_file: &Path, entry: &str, root: &Path) -> Result<PathBuf> {
    let dir = bench_file.parent().unwrap_or(Path::new("."));
    let source = std::fs::canonicalize(dir.join(entry)).map_err(|_| Error::usage(format!("script entry {entry}: no such file next to {}", bench_file.display())))?;
    // Relative to the repository, not the folder with strobe.json: a variant's folder is a whole checkout (a git
    // ref's worktree, or a directory variant), so a harness under games/x/perf is at games/x/perf there too.
    let repo = host::git::toplevel(dir);
    let project = std::fs::canonicalize(repo.unwrap_or_else(|| config::project_root(dir)))?;
    let rel = source.strip_prefix(&project).map_err(|_| Error::usage(format!("script entry {} is outside the project {}", source.display(), project.display())))?;
    let placed = root.join(rel);
    let owned = std::fs::canonicalize(root).ok().zip(std::fs::canonicalize(host::paths::sub("builds")).ok()).is_some_and(|(r, b)| r.starts_with(b));
    if owned {
        let body = std::fs::read(&source)?;
        if std::fs::read(&placed).ok().as_deref() != Some(body.as_slice()) {
            if let Some(p) = placed.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&placed, body)?;
        }
    }
    if !placed.is_file() {
        return Err(Error::usage(format!("the harness {} is missing in {}: commit it there, or compare against a git ref", rel.display(), root.display())));
    }
    Ok(placed)
}

impl Host {
    fn spawn(target: &Value, entry: &Path, root: &Path, cpus: Option<u64>, profiles: Option<&Path>, log: &Path) -> Result<Host> {
        let runtime = target["runtime"].as_str().unwrap_or("bun");
        let exe = match runtime {
            "bun" => crate::runner::find_bun().ok_or_else(|| Error::msg("script targets on Bun need Bun").hint("install it from https://bun.sh"))?,
            "node" => host::find_exe("node", &[]).ok_or_else(|| Error::msg("script targets on Node need Node on PATH"))?,
            other => return Err(Error::usage(format!("script runtime: bun or node, not {other}"))),
        };
        let mut cmd = Command::new(exe);
        // `js.heap.retained` collects garbage between runs: Bun has `Bun.gc`, Node needs the flag.
        if runtime == "node" {
            cmd.arg("--expose-gc");
        }
        if let Some(dir) = profiles {
            // The whole process's profiles, written on a normal exit; samples at the interval pages get.
            cmd.args(["--cpu-prof", &format!("--cpu-prof-interval={}", probe::cpu::interval_us()), "--heap-prof"])
                .arg(format!("--cpu-prof-dir={}", dir.display()))
                .arg(format!("--heap-prof-dir={}", dir.display()));
        }
        cmd.arg(crate::runner::sdk_dir()?.join("host.mjs")).arg(entry).current_dir(root);
        for (k, v) in target["env"].as_object().into_iter().flatten() {
            cmd.env(k, v.as_str().unwrap_or(""));
        }
        // Requests go over a loopback socket, never the host's stdio: the module and the children it
        // spawns own that (Bun's `$` hands stdin on, and stalled on a protocol pipe that never closes).
        // stdin is empty; stdout and stderr go to the experiment's script.log.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        cmd.env("STROBE_HOST_PORT", listener.local_addr()?.port().to_string());
        let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        let err = out.try_clone()?;
        cmd.stdin(Stdio::null()).stdout(out).stderr(err);
        let mut child = host::spawn_owned(&mut cmd, cpus, 0, None).map_err(|e| Error::msg(format!("start the script host: {e}")))?;
        let failed = |log: &Path| {
            let tail: Vec<String> = std::fs::read_to_string(log).unwrap_or_default().lines().rev().take(12).map(str::to_string).collect::<Vec<_>>().into_iter().rev().collect();
            Error::msg(format!("the script host did not load {}:\n{}", entry.display(), tail.join("\n")))
        };
        // The host connects once its runtime is up; a module that fails to import ends it first.
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let conn = loop {
            match listener.accept() {
                Ok((conn, _)) => break conn,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(Error::msg(format!("script host connection: {e}"))),
            }
            if Instant::now() > deadline || child.exited().is_some() {
                return Err(failed(log));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        conn.set_nonblocking(false)?;
        conn.set_nodelay(true)?;
        let reader = conn.try_clone()?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let host = Host { child, conn, lines, next: 1 };
        loop {
            let line = host.lines.recv_timeout(Duration::from_secs(60)).map_err(|_| failed(log))?;
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
        writeln!(self.conn, "{m}").map_err(|e| Error::msg(format!("script host: {e}")))?;
        self.conn.flush().ok();
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => Error::timeout(format!("the process host did not answer in {} s", timeout.as_secs())),
                std::sync::mpsc::RecvTimeoutError::Disconnected => match self.child.exited() {
                    Some(code) => Error::msg(format!("the process host exited (code {code})")),
                    None => Error::msg("the process host closed its connection"),
                },
            })?;
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
        self.child.pid()
    }

    /// A normal exit (profiles are written), then its group, whatever is left of it (the drop).
    fn exit(mut self) {
        let _ = self.request(json!({ "op": "exit" }), Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && self.child.exited().is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// CPU ms between two readings over `wall_ms`, from cycles (exact). Never from process times, which
/// move in 15.6 ms ticks on Windows (a 2 ms call read 0 ms or 15.6 ms): unknown (NaN) without cycles,
/// and where they are not time (`host::cycles_refused`) the block refuses CPU.
pub(super) fn cpu_ms(a: (u64, Option<u64>), b: (u64, Option<u64>), wall_ms: f64) -> f64 {
    match (a.1, b.1) {
        (Some(t0), Some(t1)) if t1 > t0 && b.0 > 0 => b.0.saturating_sub(a.0) as f64 / (t1 - t0) as f64 * wall_ms,
        _ => f64::NAN,
    }
}

/// The host process tree's cycles by process (`record::per_process`) and the cycle clock.
fn cycles(pid: u32) -> (super::super::record::Cycles, Option<u64>) {
    (super::super::record::per_process(&host::process_tree(pid)), host::tsc())
}

/// One run: wall ms, CPU ms (both without its untimed stretches), its counters, the heap it retained
/// (MB), its phase marks, what the OS counted for the host's processes, and its span.
type ScriptRun = (f64, f64, BTreeMap<String, f64>, f64, Vec<(String, u64)>, probe::wakes::Counted, metrics::Timed);

/// One host process for the cases of one launch: one case, or a chunk of them with `--check --batch`
/// (SPEC §5.6), each case's runs after the case before it.
pub(super) fn chunk(ctx: &mut Ctx, cases: &[Series], v: &Variant, pair: usize, x: &Exec) -> Result<Launch> {
    let s = &cases[0];
    let batch = ctx.batch.is_some();
    let (set, keep) = (x.set, x.keep);
    let target = ctx.describe["targets"][&s.target].clone();
    let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a script target runs in a variant's folder (a directory or a git ref)", v.role)))?;
    let id = store::new_id("rec");
    let profiles = (set == Set::Attribution).then(|| host::paths::run().join(format!("{id}-prof")));
    if let Some(p) = &profiles {
        std::fs::create_dir_all(p)?;
    }
    let mut steps: Vec<(String, f64)> = Vec::new();
    let launched = Instant::now();
    let entry = place(Path::new(ctx.describe["file"].as_str().unwrap_or(".")), target["entry"].as_str().unwrap_or(""), &root)?;
    let log = store::experiment_dir(&ctx.exp)?.join("script.log");
    let host = RefCell::new(Host::spawn(&target, &entry, &root, ctx.pin, profiles.as_deref(), &log)?);
    let heap_mb = || -> Result<f64> { Ok(host.borrow_mut().request(json!({ "op": "heap" }), Duration::from_secs(120))?.as_f64().unwrap_or(f64::NAN) / (1024.0 * 1024.0)) };
    steps.push(("launch".into(), launched.elapsed().as_secs_f64() * 1e3));
    let pid = host.borrow().pid();
    let hooks = super::block::Hooks::new(ctx, move || host::tree_pids(pid));
    let cost = SelfCost::new(ctx.runner.pid());
    // The host's CPU at each edge of the run's untimed stretches.
    let edges: RefCell<Vec<super::super::record::Cycles>> = RefCell::new(Vec::new());
    let serve = |op: &str, a: &Value| -> Result<Value> {
        match op {
            "call" => host.borrow_mut().request(json!({ "name": a["name"], "args": a["args"] }), Duration::from_secs(600)),
            "count" => {
                hooks.count(a);
                Ok(Value::Bool(true))
            }
            "phase" => hooks.phase(a),
            "untimed" => hooks.untimed(a, |start, _| {
                if start {
                    cost.pause();
                }
                edges.borrow_mut().push(cycles(pid).0);
                if !start {
                    cost.resume();
                }
                Ok(())
            }),
            "driver" => Err(Error::msg("a script target has no page: call its exports with ctx.call(name, ...args)")),
            "line" => Err(Error::usage("waitForLine reads a native app's output or log files; a script target's calls return what they return")),
            other => Err(Error::msg(format!("unknown runner request {other}"))),
        }
    };
    let t = Duration::from_secs(600);
    let result = (|| -> Result<Launch> {
        let root_text = root.display().to_string();
        ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": root_text }), t, &serve)?;
        let lapped = Instant::now();
        let setup = ctx.runner.call("setup", json!({}), t, &serve);
        super::batch::launch_hook_case(ctx, "the bench file's setup")?;
        setup.map_err(|e| crate::runner::incomplete("setup", "setup", e, "", "fix the bench file's setup"))?;
        hooks.check_fixture();
        steps.push(("setup".into(), lapped.elapsed().as_secs_f64() * 1e3));
        let perf = x.counters.then(probe::wakes::Perf::new);
        let mut launch = Launch::default();
        // The launch's runs so far: a case's runs are numbered after the ones before it.
        let mut base = 0usize;
        for (k, s) in cases.iter().enumerate() {
            if k > 0 && stop_requested() {
                break;
            }
            launch.log_at.push(ctx.runner.log_len());
            let lapped = Instant::now();
            // `js.heap.retained`: the post-GC heap a run leaves behind, read outside its timed span.
            let heap = std::iter::once(&s.primary).chain(&s.metrics).any(|m| m == "js.heap.retained");
            let r = (|| -> Result<Block> {
                let mut runs: Vec<ScriptRun> = Vec::new();
                let mut ends: Vec<Option<(f64, Option<u64>)>> = Vec::new();
                // Steady once two runs cost the same CPU (SPEC §9.2).
                let mut warmup = super::block::Warmup::new(s, x);
                while warmup.more(runs.len()) {
                    let h0 = if heap { heap_mb()? } else { f64::NAN };
                    hooks.begin(&format!("run:{}", base + runs.len()));
                    let listed = probe::wakes::wall_us();
                    let k0 = probe::wakes::Counters::take(listed, &host::tree_pids(pid), perf.as_ref());
                    edges.borrow_mut().clear();
                    let c0 = cycles(pid);
                    cost.start();
                    let t0 = host::now_ns();
                    ctx.runner.call("run", json!({ "case": s.case }), t, &serve).map_err(|e| crate::runner::incomplete(&format!("{} run", s.case), "run", e, "", "the run threw or timed out"))?;
                    let t1 = host::now_ns();
                    cost.end();
                    let c1 = cycles(pid);
                    let counted = probe::wakes::counted(&k0, &probe::wakes::Counters::take(probe::wakes::wall_us(), &host::tree_pids(pid), perf.as_ref()));
                    hooks.end();
                    let timed = metrics::Timed { t0, t1, untimed: hooks.untimed_stretches() };
                    // CPU over the timed parts, each process against its own readings at their edges (a stretch
                    // the run never ended ends with it), at the cycle clock's rate over the run.
                    let mut e = edges.take();
                    if e.len() % 2 == 1 {
                        e.push(c1.0.clone());
                    }
                    let mut at: Vec<&super::super::record::Cycles> = vec![&c0.0];
                    at.extend(e.iter());
                    at.push(&c1.0);
                    let used = match (c0.1, c1.1) {
                        (Some(a), Some(b)) if b > a && c1.0.values().any(|c| *c > 0) => super::super::record::cycles_timed(&at, |_| true) as f64 / (b - a) as f64 * ((t1 - t0) as f64 / 1e6),
                        _ => f64::NAN,
                    };
                    let retained = if heap { heap_mb()? - h0 } else { f64::NAN };
                    // The host's memory as the run left it, before its reset (SPEC §9.6), where this run may be
                    // the last measured one.
                    ends.push(warmup.may_end(runs.len()).then(|| {
                        let tree = host::process_tree(pid);
                        (tree.iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0), host::pss_bytes(&tree.iter().map(|p| p.pid).collect::<Vec<_>>()))
                    }));
                    runs.push((timed.ns() as f64 / 1e6, used, hooks.counts(), retained, hooks.take_marks(), counted, timed));
                    if s.reset {
                        ctx.runner.call("reset", json!({ "case": s.case }), t, &serve).map_err(|e| crate::runner::incomplete(&format!("{} reset", s.case), "reset", e, "", "fix the case's reset"))?;
                    }
                    // Without CPU (cycles that are not time), steady by wall time.
                    warmup.after(&runs.iter().map(|r| if r.1.is_finite() { r.1 } else { r.0 }).collect::<Vec<_>>());
                }
                // In a batch the host must outlive the case: one it ended fails the case that did it.
                if batch && let Some(code) = host.borrow_mut().child.exited() {
                    return Err(super::super::record::exited(code, &format!("after {}'s runs", s.case), None));
                }
                let first = warmup.first().unwrap_or(0);
                let local = first..warmup.measured().end.clamp(first, runs.len());
                let (strobe_pct, runner_pct) = cost.share(base + local.start..base + local.end);
                let mut steps = steps.clone();
                steps.push(("runs".into(), lapped.elapsed().as_secs_f64() * 1e3));
                let measured = &runs[local.clone()];
                let key = |j: usize| format!("run:{}", base + first + j);
                let mut extra: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
                extra.insert("run.duration".into(), measured.iter().enumerate().map(|(j, r)| (key(j), r.0)).collect());
                if measured.iter().all(|r| r.1.is_finite()) {
                    extra.insert("cpu.work".into(), measured.iter().enumerate().map(|(j, r)| (key(j), r.1)).collect());
                    extra.insert("cpu.app.total".into(), measured.iter().enumerate().filter(|(_, r)| r.0 > 0.0).map(|(j, r)| (key(j), 100.0 * r.1 / r.0)).collect());
                }
                if heap {
                    extra.insert("js.heap.retained".into(), measured.iter().enumerate().map(|(j, r)| (key(j), r.3)).collect());
                }
                let hooked: Vec<super::block::Hooked> = measured.iter().map(|r| (&r.2, r.4.as_slice(), &r.6)).collect();
                extra.extend(super::block::per_run_hooks(s, &key, &hooked));
                let counted: Vec<(String, &probe::wakes::Counted)> = measured.iter().enumerate().map(|(j, r)| (key(j), &r.5)).collect();
                let (counts, uncounted) = probe::wakes::counter_metrics(&counted, declared_metrics(s));
                extra.extend(counts);
                let mut values = metrics::block_values(&metrics::BlockData::default(), "run", &extra);
                if warmup.unsteady() {
                    values.insert("warmup.unsteady".into(), if warmup.grew() { 2.0 } else { 1.0 });
                }
                // Never a reading from after the reset in its place: no value.
                let (private_mb, pss) = local.end.checked_sub(1).and_then(|i| ends.get(i).copied().flatten()).unwrap_or((0.0, None));
                if private_mb > 0.0 {
                    values.insert("memory.app.private".into(), private_mb);
                }
                if let Some(b) = pss {
                    values.insert("memory.app.pss".into(), b as f64 / (1024.0 * 1024.0));
                }
                let mut refused = uncounted;
                refuse_not_on_this_os(s, &mut values, &mut refused);
                super::block::untimed_state(s, &measured.iter().map(|r| &r.6).collect::<Vec<_>>(), &mut values, &mut refused);
                base += runs.len();
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
                    steps,
                    background: Vec::new(),
                    leak: None,
                    refused,
                    locked: false,
                    display: None,
                    screen: None,
                    reserve: None,
                    shown: None,
                    overhead: Some(Overhead { strobe_pct, runner_pct, ..Overhead::default() }),
                    process_set: Some(programs(&host::process_tree(pid))),
                    fixture_connected: None,
                    client: None,
                    frame: false,
                    browser: None,
                    directions: BTreeMap::new(),
                    ..Block::default()
                })
            })();
            let end = r.is_err() || !s.reset_check;
            launch.case_ms.push(lapped.elapsed().as_secs_f64() * 1e3);
            launch.cases.push(r);
            // Runs that build on each other on purpose (`resetCheck: false`) leave a state no later case
            // may start from: the launch ends with it, as with a failed case.
            if end {
                break;
            }
        }
        if launch.cases.iter().any(Result::is_ok) {
            let _ = ctx.runner.call("close", json!({}), t, &serve);
        }
        Ok(launch)
    })();
    host.into_inner().exit();
    let mut launch = result?;
    if let Some(dir) = profiles.filter(|_| keep || set == Set::Attribution)
        && let Some(Ok(block)) = launch.cases.first_mut()
    {
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
        let draft = store::claim_recording(&id)?;
        let rec = draft.dir().to_path_buf();
        if let Some(p) = newest("cpuprofile") {
            store::write_atomic(&rec.join("cpu-0.cpuprofile"), &std::fs::read(&p)?)?;
        }
        if let Some(p) = newest("heapprofile") {
            store::write_atomic(&rec.join("alloc-0.heapprofile"), &std::fs::read(&p)?)?;
        }
        let _ = std::fs::remove_dir_all(&dir);
        super::keep_actions(ctx, &rec)?;
        store::write_json(
            &rec.join("meta.json"),
            &json!({
                "id": id, "kind": "block", "experiment": ctx.exp, "seed": ctx.seed, "series": s.label(), "variant": v.role, "variant_label": v.label, "variant_git": variant_git(v),
                "target": target["entry"], "probes": set, "runs": block.run_ms.len(), "values": block.values,
                "created": super::super::record::now_iso(), "valid": true, "env": host::env(), "script": true,
                "note": "script profiles cover the whole host process: its start, setup, warmup and measured runs",
            }),
        )?;
        draft.keep();
        block.recording = Some(id);
    }
    Ok(launch)
}
