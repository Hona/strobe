//! Launch cases (SPEC §9.6): a block launches the target once, cold, and times it from process spawn
//! (or, for a web target, from navigation) until the case's `until` selector is visible, with
//! milestones on the way: lines in the app's output, its log files or its page marks, Node's own
//! bootstrap timing, and when the window's pixels first and last changed.

use super::*;
use crate::target::cdp::Cdp;
use crate::target::command;
use crate::target::electron::{Extra, Started};
use crate::target::isolate::epoch_ms;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

/// One milestone: the first line matching `pattern` from `source`: `output` (stdout and stderr),
/// `page` (the page's `performance.mark` names) or a glob of files under the launch's home.
pub(super) struct Milestone {
    pub(super) name: String,
    pub(super) pattern: regex_lite::Regex,
    pub(super) source: String,
}

/// A line seen during a launch: when (epoch ms), its text, and where it came from.
pub(super) type Line = (f64, String, String);

pub(super) fn block(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, x: &Exec) -> Result<Block> {
    let key = format!("{}\n{}", s.label(), v.role);
    // The first launch of a build pays for its files being read from disk and scanned: one launch per
    // variant and series is discarded before any is measured. A warm profile fills its home in it.
    if !x.untimed && ctx.launched.insert(key.clone()) {
        progress(format!("  {} warm-up launch (discarded)", v.role));
        once(ctx, s, v, pair, &key, false)?;
    }
    once(ctx, s, v, pair, &key, false)
}

/// An attribution launch: never timed. The main process starts paused under `--inspect-brk` with its
/// CPU profiler on (app and exe targets), and every page starts paused until its own profiler runs,
/// so both profiles cover the launch from the first statement.
pub(super) fn attribution(ctx: &mut Ctx, s: &Series, v: &Variant) -> Result<Block> {
    if is_native(&ctx.describe["targets"][&s.target]) {
        return super::native::launch_once(ctx, s, v, usize::MAX, true);
    }
    let key = format!("{}\n{}", s.label(), v.role);
    once(ctx, s, v, usize::MAX, &key, true)
}

