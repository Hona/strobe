//! Validators (refuse, exit 4) and analysers (warn) — SPEC §10.

use crate::host;
use crate::model::{Error, Hidden, Refusal, Result, ValidatorId, WebEngine};
use crate::target::chromium::Gpu;
use crate::target::web::WebPage as Page;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
pub struct Warning {
    pub id: String,
    pub level: String,
    pub text: String,
}

pub fn warn(id: &str, text: impl Into<String>) -> Warning {
    Warning { id: id.into(), level: "warn".into(), text: text.into() }
}

pub fn info(id: &str, text: impl Into<String>) -> Warning {
    Warning { id: id.into(), level: "info".into(), text: text.into() }
}

/// What only a display shows (`Headless` refuses it of a headless page): frames, input-to-screen
/// latencies, scrolling, canvas frames, the display's rate and a launch's pixels. Not strobe's own input
/// pacing (`input.*`), which needs no display.
pub fn display_metric(m: &str) -> bool {
    ["frames.", "interaction.", "scroll.", "canvas.", "display."].iter().any(|p| m.starts_with(p)) || m == "launch.painted" || m == "launch.settled"
}

/// GPU use (engine time, compiles), as opposed to GPU memory (`gpu.vram`, `gpu.shared`): what the app
/// allocated is the same whether or not a compositor shows it, but the work a GPU does for a screen
/// nobody sees is not.
pub fn gpu_use(metric: &str) -> bool {
    metric.starts_with("gpu.") && metric != "gpu.vram" && metric != "gpu.shared"
}

/// What a software rasterizer leaves without meaning (`SoftwareGpu`, per metric): what reached a display
/// and every GPU metric, its memory too (a software adapter holds none of its own). CPU, main-thread work,
/// memory, counters and milestones stay, with the rasterizer's work in the CPU of the processes it runs in.
pub fn software_drawn(m: &str) -> bool {
    display_metric(m) || m.starts_with("gpu.")
}

fn software_text(gpu: &Gpu) -> String {
    format!("the target renders on a software adapter ({}; compositing {})", gpu.devices.join(", "), gpu.compositing)
}

/// `SoftwareGpu` per metric (SPEC §10.1), for a headed page drawn by a software rasterizer (a virtual
/// display, a machine without a GPU): a `primary` that is drawn (`software_drawn`) refuses the block;
/// otherwise each of `declared` that is drawn comes back refused, for the caller to take its value out
/// of the block and list it refused. Nothing on a hardware adapter, and nothing headless, where `Headless`
/// refuses what reached no display.
pub fn software_refusals<'a>(gpu: &Gpu, headless: bool, primary: Option<&str>, declared: impl IntoIterator<Item = &'a str>) -> Result<Vec<(String, Refusal)>> {
    if !gpu.software || headless {
        return Ok(Vec::new());
    }
    if let Some(p) = primary.filter(|p| software_drawn(p)) {
        return Err(Refusal::new(ValidatorId::SoftwareGpu, format!("{p}, the primary, is drawn: {}", software_text(gpu)))
            .fix("run headed on a machine with a GPU, or compare a CPU metric (main.work, cpu.work); frame and GPU metrics from software rendering describe no real device")
            .into());
    }
    Ok(declared
        .into_iter()
        .filter(|m| software_drawn(m))
        .map(|m| (m.to_string(), Refusal::new(ValidatorId::SoftwareGpu, format!("{m}: {}", software_text(gpu))).fix("run headed on a machine with a GPU for frame and GPU metrics")))
        .collect())
}

/// What a headed run on a software rasterizer says once: its CPU metrics hold the rasterizer's work.
pub fn software_warning(gpu: &Gpu) -> Warning {
    warn(
        "SoftwareGpu",
        format!(
            "{}: frame and GPU metrics are refused, and the browser's CPU metrics (cpu.*) include the rasterization a GPU would do; compare them only with runs on this same setup",
            software_text(gpu)
        ),
    )
}

