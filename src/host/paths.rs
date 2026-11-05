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

/// Creates `dir` (its parents as the umask says) and makes it the user's alone on Linux and macOS:
/// created `0700`, and an existing one (an older strobe made it with the umask's mode) that others
/// may read is set to `0700` when it is seen.
fn owner_only_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
        if std::fs::metadata(dir).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    let _ = std::fs::create_dir_all(dir);
}

/// Writes `bytes` to `path` whole or not at all: into a temporary file beside it, flushed to disk,
/// then renamed over it. A crash or a full disk leaves the old file (or none), never half a file.
/// On Windows a scanner that holds the old file open fails the rename for a moment: it is tried
/// again for up to about 2.5 s.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace(path, bytes, 0o644)
}

/// `write_atomic` for a secret (a token): the file is `0600` from its creation on Linux and macOS.
/// On Windows, files in the state folder inherit its user-only ACL.
pub fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace(path, bytes, 0o600)
}

/// `write_atomic` for a program strobe runs (a downloaded engine): `0755` from its creation, so
/// no moment exists where the file is there but cannot run.
pub fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace(path, bytes, 0o755)
}

/// `mode` is the Unix permission bits of the new file; Windows has none to set.
fn replace(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let mut f = Atomic::create(path, mode)?;
    f.write_all(bytes)?;
    f.commit()
}

/// A file written in pieces (a trace streamed from the target, a heap snapshot), whole or not at
/// all, as `write_atomic` writes one at once: into a temporary file beside `path` until `commit`
/// flushes it to disk and renames it over `path`. Dropped without `commit` (an error midway), the
/// temporary file is removed and `path` stays as it was.
pub struct Atomic {
    file: Option<std::fs::File>,
    tmp: PathBuf,
    path: PathBuf,
}

impl Atomic {
    /// `mode` is the Unix permission bits of the new file (`0o644`, or `0o600` for what only its
    /// owner may read); Windows has none to set.
    pub fn create(path: &Path, mode: u32) -> std::io::Result<Atomic> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let tmp = dir.join(format!(".{name}.{}-{}.tmp", std::process::id(), csprng_hex(4)?));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let file = o.open(&tmp)?;
        Ok(Atomic { file: Some(file), tmp, path: path.to_path_buf() })
    }

    pub fn commit(mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.take() {
            f.sync_all()?;
        }
        let mut wait = std::time::Duration::from_millis(10);
        let mut tries = 0;
        loop {
            match std::fs::rename(&self.tmp, &self.path) {
                Ok(()) => {
                    self.tmp = PathBuf::new();
                    return Ok(());
                }
                // Sharing violations and access denied: another process holds the file (an antivirus).
                Err(e) if cfg!(windows) && tries < 8 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                    tries += 1;
                    std::thread::sleep(wait);
                    wait *= 2;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn file(&mut self) -> std::io::Result<&mut std::fs::File> {
        self.file.as_mut().ok_or_else(|| std::io::Error::other("the file was committed"))
    }
}

impl Write for Atomic {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file()?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file()?.flush()
    }
}

impl Drop for Atomic {
    fn drop(&mut self) {
        self.file.take();
        if !self.tmp.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.tmp);
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
