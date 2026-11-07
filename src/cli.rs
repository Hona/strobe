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

/// Every flag strobe reads, how it takes its value, and the commands that read it (`*`: every
/// command), which `strobe <command> --help` lists. The parser refuses any other flag (a typo is an
/// error, not a silently ignored setting), and MCP tool schemas take their types from here (SPEC §17),
/// so the two cannot drift. A new flag goes here.
pub const FLAGS: &[(&str, Kind, &str)] = &[
    // Global.
    ("json", Switch, "*"),
    ("quiet", Switch, "*"),
    ("verbose", Switch, "*"),
    ("help", Switch, "*"),
    ("version", Switch, "*"),
    ("progress", Value, "*"),
    // record, bench, plan, run.
    ("id", Value, "record bench run"),
    ("for", Value, "record"),
    ("probes", Value, "record bench plan"),
    ("headless", Switch, "record bench plan track"),
    ("engine", Value, "record bench plan track"),
    ("served", Value, "record"),
    ("emulate", Value, "record"),
    ("style", Switch, "record bench"),
    ("native", Switch, "record query"),
    ("app", Value, "record look"),
    ("home", Value, "record look"),
    ("os-frames", Switch, "record"),
    ("video", Switch, "record bench"),
    ("bench", Value, "record"),
    ("gpu-cache", Value, "record"),
    ("until-line", Value, "record"),
    ("stop-line", Value, "record"),
    ("head", Value, "record bench plan track run"),
    ("base", Value, "bench plan track run"),
    ("aa", Switch, "bench plan run"),
    ("check", Switch, "bench update run"),
    ("filter", Value, "bench plan track"),
    ("target", Value, "record bench top track"),
    ("params", Value, "record bench track"),
    ("primary", Value, "bench track run"),
    ("equivalence", Number, "bench plan track run"),
    ("noworse", Value, "bench plan run"),
    ("pairs", Value, "bench plan run track"),
    ("runs", Number, "record bench plan track run"),
    ("repeat", Number, "bench"),
    ("why", Value, "bench plan track"),
    ("trace", Value, "bench track"),
    ("power", Value, "record bench run track"),
    ("pin", Value, "record bench run track"),
    ("reserve-cpus", Value, "bench run track"),
    ("pixels", Switch, "bench"),
    ("pixels-only", Switch, "bench"),
    ("bisect", Value, "bench track"),
    ("keep", Switch, "bench"),
    ("max-time", Value, "bench run"),
    ("unlock-wait", Value, "bench"),
    ("require-verdict", Switch, "bench run"),
    ("require-parity", Switch, "bench"),
    ("plan", Switch, "bench"),
    ("effect", Number, "bench plan"),
    ("pilot", Switch, "bench plan"),
    ("stop", Value, "bench"),
    ("resume", Value, "bench"),
    ("gpu-store", Value, ""),
    ("metrics", Value, "run"),
    ("seed", Number, "record bench plan run track"),
    // query, report, open, history.
    ("by", Value, "query"),
    ("top", Number, "query history"),
    ("reveal", Switch, "query"),
    ("at", Value, "query"),
    ("thread", Value, "query"),
    ("phase", Value, "query"),
    ("timeline", Switch, "query"),
    ("format", Value, "report"),
    ("out", Value, "report look history"),
    ("open", Switch, "report"),
    ("update-pr", Number, "report"),
    ("repo", Value, "report"),
    ("force", Switch, "report update"),
    ("no-card", Switch, "report"),
    ("theme", Value, "report"),
    ("badge-url", Switch, "report"),
    ("scoreboard", Switch, "report"),
    ("no-browser", Switch, "open"),
    // track.
    ("ref", Value, "track"),
    ("range", Value, "track"),
    ("every", Number, "track"),
    ("blocks", Number, "bench track"),
    ("again", Switch, "track"),
    ("confirm", Switch, "track"),
    ("no-confirm", Switch, "track"),
    ("alert", Value, "track"),
    ("alert-command", Value, "track"),
    ("alert-webhook", Value, "track"),
    ("schedule", Value, "track"),
    ("schedules", Switch, "track"),
    ("unschedule", Value, "track"),
    ("from-schedule", Value, "track"),
    // look.
    ("until", Value, "look"),
    ("wait", Value, "look"),
    ("size", Value, "look"),
    ("scale", Number, "look"),
    ("chars", Number, "look"),
    ("keys", Number, "look"),
    ("full", Switch, "look"),
    ("element", Value, "look"),
    ("headed", Switch, "look"),
    ("build", Switch, "look"),
    ("timeout", Value, "look"),
    // status, top.
    ("calibrate", Switch, "status"),
    ("fast", Switch, "status"),
    ("once", Switch, "top"),
    ("interval", Value, "top"),
    ("limit", Number, "top"),
    ("sort", Value, "top"),
    // up, update, leave, helper.
    ("skip", Value, "up"),
    ("yes", Switch, "leave"),
    ("from", Value, "update"),
    ("log", Value, "helper"),
    ("sid", Value, "helper"),
];