/// `SoftwareGpu`'s one test, by adapter or renderer name: a software rasterizer (Microsoft Basic
/// Render Driver, WARP, llvmpipe, lavapipe, softpipe, SwiftShader, the Apple Software Renderer)
/// describes no real device.
pub fn is_software_gpu(name: &str) -> bool {
    let n = name.to_lowercase();
    ["basic render", "warp", "llvmpipe", "lavapipe", "swiftshader", "softpipe", "apple software renderer"].iter().any(|s| n.contains(s))
}

pub fn window_visible(page: &Page) -> Result<()> {
    let state = page.eval("document.visibilityState")?;
    if state.as_str() != Some("visible") {
        // macOS: a Firefox or Safari window counts as hidden behind other apps' (occlusion tracking), and
        // strobe makes it the active app only once nobody has used the Mac for a minute: a bench waits for that.
        let hidden = if cfg!(target_os = "macos") && page.cdp().is_none() { Hidden::NotActive } else { Hidden::Other };
        return Err(Refusal::window_hidden(hidden, format!("the page reports visibilityState={state}"))
            .fix("keep the window visible and unminimized while strobe measures; browsers throttle hidden windows")
            .into());
    }
    Ok(())
}

pub fn locked() -> Error {
    Refusal::new(ValidatorId::SessionLocked, "the session was locked while strobe measured: the lock screen covers the window, so nothing it drew reached the display")
        .fix("measure on an unlocked screen; bench waits for an unlock and redoes the block by itself")
        .into()
}

pub fn dev_build(page: &Page) -> Result<()> {
    let dev = page.eval(r#"!!document.querySelector('script[src*="@vite/client"]') || typeof window.__vite_plugin_react_preamble_installed__ !== "undefined""#)?;
    if dev.as_bool() == Some(true) {
        return Err(Refusal::new(ValidatorId::DevBuild, "the page is served by a dev server (Vite client present)").fix("measure a production build: set build and serve in strobe.json").into());
    }
    Ok(())
}

/// What a page reads of its input devices: the hover and pointer media queries, touch points and the
/// touch event API; and in the same read, whether it matches `(prefers-reduced-motion: reduce)`.
pub struct InputSeen {
    hover: bool,
    fine: bool,
    any_fine: bool,
    any_coarse: bool,
    touch_api: bool,
    touch_points: Option<f64>,
    pub reduced_motion: bool,
}

const INPUT_SCRIPT: &str = "[matchMedia('(hover: hover)').matches, matchMedia('(pointer: fine)').matches, matchMedia('(any-pointer: fine)').matches, matchMedia('(any-pointer: coarse)').matches, 'ontouchstart' in window, navigator.maxTouchPoints, matchMedia('(prefers-reduced-motion: reduce)').matches]";

impl InputSeen {
    pub fn read(page: &Page) -> Result<InputSeen> {
        let v = page.eval(INPUT_SCRIPT)?;
        let b = |i: usize| v[i].as_bool() == Some(true);
        Ok(InputSeen { hover: b(0), fine: b(1), any_fine: b(2), any_coarse: b(3), touch_api: b(4), touch_points: v[5].as_f64(), reduced_motion: b(6) })
    }

    /// A desktop: a fine pointer that hovers, nothing coarse, no touch. No pointer at all: `None`. A
    /// coarse pointer or touch points without a fine pointer: `Touch`. Anything else, a fine pointer with
    /// touch beside it (a touch-screen laptop): `Hybrid`.
    pub fn class(&self) -> InputClass {
        let touch = self.touch_points.is_none_or(|n| n > 0.0);
        if self.hover && self.fine && self.any_fine && !self.any_coarse && !self.touch_api && !touch {
            InputClass::Desktop
        } else if !self.any_fine && !self.any_coarse && !touch {
            InputClass::None
        } else if !self.any_fine {
            InputClass::Touch
        } else {
            InputClass::Hybrid
        }
    }

    fn text(&self) -> String {
        let points = self.touch_points.map_or("unknown".to_string(), |n| n.to_string());
        format!(
            "(hover: hover) {}, (pointer: fine) {}, (any-pointer: fine) {}, (any-pointer: coarse) {}, ontouchstart {}, navigator.maxTouchPoints {points}",
            self.hover, self.fine, self.any_fine, self.any_coarse, self.touch_api
        )
    }
}

/// The input devices a page sees (`input` in a series, its blocks and recordings, SPEC 13.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputClass {
    Desktop,
    Hybrid,
    Touch,
    None,
}

