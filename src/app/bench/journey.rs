//! What a journey of an implementation comparison adds to its blocks and pairs (SPEC §9.10): the
//! sides' adapters (`@name` in `expect` and `until`), the `expect.same` reads, the input each side's
//! runs gave, and the checks that refuse a pair whose two sides did not do the same thing:
//! `NotEquivalent` (they ended somewhere else, or their latency samples are not alike) and
//! `InputMismatch` (other input, or another amount of it). strobe's own reads inside runs
//! (`driver.reads`) are said per side, and warned about (`ObserverCost`).

use super::*;

/// `@name` (and any filters after it, `@row index=-1`) as `target`'s adapter entry (`ui`); any
/// other selector as it is. The bench file's hooks resolve their own in the runner; this is for the
/// selectors strobe reads from the file (`expect`, a launch's `until`).
/// `targets`: the bench file's targets, whose adapters say whether a missing name is a gap (another
/// target has it) or a typo (none has it).
pub(super) fn alias(sel: &Value, targets: &Value, target: &str) -> Result<Value> {
    let ui = &targets[target]["ui"];
    match sel {
        Value::String(s) if s.starts_with('@') => {
            let (name, rest) = s[1..].split_once(char::is_whitespace).map_or((&s[1..], ""), |(n, r)| (n, r.trim()));
            match &ui[name] {
                Value::String(e) => Ok(Value::String(if rest.is_empty() { e.clone() } else { format!("{e} {rest}") })),
                Value::Null if !targets.as_object().into_iter().flatten().any(|(_, t)| !t["ui"][name].is_null()) => {
                    Err(Error::usage(format!("{s:?}: @{name} is in no target's ui (a typo?): give the targets that run the case a ui entry {name}")))
                }
                Value::Null => Err(Refusal::new(ValidatorId::ParityGap, format!("{target} has no @{name}")).fix(format!("give {target}'s ui @{name}, or mark the case unsupported there")).into()),
                _ => Err(Error::usage(format!("@{name} is a step of {target}'s ui, not a selector: expect and until take a selector entry"))),
            }
        }
        Value::Object(o) if o.get("selector").and_then(Value::as_str).is_some_and(|s| s.starts_with('@')) => {
            let mut o = o.clone();
            o["selector"] = alias(&o["selector"], targets, target)?;
            Ok(Value::Object(o))
        }
        _ => Ok(sel.clone()),
    }
}

/// A target's `expect` with the case's over it, its `visible` selectors through the target's
/// adapter. `same` stays as the file wrote it: its names key the equivalence table.
pub(super) fn side_expect(targets: &Value, case_expect: &Value, target: &str) -> Result<Value> {
    let mut e = merged_expect(&targets[target], case_expect);
    match &e["visible"] {
        Value::Array(a) => e["visible"] = Value::Array(a.iter().map(|v| alias(v, targets, target)).collect::<Result<_>>()?),
        v if !v.is_null() => e["visible"] = alias(v, targets, target)?,
        _ => {}
    }
    Ok(e)
}

/// A launch case's `launch` with its `until` selector through the target's adapter.
pub(super) fn side_launch(launch: &Value, targets: &Value, target: &str) -> Result<Value> {
    let mut l = launch.clone();
    if launch["until"].is_string() || launch["until"].get("selector").is_some() {
        l["until"] = alias(&launch["until"], targets, target)?;
    }
    Ok(l)
}

/// What one `expect.same` element read in a block: after setup (a launch: once ready), after each
/// measured run and each reset after it, in order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct Reads {
    pub setup: Option<String>,
    pub run: Vec<String>,
    pub reset: Vec<String>,
}

impl Reads {
    /// What the journey ended on: the last run's read, else setup's.
    fn last(&self) -> Option<&String> {
        self.run.last().or(self.setup.as_ref())
    }

    /// Every read found no element.
    fn never(&self) -> bool {
        self.setup.iter().chain(&self.run).chain(&self.reset).all(|r| r == NONE)
    }
}

#[derive(Clone, Copy)]
pub(super) enum When {
    Setup,
    Run,
    Reset,
}

/// Text as `expect.same` compares it: trimmed, each run of whitespace one space.
pub(super) fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What an element that never showed reads as.
const NONE: &str = "(no element)";

/// A block's `expect.same` elements (the file's names, and the selectors they are on this side), and
/// what each read so far. Reads happen between runs, outside every timed span.
pub(super) struct Same {
    list: Vec<(String, Value)>,
    reads: RefCell<BTreeMap<String, Reads>>,
}

