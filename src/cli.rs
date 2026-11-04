//! Argument parsing (hand-rolled, like mesh) and help text.

use crate::model::{Error, Result};
use std::collections::HashMap;

/// Flags that take no value.
const SWITCHES: &[&str] = &[
    "json",
    "quiet",
    "verbose",
    "help",
    "version",
    "aa",
    "reveal",
    "require-verdict",
    "check",
    "yes",
    "open",
    "calibrate",
    "pin-runner",
    "headless",
    "keep",
    "fast",
    "once",
    "pixels",
    "pixels-only",
    "native",
    "os-frames",
    "style",
    "force",
    "timeline",
    "perfetto",
    "no-browser",
];

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
            // `--` ends strobe's flags: the rest is a command line of its own (`strobe run -- …`).
            if arg == "--" {
                pos.push(arg);
                pos.extend(iter.by_ref());
                break;
            }
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

    /// The command line as JSON, kept with an experiment so `--resume` runs it again.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "command": self.command, "pos": self.pos, "flags": self.flags })
    }

    pub fn from_json(v: &serde_json::Value) -> Option<Args> {
        Some(Args { command: v["command"].as_str()?.to_string(), pos: serde_json::from_value(v["pos"].clone()).ok()?, flags: serde_json::from_value(v["flags"].clone()).ok()? })
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
  record <target> [--for 10s] [--probes timing|attribution|memory] [--headless] [--served <dir>] [--style]
                                  record a target; prints a handle (rec_…); exe:<path> --native for a native app
  record --bench <file.bench.ts>#<case> [--head <variant>]
                                  record one bench case (it drives the page: clicks, keys) with the attribution probes
                                  (a native app: --gpu-cache warm|cold, its GPU shader caches from earlier recordings of it)
  record pid:<n>|proc:<name> [--probes timing|deep|stacks-unprivileged] [--for 10s]
                                  any process from outside: OS present events (frames), CPU, GPU; deep: native stacks
                                  (--os-frames on a Chromium record adds the OS present events beside its trace)
  bench <file.bench.ts> --head <variant> [--base <variant> | --aa | --check] [--filter <cases>] [--pairs auto|N]
        [--why auto|always|never] [--trace lean|full] [--power high|keep] [--pin auto|none|ccdN|<cpus>]
        [--params k=v,…|all]           (all: every parameter's `more` values too)
        [--probes memory --repeat N]   (a leak run per variant instead of pairs)
        [--pixels | --pixels-only]     (compare base and head at the case's page.snapshot()s, after or instead of timing)
        [--bisect good..bad]           (the commit that moved the primary; one series)
        [--style]                      (attribution blocks also trace what invalidated style: query <rec> style)
  bench --stop <exp> | --resume <exp>
                                  stop a running bench after its block (it keeps what finished), or go on with one
        --filter: case names, comma-separated; exact unless a name holds * (a glob)
                                  run an experiment; a verdict per series
  query <rec_…|exp_…|rec_a..rec_b> <view> [--top N] [--by function|file|package] [--reveal] [--native] [--thread <name>]
                                  views: summary frames threads gpu hotspots alloc layout style shifts gc heap leaks memory insights sql
                                  (a..b: heap memory; exp_…: summary progress insights; memory: --by process|layer|allocator --at <i>;
                                  threads --timeline: each thread's CPU per second from the launch, exited ones too)
  report experiment|recording <handle> [--format html|md|pr] [--out <path>] [--open]
                                  write a self-contained HTML (or markdown) report; pr: an experiment's tables for a PR body
                                  (md and pr print the markdown; the file's path goes to stderr)
  open <rec_…|exp_…[/head|base|<block>|<case>]> [--perfetto] [--no-browser]
                                  the recording in the Perfetto UI (local): one trace with the Chrome trace, CPU profiles
                                  and native stacks as flame graphs, runs, phases, inputs, presents, threads, memory
  top [--target <pid>] [--once]   live per-core load by CCD, processes with CPU, GPU, VRAM
  run [--pin auto|none|ccdN|<cpus>] [--reserve-cpus on|off] [--power high|keep] -- <command> [args…]
                                  run your own harness as a bench block runs: the bench slot, the fastest power plan,
                                  pinned to the target's CPUs with other processes kept off them; it prints its own numbers

Setup
  up [--skip deep,daemon,mcp,calibrate]  install, download the engine, deep mode (one UAC prompt), start the daemon,
                                  register MCP and the skill
  update [<version>] [--from <path>] [--check] [--force]
                                  install the latest npm release (or that version, or a local build) over the installed
                                  copy; refreshes the skill, MCP, the helper where installed, and restarts the daemon;
                                  keeps a newer or local build unless --force
  leave [--yes]                   undo up (--yes also deletes recordings and experiments)

Targets   https://… · app:<id> (a strobe.json profile, Electron or native) · exe:<path> [--app <id> | --native] · cdp:<port> · cdp:ws://… · file:<path>
          · pid:<n> · proc:<name>
Variants  a directory · a git ref (git:<ref>) · app:<id> · exe:<path> · url:<href>
Global    --json  -q  -h  -V  --progress stderr|stdout|off (stdout for shells that count stderr lines as a failure)
Exit codes: 0 ok · 1 error · 2 usage · 3 needs a human · 4 refused (invalid) · 5 inconclusive · 124 timeout · 255 unreachable
";
