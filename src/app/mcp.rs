//! The MCP server (SPEC §16, §17): the daemon serves it over Streamable HTTP on 127.0.0.1 with a bearer
//! token, `strobe mcp` over stdio. Each tool is exactly a CLI command: same arguments, same JSON.

use super::{Output, dispatch};
use crate::cli::{self, Args, Kind};
use crate::host::{http, paths};
use crate::model::{Error, Result};
use crate::store;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 47575;

pub fn instructions() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "strobe".into());
    format!("strobe: measure performance (web pages, Electron apps, any process). Load the \"strobe\" skill before using strobe tools. CLI: {exe}")
}

/// One MCP tool: exactly a CLI command (SPEC §17). Its inputs are the command's positional arguments,
/// by name, and the CLI flags it takes, whose JSON types come from the CLI's flag table (`cli::FLAGS`).
struct Tool {
    name: &'static str,
    description: &'static str,
    /// Positional arguments: input name, description.
    args: &'static [(&'static str, &'static str)],
    /// CLI flags: name, description.
    flags: &'static [(&'static str, &'static str)],
    /// Inputs every call must give.
    required: &'static [&'static str],
    /// Whether it may outlast the call (`timeout`, then it keeps running in the daemon).
    long: bool,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "status",
        description: "Environment, validity preflight (load, power, browser, engine, calibration) and recent recordings and experiments.",
        args: &[],
        flags: &[("fast", "skip the 3 s idle-load sample"), ("calibrate", "run the full calibration")],
        required: &[],
        long: false,
    },
    Tool {
        name: "record",
        description: "Record one target on one timeline. Returns a handle (rec_…) and headline frame, main-thread and CPU metrics. Targets: https://… (strobe launches Chromium), app:<id>, exe:<path>, cdp:<port>, pid:<n>, file:<path> (import). With bench (file#case), records that bench case (it drives the app) with the attribution probes; target then names one of the bench file's targets.",
        args: &[("target", "https://…, a local page.html (or file:page.html), app:<id> (a strobe.json profile, Electron or native), exe:<path>, cdp:<port>, pid:<n>, proc:<name>, or file:<path> (a trace to import); with bench: a target name from the bench file")],
        flags: &[
            ("for", "duration, e.g. 10s (default 10s)"),
            ("probes", "timing (default), attribution (CPU stacks, allocations, forced layouts, GC, a heap snapshot) or memory (a heap snapshot); pid:/proc: targets also deep; native targets (pid:, a native app) also waits: deep plus off-CPU waits and who woke each thread (query view waits), which costs the target at every context switch and refuses NoWaits (exit 4) where this OS or setup cannot take them"),
            ("native", "exe:<path> is a native app (no browser engine): recorded from outside: CPU, thread wakes, IO, GPU, memory, the UI thread's CPU and what reached the screen"),
            ("app", "exe:<path>: the strobe.json app profile it borrows"),
            ("headless", "draw off screen: attribution data stays meaningful, frame metrics do not"),
            ("engine", "a page's browser engine: chromium (default) or firefox (CPU, GPU, memory and OS frames; no trace or profilers)"),
            ("served", "folder the target's http origin serves, to resolve source-mapped frames to files on disk"),
            ("style", "also trace what invalidated style (query <rec> style)"),
            ("os-frames", "a Chromium record also takes the OS present events beside its trace"),
            ("until-line", "native app: start recording when it prints a line matching this regex (instead of after a 3 s settle)"),
            ("stop-line", "native app: stop recording when it prints a line matching this regex"),
            ("bench", "<file.bench.ts>#<case>: record that bench case (a case glob works) instead of a target"),
            ("head", "with bench: the variant to record (required through the daemon; stdio: default .)"),
            ("gpu-cache", "a native app (not with bench, where the bench target's gpuCache decides): warm or cold GPU shader caches (default warm)"),
            ("params", "with bench: k=v,k2=v2"),
            ("runs", "with bench: measured runs (default 5)"),
            ("pin", "with bench: auto, none, ccdN or a CPU list"),
            ("power", "high (default for timed recordings) or keep"),
        ],
        required: &[],
        long: true,
    },
    Tool {
        name: "bench",
        description: "Run a .bench.ts file: every series (case × target × params) gets a verdict from pairs of fresh launches (A B, B A …). Refuses invalid setups. Long runs continue in the daemon: then query the returned exp_… with view progress. stop asks a running experiment to stop (at once while it waits, else after its block); resume goes on with a stopped one.",
        args: &[("file", "absolute path to the .bench.ts file (not with stop or resume)")],
        flags: &[
            ("head", "variant under test: an absolute directory, a git ref, exe:<absolute path>, url:<href>, cmd:<line> (a cli() target's command line); or a target of the file, target[=variant] (target:name to disambiguate), for an implementation comparison (required through the daemon, whose working folder is not your project; stdio: default .)"),
            ("base", "baseline variant: an absolute directory, a git ref (e.g. origin/dev), url:<href>, cmd:<line> (a cli() target's command line); or a target of the file, target[=variant]: base and head naming two targets compare two implementations"),
            ("aa", "same build on both sides (checks the method)"),
            ("check", "validate the file: each case once on head, pass or fail with a screenshot, no statistics"),
            ("headless", "page targets in a headless browser (check and timed blocks): no display needed, runs through a locked session; frame, input-latency and GPU-use metrics refused (Headless)"),
            ("engine", "page targets' browser engine: chromium or firefox; the file's targets of that engine, else every page target in it"),
            ("blocks", "N timed blocks of head alone and their values, nothing compared (what track records for a commit)"),
            ("filter", "case names, comma-separated; exact unless a name holds * (a glob)"),
            ("target", "target names from the file, comma-separated"),
            ("params", "k=v,k2=v2, or all (adds every parameter's more values)"),
            ("primary", "override the case's primary metric"),
            ("equivalence", "equivalence bound in % (default 2)"),
            ("noworse", "non-inferiority: \"2%\" asks whether head's primary is at most 2% worse (verdicts no worse, worse, unclear); \"metric=2%,metric2=5%\" per metric"),
            ("plan", "do not run: estimate pairs and minutes per series from earlier experiments here, builds and the bench slot's queue"),
            ("effect", "with plan: the expected change of the primary in % (default: what the last run of the series measured, else 0)"),
            ("pilot", "with plan: run 4 pairs first to learn the spread"),
            ("pairs", "auto (default) or a number"),
            ("runs", "measured runs per block (default 5)"),
            ("probes", "timing skips attribution blocks; memory runs a leak run per variant (no base needed); waits adds off-CPU waits to native attribution blocks (with why always)"),
            ("repeat", "leak run: N runs to the middle snapshot and N more to the last (default 10)"),
            ("bisect", "good..bad: find the commit on the first-parent line that moved the primary (one series)"),
            ("pin", "auto (V-Cache CCD), none, ccdN, or a CPU list"),
            ("power", "high (default: the fastest plan for the run, restored after) or keep"),
            ("reserve-cpus", "auto (default: this user's other processes kept off the target's CPUs during timed blocks, put back after; Windows and Linux), on (every block, check too) or off"),
            ("why", "attribution blocks: auto (default, only when the primary differs), always, never"),
            ("trace", "timing-block trace: lean (default) or full"),
            ("keep", "keep every block's recording"),
            ("pixels", "also compare base and head at the case's page.snapshot()s"),
            ("pixels-only", "only compare snapshots, no timing"),
            ("style", "attribution blocks also trace what invalidated style"),
            ("max-time", "stop starting pairs after this long, e.g. 30m"),
            ("unlock-wait", "a series that needs a presenting window waits at most this long for an unlock (default: until unlocked), e.g. 2h"),
            ("require-verdict", "exit 5 when a series has no verdict"),
            ("require-parity", "implementation comparisons: exit 4 when a journey is a parity gap (one side cannot run it)"),
            ("stop", "exp_… (a unique prefix works): ask that running experiment to stop; it keeps what finished"),
            ("resume", "exp_…: go on with a stopped experiment from its finished pairs"),
        ],
        required: &[],
        long: true,
    },
    Tool {
        name: "query",
        description: "Views over a recording (rec_…) or experiment (exp_…): summary, frames, threads, hotspots (CPU self time by function|file|package with full stacks), alloc (bytes allocated by function|file|package with full stacks), layout (forced layouts and style recalcs by the call that forced them), shifts (the largest layout shifts and the elements they moved), gc (collections and GC time per run), heap (live objects by class with retained sizes, detached DOM, retainer paths), leaks (memory recordings: objects from the first half still alive at the end, with retainer paths), memory (by process, layer or allocator), insights (rules with evidence; exp_… too), waits (a native capture taken with probes waits: off-CPU time by the stack each thread waited at, blocked or runnable, and who woke it); those need attribution recordings; sql (Perfetto trace_processor SQL), progress (running experiments). rec_a..rec_b compares two recordings in hotspots, alloc, layout, style, threads, frames, summary, heap and memory: per-run deltas and the stacks where they concentrate; exp_… with one of those views (other than summary) compares each series' base..head attribution recordings. Tracked history (strobe track): handle history lists this device's series (or history <ser_…|bench file> one series' points with intervals), handle changes lists suspected and confirmed changes with their paired A/B and culprit; handle ser_… takes views history and changes.",
        args: &[
            ("handle", "rec_…, exp_… (a unique prefix works) or rec_a..rec_b; history, changes or ser_… for tracked history"),
            (
                "view",
                "summary (default), frames, threads (a native recording: its threads' wakes, CPU, names), hotspots, waits, alloc, layout, shifts, gc, heap, leaks, memory, insights, sql, progress (exp_… takes summary, progress, insights, the base..head views hotspots, alloc, layout, style, threads, frames, heap and memory, and on a --check of one case any recording view)",
            ),
            ("sql", "SQL for the sql view"),
        ],
        flags: &[
            ("by", "hotspots, waits and alloc: function, file, package; memory: process, layer, allocator"),
            ("top", "rows (default 15)"),
            ("reveal", "heap: show string values (redacted by default; snapshots can hold user content)"),
            ("at", "memory: which census (0 the first, -1 the last, default the last)"),
            ("native", "hotspots: the native stacks"),
            ("thread", "hotspots and waits of a native recording: one thread by name"),
            ("phase", "hotspots and waits of a native recording: one phase"),
            ("timeline", "threads: each thread's CPU per second from the launch"),
        ],
        required: &["handle"],
        long: false,
    },
    Tool {
        name: "report",
        description: "Write a self-contained HTML (or markdown) report from a recording or experiment and return its path, to show in the Review pane.",
        args: &[("handle", "rec_…, exp_… or ser_… (a tracked series)"), ("view", "experiment, recording or history (default from the handle)")],
        flags: &[
            ("format", "html (default), md, pr (an experiment's pull request block), card (its result as a 1200×630 PNG) or badge (an SVG badge)"),
            ("out", "where to write it"),
            ("theme", "card and badge: light, dark or auto"),
            ("update-pr", "with format pr: put the block in this pull request's body through gh"),
            ("repo", "with update-pr: owner/repo"),
            ("force", "with update-pr: replace a block another experiment wrote"),
            ("no-card", "with update-pr: no result card or badges"),
            ("badge-url", "with format badge: print a shields.io URL"),
            ("scoreboard", "with format badge, for an implementation comparison: its journeys' scoreboard (vs <base> | better in 4/5 · 1 gap)"),
        ],
        required: &["handle"],
        long: false,
    },
];

