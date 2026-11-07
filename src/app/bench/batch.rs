//! Many cases per launch (`--check --batch N`, SPEC §5.6): run cases one launch can hold (one target,
//! the same params, GPU cache and way of measuring) share a launch, its prepare and its setup, up to N
//! at a time. After each case's runs and reset the page or app must be back where the launch's first
//! run started (`ResetIncomplete`), still answer, show what the case expects (`WrongScreen`) and have
//! taken the OS's input where strobe put it, before the next case runs. A case that fails, crashes or
//! leaves dirty state fails alone and ends its launch: the next case gets a fresh one. One recording per
//! launch marks each case (`case:<i>:<name>:start|end`), and each case's actions name it.

use super::*;

/// Launches in a row that failed before any case ran stop the bench.
const LAUNCH_FAILURES: u32 = 3;

/// The batch over the experiment: its size and what its launches did.
pub(super) struct Batching {
    pub(super) size: usize,
    /// Results of series a launch already ran, by their place in the experiment's series.
    done: BTreeMap<usize, Value>,
    launches: Vec<Seen>,
    /// Launches in a row that failed before any case ran.
    failures: u32,
    /// Why the bench stops: the user's state disturbed, or launches that kept failing.
    pub(super) halt: Option<Error>,
}

/// One launch of the batch: how many cases it ran and its own time (ms), before its first case's runs
/// and to close; a launch that failed before any case ran saved nothing.
struct Seen {
    cases: usize,
    own_ms: f64,
    failed: bool,
}

impl Batching {
    pub(super) fn new(size: usize) -> Batching {
        Batching { size, done: BTreeMap::new(), launches: Vec::new(), failures: 0, halt: None }
    }

    /// Series `i`'s result when a launch already ran it.
    pub(super) fn take(&mut self, i: usize) -> Option<Value> {
        self.done.remove(&i)
    }

    /// `result.json`'s `batch`: the size, the launches, the cases they ran, and the time a launch per
    /// case would have taken on top: each case after a launch's first saved that launch's own time.
    pub(super) fn summary(&self) -> Value {
        let cases: usize = self.launches.iter().map(|l| l.cases).sum();
        let ok: Vec<&Seen> = self.launches.iter().filter(|l| !l.failed).collect();
        let saved: f64 = ok.iter().map(|l| l.own_ms * l.cases.saturating_sub(1) as f64).sum();
        let own = if ok.is_empty() { Value::Null } else { json!(ok.iter().map(|l| l.own_ms).sum::<f64>() / ok.len() as f64) };
        json!({
            "size": self.size, "launches": self.launches.len(), "cases": cases, "failed_launches": self.launches.iter().filter(|l| l.failed).count(),
            "launch_ms_mean": own, "saved_ms": saved,
        })
    }
}

/// The text line for `result.batch`.
pub(super) fn line(b: &Value) -> Option<String> {
    let launches = b["launches"].as_u64()?;
    let mut t = format!(
        "batch  {} cases in {launches} launch{} (--batch {})",
        b["cases"].as_u64().unwrap_or(0),
        if launches == 1 { "" } else { "es" },
        b["size"].as_u64().unwrap_or(0)
    );
    if let Some(own) = b["launch_ms_mean"].as_f64() {
        t += &format!(
            ": about {} saved against a launch per case (a launch took {} besides its cases: its start, prepare and setup, its trace, its recording and its close)",
            fmt::ms_duration(b["saved_ms"].as_f64().unwrap_or(0.0) as u64),
            fmt::ms_duration(own as u64)
        );
    }
    if let Some(n) = b["failed_launches"].as_u64().filter(|n| *n > 0) {
        t += &format!("; {n} launch{} failed before a case ran", if n == 1 { "" } else { "es" });
    }
    Some(t + "\n")
}

