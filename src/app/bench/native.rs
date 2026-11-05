//! Native targets (SPEC §9.6, §13.7): an app with no browser engine, launched in an isolated home per
//! block. From outside strobe measures its processes' CPU in cycles, its threads' wakes, IO, GPU and
//! memory, what reached the screen (OS present events), and for launches its window's pixels. The
//! bench file's `page` drives it through the OS's accessibility tree and its own message loop
//! (`driver::native`), and input latency comes from those inputs to the presents that showed them.

use super::launch::{Line, found_all, joined, milestones, no_page, screen_times, tail};
use super::*;
use crate::driver::native::{Log, Session};
use crate::probe::present::{PresentCapture, frames};
use crate::probe::memory;
use crate::probe::wakes::{self, Snapshot, Span};
use crate::target::native::{self as nt, Native};
use std::cell::OnceCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a native target launches for a variant: its app profile (`native(appId)`) or a bare
/// executable (`native(path)`), with the variant's own build or `exe:` in place of the profile's
/// executable, and the target's `args` and `env` added.
pub(super) fn app(v: &Variant, target: &Value, config: &Value) -> Result<isolate::App> {
    let base = match (&v.kind, target["app"].as_str()) {
        (Kind::App(id), _) => isolate::app(id, config)?,
        (_, Some(id)) => isolate::app(id, config)?,
        _ => {
            let path = target["path"].as_str().ok_or_else(|| Error::usage("native(): give an app profile id or an executable path"))?;
            let mut a = isolate::App::bare(Path::new(path));
            a.kind = "native".into();
            a
        }
    };
    let base = match (&v.kind, &v.exe) {
        (Kind::Exe(p), _) => base.with_exe(p),
        (_, Some(p)) if target["path"].is_null() => base.with_exe(p),
        _ => base,
    };
    let args: Vec<String> = target["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
    let env: Vec<(String, String)> = target["env"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect();
    let unset: Vec<String> = target["unset"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
    Ok(base.with_extra(&args, &env).with_unset(&unset))
}

/// The driving session for one launch, opened when a page call first needs it: the app has then
/// shown its window.
struct Drive {
    session: OnceCell<Session>,
    pid: u32,
    home: PathBuf,
    /// The main window once found (0: none, a windowless app).
    main: Cell<isize>,
}

impl Drive {
    fn pids(&self) -> Vec<u32> {
        host::process_set(self.pid, Some(&self.home)).iter().map(|p| p.pid).collect()
    }

    fn get(&self) -> Result<&Session> {
        if let Some(s) = self.session.get() {
            return Ok(s);
        }
        if self.main.get() == 0 {
            return Err(Error::usage("the app shows no window to drive (window: false)"));
        }
        let s = Session::open(&self.pids(), self.main.get())?;
        let _ = self.session.set(s);
        self.session.get().ok_or_else(|| Error::msg("internal: the driving session"))
    }

    /// The app may replace its main window (a splash, then the app): the session follows it.
    fn follow(&self, n: &Native) {
        if let Some(w) = &n.window {
            self.main.set(w.hwnd);
            if let Some(s) = self.session.get() {
                s.set_main(w.hwnd);
            }
        }
    }
}

/// The target's processes' CPU so far in cycles, and the time stamp counter.
fn cycles(n: &Native) -> (u64, Option<u64>) {
    (n.processes().iter().map(|p| p.cycles).sum(), host::tsc())
}

/// CPU ms between two `cycles` readings over `wall_ms`; unknown (NaN) without cycles.
fn cpu_ms(a: (u64, Option<u64>), b: (u64, Option<u64>), wall_ms: f64) -> f64 {
    match (a.1, b.1) {
        (Some(t0), Some(t1)) if t1 > t0 && b.0 > 0 => b.0.saturating_sub(a.0) as f64 / (t1 - t0) as f64 * wall_ms,
        _ => f64::NAN,
    }
}

/// One measured run.
struct Run {
    ms: f64,
    cpu: f64,
    span: Span,
    counts: BTreeMap<String, f64>,
    /// What the page calls did in the run: inputs and phase marks (host clock).
    log: Log,
    /// When the app announced accessibility changes, from the run's start to a second after its end.
    changes: Vec<u64>,
    /// Elements in the app's windows at the run's end, when strobe drives it (`StateDrift`).
    elements: Option<usize>,
    t0: u64,
    t1: u64,
    total: f64,
}

/// Metrics read from frames: refused together when the app spun unpaced.
fn frame_metric(metric: &str) -> bool {
    ["frames.", "interaction.", "scroll.", "display."].iter().any(|p| metric.starts_with(p))
}

pub(super) fn block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let set = x.set;
    let target = ctx.describe["targets"][&s.target].clone();
    let app = with_gpu_cache(ctx, s, v, &target, app(v, &target, &ctx.config)?)?;
    // `DevBuild`: an executable that imports a debug C runtime is a debug build (timing blocks only).
    let timed = set != Set::Attribution && set != Set::Memory && x.pixels.is_none();
    if let Some(crt) = host::debug_runtime(&app.exe).filter(|_| timed) {
        return Err(Error::refused(
            "DevBuild",
            format!("{} imports {crt}, a debug C runtime: its timings describe no build people run", app.exe.display()),
            "build the app optimized (strobe.json build: a release or profiling profile)",
        ));
    }
    // `Throttled`: there is no page to run the speed check in, so strobe runs a fixed workload of its
    // own on the target's CPUs before the launch, once the last block's app has left them quiet.
    let ratio = validity::native_speed_checked(ctx.pin)?;
    let home = isolate::prepare_home(&app, seed_dir(ctx, s, &target).as_deref(), None)?;
    crate::target::a11y::prepare();
    let launched = Instant::now();
    // Its output goes nowhere unless the target asks for it (`output: true`, for `ctx.waitForLine`):
    // a pipe changes how a C runtime buffers stdout (a pipe is block-buffered, the null device is not).
    let (mut n, guard) = match nt::launch(&app, &home, &[], target["output"] == true, ctx.pin) {
        Ok(x) => x,
        Err(e) => {
            isolate::release_home(&home.dir);
            isolate::remove_home(&home.dir);
            return Err(e);
        }
    };
    let drive = Drive { session: OnceCell::new(), pid: n.pid, home: home.dir.clone(), main: Cell::new(0) };
    ctx.target_qos = host::background_qos(Some(n.pid));
    // The per-thread timeline, from the launch to the last run's end, in untimed blocks (`--check`,
    // attribution): its system snapshot each second costs strobe about 1% of a core.
    let timeline = (x.untimed || set == Set::Attribution).then(|| {
        let (root, dir) = (n.pid, home.dir.clone());
        wakes::Timeline::start(move || host::process_set(root, Some(&dir)).iter().map(|p| p.pid).collect())
    });
    let mut steps = vec![("launch".to_string(), launched.elapsed().as_secs_f64() * 1e3)];
    let result = runs(ctx, s, v, pair, x, &target, &app, &home, &mut n, &guard, &drive, &mut steps, ratio);
    if let (Some(t), Ok(b)) = (timeline, &result) {
        let t = t.stop();
        if let Some(dir) = b.recording.as_deref().and_then(|id| store::resolve(id).ok()) {
            let _ = crate::symbol::write_gz(&dir.join("timeline.json.gz"), &t);
        }
    }
    let closing = Instant::now();
    if let Some(s) = drive.session.get() {
        s.release_all();
    }
    drop(drive);
    n.close(true);
    guard.check("after a block")?;
    result.map(|mut b| {
        b.steps = steps;
        b.steps.push(("close".into(), closing.elapsed().as_secs_f64() * 1e3));
        b
    })
}

/// `VariantMismatch` when base and head drew on different adapters or at different scales: a
/// difference there is not the change.
fn admit_setup(ctx: &mut Ctx, s: &Series, v: &Variant, adapter: Option<&str>, scale: Option<f64>) -> Result<()> {
    let setup = format!("{} at scale {}", adapter.unwrap_or("no GPU work"), scale.map_or("?".into(), |s| s.to_string()));
    let mine = format!("{}\n{}", s.target, v.role);
    if let Some((key, other)) = ctx.native_setup.iter().find(|(k, o)| k.split('\n').next() == Some(s.target.as_str()) && **k != mine && **o != setup) {
        return Err(Error::refused(
            "VariantMismatch",
            format!("{} drew on {other}, {} on {setup}", key.split('\n').nth(1).unwrap_or(""), v.role),
            "compare builds on the same adapter and scale; a difference there is not the change",
        ));
    }
    ctx.native_setup.insert(mine, setup);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn runs(
    ctx: &mut Ctx,
    s: &Series,
    v: &Variant,
    pair: usize,
    x: &Exec,
    target: &Value,
    app: &isolate::App,
    home: &isolate::Home,
    n: &mut Native,
    guard: &isolate::Guard,
    drive: &Drive,
    steps: &mut Vec<(String, f64)>,
    ratio: Option<f64>,
) -> Result<Block> {
    let (set, keep) = (x.set, x.keep);
    let lapped = Cell::new(Instant::now());
    let lap = |name: &str, steps: &mut Vec<(String, f64)>| {
        steps.push((name.to_string(), lapped.get().elapsed().as_secs_f64() * 1e3));
        lapped.set(Instant::now());
    };
    // A windowless app (a tray app, `window: false`) has no window to wait for, place or check.
    let windowless = target["window"] == false || app.windowless;
    if !windowless {
        n.wait_window(Duration::from_secs(60), guard)?;
        let viewport = target.get("viewport").filter(|v| v.is_object());
        let mut at = ctx.window.clone();
        if let Some(vp) = viewport {
            at.width = vp["width"].as_u64().unwrap_or(at.width as u64) as u32;
            at.height = vp["height"].as_u64().unwrap_or(at.height as u64) as u32;
        }
        if let Some(w) = n.place(&at, viewport.is_some()) {
            let scale = w.scale();
            ctx.used = Some(Window { x: w.rect.0, y: w.rect.1, width: (w.client.0 as f64 / scale).round() as u32, height: (w.client.1 as f64 / scale).round() as u32, scale: Some(scale) });
        }
    }
    drive.follow(n);
    guard.check("after the app opened its window")?;
    lap("attach", steps);
    // What reached the screen, from the OS's present events, for the whole block: page waits
    // (`until: "frame"`, idle) read it live, and frame metrics read the measured runs. Native stacks
    // in attribution blocks, through the helper.
    let presents: RefCell<Option<PresentCapture>> = RefCell::new(None);
    let mut presents_err: Option<String> = None;
    if !windowless && set != Set::Attribution && set != Set::Memory && !x.counters {
        match PresentCapture::start() {
            Ok(mut c) => {
                c.track(&n.pids());
                *presents.borrow_mut() = Some(c);
            }
            Err(e) => presents_err = Some(e.message),
        }
    }
    let stacks = (set == Set::Attribution && crate::probe::stacks::stacks_available()).then(|| crate::probe::stacks::StackCapture::start(crate::probe::stacks::Targets::new(n.pid, Some(&n.home))));
    let hooks = super::block::Hooks::default();
    let pixels = x.pixels.clone();
    let lines = RefCell::new(super::launch::Lines::default());
    let output = n.output.take();
    let serve = |op: &str, a: &Value| -> Result<Value> {
        match op {
            "line" => lines.borrow_mut().serve(output.as_ref(), &home.dir, a),
            "driver" => {
                let call = a["call"].as_str().unwrap_or("");
                let args = a["args"].as_array().cloned().unwrap_or_default();
                // A plain wait needs nothing from the app.
                if call == "wait" || (call == "snapshot" && pixels.is_none()) {
                    let ms = if call == "wait" { args.first().and_then(Value::as_f64).unwrap_or(0.0).max(0.0) } else { 0.0 };
                    std::thread::sleep(Duration::from_millis(ms as u64));
                    return Ok(Value::Bool(call == "wait"));
                }
                let session = drive.get().map_err(|e| Error { message: format!("page.{call}(): {}", e.message), ..e })?;
                session.dispatch(call, &args, &presents, pixels.as_deref())
            }
            "count" => {
                hooks.count(a);
                Ok(Value::Bool(true))
            }
            "phase" => {
                hooks.phase(a);
                Ok(Value::Bool(true))
            }
            other => Err(Error::msg(format!("unknown runner request {other}"))),
        }
    };
    let t = s.hook_timeout();
    let env = home.environment(app);
    // An app that exited is refused, never measured as a gone process (a windowless one has no window
    // whose loss would say so): its exit code and last output lines, when its output is read.
    let last = || output.as_ref().map(|rx| lines.borrow_mut().last(Some(rx)));
    ctx.runner.call(
        "block",
        json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": v.root, "home": home.dir, "env": env, "exe": app.exe }),
        t,
        &serve,
    )?;
    super::super::record::app_gone(n, "before the case's setup", |_| last())?;
    if !windowless {
        n.raise();
        n.check_visible()?;
        drive.follow(n);
    }
    lap("checks", steps);
    let setup = ctx.runner.call("setup", json!({}), t, &serve);
    super::super::record::app_gone(n, "before the runs", |_| last())?;
    setup.map_err(|e| crate::runner::incomplete("setup", "setup", e, "", "fix the bench file's setup"))?;
    lap("setup", steps);
    // What the window shows before the runs: its titles and pixels, and its tree only when setup
    // already drove the app (reading it now would connect strobe to the app for the runs).
    let main = n.window.as_ref().map(|w| w.hwnd);
    let start = (!windowless).then(|| {
        let se = drive.session.get().ok_or_else(|| "not read before the runs (the case had not driven the app yet)".to_string());
        shown::native(ctx, s, v, pair, "start", &n.pids(), main, se)
    });
    if let Some(sc) = &start {
        let se = drive.session.get();
        shown::expect(&s.expect, sc, "before the runs", se.is_some(), &|sel| se.map_or(Ok(true), |se| se.shows(sel, Duration::from_secs(5))))?;
    }
    let procs0 = (host::now_ns(), host::all_processes());
    let sampler = probe::os::Sampler::start(n.pid);
    let sampled = Instant::now();
    let mut cost = SelfCost::new(ctx.runner.pid());
    // The thread that owns the main window: the app's UI thread, whose CPU is `ui.cpu`.
    let ui_tid = n.window.as_ref().and_then(|w| host::window_thread(w.hwnd));
    let run_timeout = s.run_timeout();
    let mut runs: Vec<Run> = Vec::new();
    // Steady once two runs do the same work (SPEC §9.2). A leak run (`--probes memory`, SPEC §8.6):
    // 2 × N runs after warmup, the processes' memory read before the first and after each, between runs.
    let mut warmup = super::block::Warmup::new(x);
    let perf = x.counters.then(wakes::Perf::new);
    let leak = set == Set::Memory;
    let mut readings: Vec<memory::Reading> = Vec::new();
    let mut starts = shown::Starts::new(set);
    while warmup.more(runs.len()) {
        let i = runs.len();
        let began = Instant::now();
        if !windowless {
            n.raise();
        }
        // One read of the app's processes per end of a run: what the session follows, what the
        // snapshot covers, and (at the end) the cycles.
        let listed = wakes::wall_us();
        let pids = n.pids();
        if let Some(se) = drive.session.get() {
            se.set_pids(&pids);
            se.take_log();
        }
        // Every run starts from the same screen, read through the session the case drives the app
        // with (none yet: nothing read, so strobe connects to no app the case did not drive).
        starts.check(i + 1, false, &|| drive.session.get().and_then(shown::native_start))?;
        hooks.begin(&format!("run:{i}"));
        // The process's CPU goes with it when it exits, so a run it exits in is no run of the app.
        let running = n.exited().is_none();
        let snap0 = Snapshot::take_since(listed, &pids, perf.as_ref());
        let c0 = cycles(n);
        cost.start();
        let t0 = host::now_ns();
        let ran = ctx.runner.call("run", json!({ "case": s.case }), run_timeout, &serve);
        let t1 = host::now_ns();
        cost.end();
        let procs1 = n.processes();
        let c1 = (procs1.iter().map(|p| p.cycles).sum(), host::tsc());
        let snap1 = Snapshot::take_since(wakes::wall_us(), &procs1.iter().map(|p| p.pid).collect::<Vec<_>>(), perf.as_ref());
        match n.exited() {
            Some(code) if running => return Err(super::super::record::exited(code, &format!("during run {}", i + 1), last().as_deref())),
            _ => super::super::record::app_gone(n, &format!("before run {}", i + 1), |_| last())?,
        }
        ran.map_err(|e| crate::runner::incomplete(&format!("{} run", s.case), "run", e, "", "the run threw or timed out; fix the bench file or the app"))?;
        hooks.end();
        let mut log = drive.session.get().map(Session::take_log).unwrap_or_default();
        log.marks.extend(hooks.take_marks());
        let elements = drive.session.get().and_then(|se| se.count_elements().ok());
        let ms = (t1 - t0) as f64 / 1e6;
        runs.push(Run { ms, cpu: cpu_ms(c0, c1, ms), span: wakes::span(&snap0, &snap1), counts: hooks.counts(), log, changes: Vec::new(), elements, t0, t1, total: 0.0 });
        // A key or button the run left held would be down in the next one.
        if let Some(se) = drive.session.get() {
            se.release_all();
        }
        if s.reset {
            ctx.runner
                .call("reset", json!({ "case": s.case }), run_timeout, &serve)
                .map_err(|e| crate::runner::incomplete(&format!("{} reset", s.case), "reset", e, "", "fix the case's reset"))?;
            if let Some(se) = drive.session.get() {
                se.release_all();
            }
        }
        runs[i].total = began.elapsed().as_secs_f64() * 1e3;
        if let (true, Some(f)) = (leak, warmup.first()) {
            readings.push(memory::Reading::take(&n.processes(), format!("after run {}", i + 1 - f)));
        }
        if warmup.after(&runs.iter().map(|r| r.cpu).collect::<Vec<_>>()) && leak {
            readings.push(memory::Reading::take(&n.processes(), "after warmup (before run 1)"));
        }
    }
    starts.check(runs.len() + 1, true, &|| drive.session.get().and_then(shown::native_start))?;
    let unsteady = warmup.unsteady();
    let first = warmup.first().unwrap_or(0);
    let warm_ms: f64 = runs[..first].iter().map(|r| r.total).sum::<f64>() + 0.0;
    steps.push(("warmup".into(), warm_ms));
    steps.push(("runs".into(), (lapped.get().elapsed().as_secs_f64() * 1e3 - warm_ms).max(0.0)));
    lapped.set(Instant::now());
    // Accessibility changes a second past each run still count for its last inputs.
    let changes = drive.session.get().map(Session::changes).unwrap_or_default();
    for r in &mut runs {
        r.changes = changes.iter().copied().filter(|&c| c >= r.t0 && c <= r.t1 + 1_000_000_000).collect();
    }
    let (strobe_pct, runner_pct) = cost.share(first);
    let samples = sampler.stop();
    let sampler_pct = samples.cpu_ms.map(|c| 100.0 * c / (sampled.elapsed().as_secs_f64() * 1e3).max(1.0));
    let presents = presents.borrow_mut().take().map(|c| c.stop());
    let stacks = stacks.map(|r| r.and_then(|c| c.stop()));
    let procs1 = (host::now_ns(), host::all_processes());
    if !windowless {
        n.check_visible()?;
    }
    // What the window showed after the runs, its tree included: every probe has stopped.
    let shown = match start {
        Some(start) => {
            let main = n.window.as_ref().map(|w| w.hwnd);
            let end = shown::native(ctx, s, v, pair, "end", &n.pids(), main, drive.get().map_err(|e| e.message));
            shown::expect(&s.expect, &end, "after the runs", true, &|sel| drive.get().and_then(|se| se.shows(sel, Duration::from_secs(5))))?;
            Some(shown::Shown { start, end })
        }
        None => None,
    };
    let measured = &runs[first..];
    let key = |j: usize| format!("run:{}", first + j);
    let per_run = |f: &dyn Fn(&Run) -> f64| -> BTreeMap<String, f64> { measured.iter().enumerate().map(|(j, r)| (key(j), f(r))).filter(|(_, x)| x.is_finite()).collect() };
    let gpu_runs: Vec<probe::os::Summary> = measured.iter().map(|r| samples.window(r.t0, r.t1)).collect();
    // Every adapter the app's GPU work ran on in the measured runs, busiest first: a process may
    // draw on one GPU and compute on another, and each is attributed on its own.
    let mut by_adapter: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for g in &gpu_runs {
        for (a, engines) in &g.gpu_adapters {
            for (e, pct) in engines {
                *by_adapter.entry(a.clone()).or_default().entry(e.clone()).or_default() += pct / gpu_runs.len().max(1) as f64;
            }
        }
    }
    let mut adapters: Vec<String> = by_adapter.keys().cloned().collect();
    adapters.sort_by(|a, b| by_adapter[b].values().sum::<f64>().total_cmp(&by_adapter[a].values().sum::<f64>()));
    let adapter = adapters.first().cloned().or_else(|| gpu_runs.iter().find_map(|g| g.gpu_adapter.clone()));
    // `SoftwareGpu` from outside: an adapter the app's GPU work ran on is a software one.
    if let Some(a) = adapters.iter().chain(adapter.iter()).find(|a| validity::is_software_gpu(a)) {
        return Err(Error::refused(
            "SoftwareGpu",
            format!("the app renders on a software adapter ({a})"),
            "run on a machine with a GPU; GPU and frame numbers from software rendering describe no real device",
        ));
    }
    if !ctx.gpu_checked && adapter.is_some() {
        let devices = if adapters.is_empty() { adapter.iter().cloned().collect() } else { adapters.clone() };
        ctx.gpu = json!({ "devices": devices });
        ctx.gpu_checked = true;
    }
    // The display path the window shows on, for the refresh period and `CrossAdapterScanout`: an app
    // that draws on one adapter while another drives its monitor pays a copy across the bus per frame.
    let display = n.window.as_ref().and_then(|w| host::display_of(w.hwnd));
    if let Some(d) = &display {
        let luid = |name: &str| samples.adapters.iter().find(|a| a.name == name).map(|a| a.luid);
        let drawing: Vec<&String> = adapters.iter().filter(|a| by_adapter[*a].get("3D").copied().unwrap_or(0.0) > 0.0).collect();
        // An adapter the OS does not name for the display (id 0, or none of the sampled ones: a
        // single-GPU Mac) is unknown, not another adapter.
        let known = d.adapter_luid != 0 && samples.adapters.iter().any(|a| a.luid == d.adapter_luid);
        if known && !drawing.is_empty() && !drawing.iter().any(|a| luid(a) == Some(d.adapter_luid)) && !ctx.warnings.iter().any(|w| w.id == "CrossAdapterScanout") {
            let scanout = samples.adapters.iter().find(|a| a.luid == d.adapter_luid).map_or_else(|| format!("adapter 0x{:x}", d.adapter_luid), |a| a.name.clone());
            ctx.warnings.push(validity::info(
                "CrossAdapterScanout",
                format!(
                    "{} block: the app draws on {} while {scanout} drives its window's display ({}): each frame crosses between adapters, as it does for the app's users on this kind of machine",
                    v.role,
                    drawing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" and "),
                    d.device
                ),
            ));
        }
    }
    if !windowless {
        admit_setup(ctx, s, v, adapter.as_deref(), n.window.as_ref().map(|w| w.scale()))?;
    }
    let mut extra: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    extra.insert("run.duration".into(), per_run(&|r| r.ms));
    extra.insert("cpu.work".into(), per_run(&|r| r.cpu));
    extra.insert("cpu.app.total".into(), per_run(&|r| if r.ms > 0.0 { 100.0 * r.cpu / r.ms } else { f64::NAN }));
    if measured.iter().all(|r| !r.span.threads.is_empty()) {
        extra.insert("cpu.wakes".into(), per_run(&|r| r.span.wakes_per_s()));
        extra.insert("io.read".into(), per_run(&|r| r.span.io_read as f64));
        extra.insert("io.write".into(), per_run(&|r| r.span.io_write as f64));
    }
    // The UI thread (the main window's), from its own cycle counter and context switches.
    if let Some(tid) = ui_tid {
        let ui = |r: &Run| r.span.threads.iter().find(|t| t.tid == tid).cloned();
        extra.insert("ui.cpu".into(), per_run(&|r| ui(r).and_then(|t| t.cpu_ms).unwrap_or(f64::NAN)));
        if super::super::outside::not_on_this_os("ui.wakes").is_none() {
            extra.insert("ui.wakes".into(), per_run(&|r| ui(r).map_or(f64::NAN, |t| if r.span.ms > 0.0 { t.switches as f64 / (r.span.ms / 1e3) } else { f64::NAN })));
        }
    }
    let gpu_3d: BTreeMap<String, f64> = gpu_runs.iter().enumerate().filter_map(|(j, g)| Some((key(j), g.gpu_3d_pct?))).collect();
    if !gpu_3d.is_empty() {
        extra.insert("gpu.process.3d".into(), gpu_3d);
    }
    // The bench file's `ctx.count`, and `phase.<name>` from strobe's own marks.
    let hooked: Vec<super::block::Hooked> = measured.iter().map(|r| (&r.counts, r.log.marks.as_slice())).collect();
    extra.extend(super::block::per_run_hooks(s, &key, &hooked));
    // Drags: the moves strobe posted, beside the frames the app drew.
    if measured.iter().any(|r| crate::driver::input_pacing(&r.log.drags).is_some()) {
        extra.insert("input.rate".into(), per_run(&|r: &Run| crate::driver::input_pacing(&r.log.drags).map_or(f64::NAN, |p| p.0)));
        extra.insert("input.gap.p95".into(), per_run(&|r: &Run| crate::driver::input_pacing(&r.log.drags).map_or(f64::NAN, |p| p.1)));
    }
    let counted: Vec<(String, &wakes::Counted)> = measured.iter().enumerate().map(|(j, r)| (key(j), &r.span.counted)).collect();
    let (counts, mut refused) = wakes::counter_metrics(&counted, declared_metrics(s));
    extra.extend(counts);
    // Other work on the CPUs the target is pinned to (SPEC §10.2 Background), as for pages.
    let pinned: Vec<u32> = ctx.pin.map(|m| (0..64).filter(|c| m >> c & 1 == 1).collect()).unwrap_or_default();
    let foreign: Vec<f64> = gpu_runs
        .iter()
        .zip(measured)
        .filter(|_| !pinned.is_empty())
        .filter_map(|(w, r)| {
            let busy: f64 = w.cores.iter().filter(|c| pinned.contains(&c.cpu)).map(|c| c.busy_pct).collect::<Option<Vec<f64>>>()?.iter().sum();
            let cpu = if r.ms > 0.0 { 100.0 * r.cpu / r.ms } else { f64::NAN };
            cpu.is_finite().then(|| (busy - cpu).max(0.0))
        })
        .collect();
    let mut values = metrics::block_values(&metrics::BlockData::default(), "run", &extra);
    // Presented frames from the OS, and input latency from strobe's own inputs to them.
    let pids = n.pids();
    let mut reactions = frames::Reactions::default();
    let mut present_pct = None;
    match &presents {
        Some(Ok(p)) => {
            let spans: Vec<(u64, u64)> = measured.iter().map(|r| (r.t0, r.t1)).collect();
            let f = frames::native_values(p, &pids, &spans, display.as_ref());
            let refresh = f.values.get("display.hz").map(|hz| 1000.0 / hz);
            let quiet = frames::quiet_ns(refresh.unwrap_or(1000.0 / 60.0));
            for r in measured {
                let sent: Vec<frames::Sent> =
                    r.log.inputs.iter().filter(|i| i.kind == "discrete" || (i.kind == "scroll" && i.first)).map(|i| frames::Sent { t_ns: i.t_ns, scroll: i.kind == "scroll" }).collect();
                let got = frames::reactions(&sent, &r.changes, &p.presents, &pids, quiet);
                reactions.interaction.extend(got.interaction);
                reactions.scroll.extend(got.scroll);
                reactions.tried = (reactions.tried.0 + got.tried.0, reactions.tried.1 + got.tried.1);
                reactions.busy = (reactions.busy.0 + got.busy.0, reactions.busy.1 + got.busy.1);
                reactions.unshown = (reactions.unshown.0 + got.unshown.0, reactions.unshown.1 + got.unshown.1);
                reactions.unchanged += got.unchanged;
            }
            // SPEC §19: the present session's consumer may use 0.5% of a core (`Overhead`, warned for
            // timed blocks); past 2% it competes with the app it measures, and its frames describe a
            // disturbed run.
            let span_ms = p.t1_ns.saturating_sub(p.t0_ns) as f64 / 1e6;
            let overhead = p.consumer_cpu_ms.filter(|_| span_ms > 0.0).map(|c| 100.0 * c / span_ms);
            present_pct = overhead;
            let spun = refresh.and_then(|ms| frames::unpaced(&p.presents, &pids, &spans, ms));
            // A spin the toolkit cannot see (composited while locked, a display asleep): said for
            // every series, declared frame metrics or not, with the display's own rate when no
            // vertical blanks came.
            if let Some((fps, hz)) = frames::spin(&p.presents, &pids, &spans, refresh.map(|ms| 1000.0 / ms).or_else(|| display.as_ref().map(|d| d.hz)))
                && !ctx.warnings.iter().any(|w| w.id == "UnpacedSpin")
            {
                let state = screen_state(&procs0, &procs1);
                ctx.warnings.push(validity::warn(
                    "UnpacedSpin",
                    format!(
                        "{} block: the app presented {fps:.0} frames a second on a {hz:.0} Hz display: its presents finish without waiting for the display (pacing broken{}), so it draws as fast as it can and its CPU and GPU numbers describe that spin",
                        v.role,
                        match (state["session"].as_str(), state["display"].as_str()) {
                            (Some("locked"), _) => ", composited while the session is locked".to_string(),
                            (_, Some(d)) if d != "on" => format!(", the display {d}"),
                            _ => String::new(),
                        }
                    ),
                ));
            }
            let frames_refused = match overhead {
                Some(o) if o > 2.0 => Some(format!("OverheadBudget: the OS present session's consumer used {o:.2}% of a core (budget 0.5%), so the frames it saw describe a disturbed run")),
                _ => f.refused.clone().or(spun),
            };
            match frames_refused {
                Some(why) => {
                    for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| frame_metric(m)) {
                        refused.insert(m.clone(), line(&why));
                    }
                }
                None => {
                    // The OS frames replace block_values' own (a native block has no Chrome
                    // frames, so its frames.n is 0 there).
                    values.extend(f.values);
                    for m in std::iter::once(&s.primary).chain(&s.metrics) {
                        if let Some(w) = f.partial.get(m) {
                            refused.entry(m.clone()).or_insert_with(|| line(w));
                        }
                    }
                    // Only what the series declares is listed refused; the rest has no value.
                    let mut why = BTreeMap::new();
                    reactions.values(p.events_lost, &mut values, &mut why);
                    for m in std::iter::once(&s.primary).chain(&s.metrics) {
                        if let Some(w) = why.get(m) {
                            refused.entry(m.clone()).or_insert_with(|| line(w));
                        }
                    }
                }
            }
            if p.events_lost > 0 && !ctx.warnings.iter().any(|w| w.id == "PresentsLost") {
                ctx.warnings.push(validity::warn("PresentsLost", format!("{} block: the present session lost {} events; frame counts may be short", v.role, p.events_lost)));
            }
        }
        Some(Err(e)) => presents_err = Some(e.message.clone()),
        None => {}
    }
    if let Some(why) = &presents_err {
        // Said once for the bench, so frames that never show are not mistaken for none being drawn.
        if !ctx.warnings.iter().any(|w| w.id == "NoPresentSource") {
            ctx.warnings.push(validity::info("NoPresentSource", format!("frames refused: the OS present events gave nothing ({why}); frames.* and interaction.* are missing from every block")));
        }
        for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| frame_metric(m)) {
            refused.entry(m.clone()).or_insert_with(|| Refusal::new(ValidatorId::NoPresentSource, format!("{m}: OS present events: {why}")));
        }
    }
    // Inputs given through accessibility actions skip hit testing and pointer handling: they are
    // never an interaction, and a series that only gave such inputs says so.
    let patterns = measured.iter().flat_map(|r| &r.log.inputs).filter(|i| i.kind == "pattern").count();
    let real = measured.iter().flat_map(|r| &r.log.inputs).filter(|i| i.kind == "discrete" || i.kind == "scroll").count();
    if patterns > 0 && real == 0 {
        for m in std::iter::once(&s.primary).chain(&s.metrics).filter(|m| m.starts_with("interaction.") || m.starts_with("scroll.latency")) {
            refused.insert(
                m.clone(),
                Refusal::new(
                    ValidatorId::PatternInput,
                    format!("{m}: the runs acted through accessibility actions (page.act), which skip hit testing and pointer handling; click, type or press for input latency"),
                ),
            );
        }
    }
    let private_mb = n.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
    if private_mb > 0.0 {
        values.insert("memory.app.private".into(), private_mb);
    }
    // What holds that memory (host, GPU, files, images), read after the runs; a leak run read it
    // between them.
    if !leak {
        readings.push(memory::Reading::take(&n.processes(), "after the runs"));
    }
    if let Some(r) = readings.last() {
        values.extend(r.values().into_iter().map(|(k, x)| (k.to_string(), x)));
    }
    let leak_result = leak.then(|| native_leak(&readings, x.runs));
    if let Some(v) = gpu_runs.last().and_then(|g| g.vram_dedicated_mb) {
        values.insert("gpu.vram".into(), v);
    }
    // An integrated GPU has no dedicated memory: its allocations are all in system memory.
    if let Some(v) = gpu_runs.last().and_then(|g| g.vram_shared_mb) {
        values.insert("gpu.shared".into(), v);
    }
    if !foreign.is_empty() {
        values.insert("cpu.foreign".into(), metrics::median(&foreign));
        values.insert("cpu.foreign.max".into(), foreign.iter().copied().fold(0.0, f64::max));
    }
    if unsteady {
        values.insert("warmup.unsteady".into(), 1.0);
    }
    // A metric no run gave a number for (an empty per-run map's median) is missing, not a value.
    values.retain(|_, x| x.is_finite());
    let have = Have { ui_thread: ui_tid.is_some(), windowless, gpu: samples.gpu_available, pinned: !pinned.is_empty() };
    for m in std::iter::once(&s.primary).chain(&s.metrics) {
        if values.contains_key(m) || refused.contains_key(m) {
            continue;
        }
        refused.insert(m.clone(), missing(m, &have));
    }
    for m in refused.keys() {
        values.remove(m);
    }
    if let Some(why) = refused.get(&s.primary) {
        return Err(Refusal { summary: format!("{}: {}", s.primary, why.summary), fix: why.fix.clone().or_else(|| Some("pick a primary this target can measure".into())), ..why.clone() }.into());
    }
    if drive.session.get().and_then(Session::parked) == Some(false) && !ctx.warnings.iter().any(|w| w.id == "CursorNotParked") {
        ctx.warnings.push(validity::warn(
            "CursorNotParked",
            format!("{} block: the OS cursor could not be moved onto the pointer's points (a locked session?), so the app's hover tracking saw the cursor elsewhere", v.role),
        ));
    }
    let background = busiest_others(&procs0, &procs1);
    let screen = screen_state(&procs0, &procs1);
    let spans: Vec<Span> = measured.iter().map(|r| r.span.clone()).collect();
    let recording = if keep || set == Set::Attribution {
        // Discarded on any error before its meta.json: no recording is left half written.
        let (id, dir) = store::new_recording()?;
        let draft = store::Draft::new(&dir);
        let names: BTreeMap<u32, String> =
            spans.iter().flat_map(|s| s.threads.iter().map(|t| t.tid)).collect::<std::collections::BTreeSet<_>>().into_iter().filter_map(|t| Some((t, host::thread_name(t)?))).collect();
        store::write_json(&dir.join("threads.json"), &json!(wakes::by_thread(&spans, &names, &wakes::modules(&spans))))?;
        let run_rows: Vec<Value> = measured
            .iter()
            .enumerate()
            .map(|(j, r)| json!({ "run": key(j), "ms": r.ms, "cpu_ms": r.cpu, "switches": r.span.switches, "wakes_per_s": r.span.wakes_per_s(), "io_read": r.span.io_read, "io_write": r.span.io_write, "counts": r.counts, "elements": r.elements }))
            .collect();
        store::write_json(&dir.join("runs.json"), &json!(run_rows))?;
        let per: serde_json::Map<String, Value> =
            measured.iter().enumerate().map(|(j, r)| (key(j), json!({ "t0_ns": r.t0, "t1_ns": r.t1, "inputs": r.log.inputs, "marks": r.log.marks, "changes": r.changes }))).collect();
        store::write_json(&dir.join("frames.json"), &json!({ "pids": pids, "runs": per }))?;
        if let Some(Ok(p)) = &presents {
            crate::symbol::write_gz(&dir.join("presents.json.gz"), p)?;
        }
        store::write_json(&dir.join("memory.json"), &json!({ "readings": readings }))?;
        // Native stacks of the measured runs only: warmup runs, and each run's reset between them,
        // are sampled too (the session runs throughout) and are not the case.
        let stacks_note = match &stacks {
            Some(Ok(d)) => {
                let mut d = d.clone();
                // Untimed samples (macOS `sample`) all carry the capture's start, before the warmup
                // runs: keeping only the measured runs' span dropped every one of them.
                if !d.untimed {
                    d.samples.retain(|x| measured.iter().any(|r| x.0 >= r.t0 && x.0 <= r.t1));
                }
                crate::symbol::write_gz(&dir.join("deep.json.gz"), &d)?;
                format!(
                    "native stacks: {} samples (query hotspots){}{}",
                    d.samples.len(),
                    crate::probe::stacks::stacks_how(&d),
                    if d.untimed { "; untimed (macOS sample): they cover the warmup runs and the resets too" } else { "" }
                )
            }
            Some(Err(e)) => format!("native stacks failed: {}", e.message),
            None if set == Set::Attribution => crate::probe::stacks::no_stacks().into(),
            None => String::new(),
        };
        store::write_json(
            &dir.join("meta.json"),
            &json!({
                "id": id, "kind": "block", "native": true, "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label, "variant_git": variant_git(v),
                "target": app.exe, "probes": set, "runs": measured.len(), "values": values, "refused": super::refusal_map(&refused), "duration_ms": measured.iter().map(|r| r.ms).sum::<f64>() as u64,
                "created": super::super::record::now_iso(), "valid": true, "env": host::env(), "window": n.window, "gpu_adapter": adapter, "background": background, "ui_thread": ui_tid,
                "gpu_cache": home.gpu_warm.map(|w| if w { "warm" } else { "cold" }), "screen": screen,
                // The OS and GPU samples over the measured runs, for `query gpu`.
                "os": samples.window(measured[0].t0, measured[measured.len() - 1].t1), "leak": leak_result,
                "timing": if set == Set::Timing || set == Set::Lean { "clean" } else { "perturbed" },
                "note": if set == Set::Attribution { format!("a native block's attribution: OS samples, not timed; {stacks_note}") } else { String::new() },
            }),
        )?;
        draft.keep();
        Some(id)
    } else {
        None
    };
    if let Err(e) = ctx.runner.call("close", json!({}), t, &serve)
        && !ctx.warnings.iter().any(|w| w.id == "Teardown")
    {
        ctx.warnings.push(validity::warn("Teardown", format!("{} block: {}", v.role, e.message)));
    }
    lap("analyse", steps);
    Ok(Block {
        variant: v.role.to_string(),
        pair,
        values,
        run_ms: measured.iter().map(|r| r.ms).collect(),
        warmup: first,
        speed_ratio: ratio,
        // `StateDrift`: the app's accessibility element count at each run's end, as a page's DOM nodes.
        nodes: measured.iter().filter_map(|r| r.elements.map(|e| e as u64)).collect(),
        recording,
        background,
        refused,
        leak: leak_result,
        screen: Some(screen),
        shown,
        overhead: Some(Overhead { strobe_pct, runner_pct, sampler_pct, present_pct, ..Overhead::default() }),
        ..Block::default()
    })
}

