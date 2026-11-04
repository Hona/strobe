//! `strobe up` / `strobe leave`: one-time setup, repair, and its undo (SPEC §5.3, §5.10).

use super::{Output, mcp};
use crate::cli::Args;
use crate::driver::Driver;
use crate::host::{self, http, paths};
use crate::metrics;
use crate::model::{Error, Result};
use crate::probe::{self, Set};
use crate::target::chromium::{self, Window};
use crate::validity;
use crate::{engine, runner};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const SKILL: &str = include_str!("../../skill/SKILL.md");
const CALIBRATION: &str = include_str!("../../calibration/index.html");
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Setup steps, printed to stderr as each finishes (some take seconds); the command's own output is
/// then one summary line, so the list shows once.
pub(super) struct Steps {
    json: serde_json::Map<String, Value>,
    problems: usize,
}

impl Steps {
    pub(super) fn new(title: &str) -> Steps {
        eprintln!("{title}");
        Steps { json: serde_json::Map::new(), problems: 0 }
    }

    pub(super) fn line(&mut self, mark: &str, name: &str, detail: impl Into<String>) {
        let d = detail.into();
        if mark == "✗" || mark == "!" {
            self.problems += 1;
        }
        eprintln!("  {mark} {:<10} {d}", name);
        self.json.insert(name.to_string(), json!({ "ok": mark == "✓", "detail": d }));
    }

    pub(super) fn done(self, what: &str) -> Output {
        let text = match self.problems {
            0 => format!("{what}: done"),
            1 => format!("{what}: done; 1 step needs attention (marked above)"),
            n => format!("{what}: done; {n} steps need attention (marked above)"),
        };
        Output::new(text, Value::Object(self.json))
    }
}

pub fn run(args: &Args) -> Result<Output> {
    // Nothing `up` starts (the daemon, opencode's CLI) may hold a shell's pipe after `up` exits.
    #[cfg(windows)]
    host::win::private_std_handles();
    let check = args.has("check");
    let skip: Vec<&str> = args.get("skip").map(|s| s.split(',').collect()).unwrap_or_default();
    let device = host::device_name();
    let mut s = Steps::new(&format!("strobe up · {device}"));
    let env = host::env();
    let topo = host::topology();
    let groups: Vec<String> =
        topo.groups.iter().enumerate().map(|(i, g)| format!("CCD{i} {} MB L3{}", g.l3_mb, if topo.vcache().is_some_and(|v| v.mask == g.mask) { " (V-Cache)" } else { "" })).collect();
    s.line("✓", "cpu", format!("{} · {} logical{}", env.cpu, env.logical_cpus, if groups.is_empty() { String::new() } else { format!(" · {}", groups.join(", ")) }));
    let fast = env.power_plan.contains("performance");
    s.line(if fast { "✓" } else { "i" }, "power", if fast { env.power_plan.clone() } else { format!("{} (benches and timed recordings switch to High performance for the run, then back)", env.power_plan) });
    match chromium::find() {
        Some(p) => s.line("✓", "browser", p.display().to_string()),
        None => s.line("✗", "browser", "no Chromium: install Chrome or run bunx playwright install chromium"),
    }
    match runner::find_bun() {
        Some(p) => s.line("✓", "bun", p.display().to_string()),
        None => s.line("!", "bun", "missing: bench files need Bun (https://bun.sh)"),
    }
    if check {
        return Ok(s.done("strobe up --check"));
    }
    // Binary on PATH.
    let (installed, updated) = install_self(&mut s)?;
    // Engine.
    match engine::ensure() {
        Ok(p) => s.line("✓", "engine", format!("trace_processor {} · sha256 ok · {}", engine::VERSION, p.display())),
        Err(e) => s.line("✗", "engine", e.message),
    }
    // Deep mode: present events and native stacks (one UAC prompt when something is missing).
    #[cfg(windows)]
    {
        let (mark, detail) = super::helper::setup(skip.contains(&"deep") || skip.contains(&"helper"));
        s.line(mark, "deep", detail);
    }
    // Daemon: autostart and running.
    if !skip.contains(&"daemon") {
        daemon_up(&mut s, &installed, updated)?;
    }
    // MCP in opencode.
    if !skip.contains(&"mcp") {
        mcp_up(&mut s);
    }
    // Skill.
    skill_up(&mut s)?;
    // Quick calibration.
    if !skip.contains(&"calibrate") {
        match calibrate(false) {
            Ok(c) => {
                let ok = c["ok"].as_bool() == Some(true);
                s.line(if ok { "✓" } else { "!" }, "calibrate", c["summary"].as_str().unwrap_or("").to_string());
            }
            Err(e) => s.line("✗", "calibrate", e.message),
        }
    }
    Ok(s.done("strobe up"))
}

