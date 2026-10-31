//! Argument parsing (hand-rolled, like mesh) and help text.

use crate::model::{Error, Result};
use std::collections::HashMap;

/// Flags that take no value.
const SWITCHES: &[&str] = &[
    "json", "quiet", "verbose", "help", "version", "aa", "reveal", "require-verdict", "check", "yes", "open",
    "calibrate", "pin-runner", "headless", "keep",
];

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

    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    pub fn num(&self, name: &str) -> Result<Option<f64>> {
        self.get(name)
            .map(|v| v.trim_end_matches('%').parse::<f64>().map_err(|_| Error::usage(format!("--{name}: not a number: {v}"))))
            .transpose()
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
  status [target]                 environment, validity preflight, attachable targets
  record <target> [--for 10s]     record a target; prints a handle (rec_…)
  bench <file.bench.ts> --head <variant> [--base <variant>]
                                  run an experiment; a verdict per series
  query <rec_…|exp_…> <view>      views: summary frames threads sql
  report <view> <handle…>         write a markdown or HTML artifact

Setup
  up                              download the engine, install the skill, check this device

Targets   https://… · cdp:<port>
Variants  a directory · a git ref · url:<href>
Global    --json  -q  -v  -h  -V

Exit codes: 0 ok · 1 error · 2 usage · 3 needs a human · 4 refused (invalid) · 5 inconclusive · 124 timeout · 255 unreachable
";