impl Same {
    /// `targets`: the bench file's targets.
    pub(super) fn new(s: &Series, targets: &Value) -> Result<Same> {
        let list =
            s.expect["same"].as_array().into_iter().flatten().map(|v| Ok((v.as_str().map_or_else(|| v.to_string(), str::to_string), alias(v, targets, &s.target)?))).collect::<Result<Vec<_>>>()?;
        Ok(Same { list, reads: RefCell::new(BTreeMap::new()) })
    }

    pub(super) fn on(&self) -> bool {
        !self.list.is_empty()
    }

    /// Nothing to read when `off` (a windowless app).
    pub(super) fn unless(mut self, off: bool) -> Same {
        if off {
            self.list.clear();
        }
        self
    }

    /// Reads each element now with `text` (its normalized text or value; None when it never showed).
    pub(super) fn read(&self, when: When, text: &dyn Fn(&Value) -> Result<Option<String>>) -> Result<()> {
        for (name, sel) in &self.list {
            let got = text(sel)?.map_or_else(|| NONE.to_string(), |t| normalize(&t));
            let mut reads = self.reads.borrow_mut();
            let r = reads.entry(name.clone()).or_default();
            match when {
                When::Setup => r.setup = Some(got),
                When::Run => r.run.push(got),
                When::Reset => r.reset.push(got),
            }
        }
        Ok(())
    }

    /// The reads, without those of the `first` warmup runs (and their resets): the sides' warmups
    /// differ in length, their measured runs do not.
    pub(super) fn take(&self, first: usize) -> BTreeMap<String, Reads> {
        let mut out = std::mem::take(&mut *self.reads.borrow_mut());
        for r in out.values_mut() {
            r.run.drain(..first.min(r.run.len()));
            r.reset.drain(..first.min(r.reset.len()));
        }
        out
    }
}

/// A run's count of one input call, kept beside the bench file's counts (`Hooks::op`).
pub(super) const OP: &str = "\u{1}op.";

/// How much input a page call gives: one per call, a typed character each (`type`); None for calls
/// that give none (waits, reads, `goto`).
pub(super) fn op_count(call: &str, args: &[Value]) -> Option<f64> {
    match call {
        "type" => Some(args.get(1).and_then(Value::as_str).map_or(0, |t| t.chars().count()) as f64),
        "click" | "press" | "scroll" | "wheel" | "drag" | "down" | "up" | "hover" | "move" | "act" => Some(1.0),
        _ => None,
    }
}

/// The input calls of a block's measured runs (`Hooks::op` counts): each call's median per run.
pub(super) fn ops(runs: &[&BTreeMap<String, f64>]) -> BTreeMap<String, f64> {
    let keys: std::collections::BTreeSet<&str> = runs.iter().flat_map(|c| c.keys()).filter_map(|k| k.strip_prefix(OP)).collect();
    keys.into_iter().map(|k| (k.to_string(), metrics::median(&runs.iter().map(|c| c.get(&format!("{OP}{k}")).copied().unwrap_or(0.0)).collect::<Vec<_>>()))).collect()
}

fn ops_text(ops: &BTreeMap<String, f64>) -> String {
    if ops.is_empty() {
        return "none".into();
    }
    ops.iter().map(|(k, n)| format!("{k} ×{}", fmt::value(*n, "count"))).collect::<Vec<_>>().join(", ")
}