fn tools() -> Value {
    let list: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let mut props = serde_json::Map::new();
            for (k, d) in t.args {
                props.insert(k.to_string(), json!({ "type": "string", "description": d }));
            }
            for (k, d) in t.flags {
                let ty = match cli::kind(k) {
                    Some(Kind::Switch) => "boolean",
                    Some(Kind::Number) => "number",
                    _ => "string",
                };
                props.insert(k.to_string(), json!({ "type": ty, "description": d }));
            }
            if t.long {
                props.insert("timeout".into(), json!({ "type": "number", "description": "seconds before returning while it keeps running (default 600)" }));
            }
            json!({ "name": t.name, "description": t.description, "inputSchema": { "type": "object", "required": t.required, "properties": props } })
        })
        .collect();
    Value::Array(list)
}

/// Turns a tool call into CLI arguments, so MCP and the CLI share one code path. Refuses an input the
/// tool does not have, or one of the wrong type, instead of passing it on.
fn to_args(name: &str, input: &Value) -> Result<(Vec<String>, u64, Option<String>)> {
    let tool = TOOLS.iter().find(|t| t.name == name).ok_or_else(|| Error::usage(format!("unknown tool {name}")))?;
    let empty = serde_json::Map::new();
    let obj = match input {
        Value::Object(o) => o,
        Value::Null => &empty,
        _ => return Err(Error::usage(format!("{name}: arguments must be an object"))),
    };
    let mut flags: Vec<String> = Vec::new();
    for (k, v) in obj {
        if tool.long && k == "timeout" {
            if !(v.is_null() || v.as_u64().is_some() || v.as_str().is_some_and(|s| s.trim().parse::<u64>().is_ok())) {
                return Err(Error::usage(format!("{name}: timeout is a whole number of seconds, not {v}")));
            }
            continue;
        }
        if tool.args.iter().any(|(a, _)| a == k) {
            if !(v.is_string() || v.is_number() || v.is_null()) {
                return Err(Error::usage(format!("{name}: {k} is a string")));
            }
            continue;
        }
        if !tool.flags.iter().any(|(f, _)| f == k) {
            let all: Vec<&str> = tool.args.iter().map(|a| a.0).chain(tool.flags.iter().map(|f| f.0)).collect();
            return Err(Error::usage(format!("{name} has no input {k}; it takes {}", all.join(", "))));
        }
        match (cli::kind(k), v) {
            (_, Value::Null) | (Some(Kind::Switch), Value::Bool(false)) => {}
            (Some(Kind::Switch), Value::Bool(true)) => flags.push(format!("--{k}")),
            (Some(Kind::Switch), _) => return Err(Error::usage(format!("{name}: {k} is a boolean"))),
            (Some(Kind::Number), Value::Number(n)) => flags.extend([format!("--{k}"), n.to_string()]),
            (Some(Kind::Number), Value::String(s)) if s.trim_end_matches('%').parse::<f64>().is_ok() => flags.extend([format!("--{k}"), s.clone()]),
            (Some(Kind::Number), _) => return Err(Error::usage(format!("{name}: {k} is a number"))),
            (_, Value::String(s)) => flags.extend([format!("--{k}"), s.clone()]),
            (_, Value::Number(n)) => flags.extend([format!("--{k}"), n.to_string()]),
            _ => return Err(Error::usage(format!("{name}: {k} is a string"))),
        }
    }
    for r in tool.required {
        if obj.get(*r).is_none_or(Value::is_null) {
            return Err(Error::usage(format!("{name} needs {r}")));
        }
    }
    let mut a = vec![name.to_string()];
    let get = |k: &str| obj.get(k).and_then(|v| v.as_str().map(str::to_string).or_else(|| (v.is_number()).then(|| v.to_string())));
    let has = |k: &str| obj.get(k).is_some_and(|v| !v.is_null() && *v != Value::Bool(false));
    let mut pending_id = None;
    match name {
        "record" => {
            // A bench case is an experiment (`exp_…`) that keeps its recordings; `target` then picks
            // one of the bench file's targets (`--target`).
            let id = if has("bench") {
                need_head(obj)?;
                if let Some(t) = get("target") {
                    a.extend(["--target".into(), t]);
                }
                store::new_id("exp")
            } else {
                a.push(get("target").ok_or_else(|| Error::usage("record needs target, or bench <file>#<case>"))?);
                store::new_id("rec")
            };
            a.push("--id".into());
            a.push(id.clone());
            pending_id = Some(id);
        }
        "bench" => {
            if has("stop") || has("resume") {
                if has("stop") && has("resume") {
                    return Err(Error::usage("bench: stop or resume, not both"));
                }
                if has("file") {
                    return Err(Error::usage("bench: stop and resume take an experiment, not a file"));
                }
                if let Some(h) = get("resume") {
                    pending_id = super::bench::slot::experiment(&h).ok().map(|(_, id)| id);
                }
            } else {
                a.push(get("file").ok_or_else(|| Error::usage("bench needs file (or stop / resume with an experiment)"))?);
                need_head(obj)?;
                let id = store::new_id("exp");
                a.push("--id".into());
                a.push(id.clone());
                pending_id = Some(id);
            }
        }
        "query" => {
            a.push(get("handle").ok_or_else(|| Error::usage("query needs handle"))?);
            let view = get("view").unwrap_or_else(|| "summary".into());
            a.push(view.clone());
            if view == "sql" {
                a.push(get("sql").ok_or_else(|| Error::usage("the sql view needs sql"))?);
            }
        }
        "report" => {
            let h = get("handle").ok_or_else(|| Error::usage("report needs handle"))?;
            let view = get("view").unwrap_or_else(|| {
                if h.starts_with("ser_") {
                    "history".into()
                } else if store::resolve(&h).is_ok_and(|d| store::is_experiment(&d)) {
                    "experiment".into()
                } else {
                    "recording".into()
                }
            });
            a.push(view);
            a.push(h);
        }
        _ => {}
    }
    a.extend(flags);
    let timeout = obj.get("timeout").and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())).unwrap_or(600);
    Ok((a, timeout, pending_id))
}

