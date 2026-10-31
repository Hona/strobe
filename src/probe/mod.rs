//! Collecting data from a target (SPEC §8).

pub mod cpu;
pub mod trace;

/// Named probe sets (SPEC §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Set {
    Timing,
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
