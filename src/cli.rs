//! Argument parsing (hand-rolled, like mesh) and help text.

use crate::model::{Error, Result};
use std::collections::HashMap;

/// How a flag takes its value. `Number` is a value that MCP tool schemas type as a number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Switch,
    Value,
    Number,
}

use Kind::{Number, Switch, Value};

/// Every flag strobe reads and how it takes its value. The parser refuses any other flag (a typo is an
/// error, not a silently ignored setting), and MCP tool schemas take their types from here (SPEC §17),
/// so the two cannot drift. A new flag goes here.
pub const FLAGS: &[(&str, Kind)] = &[
    // Global.
    ("json", Switch),
    ("quiet", Switch),
    ("verbose", Switch),
    ("help", Switch),
    ("version", Switch),
    ("progress", Value),
    // record, bench, plan, run.
    ("id", Value),
    ("for", Value),
    ("probes", Value),
    ("headless", Switch),
    ("served", Value),
    ("style", Switch),
    ("native", Switch),
    ("app", Value),
    ("home", Value),
    ("os-frames", Switch),
    ("bench", Value),
    ("gpu-cache", Value),
    ("until-line", Value),
    ("stop-line", Value),
    ("head", Value),
    ("base", Value),
    ("aa", Switch),
    ("check", Switch),
    ("filter", Value),
    ("target", Value),
    ("params", Value),
    ("primary", Value),
    ("equivalence", Number),
    ("noworse", Value),
    ("pairs", Value),
    ("runs", Number),
    ("repeat", Number),
    ("why", Value),
    ("trace", Value),
    ("power", Value),
    ("pin", Value),
    ("reserve-cpus", Value),
    ("pixels", Switch),
    ("pixels-only", Switch),
    ("bisect", Value),
    ("keep", Switch),
    ("max-time", Value),
    ("unlock-wait", Value),
    ("require-verdict", Switch),
    ("plan", Switch),
    ("effect", Number),
    ("pilot", Switch),
    ("stop", Value),
    ("resume", Value),
    ("gpu-store", Value),
    // query, report, open, history.
    ("by", Value),
    ("top", Number),
    ("reveal", Switch),
    ("at", Value),
    ("thread", Value),
    ("phase", Value),
    ("timeline", Switch),
    ("format", Value),
    ("out", Value),
    ("open", Switch),
    ("update-pr", Number),
    ("repo", Value),
    ("force", Switch),
    ("no-card", Switch),
    ("theme", Value),
    ("badge-url", Switch),
    ("no-browser", Switch),
    // track.
    ("ref", Value),
    ("range", Value),
    ("every", Number),
    ("blocks", Number),
    ("again", Switch),
    ("confirm", Switch),
    ("no-confirm", Switch),
    ("alert", Value),
    ("alert-command", Value),
    ("alert-webhook", Value),
    ("schedule", Value),
    ("schedules", Switch),
    ("unschedule", Value),
    ("from-schedule", Value),
    // look.
    ("until", Value),
    ("wait", Value),
    ("size", Value),
    ("scale", Number),
    ("chars", Number),
    ("keys", Number),
    ("full", Switch),
    ("headed", Switch),
    ("timeout", Value),
    // status, top.
    ("calibrate", Switch),
    ("fast", Switch),
    ("once", Switch),
    ("interval", Value),
    ("limit", Number),
    ("sort", Value),
    // up, update, leave, helper.
    ("skip", Value),
    ("yes", Switch),
    ("from", Value),
    ("log", Value),
    ("sid", Value),
];

/// How `--name` takes its value, when strobe has such a flag.
pub fn kind(name: &str) -> Option<Kind> {
    FLAGS.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
}

/// The refusal for a flag strobe does not have, naming the closest one.
pub fn unknown_flag(name: &str) -> Error {
    let distance = |a: &str, b: &str| {
        let b: Vec<char> = b.chars().collect();
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let cur = row[j + 1];
                row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
                prev = cur;
            }
        }
        row[b.len()]
    };
    let near = FLAGS.iter().map(|(n, _)| (distance(name, n), *n)).filter(|(d, n)| *d <= 1 + name.len() / 4 || (name.len() >= 3 && n.starts_with(name))).min();
    let e = Error::usage(format!("unknown flag --{name}"));
    match near {
        Some((_, n)) => e.hint(format!("did you mean --{n}? strobe --help lists the flags")),
        None => e.hint("strobe --help lists the flags"),
    }
}

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
                // As gh names a repository (`report … --update-pr 12 -R owner/repo`).
                "-R" => Some("repo".to_string()),
                a if a.starts_with("--") && a.len() > 2 => Some(a[2..].to_string()),
                // A short flag strobe does not have (a negative number stays an argument).
                a if a.len() > 1 && a.starts_with('-') && !a[1..].starts_with(|c: char| c.is_ascii_digit() || c == '.') => {
                    return Err(Error::usage(format!("unknown flag {a}")).hint("short flags: -h -V -q -v -R; strobe --help lists the rest"));
                }
                _ => None,
            };
            let Some(name) = name else {
                pos.push(arg);
                continue;
            };
            if let Some((key, value)) = name.split_once('=') {
                match (kind(key).ok_or_else(|| unknown_flag(key))?, value) {
                    // `--check=false` is the switch off, not on.
                    (Switch, "true") => flags.insert(key.to_string(), "true".to_string()),
                    (Switch, "false") => flags.remove(key),
                    (Switch, _) => return Err(Error::usage(format!("--{key} is a switch: --{key}, --{key}=true or --{key}=false, not --{key}={value}"))),
                    _ => flags.insert(key.to_string(), value.to_string()),
                };
                continue;
            }
            if kind(&name).ok_or_else(|| unknown_flag(&name))? == Switch {
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

/// Parses `500ms`, `10s`, `2m`, `2h`, `1d`, or plain seconds; a negative or non-finite one is refused.
pub fn duration_ms(text: &str) -> Result<u64> {
    let t = text.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000.0)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60_000.0)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3_600_000.0)
    } else if let Some(n) = t.strip_suffix('d') {
        (n, 86_400_000.0)
    } else {
        (t, 1000.0)
    };
    match num.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok((v * mult) as u64),
        _ => Err(Error::usage(format!("not a duration: {text} (500ms, 10s, 2m, 2h, 1d)"))),
    }
}

