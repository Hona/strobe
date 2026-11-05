//! Commands: orchestration only. Each returns an `Output` that the CLI prints and MCP returns (SPEC §17).

pub mod bench;
pub mod bisect;
pub mod card;
pub mod config;
pub mod fmt;
pub mod helper;
pub mod history;
pub mod look;
pub mod mcp;
pub mod open;
pub mod outside;
pub mod pr;
pub mod query;
pub mod record;
pub mod report;
pub mod run;
pub mod status;
pub mod top;
pub mod track;
pub mod up;
pub mod update;

use crate::cli::{Args, HELP};
use crate::host;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What a command produced: text for people and models, JSON for `--json` and MCP.
pub struct Output {
    pub text: String,
    pub json: Value,
    pub code: i32,
}

impl Output {
    pub fn new(text: String, json: Value) -> Output {
        Output { text, json, code: 0 }
    }
}

pub fn main() -> i32 {
    let args = match Args::parse(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => return fail(&e, false),
    };
    if let Err(e) = progress_mode(&args) {
        return fail(&e, args.json());
    }
    if args.has("version") {
        // `update` reads the version as the second word; the commit and build tell local builds apart.
        let commit = env!("STROBE_COMMIT");
        println!("strobe {} ({}build {})", env!("CARGO_PKG_VERSION"), if commit.is_empty() { String::new() } else { format!("{commit}, ") }, mcp::build_id());
        return 0;
    }
    if args.command.is_empty() || (args.has("help") && args.command.is_empty()) {
        print!("{HELP}");
        return 0;
    }
    let result = dispatch(&args);
    match result {
        Ok(out) => {
            if args.json() {
                println!("{}", serde_json::to_string_pretty(&out.json).unwrap_or_default());
            } else if !args.has("quiet") || out.code != 0 {
                print!("{}", out.text);
                if !out.text.ends_with('\n') {
                    println!();
                }
            }
            out.code
        }
        Err(e) => fail(&e, args.json()),
    }
}

pub fn dispatch(args: &Args) -> Result<Output> {
    if args.has("help") {
        let help = crate::cli::help(&args.command);
        return Ok(Output::new(help.clone(), json!({ "help": help })));
    }
    // A strobe killed mid-run left the user on its power plan: put theirs back first.
    crate::host::power::restore_stale();
    // And the CPU sets of processes it moved off a timed block's CPUs (`--reserve-cpus`).
    crate::host::reserve::restore_stale();
    match args.command.as_str() {
        "record" => record::run(args),
        "bench" => bench::run(args),
        "plan" => bench::plan::run(args),
        "query" => query::run(args),
        "report" => report::run(args),
        "open" => open::run(args),
        "look" => look::run(args),
        "config" => config::run(args),
        "run" => run::run(args),
        "status" => status::run(args),
        "top" => top::run(args),
        "track" => track::run(args),
        "history" => history::run(args),
        "up" => up::run(args),
        "update" => update::run(args),
        "leave" => up::leave(args),
        "helper" => helper::run(args),
        "mcp" => mcp::stdio().map(|_| Output::new(String::new(), Value::Null)),
        "daemon" => mcp::daemon(args).map(|_| Output::new(String::new(), Value::Null)),
        other => Err(Error::usage(format!("unknown command {other}; see strobe --help"))),
    }
}

fn fail(e: &Error, json: bool) -> i32 {
    if json {
        println!("{}", json!({ "error": e.message, "hint": e.hint, "code": e.code }));
    } else {
        eprintln!("strobe: {}", e.message);
        if let Some(h) = &e.hint {
            eprintln!("  fix: {h}");
        }
    }
    e.code
}

/// One invocation's progress log and stop requests. The daemon runs several invocations at once, each
/// on its own thread (an MCP call), so neither is process-wide. Invocations nest (a bisect's steps, a
/// track's benches, `record --bench`): the innermost one's log gets the progress lines, and a stop asked
/// of any enclosing one stops it too.
#[derive(Clone, Default)]
struct RunScope {
    progress: Option<PathBuf>,
    stops: Vec<PathBuf>,
}

thread_local! {
    static SCOPE: std::cell::RefCell<RunScope> = std::cell::RefCell::new(RunScope::default());
}

