//! Commands: orchestration only. Each returns an `Output` that the CLI prints and MCP returns (SPEC §17).

pub mod bench;
pub mod bisect;
pub mod fmt;
pub mod helper;
pub mod mcp;
pub mod outside;
pub mod query;
pub mod record;
pub mod report;
pub mod status;
pub mod top;
pub mod up;

use crate::cli::{Args, HELP};
use crate::model::{Error, Result};
use serde_json::{Value, json};

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
    if args.has("version") {
        println!("strobe {}", env!("CARGO_PKG_VERSION"));
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
        return Ok(Output::new(HELP.to_string(), json!({ "help": HELP })));
    }
    // A strobe killed mid-run left the user on its power plan: put theirs back first.
    crate::host::power::restore_stale();
    match args.command.as_str() {
        "record" => record::run(args),
        "bench" => bench::run(args),
        "query" => query::run(args),
        "report" => report::run(args),
        "status" => status::run(args),
        "top" => top::run(args),
        "up" => up::run(args),
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

static PROGRESS_FILE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Also appends progress lines to a file (a running experiment's progress.log).
pub fn progress_to(path: Option<std::path::PathBuf>) {
    *PROGRESS_FILE.lock().unwrap() = path;
}

/// Progress lines for long commands go to stderr so stdout stays the result.
pub fn progress(msg: impl AsRef<str>) {
    if std::env::var_os("STROBE_QUIET").is_none() {
        eprintln!("{}", msg.as_ref());
    }
    if let Some(p) = PROGRESS_FILE.lock().unwrap().as_ref()
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}