/// In the daemon a bench's `head` has no default: `.` would be the daemon's own working folder (strobe's
/// install folder), not the caller's project, and a relative path (the bench file's too) would name a
/// file under it.
fn need_head(obj: &serde_json::Map<String, Value>) -> Result<()> {
    if !super::in_daemon() {
        return Ok(());
    }
    let relative = |p: &str| std::path::Path::new(p).is_relative();
    let file = obj.get("file").or(obj.get("bench")).and_then(Value::as_str).map(|f| f.rsplit_once('#').map_or(f, |(f, _)| f));
    if let Some(f) = file.filter(|f| relative(f)) {
        return Err(Error::usage(format!("bench file {f:?} is relative, which the daemon would read from its own working folder: give the absolute path")));
    }
    if obj.get("head").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(Error::usage(
            "bench needs head through the daemon: the variant under test as an absolute directory, a git ref (git:<ref>), exe:<absolute path>, app:<id>, url:<href> or a target of the file; the daemon's working folder is not your project",
        ));
    }
    for side in ["head", "base"] {
        let Some(spec) = obj.get(side).and_then(Value::as_str) else { continue };
        if let Some(path) = relative_variant(spec) {
            return Err(Error::usage(format!("{side} {spec:?} names {path:?}, a relative path the daemon would read from its own working folder: give the absolute path")));
        }
    }
    Ok(())
}

