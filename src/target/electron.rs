//! Launching Electron apps (OpenCode desktop) fully isolated, with the user-service guard (SPEC §13.2).
//!
//! The recipe is `packages/desktop/script/bench-startup.ts`. `OPENCODE_DESKTOP_TEST_ROOT` alone is not
//! enough: the desktop deletes XDG_STATE_HOME on Windows and then replaces the user's live service.

use super::cdp::Cdp;
use super::chromium::{Browser, Window};
use crate::host;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The user's own opencode background service, as registered before strobe launched anything.
#[derive(Clone, Debug, PartialEq)]
pub struct UserService {
    pub pid: u64,
    pub url: String,
}

/// Reads the real registration (from the real home, never from overridden env).
pub fn user_service() -> Option<UserService> {
    let p = host::home().join(".local").join("state").join("opencode").join("service.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    Some(UserService { pid: v["pid"].as_u64()?, url: v["url"].as_str().unwrap_or("").to_string() })
}

/// Fails when the user's service changed or stopped answering since `before`.
pub fn guard(before: &Option<UserService>, when: &str) -> Result<()> {
    let Some(b) = before else { return Ok(()) };
    let now = user_service();
    let answering = b.url.strip_prefix("http://").and_then(|h| h.split('/').next()).and_then(|h| h.parse::<std::net::SocketAddr>().ok()).is_some_and(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok());
    if now.as_ref() != Some(b) || !answering {
        return Err(Error::msg(format!(
            "stopped {when}: your opencode service changed (pid {} → {}); strobe never measures past a disturbed user service",
            b.pid,
            now.map(|n| n.pid.to_string()).unwrap_or_else(|| "none".into())
        ))
        .hint("this is a strobe bug in isolation: report it; restart your service with `opencode service start` if it is down"));
    }
    Ok(())
}

/// Installed OpenCode desktop channels: `opencode`, `opencode-beta`, `opencode-dev`.
pub fn find_app(id: &str) -> Option<PathBuf> {
    let (dir, exe) = match id {
        "opencode" => ("ai.opencode.desktop", "OpenCode"),
        "opencode-beta" => ("ai.opencode.desktop.beta", "OpenCode Beta"),
        "opencode-dev" => ("ai.opencode.desktop.dev", "OpenCode Dev"),
        _ => return None,
    };
    if cfg!(windows) {
        let p = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?.join("Programs").join(dir).join(format!("{exe}.exe"));
        return p.is_file().then_some(p);
    }
    if cfg!(target_os = "macos") {
        let p = PathBuf::from(format!("/Applications/{exe}.app/Contents/MacOS/{exe}"));
        return p.is_file().then_some(p);
    }
    None
}

/// The Electron app id (userData folder name) for an OpenCode executable.
fn app_id(exe: &Path) -> String {
    let name = exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name.contains("beta") {
        "ai.opencode.desktop.beta".into()
    } else if name.contains("dev") {
        "ai.opencode.desktop.dev".into()
    } else {
        "ai.opencode.desktop".into()
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let p = e.path();
        let dst = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dst)?;
        } else {
            std::fs::copy(&p, &dst)?;
        }
    }
    Ok(())
}