/// An entered scope; dropping it puts the enclosing one back.
pub struct Scope(Option<RunScope>);

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(prev) = self.0.take() {
            SCOPE.with(|s| *s.borrow_mut() = prev);
        }
    }
}

/// Enters an experiment's scope on this thread: progress lines also go to `<dir>/progress.log`, and
/// `<dir>/stop` (`strobe bench --stop`) stops it. A stop file an earlier run of it left is removed first.
pub fn experiment_scope(dir: &Path) -> Scope {
    let _ = std::fs::remove_file(dir.join("stop"));
    // Outside the daemon, from here a Ctrl-C stops the experiment instead of ending strobe.
    if !in_daemon() {
        host::ctrl_c();
    }
    SCOPE.with(|s| {
        let mut s = s.borrow_mut();
        let prev = s.clone();
        s.progress = Some(dir.join("progress.log"));
        s.stops.push(dir.join("stop"));
        Scope(Some(prev))
    })
}

static DAEMON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// No progress on stderr or stdout: the MCP servers (stdio carries JSON-RPC; the daemon has no
/// console). An invocation's progress log still gets every line.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process is the daemon (`strobe daemon`), which runs MCP calls' benches.
pub fn in_daemon() -> bool {
    DAEMON.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether this invocation should stop once the block that runs ends: `strobe bench --stop` of it or
/// of an enclosing experiment, or (outside the daemon, where one process is one invocation) Ctrl-C,
/// SIGINT or SIGTERM. Waits (a locked session, the slot, busy CPUs) end at once. The daemon never
/// installs a signal handler: a SIGTERM ends it as the signal would.
pub fn stop_requested() -> bool {
    (!in_daemon() && host::ctrl_c().load(std::sync::atomic::Ordering::SeqCst)) || SCOPE.with(|s| s.borrow().stops.iter().any(|p| p.exists()))
}

pub fn stop_error() -> Error {
    Error::new(130, "stopped (Ctrl-C, SIGTERM or strobe bench --stop)").hint("strobe bench --resume <exp> runs what is left")
}

/// A bench that ended before it finished, with what it finished written (a stop: exit 130; its
/// `--unlock-wait` ran out: exit 124), as that error, for callers that run several (track, bisect):
/// they must not read its partial result as an answer, and a stop ends them too.
pub fn unless_halted(out: Output) -> Result<Output> {
    match out.code {
        130 => Err(stop_error()),
        124 => {
            let e = Error::timeout(out.json["stopped"]["why"].as_str().unwrap_or("the bench ended before it finished"));
            Err(match out.json["id"].as_str() {
                Some(id) => e.hint(format!("strobe bench --resume {id} goes on")),
                None => e,
            })
        }
        _ => Ok(out),
    }
}

/// Where progress lines go (`--progress`, else `STROBE_PROGRESS`): `stderr` (default), `stdout` or `off`.
static PROGRESS: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

fn progress_mode(args: &Args) -> Result<()> {
    let mode = match args.get("progress").map(str::to_string).or_else(|| std::env::var("STROBE_PROGRESS").ok()).as_deref() {
        None | Some("stderr") => "stderr",
        // Never into `--json` output: one document on stdout.
        Some("stdout") if args.json() => "off",
        Some("stdout") => "stdout",
        Some("off") => "off",
        Some(other) => return Err(Error::usage(format!("--progress is stderr, stdout or off, not {other}"))),
    };
    let _ = PROGRESS.set(mode);
    Ok(())
}

/// Progress lines for long commands go to stderr so stdout stays the result; `--progress stdout`
/// for shells that count any stderr line as a failure (Windows PowerShell 5.1 with `2>&1` or `*>`
/// sets `$?` to false, and `powershell -Command` then exits 1, while strobe's own exit code,
/// `$LASTEXITCODE`, is 0).
pub fn progress(msg: impl AsRef<str>) {
    match *PROGRESS.get().unwrap_or(&"stderr") {
        "off" => {}
        _ if QUIET.load(std::sync::atomic::Ordering::Relaxed) => {}
        "stdout" => println!("{}", msg.as_ref()),
        _ => eprintln!("{}", msg.as_ref()),
    }
    if let Some(p) = SCOPE.with(|s| s.borrow().progress.clone())
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}
