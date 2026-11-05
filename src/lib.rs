//! strobe: measure and explain performance (SPEC.md is the source of truth).

pub mod app;
pub mod cli;
pub mod config;
pub mod driver;
pub mod engine;
pub mod etw;
pub mod fmt;
pub mod heapsnap;
pub mod history;
pub mod host;
pub mod metrics;
pub mod model;
pub mod perfetto;
pub mod png;
pub mod probe;
pub mod runner;
pub mod stats;
pub mod store;
pub mod symbol;
pub mod target;
pub mod validity;