/// The relative path a variant spec names, if it names one: a folder (`.`, `..`, `./x`, `.\x`, or a
/// relative folder that exists from here) or `exe:<relative>`, also after `<target>=`. A git ref, a
/// target's name, `app:`, `url:` and `git:` name no path.
fn relative_variant(spec: &str) -> Option<&str> {
    // `<target>=<variant>`: a target's name holds no `:` or slash (a URL's query may hold `=`).
    let v = match spec.split_once('=') {
        Some((t, v)) if !t.contains([':', '/', '\\']) => v,
        _ => spec,
    };
    if let Some(exe) = v.strip_prefix("exe:") {
        return std::path::Path::new(exe).is_relative().then_some(exe);
    }
    if ["url:", "git:", "app:", "target:", "cmd:"].iter().any(|p| v.starts_with(p)) {
        return None;
    }
    let dotted = v == "." || v == ".." || ["./", ".\\", "../", "..\\"].iter().any(|p| v.starts_with(p));
    (dotted || (std::path::Path::new(v).is_relative() && std::path::Path::new(v).is_dir())).then_some(v)
}

fn result(out: Output) -> Value {
    json!({ "content": [{ "type": "text", "text": out.text }], "structuredContent": out.json, "isError": false })
}

fn call_tool(name: &str, input: &Value) -> Value {
    let (argv, timeout, pending) = match to_args(name, input) {
        Ok(x) => x,
        Err(e) => return json!({ "content": [{ "type": "text", "text": e.message }], "isError": true }),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = Args::parse(argv).and_then(|args| dispatch(&args));
        let _ = tx.send(r);
    });
    match rx.recv_timeout(Duration::from_secs(timeout)) {
        Ok(Ok(out)) => result(out),
        Ok(Err(e)) if e.code == 4 => {
            let text = format!("refused: {}\nfix: {}", e.message, e.hint.clone().unwrap_or_default());
            json!({ "content": [{ "type": "text", "text": text }], "structuredContent": { "refused": true, "error": e.message, "hint": e.hint, "code": 4 }, "isError": false })
        }
        Ok(Err(e)) => {
            let text = format!("{}{}", e.message, e.hint.map(|h| format!("\nfix: {h}")).unwrap_or_default());
            json!({ "content": [{ "type": "text", "text": text }], "structuredContent": { "error": e.message, "code": e.code }, "isError": true })
        }
        Err(_) => {
            let id = pending.unwrap_or_default();
            let text = if id.starts_with("rec_") {
                format!("still recording after {timeout} s; it continues in the daemon. Read it with query {{ handle: \"{id}\" }} once it finishes")
            } else {
                format!("still running after {timeout} s; it continues in the daemon. Follow it with query {{ handle: \"{id}\", view: \"progress\" }}")
            };
            json!({ "content": [{ "type": "text", "text": text }], "structuredContent": { "id": id, "state": "running" }, "isError": false })
        }
    }
}