/// Why the two blocks of a pair did not do the same journey, if they did not: `NotEquivalent` when an
/// `expect.same` element read differently, `InputMismatch` when one side acted through accessibility
/// actions where the other gave pointer or key input, when the amount of input differs by over 5%
/// (not for a case with steps per target, whose steps differ by design), or when the drags' move rate
/// does. `names`: the sides' names, base first.
pub(super) fn pair_refusal(adapted: bool, names: (&str, &str), base: &Block, head: &Block) -> Option<Refusal> {
    let keys: std::collections::BTreeSet<&String> = base.same.keys().chain(head.same.keys()).collect();
    for name in keys {
        let (b, h) = (base.same.get(name).cloned().unwrap_or_default(), head.same.get(name).cloned().unwrap_or_default());
        // Two sides that never showed it agree on nothing.
        if b.never() && h.never() {
            return Some(
                Refusal::new(ValidatorId::NotEquivalent, format!("{name} showed on neither side ({} nor {}): nothing shows that both reached the same place", names.0, names.1))
                    .fix("point expect.same at an element both sides show, through each side's ui"),
            );
        }
        if let Some((when, x, y)) = disagree(&b, &h) {
            let show = |v: &[String]| v.iter().map(|t| format!("{:?}", t.chars().take(80).collect::<String>())).collect::<Vec<_>>().join(" / ");
            return Some(
                Refusal::new(ValidatorId::NotEquivalent, format!("{name} read differently {when}: {} {} vs {} {}", names.0, show(&x), names.1, show(&y)))
                    .fix("make both sides end on the same screen and data (the same fixture seed, the same steps), or name an element both show the same way"),
            );
        }
    }
    let total = |o: &BTreeMap<String, f64>| o.iter().filter(|(k, _)| *k != "act").map(|(_, n)| n).sum::<f64>();
    let acts = |o: &BTreeMap<String, f64>| o.get("act").copied().unwrap_or(0.0) > 0.0;
    let mismatch = |why: String| {
        Some(Refusal::new(ValidatorId::InputMismatch, why).fix("give both sides the same input: pointer and keys on both (page.act skips hit testing and pointer handling), the same number of calls"))
    };
    if acts(&base.ops) != acts(&head.ops) && total(&base.ops) + total(&head.ops) > 0.0 {
        let (a, other) = if acts(&base.ops) { (names.0, names.1) } else { (names.1, names.0) };
        return mismatch(format!(
            "{a}'s runs act through accessibility actions where {other}'s give pointer or key input ({}: {}; {}: {})",
            names.0,
            ops_text(&base.ops),
            names.1,
            ops_text(&head.ops)
        ));
    }
    if !adapted {
        // Each kind of call within 5% of the side that gave more of it: two clicks are not a press and a click.
        let keys: std::collections::BTreeSet<&String> = base.ops.keys().chain(head.ops.keys()).collect();
        let off = keys.into_iter().find(|k| {
            let (a, b) = (base.ops.get(*k).copied().unwrap_or(0.0), head.ops.get(*k).copied().unwrap_or(0.0));
            (a - b).abs() > 0.05 * a.max(b)
        });
        if let Some(k) = off {
            return mismatch(format!("the sides' runs gave other input ({k}; {}: {}; {}: {})", names.0, ops_text(&base.ops), names.1, ops_text(&head.ops)));
        }
    }
    if let (Some(&a), Some(&b)) = (base.values.get("input.rate"), head.values.get("input.rate"))
        && a.max(b) > 0.0
        && (a - b).abs() / a.max(b) > 0.05
    {
        return mismatch(format!("the drags' moves went out at {a:.0}/s to {} and {b:.0}/s to {}: a driver that starved on one side looks like a slow app", names.0, names.1));
    }
    None
}

/// The series' equivalence table (`equivalence`): each `expect.same` element, what both sides read
/// at the end of the journey in the last pair, whether every pair agreed, and how many were read.
pub(super) fn equivalence(blocks: &[Block]) -> Value {
    let pairs: Vec<(usize, usize)> =
        (0..blocks.len()).filter(|&a| blocks[a].variant == "base").filter_map(|a| Some((a, (0..blocks.len()).find(|&b| blocks[b].variant == "head" && blocks[b].pair == blocks[a].pair)?))).collect();
    let names: std::collections::BTreeSet<&String> = pairs.iter().flat_map(|&(a, b)| blocks[a].same.keys().chain(blocks[b].same.keys())).collect();
    names
        .into_iter()
        .map(|name| {
            let read = |i: usize| blocks[i].same.get(name).cloned().unwrap_or_default();
            let ok = pairs.iter().all(|&(a, b)| disagree(&read(a), &read(b)).is_none() && !(read(a).never() && read(b).never()));
            let last = pairs.last().map(|&(a, b)| (read(a), read(b))).unwrap_or_default();
            json!({ "selector": name, "base": last.0.last(), "head": last.1.last(), "ok": ok, "pairs": pairs.len() })
        })
        .collect::<Vec<_>>()
        .into()
}

/// Where two blocks' reads of one element differ, with what each read there: after setup, or after
/// their measured runs or resets, run by run.
fn disagree(a: &Reads, b: &Reads) -> Option<(&'static str, Vec<String>, Vec<String>)> {
    [("after setup", a.setup.iter().cloned().collect(), b.setup.iter().cloned().collect()), ("after the runs", a.run.clone(), b.run.clone()), ("after the resets", a.reset.clone(), b.reset.clone())]
        .into_iter()
        .find(|(_, x, y)| x != y)
}