impl InputClass {
    pub fn name(self) -> &'static str {
        match self {
            InputClass::Desktop => "desktop",
            InputClass::Hybrid => "hybrid",
            InputClass::Touch => "touch",
            InputClass::None => "none",
        }
    }
}

/// `DeviceMismatch` (SPEC 10.1): a desktop page target sees a desktop's input, a fine pointer that
/// hovers, nothing coarse and no touch, headless too. A machine without a mouse (a server, a VM)
/// reports (hover: none) and no pointer, a touch-screen laptop touch points, and the page takes its touch
/// paths: another workload than people's desktops run. strobe sets that input at launch where the engine
/// has a setting for it (Chromium, Firefox); this reads what the page sees, before anything is measured.
fn desktop_input(page: &Page, seen: &InputSeen) -> Result<InputClass> {
    let class = seen.class();
    if class == InputClass::Desktop {
        return Ok(class);
    }
    let fix = match page.engine() {
        WebEngine::Chromium => {
            "strobe launches Chromium with a fine pointer that hovers and no touch (--blink-settings, --touch-events=disabled): a STROBE_BROWSER wrapper that rewrites strobe's arguments can drop them; run with strobe's arguments as they are"
        }
        WebEngine::Firefox => {
            "strobe sets Firefox's ui.*PointerCapabilities, dom.maxtouchpoints.testing.value and dom.w3c_touch_events.enabled to a fine pointer that hovers and no touch: a STROBE_FIREFOX build or wrapper that changes the profile's preferences undoes them"
        }
        WebEngine::Webkit => "WebKit has no setting for the input devices a page sees: measure it headed on a machine with a mouse and no touch screen, or the page in Chromium or Firefox",
    };
    Err(Refusal::new(ValidatorId::DeviceMismatch, format!("a desktop page target's page sees {} input, not a desktop's", class.name())).detail(format!("the page sees {}", seen.text())).fix(fix).into())
}

/// `DeviceMismatch` for any emulation (`chromium::Emulation`), in the same input classes: a desktop as
/// `desktop_input`; a touch screen reads `hybrid` with its primary pointer fine and hovering and touch
/// points; a phone reads `touch` (a coarse pointer, no fine one) with no hover, touch points and the
/// touch event API, and a mobile user agent (or the one the target names). Read before anything is
/// measured; returns the class the page saw and whether it matched `(prefers-reduced-motion: reduce)`
/// (the same read).
pub fn emulated_input(page: &Page, emulation: &crate::target::chromium::Emulation) -> Result<(InputClass, bool)> {
    let seen = InputSeen::read(page)?;
    if !emulation.touches() {
        let class = desktop_input(page, &seen)?;
        if emulation.user_agent.is_none() {
            return Ok((class, seen.reduced_motion));
        }
    }
    let class = seen.class();
    let want = if emulation.mobile {
        InputClass::Touch
    } else if emulation.touch {
        InputClass::Hybrid
    } else {
        InputClass::Desktop
    };
    let mut wrong = Vec::new();
    if class != want {
        wrong.push(format!("it sees {} input, not {}", class.name(), want.name()));
    }
    if emulation.mobile && seen.hover {
        wrong.push("(hover: hover) matches".to_string());
    }
    if emulation.mobile && !seen.touch_api {
        wrong.push("there is no touch event API ('ontouchstart' in window is false)".to_string());
    }
    if emulation.touch && !emulation.mobile && !(seen.hover && seen.fine) {
        wrong.push("its primary pointer is not a fine one that hovers".to_string());
    }
    if emulation.touches() && seen.touch_points.is_none_or(|n| n < 1.0) {
        wrong.push("navigator.maxTouchPoints is 0".to_string());
    }
    let agent = page.eval("navigator.userAgent")?.as_str().unwrap_or("").to_string();
    match &emulation.user_agent {
        Some(want) if agent != *want => wrong.push(format!("the user agent is {agent:?}, not the target's")),
        None if emulation.mobile && !agent.contains("Mobile") => wrong.push(format!("the user agent is a desktop's ({agent:?})")),
        _ => {}
    }
    if wrong.is_empty() {
        return Ok((class, seen.reduced_motion));
    }
    let fix = match page.engine() {
        WebEngine::Chromium => "strobe launches Chromium with the emulation's pointer (--blink-settings) and emulates its touch points and user agent over DevTools: a STROBE_BROWSER wrapper that rewrites strobe's arguments can drop them; run with strobe's arguments as they are",
        WebEngine::Firefox => "strobe sets the emulation in the Firefox profile (ui.*PointerCapabilities, dom.w3c_touch_events.enabled, dom.maxtouchpoints.testing.value, general.useragent.override): a Firefox that ignores them is not one strobe supports for it; emulate it in Chromium",
        WebEngine::Webkit => "WebKit has no setting for the input devices or user agent a page sees: emulate it in Chromium or Firefox",
    };
    Err(Refusal::new(ValidatorId::DeviceMismatch, format!("a page target emulating {} does not see one: {}", emulation.kind(), wrong.join(", ")))
        .detail(format!("the page sees {}", seen.text()))
        .fix(fix)
        .into())
}