pub(super) fn install_dir() -> PathBuf {
    if cfg!(windows) { std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(host::home).join("Programs").join("strobe") } else { host::home().join(".local").join("bin") }
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Copies this binary (and strobew next to it) into the install dir and puts that dir on the user PATH.
/// Copies this build into the install folder; also says whether it replaced anything.
fn install_self(s: &mut Steps) -> Result<(PathBuf, bool)> {
    let dir = install_dir();
    std::fs::create_dir_all(&dir)?;
    let me = std::env::current_exe()?;
    let from = me.parent().unwrap_or(Path::new(".")).to_path_buf();
    let same = std::fs::canonicalize(&from).ok() == std::fs::canonicalize(&dir).ok();
    let changed = !same && replace_binaries(&from, &dir)?;
    let on_path = path_has(&dir);
    s.line("✓", "binary", format!("{}{}{}", dir.join(exe("strobe")).display(), if changed { " · replaced" } else { "" }, if on_path { "" } else { " · added to PATH (new terminals)" }));
    Ok((dir, changed))
}

/// Puts the strobe binaries in `from` over those in `dir`; says whether any changed.
pub(super) fn replace_binaries(from: &Path, dir: &Path) -> Result<bool> {
    let mut changed = false;
    for name in ["strobe", "strobew"] {
        let src = from.join(exe(name));
        let dst = dir.join(exe(name));
        if !src.is_file() {
            // A build without strobew: an older strobew left here would run the daemon.
            if dst.exists() {
                set_aside(dir, name)?;
                changed = true;
            }
            continue;
        }
        if std::fs::read(&src).ok() == std::fs::read(&dst).ok() {
            continue;
        }
        // Copy beside it, then swap: a running binary (the daemon, the strobe running `update`)
        // can be renamed on Windows but not overwritten.
        let new = dir.join(format!("{}.new", exe(name)));
        std::fs::copy(&src, &new)?;
        if dst.exists() {
            set_aside(dir, name)?;
        }
        std::fs::rename(&new, &dst)?;
        changed = true;
    }
    Ok(changed)
}

/// Moves `name` out of the way as `name.old` (or `.old1`, … while an older one still runs), after
/// removing the old copies nothing runs any more.
fn set_aside(dir: &Path, name: &str) -> Result<()> {
    let file = exe(name);
    for entry in std::fs::read_dir(dir)?.flatten() {
        let n = entry.file_name().to_string_lossy().to_string();
        if n.starts_with(&format!("{file}.old")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let free = (0..100).map(|i| dir.join(if i == 0 { format!("{file}.old") } else { format!("{file}.old{i}") })).find(|p| !p.exists());
    let to = free.ok_or_else(|| Error::msg(format!("{}: 100 old copies still running", dir.join(&file).display())))?;
    std::fs::rename(dir.join(&file), to)?;
    Ok(())
}

fn path_has(dir: &Path) -> bool {
    #[cfg(windows)]
    {
        let current = host::win::user_value("Environment", "Path").unwrap_or_default();
        let want = dir.display().to_string();
        if current.split(';').any(|p| p.trim_end_matches('\\').eq_ignore_ascii_case(want.trim_end_matches('\\'))) {
            return true;
        }
        let next = if current.is_empty() { want } else { format!("{};{want}", current.trim_end_matches(';')) };
        host::win::set_user_value("Environment", "Path", Some(&next), true);
        host::win::broadcast_env_change();
        false
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|x| x == dir))
    }
}

fn daemon_up(s: &mut Steps, dir: &Path, updated: bool) -> Result<()> {
    let state_path = paths::state_dir().join("state.json");
    let mut state: Value = std::fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
    if state["mcp_port"].as_u64().is_none() {
        let port = (mcp::DEFAULT_PORT..mcp::DEFAULT_PORT + 100).find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(mcp::DEFAULT_PORT);
        state["mcp_port"] = json!(port);
        std::fs::write(&state_path, serde_json::to_vec_pretty(&state)?)?;
    }
    mcp::token()?;
    let daemon_exe = if dir.join(exe("strobew")).is_file() { dir.join(exe("strobew")) } else { dir.join(exe("strobe")) };
    #[cfg(windows)]
    host::win::set_user_value(RUN_KEY, "strobe", Some(&format!("\"{}\" daemon", daemon_exe.display())), false);
    let _ = RUN_KEY;
    // The daemon runs MCP calls in its own process: a new build only counts once it restarts.
    let stale = mcp::running() && (updated || mcp::daemon_build().is_some_and(|b| b != mcp::file_id(&daemon_exe)));
    if stale {
        if daemon_benching() {
            s.line("!", "daemon", "keeps the previous build until its bench finishes; run strobe up again then");
            return Ok(());
        }
        let _ = quit_daemon();
        for _ in 0..30 {
            if !mcp::running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if !mcp::running() {
        spawn_detached(&daemon_exe, &["daemon"])?;
        for _ in 0..30 {
            if mcp::running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if mcp::running() {
        s.line("✓", "daemon", format!("running on 127.0.0.1:{} · starts at login", mcp::port()));
    } else {
        s.line("✗", "daemon", "did not start; run `strobe daemon` to see why");
    }
    Ok(())
}

fn spawn_detached(exe: &Path, args: &[&str]) -> Result<()> {
    #[cfg(windows)]
    {
        host::win::spawn_detached(exe, args).map_err(|e| Error::msg(format!("start {}: {e}", exe.display())))?;
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new(exe);
        // Its own session: a terminal closing does not take the daemon with it.
        c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).current_dir(exe.parent().unwrap_or(Path::new("/"))).process_group(0);
        c.spawn().map_err(|e| Error::msg(format!("start {}: {e}", exe.display())))?;
    }
    Ok(())
}

/// The opencode CLI to talk to: the running service's own build first (as mesh does), then PATH.
fn opencode() -> Option<PathBuf> {
    let state = host::home().join(".local").join("state").join("opencode").join("service.json");
    #[cfg(windows)]
    if let Some(pid) = std::fs::read_to_string(&state).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_u64())
        && let Some(p) = host::win::exe_of(pid as u32)
    {
        return Some(p);
    }
    let _ = state;
    host::find_exe("opencode2", &[]).or_else(|| host::find_exe("opencode", &[]))
}

fn global_config() -> PathBuf {
    let dir = host::home().join(".config").join("opencode");
    for name in ["opencode.jsonc", "opencode.json"] {
        if dir.join(name).is_file() {
            return dir.join(name);
        }
    }
    dir.join("opencode.jsonc")
}

fn mcp_up(s: &mut Steps) {
    let Some(oc) = opencode() else {
        s.line("!", "mcp", "opencode not found: add the MCP entry by hand (see strobe status)");
        return;
    };
    let url = format!("http://127.0.0.1:{}/mcp", mcp::port());
    let token_file = mcp::token_path().display().to_string().replace('\\', "/");
    let config = global_config();
    let existing = std::fs::read_to_string(&config).unwrap_or_default();
    if !existing.contains("\"strobe\"") {
        let ok = Command::new(&oc)
            .args(["mcp", "add", "strobe", "--global", "--url", &url, "--header", &format!("Authorization=Bearer {{file:{token_file}}}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|st| st.success());
        if !ok {
            s.line("!", "mcp", format!("`{} mcp add` failed; add strobe to {} by hand", oc.display(), config.display()));
            return;
        }
    }
    // `mcp add` has no flag for oauth; strobe's own entry gets "oauth": false, as mesh's does.
    if let Ok(text) = std::fs::read_to_string(&config)
        && let Some(i) = text.find("\"strobe\": {")
        && !text[i..].split('}').next().unwrap_or("").contains("oauth")
    {
        let at = i + "\"strobe\": {".len();
        let next = format!("{}\"oauth\": false, {}", &text[..at], &text[at..]);
        let _ = std::fs::write(&config, next);
    }
    // Live in every loaded location, no reload.
    let token = mcp::token().unwrap_or_default();
    let entry = json!({ "config": { "type": "remote", "url": url, "oauth": false, "headers": { "Authorization": format!("Bearer {token}") } } }).to_string();
    let locations: Vec<String> = Command::new(&oc)
        .args(["api", "debug.location.list"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|l| l["directory"].as_str().map(str::to_string)).collect()))
        .unwrap_or_default();
    // One CLI call per location takes seconds; eight at a time keeps `up` short with many locations.
    let live: usize = locations
        .chunks(8)
        .map(|chunk| {
            std::thread::scope(|scope| {
                let calls: Vec<_> = chunk
                    .iter()
                    .map(|dir| {
                        let (oc, entry) = (&oc, &entry);
                        scope.spawn(move || {
                            Command::new(oc)
                                .args(["api", "experimental.mcp.add", "--param", "server=strobe", "--param", &format!("directory={dir}"), "--data", entry])
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status()
                                .is_ok_and(|st| st.success())
                        })
                    })
                    .collect();
                calls.into_iter().map(|c| c.join().unwrap_or(false)).filter(|ok| *ok).count()
            })
        })
        .sum();
    s.line("✓", "mcp", format!("registered \"strobe\" · {url} · live in {live} opencode location{}", if live == 1 { "" } else { "s" }));
}

fn skill_path() -> PathBuf {
    host::home().join(".config").join("opencode").join("skills").join("strobe").join("SKILL.md")
}

/// The installed skill against this build's: `current`, `outdated` (from another build, untouched),
/// `customised` (edited by hand; `up` leaves it) or `missing`.
pub fn skill_state() -> &'static str {
    use sha2::Digest;
    let hash = |text: &str| -> String { sha2::Sha256::digest(text.split_once("\n---\n").map(|x| x.1).unwrap_or(text).as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect() };
    let Ok(existing) = std::fs::read_to_string(skill_path()) else { return "missing" };
    let recorded = existing.lines().find_map(|l| l.trim().strip_prefix("strobe/hash: ")).map(str::to_string);
    match recorded {
        Some(h) if h != hash(&existing) => "customised",
        Some(h) if h == hash(SKILL) => "current",
        _ => "outdated",
    }
}

fn skill_up(s: &mut Steps) -> Result<()> {
    use sha2::Digest;
    let body = SKILL.split_once("\n---\n").map(|x| x.1).unwrap_or(SKILL);
    let hash: String = sha2::Sha256::digest(body.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = skill_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let old_hash = existing.lines().find_map(|l| l.trim().strip_prefix("strobe/hash: ")).map(str::to_string);
        let old_body = existing.split_once("\n---\n").map(|x| x.1).unwrap_or(&existing);
        let old_body_hash: String = sha2::Sha256::digest(old_body.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
        if old_hash.as_deref().is_some_and(|h| h != old_body_hash) {
            s.line("!", "skill", format!("{} was customised; left as is", path.display()));
            return Ok(());
        }
    }
    let front_end = SKILL.find("\n---\n").unwrap_or(0);
    let with_meta = format!("{}\nmetadata:\n  strobe/version: {}\n  strobe/hash: {hash}{}", &SKILL[..front_end], env!("CARGO_PKG_VERSION"), &SKILL[front_end..]);
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    std::fs::write(&path, with_meta)?;
    s.line("✓", "skill", path.display().to_string());
    Ok(())
}

/// Runs the calibration page and stores the speed baseline (SPEC §10.3).
pub fn calibrate(full: bool) -> Result<Value> {
    let port = http::serve_static(vec![("/", "text/html; charset=utf-8", CALIBRATION), ("/index.html", "text/html; charset=utf-8", CALIBRATION)])?;
    let base = format!("http://127.0.0.1:{port}/");
    let browser = chromium::launch(&Window::default(), false, None, &[])?;
    let result = (|| -> Result<Value> {
        validity::software_gpu(&browser)?;
        let page = browser.page()?;
        page.navigate(&format!("{base}?case=idle"), Duration::from_secs(30))?;
        validity::window_visible(&page)?;
        let mut speeds: Vec<f64> = (0..5).filter_map(|_| validity::speed_check(&page).ok()).collect();
        speeds.sort_by(|a, b| a.total_cmp(b));
        let speed = metrics::median(&speeds);
        let mut checks = Vec::new();
        let measure = |query: &str, ms: u64| -> Result<BTreeMap<String, f64>> {
            page.navigate(&format!("{base}?{query}"), Duration::from_secs(30))?;
            let token = format!("cal{}", host::now_ns() % 1_000_000);
            let driver = Driver::new(page.clone(), token.clone());
            let tracing = probe::trace::start(&browser.cdp, Set::Timing)?;
            std::thread::sleep(Duration::from_millis(300));
            driver.mark("run:0:start")?;
            std::thread::sleep(Duration::from_millis(ms));
            driver.mark("run:0:end")?;
            let file = paths::run().join(format!("{token}.pftrace"));
            tracing.stop(&file)?;
            let data = metrics::read(&file, &token)?;
            let _ = std::fs::remove_file(&file);
            Ok(metrics::block_values(&data, "run", &BTreeMap::new()))
        };
        let light = measure("case=busy&ms=2", 2000)?;
        let vsync = light.get("display.hz").map(|h| 1000.0 / h).unwrap_or(f64::NAN);
        let light_ok = light.get("frames.dropped.pct").is_some_and(|d| *d < 2.0) && light.get("frames.interval.p50").is_some_and(|i| (i - vsync).abs() / vsync < 0.05);
        checks.push(json!({ "case": "busy 2 ms", "ok": light_ok, "dropped_pct": light.get("frames.dropped.pct"), "interval_p50": light.get("frames.interval.p50"), "vsync_ms": vsync }));
        if vsync.is_finite() {
            let heavy_ms = (vsync * 1.5).round();
            let heavy = measure(&format!("case=busy&ms={heavy_ms}"), 2000)?;
            let d = heavy.get("frames.dropped.pct").copied().unwrap_or(f64::NAN);
            checks.push(json!({ "case": format!("busy {heavy_ms} ms"), "ok": (25.0..75.0).contains(&d), "dropped_pct": d }));
        }
        let idle = measure("case=idle", 1500)?;
        let idle_ok = idle.get("main.busy").is_none_or(|b| *b < 5.0);
        checks.push(json!({ "case": "idle", "ok": idle_ok, "main_busy": idle.get("main.busy") }));
        if full {
            let gpu = measure("case=gpu", 2000)?;
            checks.push(json!({ "case": "gpu", "ok": true, "frames": gpu.get("frames.n"), "dropped_pct": gpu.get("frames.dropped.pct") }));
        }
        let ok = checks.iter().all(|c| c["ok"].as_bool() == Some(true));
        let hz = light.get("display.hz").copied().unwrap_or(f64::NAN);
        let summary = format!("speed check {speed:.1} ms · display {hz:.0} Hz · {}/{} cases within tolerance", checks.iter().filter(|c| c["ok"].as_bool() == Some(true)).count(), checks.len());
        Ok(json!({ "at": super::record::now_iso(), "speed_ms": speed, "display_hz": hz, "checks": checks, "ok": ok, "summary": summary }))
    })();
    browser.close();
    let cal = result?;
    let state_path = paths::state_dir().join("state.json");
    let mut state: Value = std::fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
    state["last_calibration"] = cal.clone();
    std::fs::write(&state_path, serde_json::to_vec_pretty(&state)?)?;
    Ok(cal)
}

pub fn leave(args: &Args) -> Result<Output> {
    #[cfg(windows)]
    host::win::private_std_handles();
    let mut s = Steps::new("strobe leave");
    #[cfg(windows)]
    host::win::set_user_value(RUN_KEY, "strobe", None, false);
    s.line("✓", "autostart", "removed");
    if mcp::running() {
        let _ = quit_daemon();
    }
    s.line("✓", "daemon", "stopped");
    // MCP: cut our own entry from the global config, then remove it live.
    let config = global_config();
    if let Ok(text) = std::fs::read_to_string(&config)
        && let Some(cut) = cut_entry(&text, "strobe")
    {
        std::fs::write(&config, cut)?;
    }
    if let Some(oc) = opencode() {
        let locations: Vec<String> = Command::new(&oc)
            .args(["api", "debug.location.list"])
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
            .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|l| l["directory"].as_str().map(str::to_string)).collect()))
            .unwrap_or_default();
        for dir in locations {
            let _ = Command::new(&oc).args(["api", "experimental.mcp.remove", "--param", "server=strobe", "--param", &format!("directory={dir}")]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
    s.line("✓", "mcp", "removed");
    let _ = std::fs::remove_dir_all(skill_path().parent().unwrap_or(Path::new("")));
    s.line("✓", "skill", "removed");
    // The deep-mode helper lives where only an elevated step writes: one UAC prompt removes it.
    #[cfg(windows)]
    if crate::etw::access().helper
        && let Some(sid) = crate::etw::helper::user_sid()
    {
        match crate::etw::helper::elevated(&format!("helper uninstall --sid {sid}")) {
            Ok(0) => s.line("✓", "deep", "removed the helper task, its copy and Performance Log Users membership"),
            Ok(_) => s.line("!", "deep", "helper uninstall failed; run strobe leave again"),
            Err(e) => s.line("!", "deep", format!("{}: the helper stays", e.message)),
        }
    }
    // The install folder `up` added to the user PATH (the binaries stay: this one is running).
    #[cfg(windows)]
    {
        let want = install_dir().display().to_string();
        let current = host::win::user_value("Environment", "Path").unwrap_or_default();
        let kept: Vec<&str> = current.split(';').filter(|p| !p.is_empty() && !p.trim_end_matches('\\').eq_ignore_ascii_case(want.trim_end_matches('\\'))).collect();
        if kept.len() != current.split(';').filter(|p| !p.is_empty()).count() {
            host::win::set_user_value("Environment", "Path", Some(&kept.join(";")), true);
            host::win::broadcast_env_change();
            s.line("✓", "path", format!("removed {want}"));
        }
    }
    if args.has("yes") {
        let _ = std::fs::remove_dir_all(paths::state_dir());
        s.line("✓", "state", "removed (recordings and experiments included)");
    } else {
        s.line("i", "state", format!("kept {} (pass --yes to delete recordings and experiments)", paths::state_dir().display()));
    }
    Ok(s.done("strobe leave"))
}

/// Whether a bench runs inside the daemon (MCP benches do): restarting it would kill that bench.
pub(super) fn daemon_benching() -> bool {
    super::bench::slot::daemon_benching()
}

pub(super) fn quit_daemon() -> Result<()> {
    use std::io::Write;
    let port = mcp::port();
    let token = mcp::token()?;
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "strobe/quit" }).to_string();
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(s, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())?;
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

/// Removes `"name": { … }` (and a trailing comma) from JSONC text, leaving every other byte.
fn cut_entry(text: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\":");
    let start = text.find(&key)?;
    let open = start + text[start..].find('{')?;
    let mut depth = 0;
    let mut end = None;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let mut end = end?;
    let rest = &text[end..];
    if let Some(comma) = rest.find(',')
        && rest[..comma].trim().is_empty()
    {
        end += comma + 1;
    }
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(start);
    let prefix = if text[line_start..start].trim().is_empty() { line_start } else { start };
    Some(format!("{}{}", &text[..prefix], text[end..].trim_start_matches([' ', '\t']).trim_start_matches(['\r', '\n'])))
}
