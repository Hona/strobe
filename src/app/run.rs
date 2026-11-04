//! `strobe run -- <command>`: an ad-hoc harness under a bench block's conditions (SPEC §5.12): the
//! device's bench slot, the fastest power plan, pinned to the target's CPUs with this user's other
//! processes kept off them. It measures nothing itself: the harness prints its own numbers.

use super::{Output, bench, progress};
use crate::cli::Args;
use crate::host;
use crate::model::{Error, Result};
use serde_json::json;
use std::process::Command;
use std::time::Instant;

pub fn run(args: &Args) -> Result<Output> {
    let at = args.pos.iter().position(|a| a == "--").ok_or_else(|| Error::usage("run needs a command after --: strobe run [--pin auto] [--reserve-cpus on|off] [--power high|keep] -- <command> [args…]"))?;
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
    // The child inherits strobe's affinity at spawn; strobe's own comes back right after.
    let previous = pin.and_then(host::set_affinity);
    let started = Instant::now();
    let spawned = Command::new(program).args(rest).spawn();
    if let Some(p) = previous {
        host::set_affinity(p);
    }
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