/// The fixed speed-check workload; returns its time in ms (median of 3).
pub fn speed_check(page: &Page) -> Result<f64> {
    let v = page.eval(
        r#"(() => { const t = []; for (let r = 0; r < 3; r++) { const a = new Float64Array(1 << 16); const s = performance.now(); let x = 0; for (let i = 0; i < 6e6; i++) { const j = (i * 2654435761) & 65535; a[j] += i; x ^= a[(j + 7) & 65535] | 0 } t.push(performance.now() - s + (x & 0)) } t.sort((p, q) => p - q); return t[1] })()"#,
    )?;
    Ok(v.as_f64().unwrap_or(f64::NAN))
}

pub fn throttled(ms: f64, baseline: Option<f64>) -> Result<Option<f64>> {
    let Some(base) = baseline.filter(|b| *b > 0.0) else { return Ok(None) };
    let ratio = ms / base;
    if ratio > 1.5 {
        return Err(Refusal::new(ValidatorId::Throttled, format!("the speed check ran {ratio:.2}× slower than this device's calibration ({ms:.1} ms vs {base:.1} ms)"))
            .fix("remove CPU throttling, plug in power, leave power saving, let the machine cool down")
            .into());
    }
    Ok(Some(ratio))
}

/// Hypervisor steal over a stretch (a speed check): the share of the CPUs' time the hypervisor gave to
/// something else while the guest wanted them, which a guest sees as neither its own work nor idle.
/// Linux only; elsewhere none is read and a slow check with no process named says "system work".
pub struct Steal {
    at: Option<(Vec<u64>, std::time::Instant)>,
}

impl Steal {
    pub fn start() -> Steal {
        let mut v = Vec::new();
        Steal { at: host::steal_ns(&mut v).then(|| (v, std::time::Instant::now())) }
    }

    /// Steal since `start` as % of the time of `cpus` (all when `None`), their mean; None where the OS
    /// does not report it.
    pub fn pct(&self, cpus: Option<u64>) -> Option<f64> {
        let (a, t0) = self.at.as_ref()?;
        let mut b = Vec::new();
        let span = t0.elapsed().as_nanos() as f64;
        (host::steal_ns(&mut b) && b.len() == a.len() && span > 0.0).then_some(())?;
        let per: Vec<f64> =
            a.iter().zip(&b).enumerate().filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1)).map(|(_, (x, y))| (100.0 * y.saturating_sub(*x) as f64 / span).min(100.0)).collect();
        (!per.is_empty()).then(|| per.iter().sum::<f64>() / per.len() as f64)
    }
}

/// Steal from which it is named as a cause (% of the CPUs' time): `/proc/stat` counts in 10 ms ticks,
/// and a tick of it in a check of a second or two is noise.
const STEAL_CAUSE_PCT: f64 = 0.5;

