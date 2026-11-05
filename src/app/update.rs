//! `strobe update` (SPEC §5.10): replace the installed copy with the latest npm release (or one
//! version, or a local build with `--from`), then let that build's own `up` install it, refresh the
//! skill, the MCP registration and the helper, and restart the daemon.

use super::Output;
use crate::cli::Args;
use crate::host::paths;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PACKAGE: &str = "@hona/strobe";

/// npm's name for this OS and CPU (`@hona/strobe-<platform>-<arch>`), as the release publishes them.
fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("win32-x64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

pub fn run(args: &Args) -> Result<Output> {
    #[cfg(windows)]
    crate::host::win::private_std_handles();
    let wanted = args.pos.first().map(String::as_str);
    let stage = paths::run().join(format!("update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    let result = (|| -> Result<Output> {
        let (source, version) = match args.get("from") {
            Some(from) => (from_local(Path::new(from), &stage)?, None),
            None => {
                let (dir, v) = from_npm(wanted, &stage)?;
                (dir, Some(v))
            }
        };
        let installed = super::up::install_dir().join(exe("strobe"));
        let new = source.join(exe("strobe"));
        let reported = Command::new(&new).arg("--version").stdin(Stdio::null()).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let Some(reported) = reported else { return Err(Error::msg(format!("{} does not run (strobe --version failed)", new.display()))) };
        let current = Command::new(&installed).arg("--version").stdin(Stdio::null()).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        // One build id is one build (its sources, target and profile); copies without one compare bytes.
        let same = match (super::mcp::file_build(&installed), super::mcp::file_build(&new)) {
            (Some(a), Some(b)) => a == b,
            _ => sha256(&installed).ok().is_some_and(|a| sha256(&new).ok().as_ref() == Some(&a)),
        };
        if args.has("check") {
            let text = if same { format!("up to date: {current}") } else { format!("installed {}; available {reported}{}", if current.is_empty() { "nothing".into() } else { current.clone() }, version.as_deref().map(|v| format!(" (npm {v})")).unwrap_or_default()) };
            return Ok(Output::new(text, json!({ "installed": current, "available": reported, "current": same })));
        }
        if same {
            return Ok(Output::new(format!("up to date: {current} at {}", installed.display()), json!({ "installed": current, "updated": false })));
        }
        // The latest release over a newer copy (a local build from main that others' sessions use) is a
        // downgrade nobody asked for: an explicit version, `--from` or `--force` says so.
        let semver = |s: &str| -> Option<(u64, u64, u64)> {
            // `strobe 0.6.0 (a1b2c3d4e5, build …)`: the version is the second word.
            let v = s.split_whitespace().nth(1)?.split(['-', '+']).next()?;
            let mut p = v.split('.').map(|x| x.parse::<u64>().ok());
            Some((p.next()??, p.next()??, p.next()??))
        };
        if wanted.is_none() && args.get("from").is_none() && !args.has("force")
            && let (Some(a), Some(b)) = (semver(&current), semver(&reported))
            && a >= b
        {
            let what = if a > b { format!("{current} is newer than npm's {reported}") } else { format!("{current} is another build than npm's {reported}") };
            return Err(Error::usage(format!("the installed copy at {}: {what}; strobe update keeps it (another session may use it)", installed.display()))
                .hint(format!("strobe update --force replaces it with npm's {reported}; strobe update <version> installs that version")));
        }
        super::progress(format!("installing {reported} over {}", if current.is_empty() { "nothing" } else { current.as_str() }));
        // This build swaps the files (whatever the new build's own install does, a running strobe
        // is renamed aside, never overwritten) and stops the daemon, so the new build's `up` starts
        // its own.
        let dir = super::up::install_dir();
        std::fs::create_dir_all(&dir)?;
        super::up::replace_binaries(&source, &dir)?;
        if super::mcp::running() {
            if super::up::daemon_benching() {
                super::progress("  the daemon keeps the previous build until its bench finishes, then restarts as this one by itself");
            } else {
                let _ = super::up::quit_daemon();
                for _ in 0..30 {
                    if !super::mcp::running() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        // The new build writes its own skill, MCP registration and helper. The helper is refreshed
        // only where one is installed: an update never asks for UAC to add deep mode the user never
        // set up.
        let new = dir.join(exe("strobe"));
        let mut skip = vec!["calibrate"];
        #[cfg(windows)]
        if !crate::probe::present::access().helper {
            skip.push("deep");
        }
        #[cfg(not(windows))]
        skip.push("deep");
        let status = run_relayed(Command::new(&new).args(["up", "--skip", &skip.join(",")])).map_err(|e| Error::msg(format!("run {}: {e}", new.display())))?;
        let mut out = Output::new(format!("strobe update: {} → {reported}", if current.is_empty() { "nothing" } else { current.as_str() }), json!({ "installed": reported, "previous": current, "updated": status.success() }));
        if !status.success() {
            out.text = format!("strobe update: the new build's `strobe up` failed (exit {}); the steps above say which", status.code().unwrap_or(1));
            out.code = 1;
        }
        Ok(out)
    })();
    // The staged copy ran `up`, which copied it; a binary still running from it (none should) keeps
    // its folder until the next update.
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// A local build: a strobe executable, or the folder holding it (and strobew next to it).
fn from_local(from: &Path, stage: &Path) -> Result<PathBuf> {
    let dir = if from.is_dir() { from.to_path_buf() } else { from.parent().unwrap_or(Path::new(".")).to_path_buf() };
    let main = if from.is_file() { from.to_path_buf() } else { dir.join(exe("strobe")) };
    if !main.is_file() {
        return Err(Error::usage(format!("--from: no {} at {}", exe("strobe"), from.display())));
    }
    // Copied first: the build folder stays free for the next build, and `up` copies from one place.
    std::fs::copy(&main, stage.join(exe("strobe")))?;
    let w = dir.join(exe("strobew"));
    if cfg!(windows) && w.is_file() {
        std::fs::copy(&w, stage.join(exe("strobew")))?;
    }
    Ok(stage.to_path_buf())
}

/// The platform package from npm: the latest release, or `version`. Its tarball is checked against
/// the registry's sha512 integrity before anything is unpacked.
fn from_npm(version: Option<&str>, stage: &Path) -> Result<(PathBuf, String)> {
    use base64::Engine;
    use sha2::Digest;
    let platform = platform().ok_or_else(|| Error::msg(format!("no strobe release for {} {}", std::env::consts::OS, std::env::consts::ARCH)))?;
    let name = format!("{PACKAGE}-{platform}");
    let url = format!("https://registry.npmjs.org/{}/{}", name.replace('/', "%2f"), version.unwrap_or("latest"));
    let meta: Value = serde_json::from_slice(&curl(&url, None)?).map_err(|_| Error::unreachable(format!("{url}: not JSON")))?;
    let v = meta["version"].as_str().ok_or_else(|| Error::msg(format!("{name}: no {} release on npm", version.unwrap_or("latest"))))?.to_string();
    let tarball = meta["dist"]["tarball"].as_str().ok_or_else(|| Error::msg(format!("{name}@{v}: no tarball")))?;
    let integrity = meta["dist"]["integrity"].as_str().and_then(|i| i.strip_prefix("sha512-")).ok_or_else(|| Error::msg(format!("{name}@{v}: no sha512 integrity")))?;
    let file = stage.join("package.tgz");
    super::progress(format!("download {name}@{v}"));
    curl(tarball, Some(&file))?;
    let bytes = std::fs::read(&file)?;
    let got = base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&bytes));
    if got != integrity {
        return Err(Error::msg(format!("{name}@{v}: the tarball's sha512 does not match the registry's integrity")));
    }
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut tar)?;
    let files = untar(&tar);
    let mut found = false;
    for n in ["strobe", "strobew"] {
        if let Some((_, body)) = files.iter().find(|(p, _)| p == &format!("package/{}", exe(n))) {
            let to = stage.join(exe(n));
            std::fs::write(&to, body)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))?;
            }
            found |= n == "strobe";
        }
    }
    if !found {
        return Err(Error::msg(format!("{name}@{v}: the package has no {}", exe("strobe"))));
    }
    Ok((stage.to_path_buf(), v))
}

