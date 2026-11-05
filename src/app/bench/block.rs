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

/// The bench file's `ctx.count` and `ctx.phase` during a block: they count only inside a run, whose
/// key (`run:<i>`) names its phase marks.
#[derive(Default)]
pub(super) struct Hooks {
    run: RefCell<String>,
    counts: RefCell<BTreeMap<String, f64>>,
    /// Phase edges on the host clock, for targets strobe times itself (native, script).
    marks: RefCell<Vec<(String, u64)>>,
}

impl Hooks {
    /// A run starts: what it counts and marks starts empty.
    pub(super) fn begin(&self, key: &str) {
        *self.run.borrow_mut() = key.to_string();
        self.counts.borrow_mut().clear();
        self.marks.borrow_mut().clear();
    }

    /// The run ended: from here nothing counts.
    pub(super) fn end(&self) {
        self.run.borrow_mut().clear();
    }

    pub(super) fn in_run(&self) -> bool {
        !self.run.borrow().is_empty()
    }

    /// `ctx.count(name, n)`; outside a run it counts nothing.
    pub(super) fn count(&self, a: &Value) {
        if self.in_run() {
            self.add(&BTreeMap::from([(a["name"].as_str().unwrap_or("").to_string(), a["n"].as_f64().unwrap_or(1.0))]));
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
    for m in declared_metrics(s) {
        if let Some(name) = m.strip_prefix("count.") {
            extra.insert(m.clone(), runs.iter().enumerate().map(|(j, (c, _))| (key(j), c.get(name).copied().unwrap_or(0.0))).collect());
        } else if let Some(name) = m.strip_prefix("phase.") {
            extra.insert(m.clone(), runs.iter().enumerate().map(|(j, (_, marks))| (key(j), super::phase_ms(marks, name))).filter(|(_, x)| x.is_finite()).collect());
        }
    }
    extra
}