/// `NotEquivalent` for input latency: both sides' samples come from one rule (an input at rest, a
/// change of the app's model within a second, a present within a second), and a side whose runs gave
/// fewer than 0.8 or more than 1.25 times the other's samples measured other inputs (with a few
/// samples a block, one apart is alike: 2 and 3 are one input). By metric prefix (`interaction.`,
/// `scroll.latency.`).
pub(super) fn latency_skew(blocks: &[Block], names: (&str, &str)) -> Vec<(&'static str, Refusal)> {
    let mut out = Vec::new();
    for (prefix, count, what) in [("interaction.", "interaction.n", "inputs"), ("scroll.latency.", "scroll.n", "scroll gestures")] {
        let side = |role: &str| {
            let v: Vec<f64> = blocks.iter().filter(|b| b.variant == role).filter_map(|b| b.values.get(count).copied()).collect();
            (!v.is_empty()).then(|| metrics::median(&v))
        };
        if let (Some(b), Some(h)) = (side("base"), side("head"))
            && (b.min(h) <= 0.0 || !(0.8..=1.25).contains(&(h / b)))
            && !(b.min(h) >= 2.0 && (b - h).abs() <= 1.0)
            && b.max(h) > 0.0
        {
            out.push((
                prefix,
                Refusal::new(
                    ValidatorId::NotEquivalent,
                    format!(
                        "{} gave {b:.0} latency samples a block from its {what}, {} {h:.0}: outside 0.8 to 1.25 times each other (and more than one apart), so they timed other inputs",
                        names.0, names.1
                    ),
                )
                .fix("give both sides the same input at rest, each changing what the app shows"),
            ));
        }
    }
    out
}

/// strobe's reads of a side's page or tree inside its measured runs: per run, and their share of the
/// run's wall time (medians over its blocks), from each block's `driver.reads` and `driver.read_ms`.
fn reads_of(blocks: &[Block], role: &str) -> Option<(f64, f64)> {
    let mine: Vec<&Block> = blocks.iter().filter(|b| b.variant == role).collect();
    let per_run: Vec<f64> = mine.iter().filter_map(|b| b.values.get("driver.reads").copied()).collect();
    let share: Vec<f64> = mine.iter().filter_map(|b| Some(b.values.get("driver.read_ms")? / metrics::median(&b.run_ms).max(1e-3))).collect();
    (!per_run.is_empty()).then(|| (metrics::median(&per_run), if share.is_empty() { 0.0 } else { metrics::median(&share) }))
}

/// `ObserverCost` (SPEC §10.2): strobe reads each side through what it offers (a page's DOM, a native
/// app's accessibility tree), and a read inside a run costs the app work a person's use would not (a
/// lazy accessibility provider builds and updates its tree once a client reads it). Warned when a
/// side's reads took over 2% of its runs' wall time, when a side's runs read over 8 times, or when one
/// side reads at least 4 more times a run than the other and over 4 times as often.
pub(super) fn observer_cost(blocks: &[Block], names: (&str, &str)) -> Option<Warning> {
    let (b, h) = (reads_of(blocks, "base")?, reads_of(blocks, "head")?);
    let text = format!("strobe read {} {:.1} times a run ({:.1}% of its wall time) and {} {:.1} times a run ({:.1}%) inside the measured runs", names.0, b.0, 100.0 * b.1, names.1, h.0, 100.0 * h.1);
    let why = if b.1.max(h.1) > 0.02 {
        "reading took over 2% of a run, time the side being read spent answering strobe"
    } else if b.0.max(h.0) > 8.0 {
        "over 8 reads a run: each costs the side read work a person's use does not (wait on until: \"frame\" or \"idle\", which read nothing)"
    } else if (b.0 - h.0).abs() >= 4.0 && b.0.max(h.0) > 4.0 * b.0.min(h.0) {
        "one side is read over 4 times as often, and each read costs that app work the other does not do"
    } else {
        return None;
    };
    Some(validity::warn("ObserverCost", format!("{text}: {why}")))
}