/// How `--name` takes its value, when strobe has such a flag.
pub fn kind(name: &str) -> Option<Kind> {
    FLAGS.iter().find(|(n, _, _)| *n == name).map(|(_, k, _)| *k)
}

/// `strobe <command> --help`: that command's lines of the help, then every flag it reads (from
/// `FLAGS`). A command the help does not name gets the whole help.
pub fn help(command: &str) -> String {
    let mut lines = Vec::new();
    let mut taking = false;
    for l in HELP.lines() {
        let indent = l.len() - l.trim_start().len();
        if indent == 2 {
            taking = l[2..].split([' ', '\t']).next() == Some(command);
        } else if indent < 2 {
            taking = false;
        }
        if taking {
            lines.push(l);
        }
    }
    if lines.is_empty() {
        return HELP.to_string();
    }
    let mut out = format!("strobe {command}\n\n{}\n\nFlags\n", lines.join("\n"));
    let mut row = String::from(" ");
    for (name, kind, _) in FLAGS.iter().filter(|(_, _, c)| c.split(' ').any(|c| c == command)) {
        let flag = format!(" --{name}{}", if *kind == Switch { "" } else { " <value>" });
        if row.len() + flag.len() > 100 {
            out += &format!("{row}\n");
            row = String::from(" ");
        }
        row += &flag;
    }
    out += &format!("{row}\n");
    out += "Global    --json  -q  -h  -V  --progress stderr|stdout|off\n";
    out
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
    let near = FLAGS.iter().map(|(n, _, _)| (distance(name, n), *n)).filter(|(d, n)| *d <= 1 + name.len() / 4 || (name.len() >= 3 && n.starts_with(name))).min();
    let e = Error::usage(format!("unknown flag --{name}"));
    match near {
        Some((_, n)) => e.hint(format!("did you mean --{n}? strobe <command> --help lists a command's flags")),
        None => e.hint("strobe <command> --help lists a command's flags"),
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

/// Parses `500ms`, `10s`, `2m`, `2h`, `1d` (and a bare `0`); a negative or non-finite one, or a number
/// without its unit, is refused: `--for 2000` meant as milliseconds once recorded for 33 minutes.
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
        if t.parse::<f64>().is_ok_and(|v| v != 0.0) {
            return Err(Error::usage(format!("not a duration: {text}; give a unit: {text}ms or {text}s")));
        }
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
  record <target> [--for 10s] [--probes timing|attribution|memory] [--headless] [--emulate mobile|touch] [--served <dir>] [--style]
                                  record a target; prints a handle (rec_…); exe:<path> --native for a native app
                                  (a native app: --gpu-cache warm|cold, its GPU shader caches from earlier recordings of it)
                                  (--video: also film it, timing perturbed: the window's screen, or a headless page's renderer)
  record --bench <file.bench.ts>#<case> [--head <variant>] [--target <name>] [--video]
                                  record one bench case (it drives the page: clicks, keys) with the attribution probes
  record pid:<n>|proc:<name> [--probes timing|deep|stacks-unprivileged] [--for 10s]
                                  any process from outside: OS present events (frames), CPU, GPU; deep: native stacks
                                  (--os-frames on a Chromium record adds the OS present events beside its trace)
  bench <file.bench.ts> --head <variant> [--base <variant> | --aa | --check | --blocks N] [--filter <cases>] [--pairs auto|N]
        [--why auto|always|never] [--trace lean|full] [--power high|keep]
        [--params k=v,…|all]           (all: every parameter's `more` values too)
        [--probes memory --repeat N]   (a leak run per variant instead of pairs)
        [--pixels | --pixels-only]     (compare base and head at the case's page.snapshot()s, after or instead of timing)
        [--video]                      (film untimed: the check run, or one warmed run per side after the timed pairs)
        [--pin auto|none|ccdN|<cpus>]  (auto: the V-Cache CCD; equal CCDs: the first; a hybrid CPU: its fastest cores;
                                        otherwise all but the last 1-2 whole cores, which strobe and the runner get;
                                        fewer than 4 cores, more than 64 CPUs, or macOS: unpinned)
        [--reserve-cpus auto|on|off]   (keep this user's other processes off the target's CPUs: timed blocks, every
                                        block, or never)
        [--bisect good..bad]           (the commit that moved the primary; one series)
        [--style]                      (attribution blocks also trace what invalidated style: query <rec> style)
        [--noworse 2% | metric=2%,…]   (non-inferiority: is head at most 2% worse? verdicts no worse, worse, unclear)
        [--unlock-wait 2h]             (a series that needs a presenting window waits at most that long for an unlock)
        [--seed <n>]                   (ctx.seed and ctx.random(): replay a seeded workload; without it strobe draws one
                                        and prints it)
        --filter: case names, comma-separated; exact unless a name holds * (a glob)
        --base <target>[=<variant>] --head <target>[=<variant>]: two targets of the file compared (two
                                  implementations of one app); target:<name> when a folder or ref has that name
                                  run an experiment; a verdict per series
  plan <file.bench.ts> [--base <variant>] [--head <variant>] [--filter …] [--effect <pct>] [--pilot]
                                  before a bench: pairs and minutes per series from earlier runs here, builds, the slot's
                                  queue (= bench --plan; --pilot runs 4 pairs first to learn the spread)
  bench --stop <exp> | --resume <exp>
                                  stop a running bench after its block (at once while it waits; it keeps what finished),
                                  or go on with one (a unique prefix of the id works)
  config [<dir>]                  check the project's strobe.json (and your own) without a bench: where it is, each
                                  app profile resolved for this OS (its executable, guards, window), build and serve
  query <rec_…|exp_…|rec_a..rec_b> <view> [--top N] [--by function|file|package] [--reveal] [--native] [--thread <name>]
                                  views: summary frames threads gpu hotspots waits alloc layout style shifts gc heap leaks memory insights
                                  actions sql
                                  (actions: what a bench block's hooks sent the target, in order; --phase; --reveal shows
                                  typed text, act values, ctx.call arguments and ctx.exec lines)
                                  (waits: where threads were off their CPUs and who woke them, from a native capture;
                                  --thread <name> --phase <name> --by function|file|package)
                                  (a..b: hotspots alloc layout style threads frames summary heap memory, per-run deltas;
                                  exp_…: summary progress insights, and those views as each series' base..head;
                                  memory: --by process|layer|allocator --at <i>;
                                  threads --timeline: each thread's CPU per second from the launch, exited ones too)
  report experiment|recording <handle> [--format html|md|pr] [--out <path>] [--open]
                                  write a self-contained HTML (or markdown) report; pr: an experiment's tables for a PR body
                                  (md and pr print the markdown; the file's path goes to stderr)
  report experiment <exp> --format pr --update-pr <n> [-R owner/repo] [--force] [--no-card]
                                  put the block in PR n's body, with the result card and badges attached through gh
  report experiment <exp> --format card|badge [--out <file.png|file.svg>] [--theme light|dark|auto] [--badge-url]
                                  [--scoreboard]
                                  the result as an image: card 1200×630 (decided wins large, else what happened), badge one
                                  line; SVG follows the reader's colour scheme (auto); --badge-url prints a shields.io URL;
                                  --scoreboard: an implementation comparison's journeys (vs <base> | better in 4/5 · 1 gap)
  open <rec_…|exp_…[/head|base|<block>|<case>]> [--no-browser]
                                  the recording in the Perfetto UI (local): one trace with the Chrome trace, CPU profiles
                                  and native stacks as flame graphs, runs, phases, inputs, presents, threads, memory
                                  (--no-browser: print the URL instead of opening it)
  track <file.bench.ts> [--ref <rev> | --range a..b [--every N]] [--filter …] [--blocks 6] [--no-confirm]
        [--alert-command '<cmd>'] [--alert-webhook <url>] [--alert regressions|all]
                                  a point per series per commit in this device's history; change points are confirmed
                                  by a paired A/B of the commits around them, then bisected; only confirmed ones alert
  track <file> --ref <branch> --schedule commits|nightly|6h    the daemon tracks the branch (--schedules, --unschedule <id>)
  track <file> --base <target>[=<variant>] --head <target>=git:<ref> [--range a..b] [--pairs 3] [--schedule …]
                                  a port: per head commit, a few alternating pairs of both implementations, ln(head/base)
                                  per journey; a move is confirmed by an A/B of the head alone and bisected; a comparator
                                  that changed is an epoch, never an alert; journeys going gap -> compared are marked
  track --confirm [<ser_…|chg_…>] confirm suspected changes (or one again)
  history [<ser_…|file>]          tracked series: points with intervals, a sparkline, changes and culprits; a port's
                                  journeys x commits (query history|changes, query <ser_…> history|changes;
                                  report history <ser_…|file> [--format card | --format badge [--scoreboard]])
  look <url|file> [--until <css>] [--wait 1s] [--size 1280x800] [--out <png>] [--chars 4000] [--full | --element <css>] [--headed]
  look app:<id> | exe:<path> [--app <id>] [--build] [--until <selector>] [--wait 2s] [--keys 40]
                                  a page in a headless browser, nothing measured: title, address, headings, visible text,
                                  console messages and uncaught errors, and a screenshot (exit 124 when it never loads)
  top [--target <pid>] [--once]   live per-core load by CCD, processes with CPU, GPU, VRAM
  run [--pin auto|none|ccdN|<cpus>] [--reserve-cpus on|off] [--power high|keep] -- <command> [args…]
                                  run your own harness as a bench block runs: the bench slot, the fastest power plan,
                                  pinned to the target's CPUs with other processes kept off them (--pin as for bench);
                                  it prints its own numbers
  run --base \"<command>\" --head \"<command>\" [--aa] [--check] [--primary cpu.work] [--metrics metric.<name>,…] [--pairs auto|N]
      [--runs 5] [--noworse 2%] [--pin …] [--reserve-cpus auto|on|off] [--power high|keep] [--require-verdict]
                                  a paired A/B of two commands, as a bench (verdicts, report, PR block): each run starts the
                                  command and waits for its exit; cpu.work, run.duration, cli.launch, cpu.wakes, memory.peak, io.*, and
                                  metric.<name> from `strobe:metric <name>=<value>` lines it prints (--aa --head: an A/A)

Setup
  up [--skip deep,daemon,mcp,calibrate]  install, download the engine, deep mode (one UAC prompt), start the daemon,
                                  register MCP and the skill
  update [<version>] [--from <path>] [--check] [--force]
                                  install the latest npm release (or that version, or a local build) over the installed
                                  copy; refreshes the skill, MCP, the helper where installed, and restarts the daemon;
                                  keeps a newer or local build unless --force
  leave [--yes]                   undo up (--yes also deletes recordings and experiments)

Targets   https://… · a local page.html (or file:page.html) · app:<id> (a strobe.json profile, Electron or native) · exe:<path> [--app <id> | --native]
          · cdp:<port> · cdp:ws://… · file:<trace> (import) · pid:<n> · proc:<name>
Variants  a directory · a git ref (git:<ref>) · app:<id> · exe:<path> · url:<href> · cmd:<line> (a cli() target's command)
Global    --json  -q  -h  -V  --progress stderr|stdout|off (stdout for shells that count stderr lines as a failure)
Exit codes: 0 ok · 1 error · 2 usage · 3 needs a human · 4 refused (invalid) · 5 inconclusive · 124 timeout · 255 unreachable
";
