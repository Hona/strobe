//! Argument parsing (hand-rolled, like mesh) and help text.

use crate::model::{Error, Result};
use std::collections::HashMap;

/// Flags that take no value.
const SWITCHES: &[&str] = &["json", "quiet", "verbose", "help", "version", "aa", "reveal", "require-verdict", "check", "yes", "open", "calibrate", "pin-runner", "headless", "keep", "fast", "once"];

#[derive(Clone)]
pub struct Args {
    pub command: String,
    pub pos: Vec<String>,
    flags: HashMap<String, String>,
}

impl Args {
    pub fn parse(raw: Vec<String>) -> Result<Args> {
        let mut pos = Vec::new();
        let mut flags = HashMap::new();
        let mut iter = raw.into_iter().peekable();
        while let Some(arg) = iter.next() {
            let name = match arg.as_str() {
                "-h" => Some("help".to_string()),
                "-V" => Some("version".to_string()),
                "-q" => Some("quiet".to_string()),
                "-v" => Some("verbose".to_string()),
                a if a.starts_with("--") && a.len() > 2 => Some(a[2..].to_string()),
                _ => None,
            };
            let Some(name) = name else {
                pos.push(arg);
                continue;
            };
            if let Some((key, value)) = name.split_once('=') {
                flags.insert(key.to_string(), value.to_string());
                continue;
            }
            if SWITCHES.contains(&name.as_str()) {
                flags.insert(name, "true".to_string());
                continue;
            }
            let value = iter.next().ok_or_else(|| Error::usage(format!("--{name} needs a value")))?;
            flags.insert(name, value);
        }
        let command = if pos.is_empty() { String::new() } else { pos.remove(0) };
        Ok(Args { command, pos, flags })
    }

    /// Sets a flag (a derived command line, such as one bisect step).
    pub fn set(&mut self, name: &str, value: &str) {
        self.flags.insert(name.to_string(), value.to_string());
    }

    pub fn remove(&mut self, name: &str) {
        self.flags.remove(name);
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    pub fn num(&self, name: &str) -> Result<Option<f64>> {
        self.get(name).map(|v| v.trim_end_matches('%').parse::<f64>().map_err(|_| Error::usage(format!("--{name}: not a number: {v}")))).transpose()
    }

    pub fn json(&self) -> bool {
        self.has("json")
    }
}

/// Parses `10s`, `500ms`, `2m`, or plain seconds.
pub fn duration_ms(text: &str) -> Result<u64> {
    let t = text.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000.0)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60_000.0)
    } else {
        (t, 1000.0)
    };
    num.parse::<f64>().map(|v| (v * mult) as u64).map_err(|_| Error::usage(format!("not a duration: {text}")))
}

pub const HELP: &str = "strobe — measure and explain performance

Usage: strobe <command> [flags]

Use
  status                          environment, validity preflight, recent recordings and experiments
  record <target> [--for 10s] [--probes timing|attribution|memory] [--headless] [--served <dir>]
                                  record a target; prints a handle (rec_…)
  bench <file.bench.ts> --head <variant> [--base <variant> | --aa] [--filter <case>] [--pairs auto|N]
        [--why auto|always|never] [--trace lean|full] [--power keep|high] [--pin auto|none|ccdN|<cpus>]
        [--probes memory --repeat N]   (a leak run per variant instead of pairs)
        [--bisect good..bad]           (the commit that moved the primary; one series)
                                  run an experiment; a verdict per series
  query <rec_…|exp_…|rec_a..rec_b> <view> [--top N] [--by function|file|package] [--reveal]
                                  views: summary frames threads hotspots alloc layout gc heap leaks memory insights sql
                                  (a..b: heap memory; exp_…: summary progress insights; memory: --by process|layer|allocator --at <i>)
  report experiment|recording <handle> [--format html|md] [--out <path>] [--open]
                                  write a self-contained HTML (or markdown) report
  top [--target <pid>] [--once]   live per-core load by CCD, processes with CPU, GPU, VRAM

Setup
  up [--skip daemon,mcp,calibrate]  install, download the engine, start the daemon, register MCP and the skill
  leave [--yes]                   undo up (--yes also deletes recordings and experiments)

Targets   https://… · app:<id> (a strobe.json profile) · exe:<path> [--app <id>] · cdp:<port> · cdp:ws://… · file:<path>
Variants  a directory · a git ref (git:<ref>) · app:<id> · exe:<path> · url:<href>
Global    --json  -q  -h  -V
Exit codes: 0 ok · 1 error · 2 usage · 3 needs a human · 4 refused (invalid) · 5 inconclusive · 124 timeout · 255 unreachable
";