pub const HELP: &str = "strobe — measure and explain performance

Usage: strobe <command> [flags]

Use
  status                          environment, validity preflight, recent recordings and experiments
  record <target> [--for 10s] [--probes timing|attribution|memory] [--headless] [--served <dir>] [--style]
                                  record a target; prints a handle (rec_…); exe:<path> --native for a native app
                                  (a native app: --gpu-cache warm|cold, its GPU shader caches from earlier recordings of it)
  record --bench <file.bench.ts>#<case> [--head <variant>] [--target <name>]
                                  record one bench case (it drives the page: clicks, keys) with the attribution probes
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
        [--noworse 2% | metric=2%,…]   (non-inferiority: is head at most 2% worse? verdicts no worse, worse, unclear)
        [--unlock-wait 2h]             (a series that needs a presenting window waits at most that long for an unlock)
  plan <file.bench.ts> [--base <variant>] [--head <variant>] [--filter …] [--effect <pct>] [--pilot]
                                  before a bench: pairs and minutes per series from earlier runs here, builds, the slot's
                                  queue (= bench --plan; --pilot runs 4 pairs first to learn the spread)
  bench --stop <exp> | --resume <exp>
                                  stop a running bench after its block (at once while it waits; it keeps what finished),
                                  or go on with one (a unique prefix of the id works)
        --filter: case names, comma-separated; exact unless a name holds * (a glob)
                                  run an experiment; a verdict per series
  query <rec_…|exp_…|rec_a..rec_b> <view> [--top N] [--by function|file|package] [--reveal] [--native] [--thread <name>]
                                  views: summary frames threads gpu hotspots alloc layout style shifts gc heap leaks memory insights sql
                                  (a..b: heap memory; exp_…: summary progress insights; memory: --by process|layer|allocator --at <i>;
                                  threads --timeline: each thread's CPU per second from the launch, exited ones too)
  report experiment|recording <handle> [--format html|md|pr] [--out <path>] [--open]
                                  write a self-contained HTML (or markdown) report; pr: an experiment's tables for a PR body
                                  (md and pr print the markdown; the file's path goes to stderr)
  report experiment <exp> --format pr --update-pr <n> [-R owner/repo] [--force] [--no-card]
                                  put the block in PR n's body, with the result card and badges attached through gh
  report experiment <exp> --format card|badge [--out <file.png|file.svg>] [--theme light|dark|auto] [--badge-url]
                                  the result as an image: card 1200×630 (decided wins large, else what happened), badge one
                                  line; SVG follows the reader's colour scheme (auto); --badge-url prints a shields.io URL
  open <rec_…|exp_…[/head|base|<block>|<case>]> [--no-browser]
                                  the recording in the Perfetto UI (local): one trace with the Chrome trace, CPU profiles
                                  and native stacks as flame graphs, runs, phases, inputs, presents, threads, memory
                                  (--no-browser: print the URL instead of opening it)
  track <file.bench.ts> [--ref <rev> | --range a..b [--every N]] [--filter …] [--blocks 6] [--no-confirm]
        [--alert-command '<cmd>'] [--alert-webhook <url>] [--alert regressions|all]
                                  a point per series per commit in this device's history; change points are confirmed
                                  by a paired A/B of the commits around them, then bisected; only confirmed ones alert
  track <file> --ref <branch> --schedule commits|nightly|6h    the daemon tracks the branch (--schedules, --unschedule <id>)
  track --confirm [<ser_…|chg_…>] confirm suspected changes (or one again)
  history [<ser_…|file>]          tracked series: points with intervals, a sparkline, changes and culprits
                                  (query history|changes, query <ser_…> history|changes; report history <ser_…|file>)
  look <url|file> [--until <css>] [--wait 1s] [--size 1280x800] [--out <png>] [--chars 4000] [--full] [--headed]
  look app:<id> | exe:<path> [--app <id>] [--until <selector>] [--wait 2s] [--keys 40]
                                  a page in a headless browser, nothing measured: title, address, headings, visible text,
                                  console messages and uncaught errors, and a screenshot (exit 124 when it never loads)
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
