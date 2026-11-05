//! The git questions `bench`, `bisect`, `track` and their reports ask of a checkout: one place that
//! runs `git -C <root>`, so each asks the same way and fails the same way.

use crate::model::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).stdin(Stdio::null());
    c
}

/// `git <args>` in `root`: its stdout, or an error with git's own message.
pub fn run(root: &Path, args: &[&str]) -> Result<String> {
    let out = git(root).args(args).output()?;
    if !out.status.success() {
        return Err(Error::msg(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `git <args>` in `root`, trimmed, when it succeeds.
pub fn out(root: &Path, args: &[&str]) -> Option<String> {
    super::output(git(root).args(args))
}

/// `git <args>` in `root`, its stdout byte for byte (a binary diff), whatever it exits with.
pub fn bytes(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    git(root).args(args).output().ok().map(|o| o.stdout)
}

/// Whether `git <args>` in `root` succeeds; its output goes nowhere.
pub fn ok(root: &Path, args: &[&str]) -> bool {
    git(root).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The full commit `rev` names in `root` (`git:<rev>` as variants write it).
pub fn commit(root: &Path, rev: &str) -> Option<String> {
    let rev = rev.strip_prefix("git:").unwrap_or(rev);
    out(root, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
}

/// The commit's subject line, or empty.
pub fn subject(root: &Path, rev: &str) -> String {
    out(root, &["log", "-1", "--format=%s", rev]).unwrap_or_default()
}

/// The commit's time (seconds since the epoch), or 0.
pub fn commit_time(root: &Path, rev: &str) -> u64 {
    out(root, &["log", "-1", "--format=%ct", rev]).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// The top of the checkout `dir` is in.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    out(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// A commit as people read it: its first 10 hex digits.
pub fn short(rev: &str) -> String {
    rev.chars().take(10).collect()
}