/// The busiest processes during a check (% of a core), and the hypervisor's steal (% of the target's
/// CPUs' time, where the OS reports it) as a cause from `STEAL_CAUSE_PCT`. With no process named: the
/// steal, else the OS's own work, saying a steal that read under it.
pub fn named_work(who: &[(String, f64)], steal: Option<f64>) -> String {
    let stolen = steal.filter(|s| *s >= STEAL_CAUSE_PCT).map(|s| format!("hypervisor steal {s:.0}% of those CPUs during the check"));
    if who.is_empty() {
        return match (stolen, steal) {
            (Some(s), _) => format!("none named ({s})"),
            (None, Some(s)) => format!("none named (system work; {s:.0}% hypervisor steal)"),
            (None, None) => "none named (system work)".to_string(),
        };
    }
    let list = who.iter().map(|(n, p)| format!("{n} {p:.0}%")).collect::<Vec<_>>().join(", ");
    match stolen {
        Some(s) => format!("{list}; {s}"),
        None => list,
    }
}

/// The native speed check (`Throttled` for targets with no page to run the JS check in): a fixed
/// integer workload on a strobe thread on `cpus` (the target's CPUs), median of 3, in ms. strobe's own
/// mask (the runner's CPUs) is widened to them for the check and comes back after it.
pub fn native_speed(cpus: Option<u64>) -> f64 {
    let previous = cpus.and_then(host::set_affinity);
    let ms = std::thread::spawn(move || {
        if let Some(m) = cpus {
            host::set_thread_affinity(m);
        }
        let mut t: Vec<f64> = (0..3)
            .map(|_| {
                let mut a = vec![0f64; 1 << 16];
                let started = std::time::Instant::now();
                let mut x = 0u64;
                for i in 0..24_000_000u64 {
                    let j = (i.wrapping_mul(2_654_435_761) & 65535) as usize;
                    a[j] += i as f64;
                    x ^= a[(j + 7) & 65535] as u64;
                }
                std::hint::black_box(x);
                started.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        t.sort_by(f64::total_cmp);
        t[1]
    })
    .join()
    .unwrap_or(f64::NAN);
    if let Some(p) = previous {
        host::set_affinity(p);
    }
    ms
}

/// Busy % of each of `cpus` (all when `None`) over `ms`, from idle cycles against the TSC.
fn busy(cpus: Option<u64>, ms: u64) -> Option<Vec<f64>> {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let t0 = host::tsc()?;
    host::idle_cycles(&mut a).then_some(())?;
    std::thread::sleep(std::time::Duration::from_millis(ms));
    let t1 = host::tsc()?;
    host::idle_cycles(&mut b).then_some(())?;
    let span = t1.saturating_sub(t0) as f64;
    (span > 0.0 && a.len() == b.len()).then_some(())?;
    Some(
        a.iter()
            .zip(&b)
            .enumerate()
            .filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1))
            .map(|(_, (x, y))| (100.0 * (1.0 - y.saturating_sub(*x) as f64 / span)).clamp(0.0, 100.0))
            .collect(),
    )
}

/// Waits, up to `max`, until `cpus` are quiet: two 100 ms looks in a row with their mean busy under
/// 5% and none over 25% (an app that just ended still frees its memory and GPU objects on them).
/// Returns the last look's mean and busiest CPU, in %.
pub fn wait_quiet(cpus: Option<u64>, max: std::time::Duration) -> (f64, f64) {
    let deadline = std::time::Instant::now() + max;
    let mut calm = 0;
    loop {
        let Some(b) = busy(cpus, 100) else { return (f64::NAN, f64::NAN) };
        let mean = b.iter().sum::<f64>() / b.len().max(1) as f64;
        let top = b.iter().copied().fold(0.0, f64::max);
        calm = if mean < 5.0 && top < 25.0 { calm + 1 } else { 0 };
        if calm >= 2 || std::time::Instant::now() > deadline {
            return (mean, top);
        }
    }
}

/// The speed check's reference for one CPU mask: the median of the last checks that passed there.
const KEPT: usize = 20;

/// Other work on the target's CPUs during a check (% of one core) past which a slow check is that
/// work's doing, not the device's.
const FOREIGN_PCT: f64 = 25.0;

