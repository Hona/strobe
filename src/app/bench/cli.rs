//! `cli()` targets (SPEC §9.6): a command run to its exit, once per `ctx.exec()`, in the variant's
//! folder (or a `cmd:` variant's own command line), measured from outside: what the OS counted for it
//! and everything it started (`host::spawn::Owned::run_to_exit`), and the `strobe:metric` lines it
//! printed. Nothing of strobe's runs in the command; the runner and strobe stay on the other CPUs.

use super::*;
use std::io::{BufRead, BufReader, Write};

/// One command run: wall ms (spawn to exit), what the OS counted, and the metrics it printed.
struct Ran {
    wall: f64,
    usage: host::spawn::Usage,
    metrics: BTreeMap<String, f64>,
}

/// A run's commands summed, or None when one of them has no value.
fn sum_of(r: &[Ran], f: impl Fn(&Ran) -> Option<f64>) -> Option<f64> {
    r.iter().map(f).sum()
}

/// The line `ctx.exec(line?)` runs: the call's own, else the variant's (`cmd:`), else the target's.
fn line_for(asked: &Value, v: &Variant, target: &Value) -> Result<String> {
    let line = asked.as_str().map(str::to_string).or_else(|| match &v.kind {
        Kind::Cmd(l) => Some(l.clone()),
        _ => None,
    });
    let line = line.or_else(|| target["run"].as_str().map(str::to_string)).filter(|l| !l.trim().is_empty());
    line.ok_or_else(|| Error::usage("a cli target needs a command: cli(\"<line>\"), a cmd:<line> variant, or ctx.exec(\"<line>\")"))
}

/// The command for `line`: on Unix the shell, which execs into it (the command is the child, not a
/// shell around it); on Windows its program directly, so the child is the program itself (a
/// `.cmd` or `.bat`, or a program not found as an executable, runs under cmd).
fn command(line: &str, shell: bool) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let line = line.trim();
        let (program, rest) = match line.strip_prefix('"') {
            Some(q) => q.split_once('"').map(|(p, r)| (p.to_string(), r.to_string())).unwrap_or((q.to_string(), String::new())),
            None => line.split_once(char::is_whitespace).map(|(p, r)| (p.to_string(), r.to_string())).unwrap_or((line.to_string(), String::new())),
        };
        let script = Path::new(&program).extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
        if shell || script {
            return crate::target::command::shell(line);
        }
        let mut c = Command::new(program);
        if !rest.trim().is_empty() {
            c.raw_arg(rest.trim());
        }
        c
    }
    #[cfg(not(windows))]
    {
        let _ = shell;
        crate::target::command::shell(&format!("exec {line}"))
    }
}

/// `strobe:metric <name>=<value>` at the start of a line (after spaces): the value of `metric.<name>`.
fn metric_line(line: &str) -> Option<(String, f64)> {
    let (name, value) = line.trim_start().strip_prefix("strobe:metric ")?.trim().split_once('=')?;
    let name = name.trim();
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    let value = value.split_whitespace().next()?.parse::<f64>().ok().filter(|v| v.is_finite())?;
    ok.then(|| (name.to_string(), value))
}