/// A native leak run's verdict (SPEC §8.6): growth per run of the processes' host memory (heap,
/// stacks, anonymous memory: `memory.app.host`) over the 2 × N + 1 readings, Theil–Sen with Sen's
/// 95% interval, against max(16 KB, 0.1% of it) per run. A native allocator keeps freed memory for
/// reuse, so a heap that fills up and then holds steady reads "grows, then levels off".
fn native_leak(readings: &[memory::Reading], n: usize) -> Value {
    let complete = readings.iter().all(memory::Reading::complete) && !readings.is_empty();
    if !complete {
        return json!({ "native_host": { "verdict": format!("unclear: no complete memory reading ({})", memory::why_empty()), "verdict_kind": crate::model::VerdictKind::Unclear, "decided": false }, "runs": 2 * n });
    }
    let host: Vec<f64> = readings.iter().map(|r| r.host() as f64).collect();
    let private: Vec<f64> = readings.iter().map(|r| r.processes.iter().map(|m| m.private).sum::<u64>() as f64).collect();
    let bytes = |v: f64| format!("{}{}", if v < 0.0 { "−" } else { "+" }, fmt::bytes(v.abs()));
    let bound = (0.001 * metrics::median(&host)).max(16_384.0);
    json!({ "runs": 2 * n, "readings": readings.len(), "native_host": growth(&host, n, bound, &bytes), "native_private": growth(&private, n, bound, &bytes), "host_bytes": host, "private_bytes": private })
}