/// One speed check: its time, the other work on the target's CPUs meanwhile (their busy time less
/// the check's own core), and the processes outside strobe that used the most CPU (any CPU).
#[derive(Clone)]
struct Try {
    ms: f64,
    foreign_pct: f64,
    others: Vec<(String, f64)>,
    /// Hypervisor steal on the target's CPUs meanwhile (% of their time; Linux guests).
    steal: Option<f64>,
}

fn timed_check(cpus: Option<u64>) -> Try {
    let me = std::process::id();
    // The process lists outside the span the idle counters cover, strobe's own CPU just inside it:
    // what is left of the CPUs' busy time is other work, the check's own core never assumed whole.
    let p0 = host::all_processes();
    let stolen = Steal::start();
    let (c0, t0, own0) = (idle_now(), host::tsc(), host::process_cycles(me));
    let started = std::time::Instant::now();
    let ms = native_speed(cpus);
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let (own1, c1, t1) = (host::process_cycles(me), idle_now(), host::tsc());
    let steal = stolen.pct(cpus);
    let p1 = host::all_processes();
    let foreign_pct = match (c0, c1, t0, t1, own0, own1) {
        (Some(a), Some(b), Some(x), Some(y), Some(o0), Some(o1)) if y > x && a.len() == b.len() => {
            let span = (y - x) as f64;
            let busy: f64 = a
                .iter()
                .zip(&b)
                .enumerate()
                .filter(|(c, _)| cpus.is_none_or(|m| *c < 64 && m >> c & 1 == 1))
                .map(|(_, (i0, i1))| (100.0 * (1.0 - i1.saturating_sub(*i0) as f64 / span)).clamp(0.0, 100.0))
                .sum();
            (busy - 100.0 * o1.saturating_sub(o0) as f64 / span).max(0.0)
        }
        _ => 0.0,
    };
    let before: std::collections::HashMap<u32, u64> = p0.iter().map(|p| (p.pid, p.cpu_ns)).collect();
    let mut others: Vec<(String, f64)> = p1
        .iter()
        .filter(|p| p.pid != me && p.pid != 0)
        .filter_map(|p| {
            let d = p.cpu_ns.saturating_sub(*before.get(&p.pid)?) as f64 / 1e6;
            let pct = 100.0 * d / wall_ms.max(1.0);
            (pct >= 5.0).then(|| (format!("{} ({})", p.name, p.pid), pct))
        })
        .collect();
    others.sort_by(|a, b| b.1.total_cmp(&a.1));
    others.truncate(3);
    Try { ms, foreign_pct, others, steal }
}

fn idle_now() -> Option<Vec<u64>> {
    let mut v = Vec::new();
    host::idle_cycles(&mut v).then_some(v)
}

