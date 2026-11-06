//! `strobe run -- <command>`: an ad-hoc harness under a bench block's conditions (SPEC §5.5): the
//! device's bench slot, the fastest power plan, pinned to the target's CPUs with this user's other
//! processes kept off them. It measures nothing itself: the harness prints its own numbers.
//!
//! `strobe run --base "<command>" --head "<command>"`: a paired A/B of two commands, as a bench of a
//! `cli()` target whose variants are the command lines (`cmd:<line>`): the same blocks, looks,
//! verdicts, result and report as any bench.

use super::{Output, bench, progress};
use crate::cli::Args;
use crate::host;
use crate::model::{Error, Result};
use serde_json::json;
use std::process::Command;
use std::time::Instant;

pub fn run(args: &Args) -> Result<Output> {
    if args.get("base").is_some() || args.get("head").is_some() || args.has("aa") {
        return paired(args);
    }
    if args.has("seed") {
        return Err(Error::usage("--seed seeds a bench's ctx.random(); strobe run -- <command> runs your own harness: give it its own seed"));
    }
    let at = args.pos.iter().position(|a| a == "--").ok_or_else(|| {
        Error::usage("run needs a command after --: strobe run [--pin auto] [--reserve-cpus on|off] [--power high|keep] -- <command> [args…], or two to compare: strobe run --base \"<command>\" --head \"<command>\"")
    })?;
    let cmd = &args.pos[at + 1..];
    let (program, rest) = cmd.split_first().ok_or_else(|| Error::usage("run: nothing after --"))?;
    let (pin, _, label) = bench::pinning(args.get("pin").unwrap_or("auto"), &host::topology())?;
    let pin_label = label.replace(" · runner on the other CPUs", "");
    let id = crate::store::new_id("run");
    // Takes turns with benches: a harness and a timed block never share the CPUs.
    let reg = bench::slot::Registration::new(&id, &cmd.join(" "));
    let (slot, waited) = reg.acquire(&format!("strobe run {program}"))?;
    let power = bench::power_for(args)?;
    let reserve = match (args.get("reserve-cpus").unwrap_or("on"), pin) {
        ("off", _) | (_, None) => None,
        (_, Some(cpus)) => match host::reserve::Reserve::start(cpus) {
            Ok(r) => Some(r),
            Err(why) => {
                progress(format!("  other processes stay on the target's CPUs: {why}"));
                None
            }
        },
    };
    let awake = host::Awake::start();
    awake.hold(true);
    progress(format!("strobe run · {pin_label} · power plan {}{}", power.used, if reserve.is_some() { " · others kept off those CPUs" } else { "" }));
    let started = Instant::now();
    let spawned = host::spawn_pinned(Command::new(program).args(rest), pin, 0);
    let status = spawned.map_err(|e| Error::msg(format!("run {program}: {e}"))).and_then(|mut c| c.wait().map_err(Error::from));
    let ms = started.elapsed().as_secs_f64() * 1e3;
    let report = reserve.map(host::reserve::Reserve::finish);
    awake.hold(false);
    drop(power);
    drop(slot);
    let status = status?;
    let code = status.code().unwrap_or(1);
    let moved = report.as_ref().map(|r| r.moved_total()).unwrap_or(0);
    let text = format!("ran {program} in {} · exit {code} · {pin_label}{}\n", super::fmt::ms_duration(ms as u64), if report.is_some() { format!(" · {moved} other processes kept off its CPUs") } else { String::new() });
    let mut out = Output::new(text, json!({ "id": id, "command": cmd, "exit": code, "ms": ms, "waited_ms": waited.as_secs_f64() * 1e3, "pin": pin_label, "moved": moved }));
    out.code = code;
    Ok(out)
}

/// The bench file of a command A/B: one `cli()` target whose variants supply the command, one case
/// that runs it once per run. Its text depends only on the declared metrics, so the rerun ledger sees
/// the same harness for the same question.
fn bench_file(metrics: &[String]) -> String {
    format!(
        "// Written by `strobe run --base … --head …`: a paired A/B of two commands (SPEC §5.5).\nimport {{ bench, cli }} from \"@hona/strobe\"\n\nexport default bench({{\n  targets: {{ command: cli() }},\n  cases: {{\n    run: {{ primary: \"cpu.work\", metrics: {}, resetCheck: false, run: (ctx) => ctx.exec() }},\n  }},\n}})\n",
        serde_json::to_string(metrics).unwrap_or_else(|_| "[]".into())
    )
}

fn paired(args: &Args) -> Result<Output> {
    if args.pos.iter().any(|a| a == "--") {
        return Err(Error::usage("strobe run compares commands given as --base \"<command>\" --head \"<command>\" (or --aa --head \"<command>\"), not after --"));
    }
    let head = args.get("head").ok_or_else(|| Error::usage("strobe run --base \"<command>\" --head \"<command>\": --head names the command to measure"))?;
    match (args.get("base"), args.has("aa"), args.has("check")) {
        (Some(_), true, _) => return Err(Error::usage("--aa runs --head against itself; drop --base")),
        (Some(_), _, true) => return Err(Error::usage("--check runs --head's command once; drop --base")),
        (None, false, false) => return Err(Error::usage("say what to compare: --base \"<command>\" (or --aa for the same command on both sides, or --check to run it once)")),
        _ => {}
    }
    let metrics: Vec<String> = args.get("metrics").unwrap_or("").split(',').map(str::trim).filter(|m| !m.is_empty()).map(str::to_string).collect();
    if let Some(bad) = metrics.iter().find(|m| crate::model::metric(m).is_none()) {
        return Err(Error::usage(format!("--metrics: {bad} is no metric (a command gives cpu.work, run.duration, cpu.wakes, memory.peak, io.read, io.write and metric.<name>)")));
    }
    let text = bench_file(&metrics);
    use sha2::Digest;
    let hash: String = sha2::Sha256::digest(text.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
    let dir = host::paths::run().join(format!("cmd-{hash}"));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("run.bench.ts");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
        crate::store::write_atomic(&file, text.as_bytes())?;
    }
    let mut b = args.clone();
    b.command = "bench".into();
    b.pos = vec![file.display().to_string()];
    b.set("head", &format!("cmd:{head}"));
    if let Some(base) = args.get("base") {
        b.set("base", &format!("cmd:{base}"));
    }
    bench::run(&b)
}