/// Launches an Electron app in a fresh isolated home (seeded from `seed`), with a debugging port.
pub fn launch(exe: &Path, seed: Option<&Path>, window: &Window, cpus: Option<u64>) -> Result<(Browser, Option<UserService>)> {
    let before = user_service();
    live_channel(exe, &before)?;
    let home = host::paths::run().join(format!("home-{}-{}", std::process::id(), host::now_ns()));
    let appdata = home.join("AppData").join("Roaming");
    let local = home.join("AppData").join("Local");
    let temp = local.join("Temp");
    let config = home.join(".config").join("opencode");
    let share = home.join(".local").join("share");
    for d in [&appdata, &temp, &config, &share.join("opencode"), &home.join(".local").join("state"), &home.join(".cache")] {
        std::fs::create_dir_all(d)?;
    }
    if let Some(s) = seed {
        copy_dir(s, &home)?;
    }
    let service_port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    std::fs::write(config.join("service.json"), json!({ "port": service_port }).to_string())?;
    let id = app_id(exe);
    let user_data = appdata.join(&id);
    let mut cmd = Command::new(exe);
    for (k, _) in std::env::vars() {
        if k.starts_with("OPENCODE_") || k.starts_with("OTEL_") || k.starts_with("SENTRY_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("APPDATA", &appdata)
        .env("LOCALAPPDATA", &local)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("XDG_DATA_HOME", &share)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("OPENCODE_DB", share.join("opencode").join("opencode.db"))
        .env("OPENCODE_CONFIG_DIR", &config);
    if id != "ai.opencode.desktop.dev" {
        // Update checks on start fail fast and offline.
        cmd.env("HTTPS_PROXY", "http://127.0.0.1:9");
    }
    cmd.arg("--remote-debugging-port=0").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let previous = cpus.and_then(host::set_affinity);
    let spawned = cmd.spawn();
    if previous.is_some() {
        host::set_affinity(previous.unwrap_or(0));
    }
    let child = spawned.map_err(|e| Error::msg(format!("launch {}: {e}", exe.display())))?;
    let pid = child.id();
    let mut browser_guard = Launching { pid, home: Some(home.clone()) };
    let port_file = user_data.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(60);
    let text = loop {
        if let Err(e) = guard(&before, "while the app started") {
            return Err(e);
        }
        if let Ok(t) = std::fs::read_to_string(&port_file)
            && t.lines().count() >= 2
        {
            break t;
        }
        if Instant::now() > deadline {
            return Err(Error::unreachable(format!("{} did not open a debugging port in 60 s (expected {})", exe.display(), port_file.display())));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut lines = text.lines();
    let (port, path) = (lines.next().unwrap_or("").trim(), lines.next().unwrap_or("").trim());
    let ws = format!("ws://127.0.0.1:{port}{path}");
    let cdp = Cdp::connect(&ws)?;
    let version = cdp.send("Browser.getVersion", json!({}), None)?.get("product").and_then(Value::as_str).unwrap_or("").to_string();
    browser_guard.home = None;
    std::mem::forget(browser_guard);
    Ok((Browser { cdp, ws, child: Some(child), pid, profile: Some(home), exe: exe.to_path_buf(), version, window: window.clone() }, before))
}

/// Refuses to launch a channel the user is using: one that is running, or the one their live service
/// belongs to. Isolation should make it safe, but the user's own app is never the thing to experiment on.
fn live_channel(exe: &Path, service: &Option<UserService>) -> Result<()> {
    #[cfg(windows)]
    {
        let dir = exe.parent().map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !host::win::running(exe).is_empty() {
            return Err(Error::refused("LiveChannel", format!("{} is running: that is your app", exe.display()), "benchmark another channel (app:opencode, app:opencode-beta) or close it"));
        }
        if let Some(s) = service
            && let Some(service_exe) = host::win::exe_of(s.pid as u32)
        {
            // The service's CLI lives under the channel's own data folder (…\ai.opencode.desktop.dev\cli\…).
            let service_path = service_exe.to_string_lossy().to_lowercase();
            let channel = exe.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !channel.is_empty() && (service_path.contains(&format!("\\{channel}\\")) || service_path.starts_with(&dir)) {
                return Err(Error::refused("LiveChannel", format!("your live opencode service runs from the {channel} channel ({})", service_exe.display()), "benchmark another channel"));
            }
        }
    }
    let _ = (exe, service);
    Ok(())
}

/// Kills a half-started launch when `launch` returns early.
struct Launching {
    pid: u32,
    home: Option<PathBuf>,
}

impl Drop for Launching {
    fn drop(&mut self) {
        kill_tree(self.pid);
        if let Some(h) = self.home.take() {
            let _ = std::fs::remove_dir_all(h);
        }
    }
}

pub fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    } else {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// The app window's page: the first page target that is not DevTools.
pub fn main_page(browser: &Browser) -> Result<super::chromium::Page> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let targets = browser.cdp.send("Target.getTargets", json!({}), None)?;
        let found = targets["targetInfos"].as_array().and_then(|a| {
            a.iter()
                .filter(|t| t["type"] == "page" && !t["url"].as_str().unwrap_or("").starts_with("devtools://"))
                .map(|t| t["targetId"].as_str().unwrap_or("").to_string())
                .next()
        });
        if let Some(target) = found {
            let attached = browser.cdp.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
            let session = attached["sessionId"].as_str().ok_or_else(|| Error::msg("attach: no session"))?.to_string();
            let page = super::chromium::Page { cdp: browser.cdp.clone(), session, target };
            page.send("Page.enable", json!({}))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while host::raise_windows(browser.pid) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            return Ok(page);
        }
        if Instant::now() > deadline {
            return Err(Error::unreachable("the app opened no window in 60 s"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