/// `Throttled` for targets with no page (SPEC §10.1): the native speed check on `cpus`, run once
/// they are quiet, against this device's reference for that mask (`<state>/native-speed.json`:
/// the median of its last 20 passing checks; the first check sets it). A slow check is retried
/// twice after the CPUs settle again: throttling (power, heat, a power plan) lasts, while an app
/// that just ended (freeing a large model) or a burst of other work passes. Returns the ratio.
pub fn native_speed_checked(cpus: Option<u64>) -> Result<Option<f64>> {
    let path = host::paths::state_dir().join("native-speed.json");
    let mut saved: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    let key = cpus.map_or("all".to_string(), |m| format!("{m:#x}"));
    let mut kept: Vec<f64> = saved[&key]["ms"].as_array().into_iter().flatten().filter_map(Value::as_f64).filter(|m| *m > 0.0).collect();
    let reference = (!kept.is_empty()).then(|| {
        let mut s = kept.clone();
        s.sort_by(f64::total_cmp);
        s[s.len() / 2]
    });
    // Up to 3 slow tries on quiet CPUs, and for as long as 2 minutes while other processes keep the
    // target's CPUs busy during the check: that is their work, not this device's speed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut best: Option<Try> = None;
    let mut slow = 0;
    let last = loop {
        wait_quiet(cpus, std::time::Duration::from_secs(10));
        let t = timed_check(cpus);
        if t.ms.is_finite() && best.as_ref().is_none_or(|b| t.ms < b.ms) {
            best = Some(t.clone());
        }
        if reference.is_none_or(|r| t.ms / r <= 1.5) {
            break t;
        }
        if t.foreign_pct > FOREIGN_PCT && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        slow += 1;
        if slow >= 3 || std::time::Instant::now() >= deadline {
            break t;
        }
    };
    let Some(best) = best else { return Ok(None) };
    let ms = best.ms;
    let ratio = reference.map(|r| ms / r);
    if let (Some(ratio), Some(r)) = (ratio, reference)
        && ratio > 1.5
    {
        let who = |t: &Try| named_work(&t.others, t.steal);
        if last.foreign_pct > FOREIGN_PCT {
            return Err(Refusal::new(
                ValidatorId::Background,
                format!(
                    "the native speed check on the target's CPUs ran {ratio:.2}× slower than this device's reference ({ms:.1} ms vs {r:.1} ms) while other work kept those CPUs {:.0}% of a core busy for 2 minutes",
                    last.foreign_pct
                ),
            )
            .detail(format!("busiest processes (% of a core): {}", who(&last)))
            .fix("let that work finish (an antivirus scanning new files, an update, a build), or exclude it from the bench's CPUs")
            .into());
        }
        return Err(Refusal::new(
            ValidatorId::Throttled,
            format!("the native speed check ran {ratio:.2}× slower than this device's reference on these CPUs ({ms:.1} ms vs {r:.1} ms, best of 3 tries on quiet CPUs)"),
        )
        .detail(format!("other work during the last: {:.0}% of a core there; busiest processes: {}", last.foreign_pct, who(&last)))
        .fix("plug in power, leave power saving, let the machine cool down")
        .into());
    }
    kept.push(ms);
    let drop = kept.len().saturating_sub(KEPT);
    kept.drain(..drop);
    if let Some(o) = saved.as_object_mut() {
        // The single all-time best older versions kept, for every mask at once.
        o.remove("ms");
    }
    saved[&key] = serde_json::json!({ "ms": kept });
    let _ = host::paths::write_atomic(&path, saved.to_string().as_bytes());
    Ok(ratio)
}

/// `Throttled` for a page in an engine other than Chromium (whose calibration is Chromium's): the
/// in-page speed check against that engine's own reference on these CPUs, the median of its last 20
/// passing checks (`<state>/engine-speed.json`, by engine and CPU mask; the first check sets it). JS
/// engines differ in speed: a check is never compared across engines.
/// `keep` false (other work kept the target's CPUs busy during the check) compares without adding the
/// check to the reference.
pub fn engine_throttled(engine: WebEngine, cpus: Option<u64>, ms: f64, keep: bool) -> Result<Option<f64>> {
    if !ms.is_finite() || ms <= 0.0 {
        return Ok(None);
    }
    let path = host::paths::state_dir().join("engine-speed.json");
    let mut saved: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    let key = format!("{engine}:{}", cpus.map_or("all".to_string(), |m| format!("{m:#x}")));
    let mut kept: Vec<f64> = saved[&key]["ms"].as_array().into_iter().flatten().filter_map(Value::as_f64).filter(|m| *m > 0.0).collect();
    let reference = (!kept.is_empty()).then(|| {
        let mut s = kept.clone();
        s.sort_by(f64::total_cmp);
        s[s.len() / 2]
    });
    let ratio = reference.map(|r| ms / r);
    if let (Some(ratio), Some(r)) = (ratio, reference)
        && ratio > 1.5
    {
        return Err(Refusal::new(ValidatorId::Throttled, format!("the page's speed check ran {ratio:.2}× slower than this device's reference for {engine} on these CPUs ({ms:.1} ms vs {r:.1} ms)"))
            .fix(format!(
                "remove CPU throttling, plug in power, leave power saving, let the machine cool down; a reference set by an unusually fast check goes with its entry {key:?} in {}",
                path.display()
            ))
            .into());
    }
    if !keep {
        return Ok(ratio);
    }
    kept.push(ms);
    let drop = kept.len().saturating_sub(KEPT);
    kept.drain(..drop);
    saved[&key] = serde_json::json!({ "ms": kept });
    let _ = host::paths::write_atomic(&path, saved.to_string().as_bytes());
    Ok(ratio)
}

