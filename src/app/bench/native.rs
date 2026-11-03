//! Native targets (SPEC §9.6, §13.7): an app with no browser engine, launched in an isolated home per
//! block. From outside strobe measures its processes' CPU in cycles, its threads' wakes, IO, GPU and
//! memory, and for launches its window's pixels. When the app's UI toolkit has the bridge, strobe also
//! drives it (the bench file's `page`) and reads its run records: UI-thread work by phase, frames it
//! submitted, exact counters.

use super::launch::{Line, found_all, joined, milestones, no_page, screen_times, tail};
use super::*;
use crate::driver::native::NativeDriver;
use crate::probe::wakes::{self, Snapshot, Span};
use crate::target::native::{self as nt, Native};
use crate::target::native_bridge::{self as nb, Bridge, Listener};
use std::cell::OnceCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a native target launches for a variant: its app profile (`native(appId)`) or a bare
/// executable (`native(path)`), with the variant's own build or `exe:` in place of the profile's
/// executable, the target's `args` and `env` added, and for a headless target the profile's headless mode.
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
    let app = base.with_extra(&args, &env);
    if target["kind"] == "headless" { app.headless() } else { Ok(app) }
}

/// The app's bridge for one launch: connected when something first needs it (a page call, run
/// records, a selector) and admitted once its hello fits the target.
struct Link {
    listener: Option<Listener>,
    bridge: OnceCell<Bridge>,
    pid: u32,
    /// When the app was spawned (host clock): its hello must come after it.
    spawned_ns: u64,
    headless: bool,
    /// A timing block: a debug build is refused there.
    timed: bool,
}

impl Link {
    fn new(target: &Value, n: &Native, timed: bool, listener: Option<Listener>) -> Link {
        Link { listener, bridge: OnceCell::new(), pid: n.pid, spawned_ns: n.at_ns, headless: target["kind"] == "headless", timed }
    }

    /// The bridge, waiting up to `wait` for the app to connect.
    fn get(&self, wait: Duration) -> Result<&Bridge> {
        if let Some(b) = self.bridge.get() {
            return Ok(b);
        }
        let l = self.listener.as_ref().ok_or_else(|| Error::usage("this target's bridge is off (native(…, { bridge: false })): it is measured from outside only"))?;
        let pid = self.pid;
        let b = l.accept(wait, &|| isolate::alive(pid))?;
        self.admit(&b)?;
        Ok(self.bridge.get_or_init(|| b))
    }

    /// The bridge if the app has connected by now.
    fn now(&self) -> Result<Option<&Bridge>> {
        if let Some(b) = self.bridge.get() {
            return Ok(Some(b));
        }
        let Some(b) = self.listener.as_ref().map(Listener::try_accept).transpose()?.flatten() else { return Ok(None) };
        self.admit(&b)?;
        Ok(Some(self.bridge.get_or_init(|| b)))
    }

    /// The hello's validators (SPEC §10.1): the clock strobe reads, the mode the target asks for, and
    /// no debug build in a timing block.
    fn admit(&self, b: &Bridge) -> Result<()> {
        let h = &b.hello;
        // Its hello's time must fall between the spawn and now on strobe's own clock.
        if h.clock != nb::CLOCK || h.now_ns < self.spawned_ns || h.now_ns > host::now_ns() + 1_000_000 {
            return Err(Error::refused(
                "ClockUnsynced",
                format!("the app's bridge reports clock \"{}\" at {} ns, outside its launch ({} ns) to now on strobe's \"{}\" clock", h.clock, h.now_ns, self.spawned_ns, nb::CLOCK),
                "the toolkit's bridge must stamp times with the OS monotonic clock strobe uses, in ns",
            ));
        }
        match (self.headless, h.mode.as_str()) {
            (true, "headless") | (false, "window") => {}
            (true, other) => return Err(Error::usage(format!("a headless() target's app says it runs in {other} mode: its profile's headless args or env did not select headless"))),
            (false, other) => return Err(Error::usage(format!("a native() target's app says it runs in {other} mode; use headless() for that"))),
        }
        if h.debug_build && self.timed {
            return Err(Error::refused(
                "DevBuild",
                format!("the app is a debug build ({} {}): its timings describe no build people run", h.toolkit, h.toolkit_version),
                "build the app optimized (strobe.json build: a release or profiling profile)",
            ));
        }
        Ok(())
    }