/// What a run case may share a launch by: its target (and with it the engine, headless mode, emulated
/// device and motion), its params (a fixture's, a seed folder's and the hooks' own), its GPU cache and
/// whether it is measured from outside. None for a case that needs a launch of its own: a launch case,
/// a command line's (`cli()`), a series that counts instructions in a block of their own (Linux).
pub(super) fn key(ctx: &Ctx, s: &Series) -> Option<String> {
    let kind = ctx.describe["targets"][&s.target]["kind"].as_str().unwrap_or("");
    if s.gap.is_some() || s.refused.is_some() || s.launch.is_object() || kind == "cli" || counting(s) != Counting::InTiming {
        return None;
    }
    Some(format!("{}\n{}\n{:?}\n{}", s.target, serde_json::to_string(&s.params).unwrap_or_default(), s.gpu_cache, s.cross_engine))
}

/// Refuses a batch whose launch hook (`what`: the target's prepare and `url` function, the file's setup)
/// read `ctx.case`: a launch runs it once for every case of its chunk, so it would set every case up as
/// the first. Without `--batch` it says nothing.
pub(super) fn launch_hook_case(ctx: &mut Ctx, what: &str) -> Result<()> {
    if ctx.runner.took_case() && ctx.batch.is_some() {
        return Err(Error::usage(format!(
            "--batch: {what} reads ctx.case, and a batch's launch runs it once for all its cases: drop --batch, or set up per case in the case's own run (or a target of its own)"
        )));
    }
    Ok(())
}

/// A case's name in a mark (`case:<i>:<name>:start`): letters, digits, `-`, `_` and `.`.
pub(super) fn mark_name(case: &str) -> String {
    case.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect()
}

/// The note on a case after which the screen was not one the next case may start from (`why`: another
/// screen than the launch began on, or not what the next case expects).
pub(super) fn left_screen(next: &Series, why: &str) -> String {
    format!("after its reset the screen was not one {} may start from ({why}), so {} ran in a fresh launch", next.case, next.case)
}

/// The page still answers after a case of a batch: one its case crashed or hung fails that case.
pub(super) fn answers(case: &str, r: Result<Value>) -> Result<()> {
    r.map(|_| ()).map_err(|e| {
        Refusal::new(ValidatorId::RunIncomplete, format!("{case}: the page stopped answering after the case's runs (its renderer crashed or hung): {}", e.message))
            .fix("fix what the case does to the page; the next case ran in a fresh launch")
            .into()
    })
}

/// A batch recording's `batch` in its meta.json: each case's label, its measured runs and values.
pub(super) fn meta<'a>(cases: impl Iterator<Item = (&'a Series, std::ops::Range<usize>, &'a Block)>) -> Value {
    json!(
        cases
            .enumerate()
            .map(|(i, (s, runs, b))| json!({ "seq": i + 1, "case": s.case, "label": s.label(), "params": s.params, "primary": s.primary, "runs": runs.map(|r| format!("run:{r}")).collect::<Vec<_>>(), "values": b.values, "refused": refusal_map(&b.refused) }))
            .collect::<Vec<_>>()
    )
}