/// What a page engine has no way to measure (`EngineUnsupported`), with the closest metric it has.
/// Firefox: the renderer main thread's task time and its parts, layouts and style counts, long tasks,
/// layout shifts, GC and GPU compiles (Chromium's Performance domain and trace), the JS heap, the page's
/// spans, and a frame's begin-to-screen cost (only a toolkit knows when it began a frame: Firefox's
/// frames come from the OS's presents, §7.2). WebKit (WebDriver Classic) also has no network events
/// (`net.*`) and runs an input action list as one batch in its UI process (`input.rate`, `input.gap.p95`
/// would time strobe's requests, not the page's input).
pub fn engine_refusal(engine: WebEngine, metric: &str) -> Option<Refusal> {
    match engine {
        WebEngine::Chromium => return None,
        // Neither has Chromium's Performance domain or trace.
        WebEngine::Firefox | WebEngine::Webkit => {}
    }
    if engine == WebEngine::Webkit {
        let classic = match metric {
            m if m.starts_with("net.") => Some(("WebDriver Classic reports no network requests", "measure it in Chromium or Firefox")),
            "input.rate" | "input.gap.p95" => Some(("WebDriver Classic delivers input actions in batches inside the browser, so strobe's send times are not the page's input times", "measure it in Chromium or Firefox")),
            _ => None,
        };
        if let Some((why, fix)) = classic {
            return Some(Refusal::new(ValidatorId::EngineUnsupported, format!("{metric}: {why}")).fix(fix));
        }
    }
    let instead = match metric {
        m if m.starts_with("main.") || matches!(m, "layout.count" | "style.count") => "cpu.work (the browser's processes' CPU per run) or cpu.renderer",
        m if m.starts_with("frames.presented.") && m != "frames.presented.count" => "frames.interval.p95 (headed) or cpu.work",
        "frames.partial" => "frames.dropped (headed)",
        m if crate::metrics::split_metric(m) => "frames.interval.p95 (headed) or cpu.gpu",
        m if m.starts_with("js.heap.") => "memory.app.private",
        // A leak run's heap snapshots and census.
        "leak.suspects.retained" | "dom.nodes.growth" => "memory.app.private over runs",
        "gpu.compile" => "gpu.process.3d",
        "layout.shift" => "a count.<name> the page keeps",
        m if m.starts_with("span.") => "phase.<name> around the same work in the case",
        _ => return None,
    };
    Some(Refusal::new(ValidatorId::EngineUnsupported, format!("{metric}: {engine} gives strobe no way to read it (it comes from Chromium's Performance domain or trace)")).fix(format!("measure it in Chromium, or use {instead}")))
}

/// The speed baseline from calibration, if any.
pub fn baseline() -> Option<f64> {
    calibration()?["speed_ms"].as_f64()
}

/// The device's last calibration (`at`, `ok`, `speed_ms`, checks), if any.
pub fn calibration() -> Option<Value> {
    let state = std::fs::read_to_string(host::paths::state_dir().join("state.json")).ok()?;
    let v: Value = serde_json::from_str(&state).ok()?;
    v.get("last_calibration").filter(|c| c.is_object()).cloned()
}

/// Spearman's rank correlation, for drift across pairs.
pub fn spearman(values: &[f64]) -> f64 {
    let n = values.len();
    if n < 3 {
        return 0.0;
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    // Ties share their mean rank: with order-of-arrival ranks, a constant series read as a perfect trend.
    let mut rank = vec![0.0; n];
    let mut start = 0;
    while start < n {
        let mut end = start + 1;
        while end < n && values[idx[end]] == values[idx[start]] {
            end += 1;
        }
        for &i in &idx[start..end] {
            rank[i] = (start + end - 1) as f64 / 2.0;
        }
        start = end;
    }
    let mean = (n - 1) as f64 / 2.0;
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (i, r) in rank.iter().enumerate() {
        let a = i as f64 - mean;
        let b = r - mean;
        num += a * b;
        da += a * a;
        db += b * b;
    }
    if da == 0.0 || db == 0.0 { 0.0 } else { num / (da * db).sqrt() }
}