/// Runs a command with its output relayed through this process instead of inherited, and returns
/// once it exits. Builds up to 0.3.0 started the daemon with the console's handles: inherited, that
/// daemon held the caller's pipe open forever, so the caller never saw `update` finish. Relayed, it
/// holds only this process's pipe, which is abandoned at exit.
fn run_relayed(c: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    // Both to stderr: `update` prints its own result line.
    let relay = |from: Box<dyn Read + Send>, tx: std::sync::mpsc::Sender<()>| {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(from).lines().map_while(std::result::Result::ok) {
                eprintln!("{line}");
            }
            let _ = tx.send(());
        })
    };
    if let Some(o) = child.stdout.take() {
        relay(Box::new(o), tx.clone());
    }
    if let Some(e) = child.stderr.take() {
        relay(Box::new(e), tx.clone());
    }
    drop(tx);
    let status = child.wait()?;
    // The pipes end at its exit unless something it started holds them: then stop listening.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    for _ in 0..2 {
        if rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).is_err() {
            break;
        }
    }
    Ok(status)
}

fn curl(url: &str, to: Option<&Path>) -> Result<Vec<u8>> {
    let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    c.args(["-fsSL", "--retry", "2"]).stdin(Stdio::null());
    if let Some(p) = to {
        c.arg("-o").arg(p);
    }
    let out = c.arg(url).output().map_err(|e| Error::unreachable(format!("curl: {e}")))?;
    if !out.status.success() {
        return Err(Error::unreachable(format!("download {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(out.stdout)
}

/// Regular files of a tar archive (ustar names with their prefix), as (path, contents).
fn untar(tar: &[u8]) -> Vec<(String, Vec<u8>)> {
    let field = |h: &[u8], a: usize, b: usize| String::from_utf8_lossy(&h[a..b]).trim_end_matches('\0').to_string();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 512 <= tar.len() {
        let h = &tar[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let size = usize::from_str_radix(field(h, 124, 136).trim(), 8).unwrap_or(0);
        let (name, prefix) = (field(h, 0, 100), field(h, 345, 500));
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let body = at + 512;
        if matches!(h[156], b'0' | 0) && body + size <= tar.len() {
            out.push((path, tar[body..body + size].to_vec()));
        }
        at = body + size.div_ceil(512) * 512;
    }
    out
}

fn sha256(path: &Path) -> Result<String> {
    use sha2::Digest;
    Ok(sha2::Sha256::digest(std::fs::read(path)?).iter().map(|b| format!("{b:02x}")).collect())
}

