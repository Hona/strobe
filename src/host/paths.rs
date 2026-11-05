//! Where strobe keeps its state (SPEC §4.2), and how it writes files there: whole or not at all.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The state folder, created user-only (`0700` on Linux and macOS; `%LOCALAPPDATA%` is the user's
/// own on Windows). Traces and heap snapshots in it can hold what the target showed (SPEC rule 9).
pub fn state_dir() -> PathBuf {
    let dir = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(super::home).join("strobe")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| super::home().join(".local/share")).join("strobe")
    };
    owner_only_dir(&dir);
    dir
}

pub fn sub(name: &str) -> PathBuf {
    let dir = state_dir().join(name);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn recordings() -> PathBuf {
    sub("recordings")
}

pub fn experiments() -> PathBuf {
    sub("experiments")
}

pub fn engine() -> PathBuf {
    sub("engine")
}

/// Temporary files: query scripts, run folders, tokens. User-only like the state folder.
pub fn run() -> PathBuf {
    let dir = state_dir().join("run");
    owner_only_dir(&dir);
    dir
}

pub fn sdk() -> PathBuf {
    sub("sdk")
}

/// Creates `dir` (and its parents) and makes it the user's alone on Linux and macOS, once per
/// process: an older strobe made it with the umask's mode.
fn owner_only_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::collections::HashSet;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;
        static DONE: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        if done.get_or_insert_with(HashSet::new).insert(dir.to_path_buf()) {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
}

/// Writes `bytes` to `path` whole or not at all: into a temporary file beside it, flushed to disk,
/// then renamed over it. A crash or a full disk leaves the old file (or none), never half a file.
/// On Windows a scanner that holds the old file open fails the rename for a moment: it is tried
/// again for up to about 2.5 s.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace(path, bytes, false)
}

/// `write_atomic` for a secret (a token): the file is `0600` from its creation on Linux and macOS.
/// On Windows, files in the state folder inherit its user-only ACL.
pub fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace(path, bytes, true)
}

fn replace(path: &Path, bytes: &[u8], owner_only: bool) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}-{}.tmp", std::process::id(), csprng_hex(4)?));
    let written = (|| {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(if owner_only { 0o600 } else { 0o644 });
        }
        #[cfg(not(unix))]
        let _ = owner_only;
        let mut f = o.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let mut wait = std::time::Duration::from_millis(10);
    let mut tries = 0;
    loop {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            // Sharing violations and access denied: another process holds the file (an antivirus).
            Err(e) if cfg!(windows) && tries < 8 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                tries += 1;
                std::thread::sleep(wait);
                wait *= 2;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
}

/// `n` bytes from the OS's random source (`BCryptGenRandom` on Windows, `getrandom` on Linux,
/// `getentropy` on macOS), as hex: ids, temporary names, tokens. It has no weaker fallback.
pub fn csprng_hex(n: usize) -> std::io::Result<String> {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).map_err(|e| std::io::Error::other(format!("the OS random source failed: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
