//! Collecting data from a target (SPEC §8).

pub mod alloc;
pub mod cpu;
pub mod gpu;
pub mod heap;
pub mod os;
pub mod symbols;
pub mod trace;

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