/// Handles one JSON-RPC message; `None` for notifications.
pub fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-03-26"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "strobe", "version": env!("CARGO_PKG_VERSION") },
            "instructions": instructions(),
        }),
        "ping" => json!({}),
        // Never mid-bench: exiting would skip putting back the power plan and CPU sets it changed.
        "strobe/quit" if super::bench::slot::daemon_benching() => {
            return id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": "a bench runs in the daemon: strobe bench --stop it first" } }));
        }
        "strobe/quit" => {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::process::exit(0);
            });
            json!({})
        }
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            call_tool(name, params.get("arguments").unwrap_or(&json!({})))
        }
        m if m.starts_with("notifications/") => return None,
        other => {
            return id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unknown method {other}") } }));
        }
    };
    id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub fn stdio() -> Result<()> {
    super::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines().map_while(std::result::Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(reply) = handle(&msg) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub fn token_path() -> std::path::PathBuf {
    paths::state_dir().join("mcp-token")
}

/// The MCP token, created on first use: 32 bytes from the OS's random source as hex, in a file only
/// the user can read (one an older strobe wrote with the umask's mode is rewritten so).
pub fn token() -> Result<String> {
    let p = token_path();
    if let Ok(t) = std::fs::read_to_string(&p)
        && t.trim().len() >= 32
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
                paths::write_owner_only(&p, t.trim().as_bytes())?;
            }
        }
        return Ok(t.trim().to_string());
    }
    let out = paths::csprng_hex(32)?;
    paths::write_owner_only(&p, out.as_bytes())?;
    Ok(out)
}

