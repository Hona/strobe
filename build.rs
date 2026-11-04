// The commit a build comes from, for `strobe 0.6.0 (a1b2c3d4e5)` in reports: Cargo.toml's version alone
// does not tell a build from main after a release from the release. Empty outside a git checkout.
//
// And the build's identity (`STROBE_BUILD_ID`): a hash of every source file that goes into the
// binaries, the target and the profile. `strobe` and `strobew` come from one run of this script, so
// they carry the same id, where a hash of each executable differed and a daemon (strobew) never
// matched the CLI (strobe) that installed it.
use std::path::Path;
use std::process::Command;

const SOURCES: &[&str] = &["src", "sdk/bench/src", "viewer", "skill", "calibration", "Cargo.toml", "Cargo.lock", "build.rs"];

/// FNV-1a, 64 bits: stable across Rust releases, unlike the standard library's hasher.
fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

fn walk(path: &Path, files: &mut Vec<std::path::PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            walk(&e, files);
        }
    } else if path.is_file() {
        files.push(path.to_path_buf());
    }
}

fn main() {
    let git = |args: &[&str]| Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=STROBE_COMMIT={commit}");
    // Rebuild when HEAD moves: a commit, a checkout or a rebase writes the worktree's HEAD log.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    let mut files = Vec::new();
    for s in SOURCES {
        walk(Path::new(s), &mut files);
        println!("cargo:rerun-if-changed={s}");
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for f in &files {
        fnv(&mut h, f.to_string_lossy().replace('\\', "/").as_bytes());
        fnv(&mut h, &std::fs::read(f).unwrap_or_default());
    }
    for k in ["TARGET", "PROFILE", "CARGO_PKG_VERSION"] {
        fnv(&mut h, std::env::var(k).unwrap_or_default().as_bytes());
    }
    println!("cargo:rustc-env=STROBE_BUILD_ID={h:016x}");
}