/// Runs one command to its exit, its output read line by line (into the experiment's cli.log).
fn exec(line: &str, root: &Path, target: &Value, cpus: Option<u64>, log: &Path) -> Result<(Ran, String)> {
    let start = |shell: bool| -> std::io::Result<(host::spawn::Owned, Instant)> {
        let mut cmd = command(line, shell);
        for (k, val) in target["env"].as_object().into_iter().flatten() {
            cmd.env(k, val.as_str().unwrap_or(""));
        }
        cmd.current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let t0 = Instant::now();
        host::spawn_owned(&mut cmd, cpus, 0, None).map(|c| (c, t0))
    };
    let (mut child, t0) = match start(false) {
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::NotFound => start(true),
        r => r,
    }
    .map_err(|e| Error::msg(format!("start `{line}`: {e}")))?;
    let (out, err) = child.output();
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let readers: Vec<_> = [out.map(|o| Box::new(o) as Box<dyn std::io::Read + Send>), err.map(|e| Box::new(e) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
        .map(|r| {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(r).lines().map_while(std::result::Result::ok) {
                    lines.lock().unwrap_or_else(|e| e.into_inner()).push(l);
                }
            })
        })
        .collect();
    let usage = child.run_to_exit().map_err(|e| Error::msg(format!("`{line}`: {e}")))?;
    let wall = usage.exited.unwrap_or_else(Instant::now).duration_since(t0).as_secs_f64() * 1e3;
    // What it left running was ended with it, so its pipes are closed.
    for r in readers {
        let _ = r.join();
    }
    let lines = std::mem::take(&mut *lines.lock().unwrap_or_else(|e| e.into_inner()));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "$ {line}\n{}", lines.join("\n"));
    }
    let tail = lines.iter().rev().take(10).cloned().collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    if usage.code != 0 {
        return Err(Error::msg(format!("`{line}` exited with {}{}", usage.code, if tail.is_empty() { String::new() } else { format!(":\n{tail}") })));
    }
    if !usage.left.is_empty() {
        let names: Vec<String> = usage.left.iter().take(5).map(|(p, n)| format!("{n} (pid {p})")).collect();
        return Err(Error::refused(
            "RunIncomplete",
            format!("`{line}` exited and left {} running: {}; what they did is not counted whole, so strobe ended them", if usage.left.len() == 1 { "a process" } else { "processes" }, names.join(", ")),
            "make the command wait for what it starts (no background jobs or daemons)",
        ));
    }
    let mut metrics = BTreeMap::new();
    for (name, value) in lines.iter().filter_map(|l| metric_line(l)) {
        *metrics.entry(name).or_insert(0.0) += value;
    }
    let output: String = lines.join("\n");
    let output = if output.len() > 64 * 1024 { output[output.len() - 64 * 1024..].to_string() } else { output };
    Ok((Ran { wall, usage, metrics }, output))
}