    fn quit(&self) {
        if let Some(b) = self.bridge.get() {
            b.quit();
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
    /// The bridge's run record (`run.end`), when the run used the bridge.
    record: Option<Value>,
    t0: u64,
    t1: u64,
    total: f64,
}

impl Run {
    /// What warmup compares: the UI thread's work with the bridge, else the processes' CPU.
    fn work(&self) -> f64 {
        self.record.as_ref().and_then(|r| r["work"]["wall_ms"].as_f64()).unwrap_or(self.cpu)
    }
}

/// Metrics a headless target can defend: work on the UI thread, exact counts and CPU (SPEC §13.7).
fn headless_allowed(metric: &str) -> bool {
    metric.starts_with("ui.") || metric.starts_with("count.") || metric.starts_with("cpu.")
}

/// Metrics read from frames: refused together when a surface skipped work or spun.
fn frame_metric(metric: &str) -> bool {
    ["ui.frames", "ui.interval.", "frames.", "interaction.", "scroll.", "display."].iter().any(|p| metric.starts_with(p))
}

/// Does the series need the app's bridge before its runs: the target says so, a headless target, a
/// metric only the bridge has, or a pixels block's snapshots.
fn needs_bridge(ctx: &Ctx, s: &Series, target: &Value) -> bool {
    target["bridge"] == true || target["kind"] == "headless" || ctx.pixels_dir.is_some() || std::iter::once(&s.primary).chain(&s.metrics).any(|m| m.starts_with("ui.") || frame_metric(m))
}

pub(super) fn block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, set: Set, keep: bool) -> Result<Block> {
    if set == Set::Memory {
        return Err(Error::usage("a leak run (--probes memory) follows a JS heap; a native target has none"));
    }
    let target = ctx.describe["targets"][&s.target].clone();
    if target["kind"] == "headless"
        && let Some(m) = std::iter::once(&s.primary).chain(&s.metrics).find(|m| !headless_allowed(m))
    {
        return Err(Error::refused(
            "NoDisplayForBudget",
            format!("{m}: a headless app draws off screen on a virtual clock, so it has no display, presentation or GPU of its own to measure"),
            "measure ui.*, count.* and cpu.* there, or use native() for the rest",
        ));
    }
    let app = app(v, &target, &ctx.config)?;
    // `Throttled`: there is no page to run the speed check in, so strobe runs a fixed workload of its
    // own on the target's CPUs before the launch.
    let ratio = validity::native_throttled(validity::native_speed(ctx.pin))?;
    let home = isolate::prepare_home(&app, seed_dir(ctx, s, &target).as_deref(), None)?;
    let listener = if target["bridge"] == false { None } else { Some(Listener::open()?) };
    let env = listener.as_ref().map(Listener::env).unwrap_or_default();
    let launched = Instant::now();
    let (mut n, guard) = match nt::launch(&app, &home, &env, false, ctx.pin) {
        Ok(x) => x,
        Err(e) => {
            isolate::release_home(&home.dir);
            isolate::remove_home(&home.dir);
            return Err(e);
        }
    };
    // Timing blocks refuse a debug build; pixels and attribution blocks are not timed.
    let timed = set != Set::Attribution && ctx.pixels_dir.is_none();
    let link = Link::new(&target, &n, timed, listener);
    let mut steps = vec![("launch".to_string(), launched.elapsed().as_secs_f64() * 1e3)];
    let result = runs(ctx, s, v, pair, set, keep, &target, &app, &home, &mut n, &guard, &link, &mut steps, ratio);
    let closing = Instant::now();
    link.quit();
    n.close(true);
    guard.check("after a block")?;
    result.map(|mut b| {
        b.steps = steps;
        b.steps.push(("close".into(), closing.elapsed().as_secs_f64() * 1e3));
        b
    })
}

/// The bridge's `info` (GPU and surfaces), checked once per block: `SoftwareGpu`, and `VariantMismatch`
/// when base and head drew with different GPU backends, adapters or scales.
fn admit_info(ctx: &mut Ctx, s: &Series, v: &Variant, b: &Bridge) -> Result<Value> {
    let info = b.request("info", Value::Null, Duration::from_secs(10))?;
    let gpu = &info["gpu"];
    let name = gpu["name"].as_str().unwrap_or("");
    if gpu["device_type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("cpu")) || software_adapter(name) {
        return Err(Error::refused(
            "SoftwareGpu",
            format!("the app renders on a software adapter ({name}, {})", gpu["backend"].as_str().unwrap_or("?")),
            "run on a machine with a GPU; GPU and frame numbers from software rendering describe no real device",
        ));
    }
    let scale = info["surfaces"].as_array().and_then(|a| a.first()).and_then(|s| s["scale"].as_f64());
    let setup = format!("{} on {name} at scale {}", gpu["backend"].as_str().unwrap_or("?"), scale.map_or("?".into(), |s| s.to_string()));
    // Per target: a headless target's offscreen surface may differ from the window target's.
    let mine = format!("{}\n{}", s.target, v.role);
    if let Some((key, other)) = ctx.native_setup.iter().find(|(k, o)| k.split('\n').next() == Some(s.target.as_str()) && **k != mine && **o != setup) {
        return Err(Error::refused(
            "VariantMismatch",
            format!("{} drew with {other}, {} with {setup}", key.split('\n').nth(1).unwrap_or(""), v.role),
            "compare builds on the same GPU backend, adapter and scale; a difference there is not the change",
        ));
    }
    ctx.native_setup.insert(mine, setup);
    if !ctx.gpu_checked {
        ctx.gpu = json!({ "devices": [name], "backend": gpu["backend"], "driver": gpu["driver"] });
        ctx.browser_version = format!("{} {}", b.hello.toolkit, b.hello.toolkit_version).trim().to_string();
        ctx.gpu_checked = true;
    }
    Ok(info)
}

#[allow(clippy::too_many_arguments)]
fn runs(
    ctx: &mut Ctx,
    s: &Series,
    v: &Variant,
    pair: usize,
    set: Set,
    keep: bool,
    target: &Value,
    app: &isolate::App,
    home: &isolate::Home,
    n: &mut Native,
    guard: &isolate::Guard,
    link: &Link,
    steps: &mut Vec<(String, f64)>,
    ratio: Option<f64>,
) -> Result<Block> {
    let lapped = Cell::new(Instant::now());
    let lap = |name: &str, steps: &mut Vec<(String, f64)>| {
        steps.push((name.to_string(), lapped.get().elapsed().as_secs_f64() * 1e3));
        lapped.set(Instant::now());
    };
    let headless = link.headless;
    if headless {
        link.get(Duration::from_secs(60))?;
    } else {
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
        if needs_bridge(ctx, s, target) {
            link.get(Duration::from_secs(60))?;
        }
    }
    guard.check("after the app opened its window")?;
    lap("attach", steps);
    let current = RefCell::new(String::new());
    let counts: RefCell<BTreeMap<String, f64>> = RefCell::new(BTreeMap::new());
    let pixels = ctx.pixels_dir.clone();
    let serve = |op: &str, a: &Value| -> Result<Value> {
        match op {
            "driver" => {
                let call = a["call"].as_str().unwrap_or("");
                let args = a["args"].as_array().cloned().unwrap_or_default();
                // A plain wait needs nothing from the app; every other call goes over its bridge.
                if call == "wait" || (call == "snapshot" && pixels.is_none()) {
                    let ms = if call == "wait" { args.first().and_then(Value::as_f64).unwrap_or(0.0).max(0.0) } else { 0.0 };
                    std::thread::sleep(Duration::from_millis(ms as u64));
                    return Ok(Value::Bool(call == "wait"));
                }
                let bridge = link.get(Duration::from_secs(30)).map_err(|e| Error::usage(format!("page.{call}(): {}", e.message)).hint(e.hint.unwrap_or_default()))?;
                NativeDriver { bridge, op_timeout: Duration::from_secs(30), pixels: pixels.clone() }.dispatch(call, &args)
            }
            "count" => {
                if !current.borrow().is_empty() {
                    *counts.borrow_mut().entry(a["name"].as_str().unwrap_or("").to_string()).or_default() += a["n"].as_f64().unwrap_or(1.0);
                }
                Ok(Value::Bool(true))
            }
            "phase" => {
                let key = current.borrow().clone();
                if let (false, Some(b)) = (key.is_empty(), link.now()?) {
                    let name: String = a["name"].as_str().unwrap_or("phase").chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
                    b.request("mark", json!({ "name": format!("{key}:phase:{name}:{}", a["edge"].as_str().unwrap_or("start")) }), Duration::from_secs(10))?;
                }
                Ok(Value::Bool(true))
            }
            other => Err(Error::msg(format!("unknown runner request {other}"))),
        }
    };
    let t = Duration::from_secs(120);
    let env = home.environment(app);
    ctx.runner.call(
        "block",
        json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": v.root, "home": home.dir, "env": env, "exe": app.exe }),
        t,
        &serve,
    )?;
    if !headless {
        n.check_visible()?;
    }
    lap("checks", steps);
    ctx.runner.call("setup", json!({}), t, &serve).map_err(|e| Error::refused("RunIncomplete", format!("setup: {}", e.message), "fix the bench file's setup"))?;
    lap("setup", steps);
    // Runs use the bridge when the app has connected by now (a page call in setup connects it).
    let bridged = link.now()?;
    let info = match bridged {
        Some(b) => Some(admit_info(ctx, s, v, b)?),
        None => None,
    };
    let procs0 = (host::now_ns(), host::all_processes());
    let sampler = probe::os::Sampler::start(n.pid);
    let run_timeout = Duration::from_secs(60);
    let mut runs: Vec<Run> = Vec::new();
    let mut first: Option<usize> = None;
    let mut unsteady = false;
    while first.is_none_or(|f| runs.len() < f + ctx.runs) {
        let i = runs.len();
        let began = Instant::now();
        if !headless {
            n.raise();
        }
        counts.borrow_mut().clear();
        if let Some(b) = bridged {
            b.request("run.begin", Value::Null, Duration::from_secs(10))?;
        }
        *current.borrow_mut() = format!("run:{i}");
        let snap0 = Snapshot::take(&n.pids());
        let c0 = cycles(n);
        let t0 = host::now_ns();
        ctx.runner
            .call("run", json!({ "case": s.case }), run_timeout, &serve)
            .map_err(|e| Error::refused("RunIncomplete", format!("{} run: {}", s.case, e.message), "the run threw or timed out; fix the bench file or the app"))?;
        let t1 = host::now_ns();
        let c1 = cycles(n);
        let snap1 = Snapshot::take(&n.pids());
        current.borrow_mut().clear();
        let record = match bridged {
            Some(b) => Some(b.request("run.end", Value::Null, Duration::from_secs(30))?),
            None => None,
        };
        let ms = (t1 - t0) as f64 / 1e6;
        runs.push(Run { ms, cpu: cpu_ms(c0, c1, ms), span: wakes::span(&snap0, &snap1), counts: counts.borrow().clone(), record, t0, t1, total: 0.0 });
        if s.reset {
            ctx.runner
                .call("reset", json!({ "case": s.case }), run_timeout, &serve)
                .map_err(|e| Error::refused("RunIncomplete", format!("{} reset: {}", s.case, e.message), "fix the case's reset"))?;
        }
        runs[i].total = began.elapsed().as_secs_f64() * 1e3;
        if ctx.check {
            first = Some(0);
        } else if first.is_none() {
            // Steady once two runs do the same work, within 5% or 1 ms, as for pages (SPEC §9.1).
            let steady = i >= 1 && {
                let (a, b) = (runs[i - 1].work(), runs[i].work());
                a.is_finite() && b.is_finite() && ((a - b).abs() <= 1.0 || (a.max(b) > 0.0 && (a - b).abs() / a.max(b) <= 0.05))
            };
            if steady || i >= MAX_WARMUP {
                unsteady = !steady;
                first = Some(if steady && set != Set::Attribution { i - 1 } else { i + 1 });
            }
        }
    }
    let first = first.unwrap_or(0);
    let warm_ms: f64 = runs[..first].iter().map(|r| r.total).sum::<f64>() + 0.0;
    steps.push(("warmup".into(), warm_ms));
    steps.push(("runs".into(), (lapped.get().elapsed().as_secs_f64() * 1e3 - warm_ms).max(0.0)));
    lapped.set(Instant::now());
    let samples = sampler.stop();
    let procs1 = (host::now_ns(), host::all_processes());
    if !headless {
        n.check_visible()?;
    }
    let measured = &runs[first..];
    let key = |j: usize| format!("run:{}", first + j);
    let per_run = |f: &dyn Fn(&Run) -> f64| -> BTreeMap<String, f64> { measured.iter().enumerate().map(|(j, r)| (key(j), f(r))).filter(|(_, x)| x.is_finite()).collect() };
    let gpu_runs: Vec<probe::os::Summary> = measured.iter().map(|r| samples.window(r.t0, r.t1)).collect();
    // `SoftwareGpu` from outside: the adapter the app's GPU work ran on, when it used one.
    let adapter = gpu_runs.iter().find_map(|g| g.gpu_adapter.clone());
    if let Some(a) = adapter.as_deref().filter(|a| software_adapter(a)) {
        return Err(Error::refused(
            "SoftwareGpu",
            format!("the app renders on a software adapter ({a})"),
            "run on a machine with a GPU; GPU and frame numbers from software rendering describe no real device",
        ));
    }
    if !ctx.gpu_checked && adapter.is_some() {
        ctx.gpu = json!({ "devices": adapter.iter().collect::<Vec<_>>() });
        ctx.gpu_checked = true;
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
    let gpu_3d: BTreeMap<String, f64> = gpu_runs.iter().enumerate().filter_map(|(j, g)| Some((key(j), g.gpu_3d_pct?))).collect();
    if !gpu_3d.is_empty() {
        extra.insert("gpu.process.3d".into(), gpu_3d);
    }
    // The bench file's `ctx.count` and, with the bridge, the app's own exact counters.
    let mut counters: std::collections::BTreeSet<String> = std::iter::once(&s.primary).chain(&s.metrics).filter_map(|m| m.strip_prefix("count.")).map(str::to_string).collect();
    for r in measured {
        counters.extend(r.record.as_ref().and_then(|x| x["counts"].as_object()).into_iter().flatten().map(|(k, _)| k.clone()));
    }
    for name in &counters {
        let count = |r: &Run| r.counts.get(name).copied().unwrap_or(0.0) + r.record.as_ref().and_then(|x| x["counts"][name.as_str()].as_f64()).unwrap_or(0.0);
        extra.insert(format!("count.{name}"), per_run(&count));
    }
    let mut refused = BTreeMap::new();
    let ui = bridged.map(|_| ui_values(measured, &key, &mut extra, &mut refused));
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
    if let Some(u) = &ui {
        values.extend(u.pooled.clone());
    }
    let private_mb = n.processes().iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
    if private_mb > 0.0 {
        values.insert("memory.app.private".into(), private_mb);
    }
    if let Some(v) = gpu_runs.last().and_then(|g| g.vram_dedicated_mb) {
        values.insert("gpu.vram".into(), v);
    }
    if !foreign.is_empty() {
        values.insert("cpu.foreign".into(), metrics::median(&foreign));
        values.insert("cpu.foreign.max".into(), foreign.iter().copied().fold(0.0, f64::max));
    }
    if unsteady {
        values.insert("warmup.unsteady".into(), 1.0);
    }
    for m in std::iter::once(&s.primary).chain(&s.metrics) {
        if values.contains_key(m) || refused.contains_key(m) {
            continue;
        }
        if let Some(why) = missing(m, bridged.is_some(), headless) {
            refused.insert(m.clone(), why);
        }
    }
    if headless {
        values.retain(|k, _| headless_allowed(k) || k.ends_with(".n") || k.starts_with("cpu.foreign") || k == "warmup.unsteady");
    }
    if let Some(why) = refused.get(&s.primary) {
        let (id, text) = why.split_once(": ").unwrap_or((why.as_str(), ""));
        return Err(Error::refused(id, format!("{}: {text}", s.primary), "pick a primary this target can measure"));
    }
    let background = busiest_others(&procs0, &procs1);
    let spans: Vec<Span> = measured.iter().map(|r| r.span.clone()).collect();
    let recording = if keep || set == Set::Attribution {
        let id = store::new_id("rec");
        let dir = store::recording_dir(&id)?;
        let names: BTreeMap<u32, String> =
            spans.iter().flat_map(|s| s.threads.iter().map(|t| t.tid)).collect::<std::collections::BTreeSet<_>>().into_iter().filter_map(|t| Some((t, host::thread_name(t)?))).collect();
        store::write_json(&dir.join("threads.json"), &json!(wakes::by_thread(&spans, &names)))?;
        let run_rows: Vec<Value> = measured
            .iter()
            .enumerate()
            .map(|(j, r)| {
                let mut rec = r.record.clone().unwrap_or(Value::Null);
                if let Some(o) = rec.as_object_mut() {
                    o.remove("frames");
                    o.remove("frame_columns");
                }
                json!({ "run": key(j), "ms": r.ms, "cpu_ms": r.cpu, "switches": r.span.switches, "wakes_per_s": r.span.wakes_per_s(), "io_read": r.span.io_read, "io_write": r.span.io_write, "counts": r.counts, "bridge": rec })
            })
            .collect();
        store::write_json(&dir.join("runs.json"), &json!(run_rows))?;
        if bridged.is_some() {
            let columns = measured.iter().find_map(|r| r.record.as_ref().map(|x| x["frame_columns"].clone())).unwrap_or(Value::Null);
            let frames: serde_json::Map<String, Value> = measured.iter().enumerate().map(|(j, r)| (key(j), r.record.as_ref().map_or(Value::Null, |x| x["frames"].clone()))).collect();
            let marks: serde_json::Map<String, Value> = measured.iter().enumerate().map(|(j, r)| (key(j), r.record.as_ref().map_or(Value::Null, |x| x["marks"].clone()))).collect();
            store::write_json(&dir.join("frames.json"), &json!({ "columns": columns, "runs": frames, "marks": marks }))?;
        }
        store::write_json(
            &dir.join("meta.json"),
            &json!({
                "id": id, "kind": "block", "native": true, "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label,
                "target": app.exe, "probes": set, "runs": measured.len(), "values": values, "refused": refused, "duration_ms": measured.iter().map(|r| r.ms).sum::<f64>() as u64,
                "created": super::super::record::now_iso(), "valid": true, "env": host::env(), "window": n.window, "gpu_adapter": adapter, "background": background,
                "hello": bridged.map(|b| &b.hello), "info": info,
                "timing": if set == Set::Attribution { "perturbed" } else { "clean" },
                "note": if set == Set::Attribution { "a native block's attribution: run records and OS samples, not timed; native stacks need the deep helper (SPEC §13.4)" } else { "" },
            }),
        )?;
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
        // `StateDrift`: the app's element count at each run's end, as a page's DOM nodes.
        nodes: measured.iter().filter_map(|r| r.record.as_ref()?["counts"]["elements"].as_u64()).collect(),
        recording,
        background,
        refused,
        ..Block::default()
    })
}

/// What the bridge's run records add, pooled over the measured runs.
struct Ui {
    pooled: BTreeMap<String, f64>,
}

/// Per run: `ui.work`, `ui.cpu` and the phases, `ui.frames`. Pooled: `ui.interval.*` from each surface's
/// submission gaps, and when the toolkit reports presentation times, `frames.presented.*`,
/// `frames.interval.*` and `interaction.*`. Frame metrics are refused when a surface skipped work
/// (occluded) or spun unpaced in a run.
fn ui_values(measured: &[Run], key: &dyn Fn(usize) -> String, extra: &mut BTreeMap<String, BTreeMap<String, f64>>, refused: &mut BTreeMap<String, String>) -> Ui {
    for (metric, field) in
        [("ui.work", "wall_ms"), ("ui.cpu", "cpu_ms"), ("ui.view", "view_ms"), ("ui.layout", "layout_ms"), ("ui.text", "text_ms"), ("ui.paint", "paint_ms"), ("ui.encode", "encode_ms")]
    {
        let v: BTreeMap<String, f64> = measured.iter().enumerate().filter_map(|(j, r)| Some((key(j), r.record.as_ref()?["work"][field].as_f64()?))).collect();
        if v.len() == measured.len() {
            extra.insert(metric.into(), v);
        }
    }
    extra.insert("ui.frames".into(), measured.iter().enumerate().map(|(j, r)| (key(j), r.record.as_ref().and_then(|x| x["frames"].as_array()).map_or(0, Vec::len) as f64)).collect());
    let mut gaps: Vec<f64> = Vec::new();
    let mut presented: Vec<f64> = Vec::new();
    let mut present_gaps: Vec<f64> = Vec::new();
    let mut inputs: Vec<f64> = Vec::new();
    let mut skipped: Option<String> = None;
    let mut spun: Option<String> = None;
    for r in measured {
        let Some(rec) = &r.record else { continue };
        let cols: Vec<&str> = rec["frame_columns"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let col = |name: &str| cols.iter().position(|c| *c == name);
        let (surface, begin, submit, present, input) = (col("surface"), col("begin_ns"), col("submit_ns"), col("present_ns"), col("input_ns"));
        let num = |row: &Value, i: Option<usize>| i.and_then(|i| row[i].as_f64()).unwrap_or(0.0);
        let mut last: BTreeMap<i64, (f64, f64)> = BTreeMap::new();
        for row in rec["frames"].as_array().into_iter().flatten() {
            let (sf, sub, pre) = (num(row, surface) as i64, num(row, submit), num(row, present));
            let prev = last.insert(sf, (sub, pre));
            if let Some((ps, pp)) = prev {
                if sub > ps {
                    gaps.push((sub - ps) / 1e6);
                }
                if pre > 0.0 && pp > 0.0 && pre > pp {
                    present_gaps.push((pre - pp) / 1e6);
                }
            }
            if pre > 0.0 {
                presented.push((pre - num(row, begin)) / 1e6);
                if num(row, input) > 0.0 {
                    inputs.push((pre - num(row, input)) / 1e6);
                }
            }
        }
        for (name, sf) in rec["surfaces"].as_object().into_iter().flatten() {
            let n = |k: &str| sf[k].as_u64().unwrap_or(0);
            if n("occluded") + n("skipped") > 0 && skipped.is_none() {
                skipped = Some(format!("WindowHidden: surface {name} skipped {} frames (occluded {}) in a measured run, so frames describe less work than a person sees", n("skipped"), n("occluded")));
            }
            if n("unpaced") > 10 && spun.is_none() {
                spun = Some(format!("UnpacedSpin: surface {name} drew {} frames without waiting for the display in a measured run", n("unpaced")));
            }
        }
    }
    let mut pooled = BTreeMap::new();
    let pct = |v: &mut Vec<f64>, prefix: &str, out: &mut BTreeMap<String, f64>| {
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            return;
        }
        for (name, p) in [("p50", 0.5), ("p90", 0.9), ("p95", 0.95), ("p99", 0.99)] {
            out.insert(format!("{prefix}.{name}"), metrics::percentile(v, p));
        }
        out.insert(format!("{prefix}.max"), *v.last().unwrap_or(&0.0));
    };
    pooled.insert("ui.n".into(), gaps.len() as f64);
    pct(&mut gaps, "ui.interval", &mut pooled);
    if !presented.is_empty() {
        pooled.insert("frames.n".into(), presented.len() as f64);
        pct(&mut presented, "frames.presented", &mut pooled);
        pct(&mut present_gaps, "frames.interval", &mut pooled);
        if !inputs.is_empty() {
            inputs.sort_by(f64::total_cmp);
            pooled.insert("interaction.n".into(), inputs.len() as f64);
            pooled.insert("interaction.p50".into(), metrics::percentile(&inputs, 0.5));
            pooled.insert("interaction.p95".into(), metrics::percentile(&inputs, 0.95));
        }
    }
    if let Some(why) = skipped.or(spun) {
        extra.retain(|k, _| !frame_metric(k));
        let names: Vec<String> = pooled.keys().filter(|k| frame_metric(k)).cloned().chain(["ui.frames".to_string()]).collect();
        for k in names {
            pooled.remove(&k);
            refused.insert(k, why.clone());
        }
        for k in ["ui.interval.p50", "ui.interval.p95", "ui.interval.max", "frames.presented.p95", "interaction.p95"] {
            refused.entry(k.to_string()).or_insert_with(|| why.clone());
        }
    }
    Ui { pooled }
}

/// Why a declared metric has no value on a native target.
fn missing(metric: &str, bridged: bool, headless: bool) -> Option<String> {
    let area = metric.split('.').next().unwrap_or("");
    if headless && !headless_allowed(metric) {
        return Some(format!("NoDisplayForBudget: {metric} needs a display; a headless app has none"));
    }
    match area {
        "frames" | "interaction" | "scroll" | "display" if bridged => Some(format!(
            "NoPresentSource: {metric} needs when frames reached the screen; this toolkit reports only when it submitted them (ui.frames, ui.interval.*), and OS present events are not read yet"
        )),
        "frames" | "interaction" | "scroll" | "display" | "ui" => {
            Some(format!("NoPresentSource: {metric} comes from inside the app; without its bridge strobe sees its processes, threads, GPU and window only"))
        }
        "canvas" | "main" | "layout" | "style" | "js" | "dom" | "net" | "gpu" if area != "gpu" || metric == "gpu.compile" => {
            Some(format!("NoPresentSource: {metric} is a browser engine's; a native app has none"))
        }
        _ => None,
    }
}

/// Software rasterizers by adapter name.
fn software_adapter(name: &str) -> bool {
    let n = name.to_lowercase();
    ["basic render", "warp", "llvmpipe", "lavapipe", "swiftshader", "softpipe"].iter().any(|s| n.contains(s))
}

/// How a native launch ends.
enum Until {
    /// The main window is visible.
    Visible,
    /// The window's pixels first changed from its first frame.
    Painted,
    /// The window's pixels last changed, once they stayed the same for a second.
    Settled,
    /// An element shows, as the app's bridge sees it.
    Selector(Value),
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
fn watch(pid: u32, capture: bool, stop: &AtomicBool, tx: std::sync::mpsc::Sender<Sample>) -> Vec<(f64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pids = Vec::new();
    let mut read = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if read.elapsed() > Duration::from_millis(300) {
            pids = host::tree_pids(pid);
            host::raise_windows(pid);
            read = Instant::now();
        }
        let shown = host::main_window(&pids).is_some();
        let px = if capture && shown { host::window_pixels(&pids) } else { None };
        let at = isolate::epoch_ms();
        if let Some(p) = &px {
            out.push((at, p.clone()));
        }
        let cycles = host::process_tree(pid).iter().map(|p| p.cycles).sum();
        let _ = tx.send(Sample { at, shown, px, cycles });
        std::thread::sleep(Duration::from_millis(30));
    }
    out
}

pub(super) fn launch(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize) -> Result<Block> {
    let key = format!("{}\n{}", s.label(), v.role);
    // One launch per variant and series is discarded first, as for apps (SPEC §9.6).
    if !ctx.check && ctx.launched.insert(key) {
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
    if target["kind"] == "headless" {
        return Err(Error::refused("NoDisplayForBudget", format!("{}: a launch is timed to what a person sees; a headless app shows nothing", s.case), "time launches on a native() target"));
    }
    let key = format!("{}\n{}", s.label(), v.role);
    let milestones = milestones(&spec)?;
    if milestones.iter().any(|m| m.source == "page") {
        return Err(Error::usage(format!("{}: a native app has no page marks; read milestones from its output or a log file", s.case)));
    }
    let until = match spec["until"].get("window").and_then(Value::as_str) {
        Some("visible") => Until::Visible,
        Some("painted") => Until::Painted,
        Some("settled") => Until::Settled,
        Some(other) => return Err(Error::usage(format!("{}: launch.until.window is visible, painted or settled, not {other}", s.case))),
        None if target["bridge"] == false => {
            return Err(Error::usage(format!("{}: launch.until a selector needs the app's bridge, which this target turns off; use {{ window: \"settled\" }}", s.case)));
        }
        None => Until::Selector(spec["until"].clone()),
    };
    let capture = spec["screen"] != false;
    if !capture && matches!(until, Until::Painted | Until::Settled) {
        return Err(Error::usage(format!("{}: launch.until.window painted or settled reads the window's pixels; drop screen: false", s.case)));
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
    let app = app(v, &target, &ctx.config)?;
    let home = if warm { ctx.homes.entry(key).or_insert_with(isolate::new_home).clone() } else { isolate::new_home() };
    let prepared = isolate::prepare_home(&app, seed_dir(ctx, s, &target).as_deref(), Some(home.clone()))?;
    let env = prepared.environment(&app);
    ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": v.root, "home": home, "env": env, "exe": app.exe }), t, &no_page)?;
    if spec["before"] == true {
        ctx.runner
            .call("before", json!({ "case": s.case }), Duration::from_secs(900), &no_page)
            .map_err(|e| Error::refused("RunIncomplete", format!("{} before: {}", s.case, e.message), "fix the launch's before hook"))?;
    }
    lap("before", &mut steps);
    let listener = if target["bridge"] == false { None } else { Some(Listener::open()?) };
    let bridge_env = listener.as_ref().map(Listener::env).unwrap_or_default();
    let clock = (host::tsc(), Instant::now());
    let (mut n, guard) = match nt::launch(&app, &prepared, &bridge_env, true, ctx.pin) {
        Ok(x) => x,
        Err(e) => {
            isolate::release_home(&home);
            if !warm {
                isolate::remove_home(&home);
            }
            return Err(e);
        }
    };
    let link = Link::new(&target, &n, !profile, listener);
    let result = (|| -> Result<Block> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = {
            let (stop, pid) = (stop.clone(), n.pid);
            std::thread::spawn(move || watch(pid, capture, &stop, tx))
        };
        let tails = {
            let globs: Vec<String> = milestones.iter().filter(|m| m.source != "output").map(|m| m.source.clone()).collect();
            let stop = stop.clone();
            let home = home.clone();
            (!globs.is_empty()).then(|| std::thread::spawn(move || tail(&home, &globs, &stop)))
        };
        let timeout = Duration::from_millis(spec["timeout"].as_u64().unwrap_or(120_000));
        let deadline = Instant::now() + timeout;
        // (epoch ms, cycles of the tree) when the launch was ready.
        let ready: (f64, u64) = if let Until::Selector(sel) = &until {
            // The toolkit stamps the commit that first showed the element on strobe's clock. A launch
            // whose element already showed in an earlier frame when strobe asked has no valid end.
            let b = link.get(timeout)?;
            let r = b
                .request("wait", json!({ "until": sel, "timeout_ms": deadline.saturating_duration_since(Instant::now()).as_millis() as u64 }), timeout + Duration::from_secs(10))
                .map_err(|e| Error::refused("RunIncomplete", format!("{} launch: {}", s.case, e.message), "the until selector never showed"))?;
            let cycles = n.processes().iter().map(|p| p.cycles).sum();
            if r["already"] == true && r["frame_seq"].as_u64().is_some_and(|f| f > 1) {
                return Err(Error::refused(
                    "RunIncomplete",
                    format!("{} launch: the until selector already showed in frame {} when strobe asked, so the launch has no valid end", s.case, r["frame_seq"]),
                    "the toolkit's bridge must connect before its first frame commits",
                ));
            }
            let at_ns = r["at_ns"].as_u64().ok_or_else(|| Error::msg("bridge wait: no at_ns in its reply"))?;
            (n.epoch_ms + (at_ns as f64 - n.at_ns as f64) / 1e6, cycles)
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
                if n.exited().is_some() && host::process_tree(n.pid).is_empty() {
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
        // The tree's cycles at ready (it started at zero) over the time stamp counter's rate.
        let cpu_ms = match (clock.0, host::tsc()) {
            (Some(a), Some(b)) if b > a && ready.1 > 0 => ready.1 as f64 / ((b - a) as f64 / (clock.1.elapsed().as_secs_f64() * 1e3)),
            _ => f64::NAN,
        };
        let mut lines: Vec<Line> = Vec::new();
        let settle = Instant::now() + Duration::from_secs(1);
        let grace = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(rx) = &n.output {
                lines.extend(rx.try_iter().map(|(at, l)| (at, l, "output".to_string())));
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
        let pixels = joined(watcher);
        lap("milestones", &mut steps);
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
        let recording = if profile {
            let id = store::new_id("rec");
            let dir = store::recording_dir(&id)?;
            store::write_json(
                &dir.join("meta.json"),
                &json!({
                    "id": id, "kind": "block", "native": true, "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label,
                    "target": app.exe, "probes": "attribution", "runs": 1, "values": values, "created": super::super::record::now_iso(), "valid": true, "env": host::env(), "launch": true,
                    "hello": link.bridge.get().map(|b| &b.hello), "timing": "perturbed",
                    "note": "a native launch's attribution: its values and output, not timed; native stacks need the deep helper (SPEC §13.4)",
                }),
            )?;
            Some(id)
        } else {
            None
        };
        Ok(Block { variant: v.role.to_string(), pair, values, run_ms: vec![ready_ms], recording, steps: steps.clone(), ..Block::default() })
    })();
    link.quit();
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
    }
}
