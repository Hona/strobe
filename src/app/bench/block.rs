//! What every block does the same way, whatever runs it (a page, a native app, a script): warm up
//! until two runs do the same work, and serve the bench file's `ctx.count` and `ctx.phase` during a
//! run, then turn them into `count.<name>` and `phase.<name>` per run.

use super::*;

/// Two runs did the same work: within 5%, or within 1 ms (an idle run does about 3 ms, where 5% is
/// noise). Work that was not read (NaN) is never steady.
fn steady(a: f64, b: f64) -> bool {
    a.is_finite() && b.is_finite() && ((a - b).abs() <= 1.0 || (a.max(b) > 0.0 && (a - b).abs() / a.max(b) <= 0.05))
}

/// Warmup until steady (SPEC §9.2): runs go on until two consecutive ones do the same work, then
/// `Exec::runs` more are measured (a leak run's: twice that). Both steady runs count as measured in a
/// timing block; an attribution or memory block starts its probes at the steady point and measures
/// the runs after it, as does a block that never got steady in `MAX_WARMUP` runs. An untimed block
/// measures its first run.
pub(super) struct Warmup {
    /// The first measured run, once decided.
    first: Option<usize>,
    unsteady: bool,
    untimed: bool,
    /// Whether the two steady runs are measured (timing blocks).
    keeps_pair: bool,
    measured: usize,
}

impl Warmup {
    pub(super) fn new(x: &Exec) -> Warmup {
        let measured = if x.set == Set::Memory { 2 * x.runs } else { x.runs };
        Warmup { first: None, unsteady: false, untimed: x.untimed, keeps_pair: !matches!(x.set, Set::Attribution | Set::Memory), measured }
    }

    /// Whether another run is due after `ran` runs.
    pub(super) fn more(&self, ran: usize) -> bool {
        self.first.is_none_or(|f| ran < f + self.measured)
    }

    /// The first measured run, once warmup is over.
    pub(super) fn first(&self) -> Option<usize> {
        self.first
    }

    /// Never steady in `MAX_WARMUP` runs (`warmup.unsteady`, `NotSteady`).
    pub(super) fn unsteady(&self) -> bool {
        self.unsteady
    }

    /// After a run, with each run's work so far: true when warmup ended with this run.
    pub(super) fn after(&mut self, work: &[f64]) -> bool {
        if self.first.is_some() || work.is_empty() {
            return false;
        }
        let i = work.len() - 1;
        if self.untimed {
            self.first = Some(0);
            return true;
        }
        let steady = i >= 1 && steady(work[i - 1], work[i]);
        if !steady && i < MAX_WARMUP {
            return false;
        }
        self.unsteady = !steady;
        self.first = Some(if steady && self.keeps_pair { i - 1 } else { i + 1 });
        true
    }
}

/// A run's count that is the fixture's CPU (ms), not the bench file's: `cpu.fixture`.
pub(super) const FIXTURE_CPU: &str = "\u{1}cpu.fixture";

/// The bench file's `ctx.count` and `ctx.phase` during a block: they count only inside a run, whose
/// key (`run:<i>`) names its phase marks. With a fixture, its CPU over each run, and whether the
/// target's processes hold a connection to it at each run's start and end (outside the run).
#[derive(Default)]
pub(super) struct Hooks<'a> {
    run: RefCell<String>,
    counts: RefCell<BTreeMap<String, f64>>,
    /// Phase edges on the host clock, for targets strobe times itself (native, script).
    marks: RefCell<Vec<(String, u64)>>,
    fixture: Option<FixtureHook<'a>>,
    /// The fixture's CPU at the run's start.
    fixture_at: Cell<Option<CpuAt>>,
}

/// The block's fixture, and how to list the target's processes now.
type FixtureHook<'a> = (std::rc::Rc<super::fixture::Running>, Box<dyn Fn() -> Vec<u32> + 'a>);

/// A tree's CPU reading (`script::cpu`) and when it was taken.
type CpuAt = ((u64, Option<u64>), Instant);

