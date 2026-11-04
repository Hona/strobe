// The commit a build comes from, for `strobe 0.6.0 (a1b2c3d4e5)` in reports: Cargo.toml's version alone
// does not tell a build from main after a release from the release. Empty outside a git checkout.
use std::process::Command;

fn main() {
    let git = |args: &[&str]| Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=STROBE_COMMIT={commit}");
    // Rebuild when HEAD moves: a commit, a checkout or a rebase writes the worktree's HEAD log.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
