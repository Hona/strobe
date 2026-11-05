//! The daemon's start at login (SPEC §16): the HKCU Run key (Windows), a LaunchAgent with ProcessType
//! Interactive (macOS: its timers fire on time, as a terminal's do), a systemd user unit (Linux). A
//! daemon restart ends the daemon only, not the tracks it started (AbandonProcessGroup, KillMode).
//! Each records the `PATH` and `XDG_DATA_HOME` of the shell `up` ran in (where the daemon finds its
//! tools and its state). The display comes from the user's session as for any user service: on Linux
//! the user manager's environment, which `up` checks.

use crate::host;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.hona.strobe.daemon";

/// What `install` registered.
pub struct Installed {
    /// How the daemon starts at login.
    pub what: String,
    /// Whether this call wrote a new registration (its service manager may hold the old one).
    pub changed: bool,
    /// What the daemon will lack that this shell has, if anything.
    pub note: Option<String>,
}

/// Runs a service manager command; its output, or its error text when it fails.
#[cfg(unix)]
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(format!("{program} {} failed: {}", args.iter().find(|a| !a.starts_with('-')).copied().unwrap_or(""), if err.is_empty() { format!("exit {}", out.status.code().unwrap_or(-1)) } else { err }))
}

#[cfg(unix)]
fn captured() -> Vec<(String, String)> {
    ["PATH", "XDG_DATA_HOME"].iter().filter_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()).map(|v| (n.to_string(), v))).collect()
}

/// Where the registration lives.
pub fn path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        host::home().join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| host::home().join(".config")).join("systemd").join("user").join("strobe.service")
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"HKCU\{}\strobe", super::RUN_KEY))
    }
}

#[cfg(target_os = "macos")]
fn content(exe: &Path) -> String {
    let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let env: String = captured().iter().map(|(k, v)| format!("\n    <key>{}</key><string>{}</string>", x(k), x(v))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by strobe up; strobe leave removes it. -->
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
  <key>WorkingDirectory</key><string>{}</string>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>AbandonProcessGroup</key><true/>
  <key>EnvironmentVariables</key><dict>{env}
  </dict>
</dict>
</plist>
"#,
        x(&exe.display().to_string()),
        x(&exe.parent().unwrap_or(Path::new("/")).display().to_string())
    )
}

/// A value quoted for a systemd unit (`"…"`, with `\`, `"` and `%` escaped).
#[cfg(target_os = "linux")]
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

/// The first quoted value of a unit line, as `quote` wrote it.
#[cfg(target_os = "linux")]
fn unquote(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.strip_prefix('"')?.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '%' => {
                chars.next();
                out.push('%');
            }
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn content(exe: &Path) -> String {
    let env: String = captured().iter().map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}")))).collect();
    format!(
        "# Written by strobe up; strobe leave removes it.\n[Unit]\nDescription=strobe daemon (MCP and reports)\n\n[Service]\nExecStart={} daemon\nWorkingDirectory={}\nKillMode=process\n{env}\n[Install]\nWantedBy=default.target\n",
        quote(&exe.display().to_string()),
        // WorkingDirectory takes the path as written (no quotes); only specifiers need escaping.
        exe.parent().unwrap_or(Path::new("/")).display().to_string().replace('%', "%%")
    )
}

/// Registers the daemon to start at login; says how, or why it could not.
pub fn install(exe: &Path) -> Result<Installed, String> {
    #[cfg(windows)]
    {
        let cmd = format!("\"{}\" daemon", exe.display());
        let changed = host::win::user_value(super::RUN_KEY, "strobe").as_deref() != Some(cmd.as_str());
        host::win::set_user_value(super::RUN_KEY, "strobe", Some(&cmd), false);
        if host::win::user_value(super::RUN_KEY, "strobe").as_deref() == Some(cmd.as_str()) {
            Ok(Installed { what: "HKCU Run key".into(), changed, note: None })
        } else {
            Err(format!("could not write {}", path().display()))
        }
    }
    #[cfg(unix)]
    {
        let p = path();
        let text = content(exe);
        let changed = std::fs::read_to_string(&p).ok().as_deref() != Some(text.as_str());
        if changed {
            std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).map_err(|e| format!("{}: {e}", p.display()))?;
            std::fs::write(&p, &text).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        #[cfg(target_os = "macos")]
        let note = None;
        #[cfg(target_os = "linux")]
        let note = {
            if changed {
                run("systemctl", &["--user", "daemon-reload"])?;
            }
            if !wants().exists() {
                run("systemctl", &["--user", "enable", "strobe.service"])?;
            }
            // The session's display reaches user services through the user manager's environment
            // (desktops import it at login); without it, windowed MCP benches find no display.
            let manager = run("systemctl", &["--user", "show-environment"]).unwrap_or_default();
            let missing: Vec<&str> = ["DISPLAY", "WAYLAND_DISPLAY"].into_iter().filter(|v| std::env::var_os(v).is_some() && !manager.lines().any(|l| l.starts_with(&format!("{v}=")))).collect();
            (!missing.is_empty()).then(|| {
                format!("the user manager has no {}, so the daemon's windowed benches would find no display (`systemctl --user import-environment {}` gives it this session's)", missing.join(" or "), missing.join(" "))
            })
        };
        Ok(Installed { what: format!("{} {}", if cfg!(target_os = "macos") { "LaunchAgent" } else { "systemd user unit" }, p.display()), changed, note })
    }
}