/// What a native block had to measure with, for why a declared metric has no value.
struct Have {
    /// The main window's thread was found (`ui.*`).
    ui_thread: bool,
    windowless: bool,
    /// The OS sampler opened a per-process GPU source.
    gpu: bool,
    /// The target runs on pinned CPUs (`cpu.foreign`).
    pinned: bool,
}

/// Why a declared metric has no value in a native run block: what this OS cannot read, what a native
/// target has none of, or what the runs gave nothing for. Every declared metric ends with a value or
/// one of these (SPEC §13.6), never as "no value".
fn missing(metric: &str, have: &Have) -> Refusal {
    if let Some(r) = super::super::outside::not_on_this_os(metric) {
        return r;
    }
    let refuse = |v: ValidatorId, why: String| Refusal::new(v, format!("{metric}{why}"));
    let area = metric.split('.').next().unwrap_or("");
    match area {
        "frames" if metric.starts_with("frames.presented.") && metric != "frames.presented.count" => refuse(
            ValidatorId::NoPresentSource,
            " is a frame's begin to its screen time; from outside strobe sees when a frame reached the screen, not when the app began it (frames.interval.*, frames.dropped and interaction.* it does see)".into(),
        ),
        "frames" | "interaction" | "scroll" | "display" if have.windowless => refuse(ValidatorId::NotForTarget, ": the app shows no window (window: false), so nothing of it reaches the screen".into()),
        "frames" | "interaction" | "scroll" | "display" => refuse(ValidatorId::NoPresentSource, ": the runs gave no OS present events for it (no frames in the runs)".into()),
        "ui" if !have.ui_thread => refuse(ValidatorId::NotForTarget, " is the UI thread's, the thread of the main window; this target shows none".into()),
        "ui" if metric == "ui.cpu" || metric == "ui.wakes" => refuse(ValidatorId::RunIncomplete, ": the UI thread was not among the app's threads at the end of every run (did the window move to another thread?)".into()),
        "ui" => refuse(ValidatorId::NotForTarget, " is not measured: native apps have ui.cpu and ui.wakes".into()),
        "canvas" | "main" | "layout" | "style" | "js" | "dom" | "net" | "leak" => refuse(ValidatorId::NotForTarget, " is a browser engine's; a native app has none".into()),
        "phase" => refuse(ValidatorId::NoPhase, format!(": no measured run entered phase(\"{}\")", &metric[6..])),
        "input" => refuse(ValidatorId::NoInput, ": no measured run dragged (page.drag, or moves while page.down(\"Mouse\") holds a button)".into()),
        "launch" => refuse(ValidatorId::NotForTarget, " is a launch case's: give the case `launch` to time the app's launch".into()),
        _ => match metric {
            "gpu.compile" | "cpu.renderer" | "cpu.gpu" | "cpu.browser" => refuse(ValidatorId::NotForTarget, " is a browser engine's; a native app has none".into()),
            "gpu.process.3d" | "gpu.vram" | "gpu.shared" if !have.gpu => super::super::outside::gpu_unread(metric),
            "cpu.foreign" | "cpu.foreign.max" if !have.pinned => refuse(ValidatorId::NotForTarget, " is other work on the CPUs the target is pinned to, and this bench pins it to none (--pin none)".into()),
            "cpu.work" | "cpu.app.total" | "cpu.process.total" | "cpu.wakes" | "io.read" | "io.write" | "memory.app.private" | "memory.app.host" | "gpu.process.3d" | "gpu.vram" | "gpu.shared"
            | "cpu.foreign" | "cpu.foreign.max" => refuse(ValidatorId::RunIncomplete, ": the app's processes could not be read in every run (they exited, or strobe may not open them)".into()),
            _ => refuse(ValidatorId::NotForTarget, " is not measured for a native app".into()),
        },
    }
}

