//! Where strobe keeps its state (SPEC §4.2).

use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    let dir = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(super::home).join("strobe")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| super::home().join(".local/share")).join("strobe")
    };
    let _ = std::fs::create_dir_all(&dir);
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

pub fn run() -> PathBuf {
    sub("run")
}

pub fn sdk() -> PathBuf {
    sub("sdk")
}