/// The `default.target.wants` link `systemctl --user enable` makes.
#[cfg(target_os = "linux")]
fn wants() -> PathBuf {
    path().parent().unwrap_or(Path::new(".")).join("default.target.wants").join("strobe.service")
}

/// Starts the daemon through its service manager (only called while none answers); Windows has none
/// for it, so it starts there as a detached process, as at login. `changed`: the registration was just
/// rewritten, so a manager that loaded the old one loads it again.
pub fn start(exe: &Path, changed: bool) -> Result<(), String> {
    #[cfg(unix)]
    let _ = exe;
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let target = format!("{domain}/{LABEL}");
        let loaded = || run("launchctl", &["print", &target]).is_ok();
        if loaded() {
            if !changed {
                return run("launchctl", &["kickstart", "-k", &target]).map(|_| ());
            }
            // bootout returns before the job is gone, and a bootstrap meanwhile fails (error 5).
            let _ = run("launchctl", &["bootout", &target]);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while loaded() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        run("launchctl", &["bootstrap", &domain, &path().display().to_string()]).map(|_| ())
    }
    #[cfg(target_os = "linux")]
    {
        let _ = changed;
        run("systemctl", &["--user", "restart", "strobe.service"]).map(|_| ())
    }
    #[cfg(windows)]
    {
        let _ = changed;
        super::spawn_detached(exe, &["daemon"]).map_err(|e| e.message)
    }
}

/// Whether the daemon starts at login, and the detail (`strobe status`): checked, not assumed.
pub fn state() -> (bool, String) {
    let exists = |p: &Path| p.is_file();
    #[cfg(windows)]
    {
        match host::win::user_value(super::RUN_KEY, "strobe") {
            Some(cmd) => {
                let exe = cmd.trim_start_matches('"').split('"').next().unwrap_or("").to_string();
                if exists(Path::new(&exe)) { (true, format!("starts at login (HKCU Run: {cmd})")) } else { (false, format!("the Run key names {exe}, which is gone: strobe up")) }
            }
            None => (false, "does not start at login: strobe up".into()),
        }
    }
    #[cfg(unix)]
    {
        let p = path();
        let Ok(text) = std::fs::read_to_string(&p) else { return (false, "does not start at login: strobe up".into()) };
        #[cfg(target_os = "macos")]
        let exe = text.split("<array><string>").nth(1).and_then(|r| r.split("</string>").next()).unwrap_or("").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
        #[cfg(target_os = "linux")]
        let exe = text.lines().find_map(|l| l.strip_prefix("ExecStart=")).and_then(unquote).unwrap_or_default();
        if !exists(Path::new(&exe)) {
            return (false, format!("{} names {exe}, which is gone: strobe up", p.display()));
        }
        #[cfg(target_os = "linux")]
        if !wants().exists() {
            return (false, format!("{} is not enabled: strobe up", p.display()));
        }
        (true, format!("starts at login ({})", p.display()))
    }
}

/// Removes the registration (`strobe leave`); a daemon its service manager runs stops with it.
pub fn remove() -> String {
    #[cfg(windows)]
    {
        host::win::set_user_value(super::RUN_KEY, "strobe", None, false);
        "removed the HKCU Run key".into()
    }
    #[cfg(unix)]
    {
        let p = path();
        #[cfg(target_os = "macos")]
        let _ = run("launchctl", &["bootout", &format!("gui/{}/{LABEL}", unsafe { libc::getuid() })]);
        #[cfg(target_os = "linux")]
        let _ = run("systemctl", &["--user", "disable", "strobe.service"]);
        let existed = p.is_file();
        let _ = std::fs::remove_file(&p);
        #[cfg(target_os = "linux")]
        if existed {
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
        if existed { format!("removed {}", p.display()) } else { "none registered".into() }
    }
}