pub fn port() -> u16 {
    std::fs::read_to_string(paths::state_dir().join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["mcp_port"].as_u64())
        .map(|p| p as u16)
        .unwrap_or(DEFAULT_PORT)
}

/// `strobe daemon`: serves MCP and reports until killed. One per user: exits if the port is taken.
pub fn daemon(_args: &Args) -> Result<()> {
    let port = port();
    let token = token()?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| Error::msg(format!("port {port} is taken ({e}); is a strobe daemon already running?")))?;
    let log = paths::state_dir().join("log");
    let _ = std::fs::create_dir_all(&log);
    super::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    super::DAEMON.store(true, std::sync::atomic::Ordering::Relaxed);
    // Tracked branches (`strobe track --schedule`): checked once a minute.
    std::thread::spawn(super::track::daemon_loop);
    std::thread::spawn(follow_installed);
    let handler: Arc<dyn Fn(http::Request) -> http::Response + Send + Sync> = Arc::new(move |req: http::Request| serve(req, port, &token));
    http::serve(listener, handler);
    Ok(())
}

/// An installed daemon whose copy `strobe update` replaced while it ran a bench (the update left it
/// for its bench, and said to run `strobe up` later): once no bench runs in it, the installed build's
/// own `up` restarts the daemon as that build. Checked once a minute; a daemon run from elsewhere (a
/// build folder) is left alone.
fn follow_installed() {
    let dir = super::up::install_dir();
    let mine = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if mine.as_deref().and_then(|d| std::fs::canonicalize(d).ok()) != std::fs::canonicalize(&dir).ok() {
        return;
    }
    let name = |n: &str| if cfg!(windows) { format!("{n}.exe") } else { n.to_string() };
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let daemon = if dir.join(name("strobew")).is_file() { dir.join(name("strobew")) } else { dir.join(name("strobe")) };
        let Some(installed) = file_build(&daemon) else { continue };
        if installed == build_id() || super::up::daemon_benching() {
            continue;
        }
        if super::up::spawn_detached(&dir.join(name("strobe")), &["up", "--skip", "deep,mcp,calibrate"]).is_ok() {
            return;
        }
    }
}