/// A refusal line (`<validator>: <why>`) from the frame readers and `missing`, typed. Every line
/// they write names a validator; one that did not would be strobe's bug, refused as unseen frames.
fn line(why: &str) -> Refusal {
    Refusal::from_line(why).unwrap_or_else(|| Refusal::new(ValidatorId::NoPresentSource, why))
}

/// How a native launch ends.
enum Until {
    /// The main window is visible.
    Visible,
    /// The window's pixels first changed from its first frame.
    Painted,
    /// The window's pixels last changed, once they stayed the same for a second.
    Settled,
    /// An element shows in the app's accessibility tree, timed to the frame that showed it.
    Selector(Value),
    /// One of the launch's milestones (an output or log line).
    Milestone(String),
}

/// One look at the launching app: when (epoch ms), whether its window shows, its pixels, and the tree's
/// CPU cycles.
struct Sample {
    at: f64,
    shown: bool,
    px: Option<Vec<u8>>,
    cycles: u64,
}

/// A cell (a 16th of the window each way) moving by more than 8 levels in a colour channel; see
/// `launch::screen_times`.
fn differs(a: &[u8], b: &[u8]) -> bool {
    a.chunks(4).zip(b.chunks(4)).any(|(x, y)| x.iter().zip(y).take(3).any(|(p, q)| p.abs_diff(*q) > 8))
}

