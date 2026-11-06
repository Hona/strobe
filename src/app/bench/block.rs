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
    /// The run's `ctx.untimed` stretches on the host clock, the one open now (its start, and the
    /// fixture's CPU then), and the fixture's CPU in the closed ones (ms).
    untimed: RefCell<Vec<(u64, u64)>>,
    open: Cell<Option<(u64, Option<CpuAt>)>>,
    fixture_untimed: Cell<f64>,
    /// No input call since the last stretch ended (`ended_untimed`).
    ended: Cell<bool>,
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
        self.untimed.borrow_mut().clear();
        self.open.set(None);
        self.fixture_untimed.set(0.0);
        self.ended.set(false);
        if let Some((f, pids)) = &self.fixture {
            f.check(&pids());
            self.fixture_at.set(Some((f.cpu(), Instant::now())));
        }
    }

    /// The run ended: from here nothing counts. A stretch still open (its end never came: the run
    /// threw) ends here.
    pub(super) fn end(&self) {
        if let Some((from, _)) = self.open.take() {
            self.untimed.borrow_mut().push((from, host::now_ns()));
        }
        self.run.borrow_mut().clear();
        if let (Some((f, pids)), Some((a, at))) = (&self.fixture, self.fixture_at.take()) {
            let ms = super::script::cpu_ms(a, f.cpu(), at.elapsed().as_secs_f64() * 1e3) - self.fixture_untimed.get();
            self.counts.borrow_mut().insert(FIXTURE_CPU.into(), ms);
            f.check(&pids());
        }
    }

    /// A `ctx.untimed` edge (SPEC §9.6): the stretch between its start and its end is left out of the
    /// run's time, CPU, frames and inputs. Inside a run `read` takes the target's readings for the
    /// edge (true at the start, and the stretch's index in the run) inside the stretch: after its
    /// start is stamped and before its end is, so what they take (a system snapshot is 15-20 ms on
    /// Windows) is in neither the run's time nor its phases. Outside a run (setup, reset) nothing is
    /// timed, so there is nothing to do. One stretch inside another is a usage error.
    pub(super) fn untimed(&self, a: &Value, read: impl FnOnce(bool, usize) -> Result<()>) -> Result<Value> {
        if !self.in_run() {
            return Ok(Value::Bool(false));
        }
        let n = self.untimed.borrow().len();
        let fixture = || self.fixture.as_ref().map(|(f, _)| (f.cpu(), Instant::now()));
        if a["edge"] == "start" {
            if self.open.get().is_some() {
                return Err(Error::usage("ctx.untimed inside ctx.untimed: one stretch at a time"));
            }
            self.open.set(Some((host::now_ns(), fixture())));
            return read(true, n).map(|_| Value::Bool(true));
        }
        let Some((from, cpu)) = self.open.get() else { return Ok(Value::Bool(false)) };
        let r = read(false, n);
        if let (Some((a, at)), Some((b, _))) = (cpu, fixture()) {
            let ms = super::script::cpu_ms(a, b, at.elapsed().as_secs_f64() * 1e3);
            if ms.is_finite() {
                self.fixture_untimed.set(self.fixture_untimed.get() + ms);
            }
        }
        self.open.set(None);
        self.untimed.borrow_mut().push((from, host::now_ns()));
        self.ended.set(true);
        r.map(|_| Value::Bool(true))
    }

    /// Whether an untimed stretch is open now.
    pub(super) fn untimed_now(&self) -> bool {
        self.open.get().is_some()
    }

    /// Whether the run's last step was an untimed stretch: no input call after its end.
    pub(super) fn ended_untimed(&self) -> bool {
        self.ended.get()
    }

    /// The run's untimed stretches so far, on the host clock.
    pub(super) fn untimed_stretches(&self) -> Vec<(u64, u64)> {
        self.untimed.borrow().clone()
    }

    /// The trace mark of an untimed edge: `untimed:<run>.<n>:start|end` (no `:run:` in it, so no reader
    /// of run windows mistakes it for a run; `metrics::BlockData` takes it out of its run).
    pub(super) fn untimed_mark(&self, n: usize, start: bool) -> String {
        let run = self.run.borrow();
        format!("untimed:{}.{n}:{}", run.trim_start_matches("run:"), if start { "start" } else { "end" })
    }

    pub(super) fn in_run(&self) -> bool {
        !self.run.borrow().is_empty()
    }

    /// `ctx.count(name, n)`; outside a run, or in an untimed stretch, it counts nothing.
    pub(super) fn count(&self, a: &Value) {
        if self.in_run() && !self.untimed_now() && a["name"] != FIXTURE_CPU {
            self.add(&BTreeMap::from([(a["name"].as_str().unwrap_or("").to_string(), a["n"].as_f64().unwrap_or(1.0))]));
        }
    }

    /// An input call of the bench file's run (`journey::op_count`), counted per run for `InputMismatch`;
    /// one in an untimed stretch is not the journey's.
    pub(super) fn op(&self, call: &str, args: &[Value]) {
        if !self.untimed_now() {
            self.ended.set(false);
        }
        if let Some(n) = super::journey::op_count(call, args).filter(|_| self.in_run() && !self.untimed_now()) {
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
    /// letters, digits, `-`, `_` and `.` made `_`); none outside a run. A phase inside an untimed
    /// stretch is a usage error: it would time what the stretch leaves out (a stretch inside a phase
    /// leaves the phase).
    pub(super) fn phase_key(&self, a: &Value) -> Result<Option<String>> {
        let run = self.run.borrow();
        if run.is_empty() {
            return Ok(None);
        }
        let name: String = a["name"].as_str().unwrap_or("phase").chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
        if self.untimed_now() {
            return Err(Error::usage(format!("ctx.phase({name:?}) inside ctx.untimed: a stretch leaves the phases it is in, so a phase in it would time nothing; put the untimed stretch inside the phase")));
        }
        Ok(Some(format!("{run}:phase:{name}:{}", a["edge"].as_str().unwrap_or("start"))))
    }

    /// `ctx.phase` edge marked on the host clock.
    pub(super) fn phase(&self, a: &Value) -> Result<Value> {
        if let Some(key) = self.phase_key(a)? {
            self.marks.borrow_mut().push((key, host::now_ns()));
        }
        Ok(Value::Bool(true))
    }

    pub(super) fn take_marks(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut self.marks.borrow_mut())
    }
}