fn serve(req: http::Request, port: u16, token: &str) -> http::Response {
    let host = req.headers.get("host").map(String::as_str).unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return http::Response::text(403, "bad host");
    }
    // The daemon's own pages (the Perfetto UI fetches its fonts and wasm in CORS mode) may read it.
    let same = |o: &str| o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}");
    if let Some(origin) = req.headers.get("origin")
        && origin != "null"
        && !(req.method == "GET" && same(origin))
    {
        return http::Response::text(403, "cross-origin requests are not allowed");
    }
    let path = req.path.split('?').next().unwrap_or("/").to_string();
    if req.method == "GET"
        && let Some(r) = super::open::serve(&req, port)
    {
        return r;
    }
    if req.method == "GET" && path.starts_with("/report/") {
        let handle = path.trim_start_matches("/report/");
        return match report_html(handle) {
            Ok(html) => http::Response::new(200, "text/html; charset=utf-8", html.into_bytes()),
            Err(e) => http::Response::text(404, &e.message),
        };
    }
    if path == "/health" {
        return http::Response::new(200, "application/json", json!({ "strobe": env!("CARGO_PKG_VERSION"), "build": build_id() }).to_string().into_bytes());
    }
    if path != "/mcp" {
        return http::Response::text(404, "not found");
    }
    if req.method != "POST" {
        return http::Response::text(405, "POST only");
    }
    let auth = req.headers.get("authorization").map(String::as_str).unwrap_or("");
    if !http::same(auth.as_bytes(), format!("Bearer {token}").as_bytes()) {
        return http::Response::text(401, "unauthorized");
    }
    if !req.headers.get("content-type").is_some_and(|c| c.starts_with("application/json")) {
        return http::Response::text(415, "application/json only");
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else { return http::Response::text(400, "bad json") };
    match handle(&msg) {
        Some(reply) => http::Response::new(200, "application/json", reply.to_string().into_bytes()),
        None => http::Response::new(202, "application/json", Vec::new()),
    }
}

fn report_html(handle: &str) -> Result<String> {
    let dir = store::resolve(handle)?;
    let (view, data) = if store::is_experiment(&dir) { ("experiment", store::read_json(&dir.join("result.json"))?) } else { ("recording", store::read_json(&dir.join("meta.json"))?) };
    Ok(super::report::html(view, &data))
}

pub use crate::host::build::{build_id, file_build};

pub fn file_id(path: &std::path::Path) -> String {
    use sha2::Digest;
    std::fs::read(path).map(|b| sha2::Sha256::digest(b).iter().take(8).map(|x| format!("{x:02x}")).collect()).unwrap_or_default()
}

/// The running daemon's build (`/health`), when it answers; empty from builds before it said.
pub fn daemon_build() -> Option<String> {
    use std::io::Read;
    let port = port();
    let mut s = std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut text = String::new();
    let _ = s.read_to_string(&mut text);
    let body = text.split_once("\r\n\r\n")?.1;
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    Some(v["build"].as_str().unwrap_or("").to_string())
}

/// Is a daemon answering on this device?
pub fn running() -> bool {
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port())), Duration::from_millis(500)).is_ok()
}