/// Every 30 ms until `stop`: keeps the app's windows on top without focus, and sends a sample (with
/// pixels when `capture`). Returns the pixel samples.
fn watch(pid: u32, home: &Path, capture: bool, stop: &AtomicBool, tx: std::sync::mpsc::Sender<Sample>) -> Vec<(f64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut raised = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        // The app's process set (its tree, what carries its home's stamp, its job), one snapshot a
        // look: a launcher that hands its window to a detached process is still the app.
        let set = host::process_set(pid, Some(home));
        let pids: Vec<u32> = set.iter().map(|p| p.pid).collect();
        if raised.elapsed() > Duration::from_millis(300) {
            host::raise_windows(&pids);
            raised = Instant::now();
        }
        let shown = host::main_window(&pids).is_some();
        let px = if capture && shown { host::window_pixels(&pids) } else { None };
        let at = isolate::epoch_ms();
        if let Some(p) = &px {
            out.push((at, p.clone()));
        }
        let cycles = set.iter().map(|p| p.cycles).sum();
        let _ = tx.send(Sample { at, shown, px, cycles });
        std::thread::sleep(Duration::from_millis(30));
    }
    out
}

pub(super) fn launch(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let key = format!("{}\n{}", s.label(), v.role);
    // One launch per variant and series is discarded first, as for apps (SPEC §9.6).
    if !x.untimed && ctx.launched.insert(key) {
        progress(format!("  {} warm-up launch (discarded)", v.role));
        launch_once(ctx, s, v, pair, false)?;
    }
    launch_once(ctx, s, v, pair, false)
}

