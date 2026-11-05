//! Collecting data from a target (SPEC §8).

pub mod alloc;
pub mod census;
pub mod cpu;
#[cfg(windows)]
pub mod etw;
pub mod gpu;
#[cfg(target_os = "linux")]
pub mod gpu_linux;
#[cfg(target_os = "macos")]
pub mod gpu_macos;
pub mod heap;
#[cfg(windows)]
pub mod helper;
#[cfg(windows)]
pub mod kernel_profile;
pub mod memory;
pub mod os;
#[cfg(target_os = "linux")]
pub mod perf;
pub mod present;
#[cfg(target_os = "macos")]
pub mod sample;
pub mod shifts;
pub mod stacks;
#[cfg(windows)]
pub mod suspend;
pub mod symbols;
pub mod trace;
pub mod wakes;

/// Joins a capture's thread once it has been told to stop, waiting at most `wait`: a stop or a drop
/// never hangs on a thread that does not end. None when it panicked or is still running (then it is
/// left to end by itself).
pub fn join_within<T>(thread: std::thread::JoinHandle<T>, wait: std::time::Duration) -> Option<T> {
    let deadline = std::time::Instant::now() + wait;
    while !thread.is_finished() {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    thread.join().ok()
}

/// Named probe sets (SPEC §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Set {
    Timing,
    /// Bench timing blocks: the trace holds only what metrics read (SPEC §8.2).
    Lean,
    Attribution,
    Memory,
}

impl Set {
    pub fn parse(s: &str) -> Option<Set> {
        match s {
            "timing" => Some(Set::Timing),
            "attribution" => Some(Set::Attribution),
            "memory" => Some(Set::Memory),
            _ => None,
        }
    }
}