impl<'a> Hooks<'a> {
    /// A block's hooks; `pids` lists the target's processes for the fixture's connection check.
    pub(super) fn new(ctx: &Ctx, pids: impl Fn() -> Vec<u32> + 'a) -> Hooks<'a> {
        Hooks { fixture: ctx.fixture_running.clone().map(|f| (f, Box::new(pids) as Box<dyn Fn() -> Vec<u32> + 'a>)), ..Hooks::default() }
    }

    /// Reads whether the target holds a connection to the block's fixture (after setup).
    pub(super) fn check_fixture(&self) {
        if let Some((f, pids)) = &self.fixture {
            f.check(&pids());
        }
    }

    /// A run starts: what it counts and marks starts empty.
    pub(super) fn begin(&self, key: &str) {
        *self.run.borrow_mut() = key.to_string();
        self.counts.borrow_mut().clear();
        self.marks.borrow_mut().clear();
        if let Some((f, pids)) = &self.fixture {
            f.check(&pids());
            self.fixture_at.set(Some((f.cpu(), Instant::now())));
        }
    }

    /// The run ended: from here nothing counts.
    pub(super) fn end(&self) {
        self.run.borrow_mut().clear();
        if let (Some((f, pids)), Some((a, at))) = (&self.fixture, self.fixture_at.take()) {
            let ms = super::script::cpu_ms(a, f.cpu(), at.elapsed().as_secs_f64() * 1e3);
            self.counts.borrow_mut().insert(FIXTURE_CPU.into(), ms);
            f.check(&pids());
        }
    }

    pub(super) fn in_run(&self) -> bool {
        !self.run.borrow().is_empty()
    }

    /// `ctx.count(name, n)`; outside a run it counts nothing.
    pub(super) fn count(&self, a: &Value) {
        if self.in_run() && a["name"] != FIXTURE_CPU {
            self.add(&BTreeMap::from([(a["name"].as_str().unwrap_or("").to_string(), a["n"].as_f64().unwrap_or(1.0))]));
        }
    }

    /// An input call of the bench file's run (`journey::op_count`), counted per run for `InputMismatch`.
    pub(super) fn op(&self, call: &str, args: &[Value]) {
        if let Some(n) = super::journey::op_count(call, args).filter(|_| self.in_run()) {
            self.add(&BTreeMap::from([(format!("{}{call}", super::journey::OP), n)]));
        }
    }

    /// Counts from another source of the run (a page's counter global).
    pub(super) fn add(&self, from: &BTreeMap<String, f64>) {
        let mut c = self.counts.borrow_mut();
        for (k, n) in from {
            *c.entry(k.clone()).or_default() += n;
        }
    }

    pub(super) fn counts(&self) -> BTreeMap<String, f64> {
        self.counts.borrow().clone()
    }

    /// `ctx.phase` edge as a mark name, `run:<i>:phase:<name>:start|end` (the name with anything but
    /// letters, digits, `-`, `_` and `.` made `_`); none outside a run.
    pub(super) fn phase_key(&self, a: &Value) -> Option<String> {
        let run = self.run.borrow();
        if run.is_empty() {
            return None;
        }
        let name: String = a["name"].as_str().unwrap_or("phase").chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
        Some(format!("{run}:phase:{name}:{}", a["edge"].as_str().unwrap_or("start")))
    }

    /// `ctx.phase` edge marked on the host clock.
    pub(super) fn phase(&self, a: &Value) {
        if let Some(key) = self.phase_key(a) {
            self.marks.borrow_mut().push((key, host::now_ns()));
        }
    }

    pub(super) fn take_marks(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut self.marks.borrow_mut())
    }
}

/// One measured run's counts and host-clock phase marks.
pub(super) type Hooked<'a> = (&'a BTreeMap<String, f64>, &'a [(String, u64)]);

/// `count.<name>` and `phase.<name>` per measured run for the series' declared metrics, from each
/// run's counts and host-clock phase marks (`phase_ms`).
pub(super) fn per_run_hooks(s: &Series, key: &dyn Fn(usize) -> String, runs: &[Hooked]) -> BTreeMap<String, BTreeMap<String, f64>> {
    let mut extra = BTreeMap::new();
    // The fixture's CPU per run, whether declared or not: result.json's fixture says it per side.
    if runs.iter().any(|(c, _)| c.contains_key(FIXTURE_CPU)) {
        extra.insert("cpu.fixture".to_string(), runs.iter().enumerate().filter_map(|(j, (c, _))| Some((key(j), *c.get(FIXTURE_CPU)?))).collect());
    }
    for m in declared_metrics(s) {
        if let Some(name) = m.strip_prefix("count.") {
            extra.insert(m.clone(), runs.iter().enumerate().map(|(j, (c, _))| (key(j), c.get(name).copied().unwrap_or(0.0))).collect());
        } else if let Some(name) = m.strip_prefix("phase.") {
            extra.insert(m.clone(), runs.iter().enumerate().map(|(j, (_, marks))| (key(j), super::phase_ms(marks, name))).filter(|(_, x)| x.is_finite()).collect());
        }
    }
    extra
}