/// Series `i`'s check result: from a launch that already ran it, else from the launch that runs it now
/// with the series after it that share its key, up to the batch's size. A case that needs its own
/// launch runs as without `--batch`.
pub(super) fn series(ctx: &mut Ctx, b: &mut Batching, all: &[Series], i: usize, variants: &mut [Variant], config: &Value) -> Result<Value> {
    if let Some(out) = b.done.remove(&i) {
        return Ok(out);
    }
    let Some(k) = key(ctx, &all[i]) else { return check_series(ctx, &all[i], variants, config) };
    // A case whose runs build on each other (`resetCheck: false`) ends its launch: the chunk ends with it.
    let mut chunk: Vec<usize> = Vec::new();
    for j in (i..all.len()).filter(|j| !b.done.contains_key(j) && !ctx.saved["series"][all[*j].label()]["done"].is_object() && key(ctx, &all[*j]).as_deref() == Some(k.as_str())) {
        chunk.push(j);
        if chunk.len() == b.size || !all[j].reset_check {
            break;
        }
    }
    let cases: Vec<Series> = chunk.iter().map(|j| all[*j].clone()).collect();
    let s = &cases[0];
    release_machine(ctx);
    ctx.reg.set("preparing", format!("{}: builds and servers", s.label()));
    prepare_all(ctx, s, variants, config)?;
    // `--video`: one film of the launch, each case's span marked in it.
    let film = ctx.args.has("video") && {
        let skip = no_video(ctx, variants[0].target_of(s));
        if let Some(why) = &skip {
            ctx.warnings.push(validity::info("Video", format!("{}: no video: {why}", s.label())));
        }
        skip.is_none()
    };
    if cases.len() > 1 {
        progress(format!("  launch {} · {} cases from {}", b.launches.len() + 1, cases.len(), s.label()));
    }
    let started = Instant::now();
    let log_from = ctx.runner.log_len();
    let r = launch_on_screen(ctx, &cases, &variants[0], 0, &Exec { video: film, ..Exec::untimed(true, None) });
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let launch = match r {
        Ok(l) => l,
        // A stop, an unlock wait that ran out, or a question the batch cannot answer ends the bench.
        Err(e) if matches!(e.code, 2 | 124 | 130) || stop_requested() => return Err(if e.code == 124 || e.code == 2 { e } else { stop_error() }),
        Err(e) => {
            // The launch, its prepare or its setup failed: its first case fails with it, the rest wait.
            b.failures += 1;
            b.launches.push(Seen { cases: 1, own_ms: wall_ms, failed: true });
            let log = runner_log(ctx, log_from, None);
            let sent = ctx.runner.take_actions();
            let mut out = check_case(ctx, s, variants, Err(e), wall_ms, log, sent, film)?;
            stamp(&mut out, b.launches.len(), 0);
            if b.failures >= LAUNCH_FAILURES {
                b.halt = Some(Error::msg(format!("{LAUNCH_FAILURES} launches in a row failed before any case ran (the last: {})", out["error"].as_str().unwrap_or(""))));
            }
            return Ok(out);
        }
    };
    // The launch, its prepare and its setup went through.
    b.failures = 0;
    let actions = launch.actions.unwrap_or_else(|| ctx.runner.take_actions());
    // What the launch took besides its cases (their runs, resets and the checks after them): its start,
    // prepare and setup, reading its trace, its recording and its close, which a launch per case pays
    // once per case. Each case's time is its own, the first's with the launch's.
    let cases_ms: f64 = launch.case_ms.iter().sum();
    let own_ms = (wall_ms - cases_ms).max(0.0);
    b.launches.push(Seen { cases: launch.cases.len(), own_ms, failed: false });
    let id = b.launches.len();
    let n = launch.cases.len();
    for (seq, r) in launch.cases.into_iter().enumerate() {
        let s = &cases[seq];
        let elapsed = launch.case_ms.get(seq).copied().unwrap_or(0.0) + if seq == 0 { own_ms } else { 0.0 };
        let from = if seq == 0 { log_from } else { launch.log_at.get(seq).copied().unwrap_or(log_from) };
        let log = runner_log(ctx, from, launch.log_at.get(seq + 1).copied().filter(|_| seq + 1 < n));
        let mut out = check_case(ctx, s, variants, r, elapsed, log, actions.of_case(&s.case, seq == 0), film)?;
        stamp(&mut out, id, seq);
        if let Some((_, note)) = launch.notes.iter().find(|(k, _)| *k == seq) {
            out["batch_note"] = json!(note);
            ctx.warnings.push(validity::warn("Batch", format!("{}: {note}", s.label())));
        }
        b.done.insert(chunk[seq], out);
    }
    if let Some(e) = launch.abort {
        b.halt = Some(e);
    }
    b.done.remove(&i).ok_or_else(stop_error)
}

/// A case's place in the batch: its launch (from 1) and its place there (from 1).
fn stamp(out: &mut Value, launch: usize, seq: usize) {
    out["batch_id"] = json!(launch);
    out["batch_seq"] = json!(seq + 1);
}