/// One native launch, timed from spawn until `until`. With `profile` (an attribution launch) it is
/// not timed and keeps a recording of its values.
pub(super) fn launch_once(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, profile: bool) -> Result<Block> {
    let spec = s.launch.clone();
    let target = ctx.describe["targets"][&s.target].clone();
    let key = format!("{}\n{}", s.label(), v.role);
    let milestones = milestones(&spec)?;
    if milestones.iter().any(|m| m.source == "page") {
        return Err(Error::usage(format!("{}: a native app has no page marks; read milestones from its output or a log file", s.case)));
    }
    let app = with_gpu_cache(ctx, s, v, &target, app(v, &target, &ctx.config)?)?;
    let windowless = target["window"] == false || app.windowless;
    let until = match spec["until"].get("window").and_then(Value::as_str) {
        // A milestone of the app's own (an output or log line): the launch needs no window at all.
        _ if spec["until"]["milestone"].is_string() => {
            let name = spec["until"]["milestone"].as_str().unwrap_or("").to_string();
            if !milestones.iter().any(|m| m.name == name) {
                return Err(Error::usage(format!("{}: launch.until.milestone {name} is not one of its milestones", s.case)));
            }
            Until::Milestone(name)
        }
        _ if windowless => return Err(Error::usage(format!("{}: a target with window: false times its launch to a milestone: until: {{ milestone: \"<name>\" }}", s.case))),
        Some("visible") => Until::Visible,
        Some("painted") => Until::Painted,
        Some("settled") => Until::Settled,
        Some(other) => return Err(Error::usage(format!("{}: launch.until.window is visible, painted or settled, not {other}", s.case))),
        None => Until::Selector(spec["until"].clone()),
    };
    let capture = spec["screen"] != false && !windowless;
    if windowless && let Some(m) = std::iter::once(&s.primary).chain(&s.metrics).find(|m| *m == "launch.painted" || *m == "launch.settled") {
        return Err(Error::usage(format!("{}: {m} reads the window's pixels; this target has window: false", s.case)));
    }
    if !capture && matches!(until, Until::Painted | Until::Settled) {
        return Err(Error::usage(format!("{}: launch.until.window painted or settled reads the window's pixels; drop screen: false", s.case)));
    }
    // A launch timed to its pixels on a Mac without Screen Recording would wait for pixels that never come.
    #[cfg(target_os = "macos")]
    if matches!(until, Until::Painted | Until::Settled) && !host::macos::screen_capture_allowed() {
        return Err(Error::refused("NoPresentSource", format!("{}: launch.until.window {}: {}", s.case, until_text(&until), super::launch::no_pixels(true)), "grant Screen Recording, or time the launch to a selector or a milestone"));
    }
    let warm = match spec["profile"].as_str().unwrap_or("fresh") {
        "fresh" => false,
        "warm" => true,
        other => return Err(Error::usage(format!("{}: launch.profile is fresh or warm, not {other}", s.case))),
    };
    let t = Duration::from_secs(120);
    let mut steps: Vec<(String, f64)> = Vec::new();
    let mut lapped = Instant::now();
    let mut lap = |name: &str, steps: &mut Vec<(String, f64)>| {
        steps.push((name.to_string(), lapped.elapsed().as_secs_f64() * 1e3));
        lapped = Instant::now();
    };
    let home = if warm { ctx.homes.entry(key).or_insert_with(isolate::new_home).clone() } else { isolate::new_home() };
    let prepared = isolate::prepare_home(&app, seed_dir(ctx, s, &target).as_deref(), Some(home.clone()))?;
    let env = prepared.environment(&app);
    ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": v.root, "home": home, "env": env, "exe": app.exe }), t, &no_page)?;
    if spec["before"] == true {
        ctx.runner
            .call("before", json!({ "case": s.case }), Duration::from_secs(900), &no_page)
            .map_err(|e| crate::runner::incomplete(&format!("{} before", s.case), "before", e, "", "fix the launch's before hook"))?;
    }
    lap("before", &mut steps);
    // A launch timed to an element ends on the frame that showed it: OS present events say when.
    let mut presents = match &until {
        Until::Selector(_) => Some(PresentCapture::start().map_err(|e| {
            Error::refused(
                "NoPresentSource",
                format!("{}: a launch timed to an element ends when the frame that showed it reached the screen: {}", s.case, e.message),
                if cfg!(windows) { "run strobe up for present events, or time the launch to { window: \"settled\" }" } else { "time the launch to { window: \"settled\" }" },
            )
        })?),
        _ => None,
    };
    crate::target::a11y::prepare();
    let clock = (host::tsc(), Instant::now());
    let (mut n, guard) = match nt::launch(&app, &prepared, &[], true, ctx.pin) {
        Ok(x) => x,
        Err(e) => {
            isolate::release_home(&home);
            if !warm {
                isolate::remove_home(&home);
            }
            return Err(e);
        }
    };
    // An attribution launch samples native stacks from right after the spawn (the session follows
    // the new tree) until ready, through the helper when it is installed.
    let mut stacks = (profile && crate::probe::stacks::stacks_available()).then(|| crate::probe::stacks::StackCapture::start(crate::probe::stacks::Targets::new(n.pid, Some(&n.home))));
    let result = (|| -> Result<Block> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        // A windowless app has no window to keep on top or sample.
        let watcher = (!windowless).then(|| {
            let (stop, pid, home) = (stop.clone(), n.pid, n.home.clone());
            std::thread::spawn(move || watch(pid, &home, capture, &stop, tx))
        });
        let mut lines: Vec<Line> = Vec::new();
        let tails = {
            let globs: Vec<String> = milestones.iter().filter(|m| m.source != "output").map(|m| m.source.clone()).collect();
            let stop = stop.clone();
            let home = home.clone();
            (!globs.is_empty()).then(|| std::thread::spawn(move || tail(&home, &globs, &stop)))
        };
        let timeout = Duration::from_millis(spec["timeout"].as_u64().unwrap_or(120_000));
        let deadline = Instant::now() + timeout;
        // (epoch ms, cycles of the tree) when the launch was ready.
        let ready: (f64, u64) = if let Until::Milestone(name) = &until {
            // The first line matching it, from the output as it arrives or the log files under the
            // home (read every 20 ms); its time is when strobe read it, or the line's own `at`.
            let m = milestones.iter().find(|m| &m.name == name).ok_or_else(|| Error::msg("internal: the until milestone"))?;
            let when = |at: f64, l: &str| m.pattern.captures(l).map(|c| c.name("at").and_then(|v| v.as_str().parse::<f64>().ok()).filter(|v| *v > 1e12).unwrap_or(at));
            loop {
                if let Some(rx) = &n.output {
                    lines.extend(rx.try_iter().map(|(at, _, l)| (at, l, "output".to_string())));
                }
                let mut found = lines.iter().filter(|(_, _, src)| *src == m.source).find_map(|(at, l, _)| when(*at, l));
                if found.is_none() && m.source != "output" {
                    let now = isolate::epoch_ms();
                    found =
                        super::launch::matching(&home, &m.source).iter().filter_map(|p| std::fs::read_to_string(p).ok()).find_map(|text| text.lines().find_map(|l| when(now, &isolate::strip_ansi(l))));
                }
                if let Some(at) = found {
                    break (at, n.processes().iter().map(|p| p.cycles).sum());
                }
                if let Some(code) = n.exited().filter(|_| n.processes().is_empty()) {
                    let out: Vec<String> = lines.iter().filter(|l| l.2 == "output").map(|l| l.1.clone()).collect();
                    let last = &out[out.len().saturating_sub(5)..];
                    return Err(super::super::record::exited(code, &format!("before its launch reached milestone {name}"), Some(last)));
                }
                guard.check("while the app started")?;
                if Instant::now() > deadline {
                    return Err(Error::refused(
                        "RunIncomplete",
                        format!("{} launch: milestone {name} never matched in {} s", s.case, timeout.as_secs()),
                        "check the milestone's match and where it looks",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        } else if let Until::Selector(sel) = &until {
            // The element shows in the app's accessibility tree between the last read that lacked it
            // and the first that had it; the frame that showed it is the first present to reach the
            // screen that started in or after that span.
            let main = loop {
                if let Some(w) = host::main_window(&n.pids()) {
                    break w;
                }
                if n.exited().is_some() && n.processes().is_empty() {
                    return Err(Error::refused("RunIncomplete", format!("{} launch: the app exited before it showed a window", s.case), "check that the app opens a window"));
                }
                guard.check("while the app started")?;
                if Instant::now() > deadline {
                    return Err(Error::refused("RunIncomplete", format!("{} launch: no window in {} s", s.case, timeout.as_secs()), "check that the app opens a window"));
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            if let Some(p) = presents.as_mut() {
                p.track(&n.pids());
            }
            let session = Session::open(&n.pids(), main.hwnd)?;
            let (lo, hi) = session
                .appeared(sel, deadline.saturating_duration_since(Instant::now()))
                .map_err(|e| Error::refused("RunIncomplete", format!("{} launch: {}", s.case, e.message), "the until selector never showed"))?;
            let cycles = n.processes().iter().map(|p| p.cycles).sum();
            let Some(lo) = lo else {
                return Err(Error::refused(
                    "RunIncomplete",
                    format!("{} launch: the until selector already showed when strobe first read the app's accessibility tree, so the frame that showed it is unknown", s.case),
                    "time the launch to an element that appears later, or to { window: \"settled\" }",
                ));
            };
            let p = presents.as_mut().ok_or_else(|| Error::msg("internal: the launch's present session"))?;
            let pids = n.pids();
            let early = p.live(&pids, lo, Duration::from_secs(2))?;
            let late = p.live(&pids, hi, Duration::from_secs(2))?;
            if late.next_screen_ns == 0 {
                return Err(Error::refused(
                    "RunIncomplete",
                    format!("{} launch: no frame reached the screen after the element showed", s.case),
                    "a window that never presents cannot be timed to an element; use a milestone",
                ));
            }
            let refresh_ns = host::refresh_at(main.rect.0, main.rect.1).map_or(16_666_667, |hz| 1_000_000_000 / hz.max(1) as u64);
            if early.next_start_ns != late.next_start_ns && late.next_screen_ns.saturating_sub(early.next_screen_ns) > refresh_ns {
                return Err(Error::refused(
                    "RunIncomplete",
                    format!(
                        "{} launch: the app showed {:.1} ms of frames while strobe read its tree, so which frame showed the element is unknown",
                        s.case,
                        (late.next_screen_ns - early.next_screen_ns) as f64 / 1e6
                    ),
                    "time the launch to an element that appears after the app stops animating, or to { window: \"settled\" }",
                ));
            }
            (n.epoch_ms + (late.next_screen_ns as f64 - n.at_ns as f64) / 1e6, cycles)
        } else {
            let mut first_px: Option<Vec<u8>> = None;
            let mut prev_px: Option<Vec<u8>> = None;
            let mut change: Option<Sample> = None;
            let sample = loop {
                if let Ok(sample) = rx.recv_timeout(Duration::from_millis(100)) {
                    match until {
                        Until::Visible if sample.shown => break sample,
                        Until::Painted | Until::Settled => {
                            if let Some(px) = sample.px.clone() {
                                let first = first_px.get_or_insert_with(|| px.clone()).clone();
                                let moved = prev_px.as_ref().is_some_and(|p| differs(p, &px));
                                prev_px = Some(px.clone());
                                if matches!(until, Until::Painted) && differs(&first, &px) {
                                    break sample;
                                }
                                if change.is_none() || moved {
                                    change = Some(sample);
                                } else if change.as_ref().is_some_and(|c| sample.at - c.at >= 1000.0) {
                                    break change.take().unwrap_or(sample);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if n.exited().is_some() && n.processes().is_empty() {
                    return Err(Error::refused(
                        "RunIncomplete",
                        format!("{} launch: the app exited before its window {}", s.case, until_text(&until)),
                        "an app that hands its window to another process cannot be timed this way",
                    ));
                }
                guard.check("while the app started")?;
                if Instant::now() > deadline {
                    return Err(Error::refused("RunIncomplete", format!("{} launch: its window never {} in {} s", s.case, until_text(&until), timeout.as_secs()), "check that the app opens a window"));
                }
            };
            (sample.at, sample.cycles)
        };
        lap("ready", &mut steps);
        // An attribution launch's threads from the spawn until ready: wakes and CPU per thread without
        // a trace, read once it is ready (threads that already exited are not in it).
        let launch_span = profile.then(|| wakes::span(&Snapshot::empty(n.at_ns, clock.0), &Snapshot::take(&n.pids())));
        // The tree's cycles at ready (it started at zero) over the time stamp counter's rate.
        let cpu_ms = match (clock.0, host::tsc()) {
            (Some(a), Some(b)) if b > a && ready.1 > 0 => ready.1 as f64 / ((b - a) as f64 / (clock.1.elapsed().as_secs_f64() * 1e3)),
            _ => f64::NAN,
        };
        let settle = Instant::now() + Duration::from_secs(1);
        let grace = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(rx) = &n.output {
                lines.extend(rx.try_iter().map(|(at, _, l)| (at, l, "output".to_string())));
            }
            let now = Instant::now();
            if now >= grace || (now >= settle && found_all(&milestones, &lines, |m| m.source == "output")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        stop.store(true, Ordering::Relaxed);
        if let Some(h) = tails {
            lines.extend(joined(h));
        }
        let pixels = watcher.map(joined).unwrap_or_default();
        lap("milestones", &mut steps);
        // What the launch ended on (the window it was waiting for, or a first-run dialog over it).
        let shown = match host::main_window(&n.pids()).filter(|_| !windowless) {
            Some(w) => {
                let session = Session::open(&n.pids(), w.hwnd);
                let end = shown::native(ctx, s, v, pair, "end", &n.pids(), Some(w.hwnd), session.as_ref().map_err(|e| e.message.clone()));
                shown::expect(&s.expect, &end, "when the launch was ready", true, &|sel| session.as_ref().map_err(|e| Error::msg(e.message.clone())).and_then(|se| se.shows(sel, Duration::from_secs(5))))?;
                Some(shown::Shown { start: shown::Screen::default(), end })
            }
            None => None,
        };
        let mut values = BTreeMap::new();
        let ready_ms = ready.0 - n.epoch_ms;
        values.insert("launch.ready".to_string(), ready_ms);
        if cpu_ms.is_finite() {
            values.insert("cpu.work".to_string(), cpu_ms);
        }
        for m in &milestones {
            let first = lines
                .iter()
                .filter(|(_, _, src)| *src == m.source)
                .filter_map(|(at, l, _)| m.pattern.captures(l).map(|c| c.name("at").and_then(|v| v.as_str().parse::<f64>().ok()).filter(|v| *v > 1e12).unwrap_or(*at)))
                .min_by(f64::total_cmp);
            match first {
                Some(at) => {
                    values.insert(format!("launch.{}", m.name), at - n.epoch_ms);
                }
                None if s.primary == format!("launch.{}", m.name) || s.metrics.iter().any(|x| *x == format!("launch.{}", m.name)) => {
                    return Err(Error::refused("RunIncomplete", format!("{} launch: milestone {} never matched in {}", s.case, m.name, m.source), "check the milestone's match and where it looks"));
                }
                None => {}
            }
        }
        if let Some((painted, settled)) = screen_times(&pixels) {
            values.insert("launch.painted".into(), painted - n.epoch_ms);
            values.insert("launch.settled".into(), settled - n.epoch_ms);
        }
        if super::launch::declares(s, "memory.app.") {
            super::launch::memory(&n.processes(), &mut values);
        }
        let why_no_pixels = if windowless { "the app shows no window (window: false)".to_string() } else { super::launch::no_pixels(capture) };
        let refused = super::launch::refusals(s, &values, &why_no_pixels, false);
        if let (false, Some(r)) = (profile, refused.get(&s.primary)) {
            return Err(r.clone().fix("pick a primary a launch case measures").into());
        }
        let recording = if profile {
            let (id, dir) = store::new_recording()?;
            let draft = store::Draft::new(&dir);
            // Stacks from the spawn until the launch was ready (its settling after is not the launch).
            let ready_ns = n.at_ns + ((ready.0 - n.epoch_ms).max(0.0) * 1e6) as u64;
            let stacks_note = match stacks.take().map(|r| r.and_then(|c| c.stop())) {
                Some(Ok(mut d)) => {
                    if !d.untimed {
                        d.samples.retain(|x| x.0 >= n.at_ns && x.0 <= ready_ns);
                    }
                    crate::symbol::write_gz(&dir.join("deep.json.gz"), &d)?;
                    format!("native stacks from just after the spawn until ready: {} samples (query hotspots){}", d.samples.len(), crate::probe::stacks::stacks_how(&d))
                }
                Some(Err(e)) => format!("native stacks failed: {}", e.message),
                None => crate::probe::stacks::no_stacks().into(),
            };
            let mut meta_values = json!(values);
            if let Some(span) = &launch_span {
                let names: BTreeMap<u32, String> = span.threads.iter().filter_map(|t| Some((t.tid, host::thread_name(t.tid)?))).collect();
                let spans = std::slice::from_ref(span);
                store::write_json(&dir.join("threads.json"), &json!(wakes::by_thread(spans, &names, &wakes::modules(spans))))?;
                store::write_json(&dir.join("runs.json"), &json!([{ "run": "launch", "ms": span.ms, "switches": span.switches, "wakes_per_s": span.wakes_per_s(), "io_read": span.io_read, "io_write": span.io_write }]))?;
                if !span.threads.is_empty() {
                    meta_values["cpu.wakes"] = json!(span.wakes_per_s());
                    meta_values["io.read"] = json!(span.io_read);
                    meta_values["io.write"] = json!(span.io_write);
                }
            }
            store::write_json(
                &dir.join("meta.json"),
                &json!({
                    "id": id, "kind": "block", "native": true, "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label, "variant_git": variant_git(v),
                    "target": app.exe, "probes": "attribution", "runs": 1, "values": meta_values, "created": super::super::record::now_iso(), "valid": true, "env": host::env(), "launch": true,
                    "gpu_cache": prepared.gpu_warm.map(|w| if w { "warm" } else { "cold" }),
                    "timing": "perturbed",
                    "note": format!("a native launch's attribution: its values and output, not timed; {stacks_note}"),
                }),
            )?;
            draft.keep();
            Some(id)
        } else {
            None
        };
        Ok(Block { variant: v.role.to_string(), pair, values, run_ms: vec![ready_ms], recording, steps: steps.clone(), shown, refused, ..Block::default() })
    })();
    // A launch that failed before its recording still ends the helper's session.
    if let Some(Ok(c)) = stacks.take() {
        let _ = c.stop();
    }
    if let Some(p) = presents {
        let _ = p.stop();
    }
    n.close(!warm);
    let closed = ctx.runner.call("close", json!({}), t, &no_page);
    guard.check("after a launch")?;
    if let Err(e) = closed
        && !ctx.warnings.iter().any(|w| w.id == "Teardown")
    {
        ctx.warnings.push(validity::warn("Teardown", format!("{} launch: {}", v.role, e.message)));
    }
    result
}

fn until_text(u: &Until) -> &'static str {
    match u {
        Until::Visible => "showed",
        Until::Painted => "painted",
        Until::Settled => "settled",
        Until::Selector(_) => "showed its until selector",
        Until::Milestone(_) => "reached its milestone",
    }
}