pub(super) fn block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    if s.launch.is_object() {
        return Err(Error::usage(format!("{}: a cli target's case runs commands (run: (ctx) => ctx.exec()); a launch case needs an app", s.case)));
    }
    if x.set == Set::Attribution || x.set == Set::Memory {
        return Err(Error::usage("a cli target has no attribution or memory blocks: its commands end inside each run (profile a long-running command with strobe record pid:<n> --probes deep)"));
    }
    let target = ctx.describe["targets"][&s.target].clone();
    let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a cli target runs in a variant's folder", v.role)))?;
    let log = store::experiment_dir(&ctx.exp)?.join("cli.log");
    let mut steps: Vec<(String, f64)> = Vec::new();
    let hooks = super::block::Hooks::default();
    let ran: RefCell<Vec<Ran>> = RefCell::new(Vec::new());
    let cpus = ctx.pin;
    let serve = |op: &str, a: &Value| -> Result<Value> {
        match op {
            "exec" => {
                let line = line_for(&a["line"], v, &target)?;
                let (r, output) = exec(&line, &root, &target, cpus, &log)?;
                let code = r.usage.code;
                // Only a run's commands count; setup's and reset's prepare.
                if hooks.in_run() {
                    ran.borrow_mut().push(r);
                }
                Ok(json!({ "code": code, "output": output }))
            }
            "count" => {
                hooks.count(a);
                Ok(Value::Bool(true))
            }
            "phase" => {
                hooks.phase(a);
                Ok(Value::Bool(true))
            }
            "call" => Err(Error::usage("ctx.call is a script() target's; a cli target runs commands with ctx.exec()")),
            "driver" => Err(Error::msg("a cli target has no page: run its commands with ctx.exec()")),
            "line" => Err(Error::usage("waitForLine reads a native app's output; ctx.exec() resolves with the command's output")),
            other => Err(Error::msg(format!("unknown runner request {other}"))),
        }
    };
    let t = s.run_timeout().max(Duration::from_secs(600));
    let root_text = root.display().to_string();
    ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": root_text }), t, &serve)?;
    let mut lapped = Instant::now();
    ctx.runner.call("setup", json!({}), s.hook_timeout(), &serve).map_err(|e| crate::runner::incomplete("setup", "setup", e, "", "fix the bench file's setup"))?;
    steps.push(("setup".into(), lapped.elapsed().as_secs_f64() * 1e3));
    lapped = Instant::now();
    /// One run: its commands, its counts and its phase marks.
    type CliRun = (Vec<Ran>, BTreeMap<String, f64>, Vec<(String, u64)>);
    let mut runs: Vec<CliRun> = Vec::new();
    let mut cost = SelfCost::new(ctx.runner.pid());
    let mut warmup = super::block::Warmup::new(x);
    let cpu_of = |r: &[Ran]| -> f64 { r.iter().map(|x| x.usage.cpu_ns.map_or(f64::NAN, |c| c as f64 / 1e6)).sum() };
    while warmup.more(runs.len()) {
        hooks.begin(&format!("run:{}", runs.len()));
        cost.start();
        let r = ctx.runner.call("run", json!({ "case": s.case }), t, &serve);
        cost.end();
        hooks.end();
        r.map_err(|e| crate::runner::incomplete(&format!("{} run", s.case), "run", e, "", "the run threw, timed out, or a command failed"))?;
        let done = std::mem::take(&mut *ran.borrow_mut());
        if done.is_empty() {
            return Err(Error::refused("RunIncomplete", format!("{} run: it ran no command", s.case), "call ctx.exec() in the case's run"));
        }
        runs.push((done, hooks.counts(), hooks.take_marks()));
        if s.reset {
            ctx.runner.call("reset", json!({ "case": s.case }), t, &serve).map_err(|e| crate::runner::incomplete(&format!("{} reset", s.case), "reset", e, "", "fix the case's reset"))?;
        }
        warmup.after(&runs.iter().map(|r| cpu_of(&r.0)).collect::<Vec<_>>());
    }
    let first = warmup.first().unwrap_or(0);
    let (strobe_pct, runner_pct) = cost.share(first);
    steps.push(("runs".into(), lapped.elapsed().as_secs_f64() * 1e3));
    let measured = &runs[first..];
    let key = |j: usize| format!("run:{}", first + j);
    let per = |f: &dyn Fn(&[Ran]) -> Option<f64>| -> BTreeMap<String, f64> { measured.iter().enumerate().filter_map(|(j, r)| Some((key(j), f(&r.0)?))).collect() };
    let wall = |r: &[Ran]| -> f64 { r.iter().map(|x| x.wall).sum() };
    let mut extra: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    let mut refused: BTreeMap<String, Refusal> = BTreeMap::new();
    let mut put = |name: &str, values: BTreeMap<String, f64>, why: &dyn Fn() -> Refusal| {
        if values.len() == measured.len() {
            extra.insert(name.to_string(), values);
        } else {
            refused.insert(name.to_string(), why());
        }
    };
    put("run.duration", per(&|r| Some(wall(r))), &|| Refusal::new(ValidatorId::RunIncomplete, "run.duration: a run had no command"));
    let cpu = |r: &[Ran]| sum_of(r, |x| x.usage.cpu_ns.map(|c| c as f64 / 1e6));
    put("cpu.work", per(&cpu), &|| Refusal::new(ValidatorId::NotOnThisOs, "cpu.work: this OS gave no CPU time for a command's processes"));
    put("cpu.app.total", per(&|r| cpu(r).map(|c| 100.0 * c / wall(r).max(1e-9))), &|| {
        Refusal::new(ValidatorId::NotOnThisOs, "cpu.app.total: this OS gave no CPU time for a command's processes")
    });
    put("cpu.wakes", per(&|r| sum_of(r, |x| x.usage.switches.map(|n| n as f64)).map(|n| n / (wall(r) / 1e3).max(1e-9))), &|| {
        Refusal::new(ValidatorId::NotOnThisOs, "cpu.wakes: Windows keeps no context-switch count of a process that has ended (its job counts CPU and IO only)")
    });
    put("memory.peak", per(&|r| r.iter().map(|x| x.usage.peak_bytes.map(|b| b as f64 / (1024.0 * 1024.0))).try_fold(0.0f64, |m, b| Some(m.max(b?)))), &|| {
        Refusal::new(ValidatorId::NotOnThisOs, "memory.peak: this OS gave no peak memory for a command's processes")
    });
    for (m, pick) in [("io.read", true), ("io.write", false)] {
        put(m, per(&|r| sum_of(r, |x| if pick { x.usage.read_bytes } else { x.usage.write_bytes }.map(|b| b as f64))), &|| {
            Refusal::new(ValidatorId::NotOnThisOs, format!("{m}: macOS keeps no disk bytes of the processes a command started and reaped, and this one started some"))
        });
    }
    let hooked: Vec<super::block::Hooked> = measured.iter().map(|r| (&r.1, r.2.as_slice())).collect();
    extra.extend(super::block::per_run_hooks(s, &key, &hooked));
    // `metric.<name>`: every measured run must print it (a run that printed none is not 0).
    let mut names: Vec<String> = measured.iter().flat_map(|r| r.0.iter().flat_map(|x| x.metrics.keys().cloned())).collect();
    names.extend(declared_metrics(s).into_iter().filter_map(|m| m.strip_prefix("metric.").map(str::to_string)));
    names.sort();
    names.dedup();
    for name in names {
        let values: BTreeMap<String, f64> =
            measured.iter().enumerate().filter_map(|(j, r)| r.0.iter().filter_map(|x| x.metrics.get(&name)).copied().reduce(|a, b| a + b).map(|v| (key(j), v))).collect();
        let missed = measured.len() - values.len();
        let m = format!("metric.{name}");
        if missed == 0 {
            extra.insert(m, values);
        } else {
            refused.insert(m.clone(), Refusal::new(ValidatorId::NoMetric, format!("{m}: {missed} of {} measured runs printed no `strobe:metric {name}=<value>` line", measured.len())));
        }
    }
    let how: Vec<&str> = measured.iter().flat_map(|r| r.0.iter().map(|x| x.usage.cpu_how)).collect();
    let mut values = metrics::block_values(&metrics::BlockData::default(), "run", &extra);
    if warmup.unsteady() {
        values.insert("warmup.unsteady".into(), 1.0);
    }
    for m in declared_metrics(s) {
        if values.contains_key(m) || refused.contains_key(m) || m.starts_with("count.") || m.starts_with("phase.") {
            continue;
        }
        let r = if probe::wakes::COUNTER_METRICS.contains(&m.as_str()) {
            Refusal::new(ValidatorId::NoCounter, format!("{m}: a command's counters are not read (it ends inside the run)"))
        } else {
            Refusal::new(ValidatorId::NotForTarget, format!("{m}: a cli target is measured from outside its commands (cpu.work, run.duration, cpu.wakes, memory.peak, io.*, metric.<name>)"))
        };
        refused.insert(m.clone(), r);
    }
    if let Some(h) = how.iter().find(|h| **h != "exact").filter(|_| pair == 0) {
        progress(format!("  {} cpu.work: {h}", v.role));
    }
    let _ = ctx.runner.call("close", json!({}), t, &serve);
    Ok(Block {
        variant: v.role.to_string(),
        pair,
        values,
        run_ms: measured.iter().map(|r| wall(&r.0)).collect(),
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
    })
}