/// One measured run's counts, host-clock phase marks and timed span.
pub(super) type Hooked<'a> = (&'a BTreeMap<String, f64>, &'a [(String, u64)], &'a metrics::Timed);

/// `count.<name>` and `phase.<name>` per measured run for the series' declared metrics, from each
/// run's counts and host-clock phase marks (`phase_ms`).
pub(super) fn per_run_hooks(s: &Series, key: &dyn Fn(usize) -> String, runs: &[Hooked]) -> BTreeMap<String, BTreeMap<String, f64>> {
    let mut extra = BTreeMap::new();
    // The fixture's CPU per run, whether declared or not: result.json's fixture says it per side.
    if runs.iter().any(|(c, _, _)| c.contains_key(FIXTURE_CPU)) {
        extra.insert("cpu.fixture".to_string(), runs.iter().enumerate().filter_map(|(j, (c, _, _))| Some((key(j), *c.get(FIXTURE_CPU)?))).collect());
    }
    for m in declared_metrics(s) {
        if let Some(name) = m.strip_prefix("count.") {
            extra.insert(m.clone(), runs.iter().enumerate().map(|(j, (c, _, _))| (key(j), c.get(name).copied().unwrap_or(0.0))).collect());
        } else if let Some(name) = m.strip_prefix("phase.") {
            extra.insert(m.clone(), runs.iter().enumerate().map(|(j, (_, marks, t))| (key(j), super::phase_ms(marks, name, t))).filter(|(_, x)| x.is_finite()).collect());
        }
    }
    extra
}

/// Run totals strobe cannot take a `ctx.untimed` stretch out of (SPEC §9.6): sampled over a window
/// (GPU use), counted by the OS per process without a reading at each edge (energy, instructions,
/// idle wakes), kept by the page per run (requests, canvas frames), or read after the run (the heap
/// it retained). A run that left a stretch out has none of them.
const NOT_UNTIMED: &[&str] = &["gpu.process.", "energy.", "cpu.instructions", "cpu.idle_wakes", "net.", "canvas.", "js.heap.retained"];

/// When a measured run used `ctx.untimed`: the block's `untimed.n` and `untimed.ms`, and the run
/// totals in `NOT_UNTIMED` refused (`Untimed`): a declared one is listed refused, an undeclared one
/// leaves the values.
pub(super) fn untimed_state(s: &Series, runs: &[&metrics::Timed], values: &mut BTreeMap<String, f64>, refused: &mut BTreeMap<String, Refusal>) {
    if runs.iter().all(|r| r.untimed.is_empty()) {
        return;
    }
    super::untimed_values(runs, values);
    let hit = |m: &str| NOT_UNTIMED.iter().any(|p| m.starts_with(p));
    values.retain(|k, _| !hit(k));
    for m in declared_metrics(s).into_iter().filter(|m| hit(m)) {
        refused.entry(m.clone()).or_insert_with(|| {
            Refusal::new(ValidatorId::Untimed, format!("{m}: the runs left stretches out with ctx.untimed, and strobe cannot take a stretch out of this total"))
                .fix("time the case without ctx.untimed for this metric, or wait with until: on the step before")
        });
    }
}