fn once(ctx: &mut Ctx, s: &Series, v: &Variant, pair: usize, key: &str, profile: bool) -> Result<Block> {
    let spec = s.launch.clone();
    let target = ctx.describe["targets"][&s.target].clone();
    let kind = target["kind"].as_str().unwrap_or("").to_string();
    let web = kind == "web" || kind == "url";
    let milestones = milestones(&spec)?;
    let t = Duration::from_secs(120);
    let mut steps: Vec<(String, f64)> = Vec::new();
    let mut lapped = Instant::now();
    let mut lap = |name: &str, steps: &mut Vec<(String, f64)>| {
        steps.push((name.to_string(), lapped.elapsed().as_secs_f64() * 1e3));
        lapped = Instant::now();
    };
    // An app's home is ready before the hook (folders, seed, profile files, a `{port}` a warm home
    // keeps), and the hook gets the environment and executable the app launches with, so it can start
    // what the app expects to find (a background service) the way the app would.
    let app = if matches!(kind.as_str(), "app" | "exe" | "webview") { Some(with_gpu_cache(ctx, s, v, &target, variant_app(v, &target, &ctx.config)?)?) } else { None };
    // An app on the OS's webview has no Node main process to profile or inspect.
    let electron = app.as_ref().is_some_and(|a| a.kind == isolate::AppKind::Electron);
    if spec["inspect"] == true && !electron {
        return Err(Error::usage(format!("{}: launch.inspect reads Node's timing from an Electron app's main process", s.case)));
    }
    if let Some(a) = app.as_ref().filter(|_| !profile) {
        dev_build(&a.exe)?;
    }
    // `fresh`: a new home per launch. `warm`: one home per series and variant, kept across its launches,
    // so caches and state from the discarded first launch are there, as for a person's second launch.
    let warm = match spec["profile"].as_str().unwrap_or("fresh") {
        "fresh" => false,
        "warm" => true,
        other => return Err(Error::usage(format!("{}: launch.profile is fresh or warm, not {other}", s.case))),
    };
    let home = (!web).then(|| if warm { ctx.homes.entry(key.to_string()).or_insert_with(isolate::new_home).clone() } else { isolate::new_home() });
    if let Some(h) = &home {
        std::fs::create_dir_all(h)?;
    }
    let seed = target["home"].as_str().map(|h| {
        let h = s.params.iter().fold(h.to_string(), |h, (k, val)| h.replace(&format!("{{{k}}}"), &value_text(val)));
        Path::new(ctx.describe["file"].as_str().unwrap_or(".")).parent().unwrap_or(Path::new(".")).join(h)
    });
    let prepared = match &app {
        Some(a) => Some(isolate::prepare_home(a, seed.as_deref(), home.clone())?),
        None => None,
    };
    let env = app.as_ref().zip(prepared.as_ref()).map(|(a, h)| h.environment(a));
    let root = v.root.as_ref().map(|r| r.display().to_string());
    let exe = app.as_ref().map(|a| a.exe.display().to_string());
    ctx.runner.call("block", json!({ "case": s.case, "params": s.params, "target": s.target, "variant": v.role, "url": "", "root": root, "home": home, "env": env, "exe": exe }), t, &no_page)?;
    if spec["before"] == true {
        ctx.runner
            .call("before", json!({ "case": s.case }), Duration::from_secs(900), &no_page)
            .map_err(|e| crate::runner::incomplete(&format!("{} before", s.case), "before", e, "", "fix the launch's before hook"))?;
    }
    lap("before", &mut steps);
    let mut window = ctx.window.clone();
    if let Some(vp) = target.get("viewport").filter(|v| v.is_object()) {
        window.width = vp["width"].as_u64().unwrap_or(1400) as u32;
        window.height = vp["height"].as_u64().unwrap_or(900) as u32;
    }
    ctx.used = Some(window.clone());
    let screen = !web && spec["screen"] != false && !profile;
    let inspect_port = if spec["inspect"] == true && !profile { Some(free_port()?) } else { None };
    let mut guard = None;
    // The time stamp counter's rate, from two readings around the launch: cycles to ms.
    let clock = (host::tsc(), Instant::now());
    let (mut browser, started) = match kind.as_str() {
        "app" | "exe" | "webview" => {
            let app = app.as_ref().ok_or_else(|| Error::msg("internal: an app target without its app"))?;
            let extra = Extra {
                args: inspect_port.map(|p| vec![format!("--inspect={p}")]).unwrap_or_default(),
                output: true,
                home: home.clone(),
                prepared: prepared.clone(),
                profile_main: if profile && electron { Some(free_port()?) } else { None },
            };
            let (b, g, st) = electron::launch_with(app, seed.as_deref(), &window, ctx.pin, &extra)?;
            guard = Some(g);
            (b, st)
        }
        "command" => {
            let root = v.root.clone().ok_or_else(|| Error::msg(format!("{}: a command target runs in a variant's folder (a directory or a git ref)", v.role)))?;
            command::launch(&command::Spec::from_target(&target, &root), home.as_deref().unwrap_or(Path::new(".")), !warm, &window, ctx.pin)?
        }
        _ if web => (chromium::launch(&window, ctx.headless, ctx.pin, &[])?, Started { epoch_ms: 0.0, output: None, main: None }),
        other => return Err(Error::usage(format!("{}: launch cases need an app, exe, command, web or url target, not {other}", s.case))),
    };
    let result = (|| -> Result<Block> {
        let watch = Watch::start(&browser.cdp, observer(&spec["until"])?, profile)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (pid, profile_dir) = (browser.pid, browser.profile.clone());
        // An app's windows stay on top without focus (an occluded window never paints); `screen` also
        // captures their pixels.
        let sampler = (!web).then(|| {
            let stop = stop.clone();
            std::thread::spawn(move || sample_screen(pid, profile_dir.as_deref(), screen, &stop))
        });
        let tails = {
            let globs: Vec<String> = milestones.iter().filter(|m| m.source != "output" && m.source != "page").map(|m| m.source.clone()).collect();
            let stop = stop.clone();
            let home = home.clone();
            home.filter(|_| !globs.is_empty()).map(|home| std::thread::spawn(move || tail(&home, &globs, &stop)))
        };
        // CPU of the target's processes in cycles (exact; process times move in 15.6 ms ticks), from
        // spawn, or from navigation for a web target whose browser was already up.
        let cpu_now = || -> (u64, u64) { browser.processes().iter().fold((0, 0), |(c, n), p| (c + p.cycles, n + p.cpu_ns)) };
        let mut cpu0 = (0, 0);
        let started_ms = if web {
            let page = browser.page()?;
            let first = if target["prepare"].as_bool() == Some(true) {
                let r = ctx.runner.call("prepare", json!({ "endpoint": browser.ws, "target": page.target }), t, &no_page)?;
                let base = variant_url(v, &target)?;
                r.get("url").and_then(Value::as_str).map(|u| join_url(origin(&base), u)).unwrap_or(base)
            } else {
                variant_url(v, &target)?
            };
            // An attribution launch's page profiler started when the page was attached, before this.
            cpu0 = cpu_now();
            let at = epoch_ms();
            page.send("Page.navigate", json!({ "url": first }))?;
            at
        } else {
            started.epoch_ms
        };
        let timeout = Duration::from_millis(spec["timeout"].as_u64().unwrap_or(120_000));
        let (ready_at, page) = match watch.ready(timeout) {
            Ok(found) => found,
            Err(e) => {
                let shot = watch.any_page().map(|p| snapshot(&p, &ctx.exp, "launch")).unwrap_or_default();
                return Err(Error::refused("RunIncomplete", format!("{} launch: {}{shot}", s.case, e.message), "the until selector never showed"));
            }
        };
        let cpu1 = cpu_now();
        let cpu_ms = match (clock.0, host::tsc()) {
            (Some(a), Some(b)) if b > a && cpu1.0 > 0 => cpu1.0.saturating_sub(cpu0.0) as f64 / ((b - a) as f64 / (clock.1.elapsed().as_secs_f64() * 1e3)),
            _ => cpu1.1.saturating_sub(cpu0.1) as f64 / 1e6,
        };
        lap("ready", &mut steps);
        if watch.late.load(Ordering::Relaxed) {
            return Err(Error::refused(
                "RunIncomplete",
                format!("{} launch: the window showed its until selector before strobe could attach, so the launch has no valid end", s.case),
                "a target strobe reaches only after its first page loads cannot be timed to a selector",
            ));
        }
        validity::window_visible(&page)?;
        // A webview app's launch is not moved or sized: the block records the window it opened.
        if browser.embedded {
            ctx.used = Some(crate::target::webview::fit(&browser, &page, None, false)?);
        }
        if !ctx.gpu_checked {
            ctx.gpu = serde_json::to_value(if ctx.headless { browser.gpu()? } else { validity::software_gpu(&browser)? })?;
            ctx.browser_version = browser.version.clone();
            ctx.gpu_checked = true;
        }
        // Milestones may land after ready (a service that comes up behind the window), and the screen
        // settles after it: lines are collected for 1 s, and up to 3 s while a milestone is missing.
        let mut lines: Vec<Line> = Vec::new();
        let settle = Instant::now() + Duration::from_secs(1);
        let grace = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(rx) = &started.output {
                lines.extend(rx.try_iter().map(|(at, _, l)| (at, l, "output".to_string())));
            }
            let now = Instant::now();
            if now >= grace || (now >= settle && found_all(&milestones, &lines, |m| m.source == "output")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        if milestones.iter().any(|m| m.source == "page") {
            lines.extend(page_marks(&page));
        }
        stop.store(true, Ordering::Relaxed);
        watch.stop();
        if let Some(h) = tails {
            lines.extend(joined(h));
        }
        let pixels = sampler.map(joined).unwrap_or_default();
        lap("milestones", &mut steps);
        let mut values = BTreeMap::new();
        let ready_ms = ready_at - started_ms;
        values.insert("launch.ready".to_string(), ready_ms);
        values.insert("cpu.work".to_string(), cpu_ms);
        for m in &milestones {
            // A capture named `at` holding epoch ms (a mark the app wrote into its log) is the time;
            // otherwise the time strobe read the line.
            let first = lines
                .iter()
                .filter(|(_, _, src)| *src == m.source)
                .filter_map(|(at, l, _)| m.pattern.captures(l).map(|c| c.name("at").and_then(|v| v.as_str().parse::<f64>().ok()).filter(|v| *v > 1e12).unwrap_or(*at)))
                .min_by(f64::total_cmp);
            match first {
                Some(at) => {
                    values.insert(format!("launch.{}", m.name), at - started_ms);
                }
                None if s.primary == format!("launch.{}", m.name) || s.metrics.iter().any(|x| *x == format!("launch.{}", m.name)) => {
                    return Err(Error::refused("RunIncomplete", format!("{} launch: milestone {} never matched in {}", s.case, m.name, m.source), "check the milestone's match and where it looks"));
                }
                None => {}
            }
        }
        if let Some((painted, settled)) = screen_times(&pixels) {
            values.insert("launch.painted".into(), painted - started_ms);
            values.insert("launch.settled".into(), settled - started_ms);
        }
        // Node's timing is context: an app that ignores `--inspect` (an Electron fuse turns it off in
        // some release builds) still has a valid launch, without `launch.node.*`.
        if let Some(port) = inspect_port {
            match node_timing(port) {
                Ok(timing) => {
                    for (name, at) in timing {
                        values.insert(format!("launch.node.{name}"), at - started_ms);
                    }
                }
                Err(e) if !ctx.warnings.iter().any(|w| w.id == "Inspect") => ctx.warnings.push(validity::warn("Inspect", format!("{}: {}", v.role, e.message))),
                Err(_) => {}
            }
            lap("node", &mut steps);
        }
        if !web && declares(s, "memory.app.") {
            memory(&browser.processes(), &mut values);
        }
        let why_no_pixels = if web { "a page's launch is timed in the page; window pixels are an app, exe or command target's".to_string() } else { no_pixels(screen) };
        let refused = refusals(s, &values, &why_no_pixels, web);
        if let (false, Some(r)) = (profile, refused.get(&s.primary)) {
            return Err(r.clone().fix("pick a primary a launch case measures").into());
        }
        let recording = if profile { Some(save_profiles(ctx, s, v, &page, &started, &values)?) } else { None };
        // What the launch ended on, once its values and profiles are taken.
        let end = shown::page(ctx, s, v, pair, "end", &page);
        shown::expect(&s.expect, &end, "when the launch was ready", true, &|sel| shown::page_shows(&page, sel))?;
        Ok(Block {
            shown: Some(shown::Shown { start: shown::Screen::default(), end }),
            variant: v.role.to_string(),
            pair,
            values,
            run_ms: vec![ready_ms],
            warmup: 0,
            speed_ratio: None,
            nodes: Vec::new(),
            recording,
            phases: BTreeMap::new(),
            frames: Vec::new(),
            intervals: Vec::new(),
            steps: steps.clone(),
            background: Vec::new(),
            leak: None,
            refused,
            locked: false,
            display: None,
            screen: None,
            reserve: None,
            overhead: None,
        })
    })();
    // The app closes first, then the bench file's teardown runs while the home still exists (it may
    // stop a service whose registration is in there), then a fresh home goes; a warm one stays.
    let home = browser.profile.take();
    browser.close();
    if let Some(h) = &home {
        isolate::release_home(h);
    }
    let closed = ctx.runner.call("close", json!({}), t, &no_page);
    // Its GPU caches go back to their store and shared seeds unlink before the home goes.
    if let Some(h) = home.filter(|_| !warm) {
        isolate::remove_home(&h);
    }
    if let Some(g) = &guard {
        g.check("after a launch")?;
    }
    if let Err(e) = closed
        && !ctx.warnings.iter().any(|w| w.id == "Teardown")
    {
        ctx.warnings.push(validity::warn("Teardown", format!("{} launch: {}", v.role, e.message)));
    }
    result
}

/// What a launch case measures is the launch: from the spawn until ready, its milestones, the CPU it
/// took and, for a launched app, its memory once ready. Every other declared metric is refused with
/// why (a run's, or one this OS cannot read), never left without a value. `pixels`: why the window's
/// pixels were not read, when they were not.
pub(super) fn refusals(s: &Series, values: &BTreeMap<String, f64>, pixels: &str, web: bool) -> BTreeMap<String, Refusal> {
    let mut out = BTreeMap::new();
    for m in std::iter::once(&s.primary).chain(&s.metrics) {
        if values.contains_key(m) || out.contains_key(m) {
            continue;
        }
        let r = super::super::outside::not_on_this_os(m).unwrap_or_else(|| match m.as_str() {
            "launch.painted" | "launch.settled" => Refusal::new(ValidatorId::NoPresentSource, format!("{m} reads the window's pixels: {pixels}")),
            _ if m.starts_with("launch.node.") => Refusal::new(
                ValidatorId::NotForTarget,
                format!("{m} is Node's own timing, read over --inspect from an app or exe target's main process (launch.inspect: true); this launch gave none"),
            ),
            "cpu.work" => Refusal::new(ValidatorId::RunIncomplete, "cpu.work: no CPU of the app's processes could be read when it was ready"),
            "memory.app.private" | "memory.app.host" if web => {
                Refusal::new(ValidatorId::NotForTarget, format!("{m}: a page's launch runs in the browser strobe started before it, so its memory is not the launch's"))
            }
            "memory.app.private" | "memory.app.host" => Refusal::new(ValidatorId::RunIncomplete, format!("{m}: the app's processes could not be read when it was ready")),
            _ => Refusal::new(
                ValidatorId::NotForTarget,
                format!("{m} is measured over a run; a launch case measures the launch from the spawn until ready (launch.*, cpu.work, memory.app.*): declare {m} on a run case"),
            ),
        });
        out.insert(m.clone(), r);
    }
    out
}

/// Whether the series declares a metric starting with `prefix` (its primary or a secondary).
pub(super) fn declares(s: &Series, prefix: &str) -> bool {
    std::iter::once(&s.primary).chain(&s.metrics).any(|m| m.starts_with(prefix))
}

/// Why a launch read none of its window's pixels.
pub(super) fn no_pixels(asked: bool) -> String {
    if !asked {
        return "the case turns them off (screen: false)".into();
    }
    #[cfg(target_os = "macos")]
    if !host::macos::screen_capture_allowed() {
        return format!("macOS gives strobe a window's pixels only with Screen Recording ({}, for {})", host::macos::screen_recording_pane(), host::macos::screen_recording_grantee());
    }
    "no frame of the window was read while the app launched (it showed none in time, or the OS gave strobe none of its pixels: a native Wayland window)".into()
}

/// A launch's memory once ready (`memory.app.private`, and `memory.app.host` where the OS gives
/// memory by what holds it), from its process set.
pub(super) fn memory(procs: &[host::ProcTimes], values: &mut BTreeMap<String, f64>) {
    let private = procs.iter().map(|p| p.private_bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
    if private > 0.0 {
        values.insert("memory.app.private".into(), private);
    }
    values.extend(probe::memory::Reading::take(procs, "when the launch was ready").values().into_iter().map(|(k, v)| (k.to_string(), v)));
}

pub(super) fn no_page(_: &str, _: &Value) -> Result<Value> {
    Err(Error::msg("a launch case's hooks have no page driver: a launch is timed from outside the app"))
}

pub(super) fn milestones(spec: &Value) -> Result<Vec<Milestone>> {
    spec["milestones"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, d)| {
            if ["ready", "painted", "settled"].contains(&name.as_str()) || name.starts_with("node.") {
                return Err(Error::usage(format!("launch milestone {name}: launch.{name} is strobe's own; name it something else")));
            }
            let pattern = pattern(d, &format!("launch milestone {name}"))?;
            Ok(Milestone { name: name.clone(), pattern, source: d["in"].as_str().unwrap_or("output").to_string() })
        })
        .collect()
}

/// A line pattern as the bench file gives it: `match` (a string, matched as is) or `regex` (a RegExp's
/// source and flags).
pub(super) fn pattern(d: &Value, what: &str) -> Result<regex_lite::Regex> {
    let source = match (&d["match"], &d["regex"]) {
        (Value::String(text), _) => regex_lite::escape(text),
        (_, r) if r.is_object() => format!("{}{}", if r["flags"].as_str().unwrap_or("").contains('i') { "(?i)" } else { "" }, r["source"].as_str().unwrap_or("")),
        _ => return Err(Error::usage(format!("{what}: `match` is a string or a RegExp"))),
    };
    regex_lite::Regex::new(&source).map_err(|e| Error::usage(format!("{what}: {e}")))
}

/// `ctx.waitForLine(match, { in, timeout })` in a block's hooks: each call registers a watch
/// (`watch`) and then polls it (`poll`) until it has the first line matching it that the app wrote
/// after the watch: from its output (`in: "output"`, the default; only a target with `output: true`
/// pipes it) or from files under its home matching a glob. A poll never blocks, so the runner keeps
/// serving the hook's other calls (a click) meanwhile. A line carries when strobe read it on the host
/// clock (ns), the clock of phase marks and presents: output as its reader thread got it, a log file's
/// line as the watch's own tail thread read it.
#[derive(Default)]
pub(super) struct Lines {
    watches: Vec<LineWatch>,
    /// The app's last few output lines, for a refusal when it exits.
    last: std::collections::VecDeque<String>,
}

struct LineWatch {
    pattern: regex_lite::Regex,
    source: String,
    deadline: Instant,
    timeout: Duration,
    tail: Option<Tail>,
    found: Option<(u64, String)>,
}

/// A log glob's tail thread: the lines it read (host clock ns), and the flag that ends it.
type Tail = (Receiver<(u64, String)>, Arc<AtomicBool>);

impl Drop for LineWatch {
    fn drop(&mut self) {
        if let Some((_, stop)) = &self.tail {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

impl Lines {
    /// Hands the app's new output lines to the watches waiting on output; with none waiting they are
    /// dropped but the last few, so nothing buffers while no case asks.
    fn pull(&mut self, rx: Option<&Receiver<isolate::OutputLine>>) {
        let Some(rx) = rx else { return };
        while let Ok((_, at, line)) = rx.try_recv() {
            for w in self.watches.iter_mut().filter(|w| w.source == "output" && w.found.is_none()) {
                if w.pattern.is_match(&line) {
                    w.found = Some((at, line.clone()));
                }
            }
            if self.last.len() == 5 {
                self.last.pop_front();
            }
            self.last.push_back(line);
        }
    }

    /// The app's last output lines (none unless the target pipes its output: `output: true`).
    pub(super) fn last(&mut self, rx: Option<&Receiver<isolate::OutputLine>>) -> Vec<String> {
        self.pull(rx);
        self.last.iter().cloned().collect()
    }

    pub(super) fn serve(&mut self, rx: Option<&Receiver<isolate::OutputLine>>, home: &Path, a: &Value) -> Result<Value> {
        self.pull(rx);
        if a["step"] == "watch" {
            let source = a["in"].as_str().unwrap_or("output").to_string();
            if source == "output" && rx.is_none() {
                return Err(Error::usage(
                    "waitForLine: strobe reads this target's output only with output: true on the target (native(…, { output: true })), which pipes it (an app that buffers a pipe must flush each line); or give `in` a glob of log files under its home",
                ));
            }
            let timeout = Duration::from_millis(a["timeout"].as_u64().unwrap_or(30_000));
            let tail = (source != "output").then(|| {
                // Files that exist now are read from their end; later ones from their start.
                let offsets: BTreeMap<PathBuf, u64> = matching(home, &source).into_iter().map(|p| (p.clone(), std::fs::metadata(&p).map_or(0, |m| m.len()))).collect();
                let (tx, lines) = channel();
                let stop = Arc::new(AtomicBool::new(false));
                let (home, glob, flag) = (home.to_path_buf(), source.clone(), stop.clone());
                std::thread::spawn(move || tail_from(&home, &glob, offsets, &flag, &tx));
                (lines, stop)
            });
            self.watches.push(LineWatch { pattern: pattern(a, "waitForLine")?, source, deadline: Instant::now() + timeout, timeout, tail, found: None });
            return Ok(json!(self.watches.len() - 1));
        }
        let id = a["id"].as_u64().unwrap_or(u64::MAX) as usize;
        let w = self.watches.get_mut(id).ok_or_else(|| Error::msg("waitForLine: no such watch"))?;
        if let Some((lines, _)) = &w.tail {
            while let (None, Ok((at, line))) = (&w.found, lines.try_recv()) {
                if w.pattern.is_match(&line) {
                    w.found = Some((at, line));
                }
            }
        }
        if let Some((at, line)) = w.found.clone() {
            w.tail = None;
            return Ok(json!({ "line": line, "at": at }));
        }
        if Instant::now() >= w.deadline {
            w.tail = None;
            return Err(Error::timeout(format!("waitForLine: no line matching {} in {} in {:.1} s", w.pattern.as_str(), w.source, w.timeout.as_secs_f64())));
        }
        Ok(json!({ "pending": true }))
    }
}

/// Lines appended to files under `home` matching `glob` after `offsets` (a file not in it: from its
/// start), stamped on the host clock as read every 10 ms, until `stop`.
fn tail_from(home: &Path, glob: &str, mut offsets: BTreeMap<PathBuf, u64>, stop: &AtomicBool, tx: &std::sync::mpsc::Sender<(u64, String)>) {
    use std::io::{Read, Seek, SeekFrom};
    let mut partial: BTreeMap<PathBuf, String> = BTreeMap::new();
    while !stop.load(Ordering::Relaxed) {
        for path in matching(home, glob) {
            let at = offsets.entry(path.clone()).or_insert(0);
            if std::fs::metadata(&path).map_or(0, |m| m.len()) <= *at {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&path) else { continue };
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(*at)).is_err() || f.read_to_end(&mut buf).is_err() {
                continue;
            }
            let now = host::now_ns();
            *at += buf.len() as u64;
            let text = partial.entry(path).or_default();
            text.push_str(&String::from_utf8_lossy(&buf));
            while let Some(i) = text.find('\n') {
                let line = isolate::strip_ansi(text[..i].trim_end_matches('\r'));
                text.drain(..=i);
                if tx.send((now, line)).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn found_all(milestones: &[Milestone], lines: &[Line], which: impl Fn(&Milestone) -> bool) -> bool {
    milestones.iter().filter(|m| which(m)).all(|m| lines.iter().any(|(_, l, src)| *src == m.source && m.pattern.is_match(l)))
}

/// The init script that resolves `window.__strobeLaunch` with the epoch time `until` first showed, and
/// returns whether it already shows (a late install). `until` is a selector in the one grammar pages
/// and native apps share (`a11y::Selector`), found by the page probe (`driver::HELPER`) that `expect`,
/// `waitFor` and `look` use, which goes in first.
fn observer(until: &Value) -> Result<String> {
    let q = crate::target::a11y::Selector::parse(until, crate::target::a11y::Dialect::Page).map_err(|e| Error::usage(format!("launch.until: {}", e.message)))?.page_spec();
    Ok(format!(
        r#"{helper}
;(() => {{
  if (window.__strobeLaunch) return false
  const q = {q}
  let done
  window.__strobeLaunch = new Promise((resolve) => (done = resolve))
  const already = document.documentElement != null && !!window.__strobe.find(q)
  // The probe's wait: every shadow root's changes, and each frame while an animation or a transition
  // runs, so an element inside a shadow root or one that fades in counts when it shows.
  window.__strobe.waitFor(q, 2147483647, "until").then(() => done(performance.timeOrigin + performance.now()))
  return already
}})()"#,
        helper = crate::driver::HELPER,
    ))
}

/// Every page of the launch, from its first script: browser-wide auto-attach pauses each new page
/// until the observer (and, for attribution, its CPU profiler) is in, then lets it run. A page that
/// already ran when strobe attached gets the observer late; if `until` already shows there, the
/// launch has no valid end (`late`).
struct Watch {
    cdp: Arc<Cdp>,
    found: Receiver<(f64, String, String)>,
    stop: Arc<AtomicBool>,
    late: Arc<AtomicBool>,
    /// (session, target) of every page seen.
    pages: Arc<Mutex<Vec<(String, String)>>>,
}

impl Watch {
    fn start(cdp: &Arc<Cdp>, observer: String, profile: bool) -> Result<Watch> {
        let events = cdp.events();
        let (tx, found) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let late = Arc::new(AtomicBool::new(false));
        let pages = Arc::new(Mutex::new(Vec::new()));
        cdp.send("Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }), None)?;
        // Pages that already ran when strobe connected (a browser opens its first page at startup).
        let running = cdp.send("Target.getTargets", json!({}), None)?;
        for t in running["targetInfos"].as_array().into_iter().flatten().filter(|t| is_page(t)) {
            let _ = cdp.send("Target.attachToTarget", json!({ "targetId": t["targetId"], "flatten": true }), None);
        }
        let watch = Watch { cdp: cdp.clone(), found, stop: stop.clone(), late: late.clone(), pages: pages.clone() };
        let (cdp, stop2, late2, pages2) = (cdp.clone(), stop, late, pages);
        std::thread::spawn(move || {
            let mut seen = std::collections::BTreeSet::new();
            while !stop2.load(Ordering::Relaxed) {
                let Ok(ev) = events.recv_timeout(Duration::from_millis(100)) else { continue };
                if ev["method"] != "Target.attachedToTarget" {
                    continue;
                }
                let p = &ev["params"];
                let session = p["sessionId"].as_str().unwrap_or("").to_string();
                let info = &p["targetInfo"];
                let waiting = p["waitingForDebugger"] == true;
                let send = |method: &str, params: Value| cdp.send_timeout(method, params, Some(&session), Duration::from_secs(10));
                let target = info["targetId"].as_str().unwrap_or("").to_string();
                if is_page(info) && seen.insert(target.clone()) {
                    let _ = send("Page.addScriptToEvaluateOnNewDocument", json!({ "source": observer }));
                    if profile {
                        let _ = send("Profiler.enable", json!({}));
                        let _ = send("Profiler.setSamplingInterval", json!({ "interval": probe::cpu::INTERVAL_US }));
                        let _ = send("Profiler.start", json!({}));
                    }
                    pages2.lock().unwrap().push((session.clone(), target.clone()));
                    let (cdp, tx, stop, late, observer) = (cdp.clone(), tx.clone(), stop2.clone(), late2.clone(), observer.clone());
                    let session = session.clone();
                    std::thread::spawn(move || {
                        let eval = |expr: &str, wait: bool| {
                            cdp.send_timeout("Runtime.evaluate", json!({ "expression": expr, "awaitPromise": wait, "returnByValue": true }), Some(&session), Duration::from_secs(10))
                        };
                        while !stop.load(Ordering::Relaxed) {
                            match eval("window.__strobeLaunch", true) {
                                Ok(r) if r["result"]["value"].is_number() => {
                                    let _ = tx.send((r["result"]["value"].as_f64().unwrap_or(0.0), session, target));
                                    return;
                                }
                                // A document that loaded before the init script was in: the observer goes in
                                // now. Its time is valid unless `until` already shows.
                                Ok(r) if r["result"]["type"] == "undefined" => {
                                    if eval(&observer, false).is_ok_and(|r| r["result"]["value"] == true) {
                                        late.store(true, Ordering::Relaxed);
                                    }
                                }
                                // The page closed: nothing more to wait for here.
                                Err(e) if e.message.contains("ession") && e.message.contains("not found") => return,
                                // A document between navigations.
                                _ => std::thread::sleep(Duration::from_millis(20)),
                            }
                        }
                    });
                }
                if waiting {
                    let _ = send("Runtime.runIfWaitingForDebugger", json!({}));
                }
            }
        });
        Ok(watch)
    }

    /// The epoch time `until` first showed and the page it showed in.
    fn ready(&self, timeout: Duration) -> Result<(f64, chromium::Page)> {
        let (at, session, target) = self.found.recv_timeout(timeout).map_err(|_| Error::timeout(format!("not visible in {} s", timeout.as_secs())))?;
        Ok((at, chromium::Page { cdp: self.cdp.clone(), session, target }))
    }

    fn any_page(&self) -> Option<chromium::Page> {
        self.pages.lock().unwrap().first().map(|(session, target)| chromium::Page { cdp: self.cdp.clone(), session: session.clone(), target: target.clone() })
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A helper thread's result, or nothing if it does not finish within 5 s: a block never waits on a
/// stuck OS call (a window that stopped answering), it goes without that thread's readings.
pub(super) fn joined<T: Default>(h: std::thread::JoinHandle<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if h.is_finished() { h.join().unwrap_or_default() } else { T::default() }
}

fn is_page(info: &Value) -> bool {
    info["type"] == "page" && !info["url"].as_str().unwrap_or("").starts_with("devtools://")
}

/// Keeps the app's windows on top without focus, re-reading its process set (its tree, what carries
/// its home's stamp, its job) every 300 ms, and with `capture` reads the window's 16×16 average
/// pixels every 30 ms (epoch ms).
fn sample_screen(pid: u32, home: Option<&Path>, capture: bool, stop: &AtomicBool) -> Vec<(f64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pids = Vec::new();
    let mut read = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if read.elapsed() > Duration::from_millis(300) {
            pids = host::process_set(pid, home).iter().map(|p| p.pid).collect();
            host::raise_windows(&pids);
            read = Instant::now();
        }
        if let Some(px) = host::window_pixels(&pids).filter(|_| capture) {
            out.push((epoch_ms(), px));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    out
}

/// When the window's pixels first differed from its first sample, and when they last changed while
/// sampled (epoch ms). A splash or a fade counts as painted: it is what a person sees.
pub(super) fn screen_times(samples: &[(f64, Vec<u8>)]) -> Option<(f64, f64)> {
    let (first_at, first) = samples.first()?;
    // A cell (a 16th of the window each way, averaged) moving by more than 8 levels in a colour
    // channel: a panel or a list filling in moves several cells by far more, a blinking caret moves
    // its cell by under one level. Alpha is ignored.
    let differs = |a: &[u8], b: &[u8]| a.chunks(4).zip(b.chunks(4)).any(|(x, y)| x.iter().zip(y).take(3).any(|(p, q)| p.abs_diff(*q) > 8));
    let painted = samples.iter().find(|(_, px)| differs(px, first)).map_or(*first_at, |(at, _)| *at);
    let settled = samples.windows(2).rev().find(|w| differs(&w[0].1, &w[1].1)).map_or(painted, |w| w[1].0).max(painted);
    Some((painted, settled))
}

/// The page's `performance.mark` names with their epoch time.
fn page_marks(page: &chromium::Page) -> Vec<Line> {
    let expr = r#"JSON.stringify(performance.getEntriesByType("mark").map((m) => [m.name, performance.timeOrigin + m.startTime]))"#;
    page.eval(expr)
        .ok()
        .and_then(|v| v.as_str().and_then(|s| serde_json::from_str::<Vec<(String, f64)>>(s).ok()))
        .unwrap_or_default()
        .into_iter()
        .map(|(name, at)| (at, name, "page".to_string()))
        .collect()
}

/// Lines appended to files matching `globs` under `home`, each with the time strobe read it, until `stop`.
pub(super) fn tail(home: &Path, globs: &[String], stop: &AtomicBool) -> Vec<Line> {
    use std::io::{Read, Seek, SeekFrom};
    let mut offsets: BTreeMap<PathBuf, (u64, String)> = BTreeMap::new();
    let mut out = Vec::new();
    let mut listed = Instant::now() - Duration::from_secs(1);
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        if listed.elapsed() > Duration::from_millis(200) || stopping {
            files = globs.iter().flat_map(|g| matching(home, g).into_iter().map(move |p| (p, g.clone()))).collect();
            listed = Instant::now();
        }
        for (path, glob) in &files {
            let Ok(meta) = std::fs::metadata(path) else { continue };
            let entry = offsets.entry(path.clone()).or_insert((0, String::new()));
            if meta.len() <= entry.0 {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(path) else { continue };
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(entry.0)).is_err() || f.read_to_end(&mut buf).is_err() {
                continue;
            }
            entry.0 += buf.len() as u64;
            entry.1.push_str(&String::from_utf8_lossy(&buf));
            let at = epoch_ms();
            while let Some(i) = entry.1.find('\n') {
                let line = isolate::strip_ansi(entry.1[..i].trim_end_matches('\r'));
                entry.1.drain(..=i);
                out.push((at, line, glob.clone()));
            }
        }
        if stopping {
            return out;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Files under `root` whose path relative to it matches `glob` (`*` within a segment, `**` across).
pub(super) fn matching(root: &Path, glob: &str) -> Vec<PathBuf> {
    let parts: Vec<&str> = glob.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let mut out = Vec::new();
    walk(root, &parts, &mut out, 0);
    out
}

fn walk(dir: &Path, parts: &[&str], out: &mut Vec<PathBuf>, depth: usize) {
    let Some((first, rest)) = parts.split_first() else { return };
    if depth > 12 {
        return;
    }
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    if *first == "**" {
        walk(dir, rest, out, depth + 1);
        for e in entries.iter().filter(|e| e.path().is_dir()) {
            walk(&e.path(), parts, out, depth + 1);
        }
        return;
    }
    for e in entries.iter().filter(|e| super::glob(first, &e.file_name().to_string_lossy())) {
        let p = e.path();
        if rest.is_empty() && p.is_file() {
            out.push(p);
        } else if !rest.is_empty() && p.is_dir() {
            walk(&p, rest, out, depth + 1);
        }
    }
}

/// Node's own bootstrap milestones in the main process, read over `--inspect` after the launch (nothing
/// attaches while it starts): when the process was created, when Node started and when it finished
/// bootstrapping, as epoch ms.
fn node_timing(port: u16) -> Result<Vec<(String, f64)>> {
    let body = chromium::http_get(&format!("127.0.0.1:{port}"), "/json/list")
        .map_err(|_| Error::msg("the main process opened no inspector, so there is no launch.node.* (an Electron fuse, EnableNodeCliInspectArguments, may turn --inspect off in this build)"))?;
    let list: Value = serde_json::from_str(&body)?;
    let ws = list.as_array().and_then(|a| a.first()).and_then(|t| t["webSocketDebuggerUrl"].as_str()).ok_or_else(|| Error::msg("the main process has no inspector target"))?.to_string();
    let cdp = Cdp::connect(&ws)?;
    let r = cdp.send(
        "Runtime.evaluate",
        json!({ "expression": "JSON.stringify({ origin: performance.timeOrigin, timing: performance.nodeTiming.toJSON(), created: process.getCreationTime?.() ?? null })", "returnByValue": true }),
        None,
    )?;
    let v: Value = serde_json::from_str(r["result"]["value"].as_str().unwrap_or("{}"))?;
    let origin = v["origin"].as_f64().unwrap_or(0.0);
    let mut out = Vec::new();
    if let Some(c) = v["created"].as_f64() {
        out.push(("created".to_string(), c));
    }
    for (name, field) in [("start", "nodeStart"), ("bootstrapped", "bootstrapComplete")] {
        if let Some(ms) = v["timing"][field].as_f64().filter(|m| *m > 0.0) {
            out.push((name.to_string(), origin + ms));
        }
    }
    Ok(out)
}

/// The attribution launch's profiles as a recording: the page's (`cpu-0`) and the main process's
/// (`cpu-main`), both from their first statement.
fn save_profiles(ctx: &Ctx, s: &Series, v: &Variant, page: &chromium::Page, started: &Started, values: &BTreeMap<String, f64>) -> Result<String> {
    let (id, draft) = store::new_recording()?;
    let dir = draft.dir().to_path_buf();
    let renderer = probe::cpu::stop(page)?;
    store::write_json(&dir.join("cpu-0.cpuprofile"), &renderer)?;
    let mut scripts = std::collections::BTreeSet::new();
    probe::symbols::urls(&renderer, &mut scripts);
    if let Some(main) = &started.main {
        let r = main.send("Profiler.stop", json!({}), None)?;
        store::write_json(&dir.join("cpu-main.cpuprofile"), &r["profile"])?;
    }
    let build = crate::target::isolate::build_for_os(&ctx.config["build"]);
    let served = v.root.as_ref().zip(ctx.config["serve"]["dir"].as_str().or(crate::target::isolate::per_os(&build["output"]))).map(|(r, d)| r.join(d));
    probe::symbols::store_maps(page, &scripts, &dir, served.as_deref())?;
    store::write_json(
        &dir.join("meta.json"),
        &json!({
            "id": id, "kind": "block", "experiment": ctx.exp, "series": s.label(), "variant": v.role, "variant_label": v.label, "variant_git": variant_git(v),
            "probes": "attribution", "runs": 1, "values": values, "created": super::super::record::now_iso(), "valid": true, "env": host::env(),
            "launch": true, "note": "a launch from the first statement: the page's profile and, for app targets, the main process's (cpu-main)",
        }),
    )?;
    draft.keep();
    Ok(id)
}