/// Each side's reads per run, for `fairness.observer.reads_per_run`.
pub(super) fn reads_per_run(series: &[Value]) -> Value {
    let blocks: Vec<Block> = series.iter().flat_map(|s| s["blocks"].as_array().cloned().unwrap_or_default()).filter_map(|b| serde_json::from_value(b).ok()).collect();
    json!({ "base": reads_of(&blocks, "base").map(|r| r.0), "head": reads_of(&blocks, "head").map(|r| r.0) })
}

/// `--check` of a file with several targets: the parity matrix (SPEC §9.10). Each journey's row
/// gives every target's outcome (pass with its primary, FAIL, or the gap), and a last column the
/// `expect.same` reads where two or more ran, or why their journeys differ (`NotEquivalent`,
/// `InputMismatch`, which fail the check). Marks the series it refuses.
pub(super) fn matrix(series: &mut [Value]) -> Option<String> {
    let mut targets: Vec<(String, String)> = Vec::new();
    for s in series.iter() {
        let t = s["target"].as_str().unwrap_or("").to_string();
        if !targets.iter().any(|x| x.0 == t) {
            targets.push((t, s["side_name"].as_str().unwrap_or(s["target"].as_str().unwrap_or("")).to_string()));
        }
    }
    if targets.len() < 2 {
        return None;
    }
    // Columns by target, whichever target a filter left first.
    targets.sort();
    let mut journeys: Vec<String> = Vec::new();
    for s in series.iter() {
        let j = s["journey"].as_str().unwrap_or("").to_string();
        if !journeys.contains(&j) {
            journeys.push(j);
        }
    }
    let width = journeys.iter().map(|j| j.chars().count()).max().unwrap_or(7).max(7) + 2;
    let col = 34;
    let mut text = format!("{:<width$}{}same\n", "journey", targets.iter().map(|t| format!("{:<col$}", t.1)).collect::<String>());
    for j in &journeys {
        let idx: Vec<usize> = (0..series.len()).filter(|&i| series[i]["journey"].as_str() == Some(j.as_str())).collect();
        let mut row = format!("{j:<width$}");
        for (t, _) in &targets {
            let cell = match idx.iter().map(|&i| &series[i]).find(|s| s["target"].as_str() == Some(t.as_str())) {
                Some(s) if s["kind"] == "gap" => format!("gap  {}", s["gap"]["missing"].as_str().map(|m| format!("no {m}")).unwrap_or_else(|| s["gap"]["reason"].as_str().unwrap_or("").to_string())),
                Some(s) if s["passed"] == true => {
                    let p = s["primary"]["metric"].as_str().unwrap_or("");
                    format!("ok  {p} {}", s["values"][p].as_f64().map_or("—".into(), |v| fmt::value(v, model::metric(p).map_or("", |d| d.unit))))
                }
                Some(s) => format!("FAIL  {}", s["error"].as_str().and_then(|e| e.split_once(':')).map_or("", |(v, _)| v).chars().take(24).collect::<String>()),
                None => "—".into(),
            };
            row += &format!("{:<col$}", cell.chars().take(col - 2).collect::<String>());
        }
        // The targets that ran it, compared with the first: their reads and their input.
        let ran: Vec<usize> = idx.iter().copied().filter(|&i| series[i]["passed"] == true).collect();
        let block = |i: usize| Block {
            variant: String::new(),
            same: serde_json::from_value(series[i]["same"].clone()).unwrap_or_default(),
            ops: serde_json::from_value(series[i]["ops"].clone()).unwrap_or_default(),
            values: serde_json::from_value(series[i]["values"].clone()).unwrap_or_default(),
            ..Block::default()
        };
        let mut same = "—".to_string();
        if ran.len() >= 2 {
            let first = ran[0];
            let adapted = series[first]["adapted"].is_string();
            let refusal =
                ran[1..].iter().find_map(|&i| pair_refusal(adapted, (series[first]["side_name"].as_str().unwrap_or(""), series[i]["side_name"].as_str().unwrap_or("")), &block(first), &block(i)));
            match refusal {
                Some(r) => {
                    same = r.message();
                    for &i in &ran {
                        series[i]["journey_refusal"] = json!(r);
                    }
                }
                None => {
                    let reads = block(first).same;
                    if !reads.is_empty() {
                        same =
                            format!("ok  {}", reads.iter().map(|(k, r)| format!("{k} {:?}", r.last().map_or("", |s| s.as_str()).chars().take(40).collect::<String>())).collect::<Vec<_>>().join(" · "));
                    }
                }
            }
        }
        text += &format!("{row}{same}\n");
    }
    Some(text)
}
