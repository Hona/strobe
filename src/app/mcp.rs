//! The MCP server (SPEC §16, §17): the daemon serves it over Streamable HTTP on 127.0.0.1 with a bearer
//! token, `strobe mcp` over stdio. Each tool is exactly a CLI command: same arguments, same JSON.

use super::{Output, dispatch};
use crate::cli::Args;
use crate::host::{self, http, paths};
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

fn tools() -> Value {
    let s = |d: &str| json!({ "type": "string", "description": d });
    let b = |d: &str| json!({ "type": "boolean", "description": d });
    let n = |d: &str| json!({ "type": "number", "description": d });
    json!([
        { "name": "status", "description": "Environment, validity preflight (load, power, browser, engine, calibration) and recent recordings and experiments.",
          "inputSchema": { "type": "object", "properties": { "target": s("optional target to check"), "fast": b("skip the 3 s idle-load sample"), "calibrate": b("run the full calibration") } } },
        { "name": "record", "description": "Record one target on one timeline. Returns a handle (rec_…) and headline frame, main-thread and CPU metrics. Targets: https://… (strobe launches Chromium), cdp:<port>, file:<path> (import).",
          "inputSchema": { "type": "object", "required": ["target"], "properties": {
            "target": s("https://…, cdp:<port>, or file:<path>"), "for": s("duration, e.g. 10s (default 10s)"),
            "probes": s("timing (default), attribution (adds CPU stacks) or memory"), "timeout": n("seconds before returning while it keeps running (default 600)") } } },
        { "name": "bench", "description": "Run a .bench.ts file: every series (case × target × params) gets a verdict from pairs of fresh launches (A B, B A …). Refuses invalid setups. Long runs continue in the daemon: then query the returned exp_… with view progress.",
          "inputSchema": { "type": "object", "required": ["file"], "properties": {
            "file": s("path to the .bench.ts file"), "head": s("variant under test: a directory (default .), a git ref, url:<href>"),
            "base": s("baseline variant: a directory, a git ref (e.g. origin/dev), url:<href>"), "aa": b("same build on both sides (checks the method)"),
            "filter": s("case glob"), "target": s("one target name from the file"), "params": s("k=v,k2=v2"),
            "primary": s("override the case's primary metric"), "equivalence": n("equivalence bound in % (default 2)"),
            "pairs": s("auto (default) or a number"), "runs": n("measured runs per block (default 5)"), "probes": s("timing skips attribution blocks"),
            "pin": s("auto (V-Cache CCD), none, ccdN, or a CPU list"), "power": s("keep (default) or high (High performance plan for the run, restored after)"),
            "why": s("attribution blocks: auto (default, only when the primary differs), always, never"), "trace": s("timing-block trace: lean (default) or full"),
            "timeout": n("seconds before returning while it keeps running (default 600)") } } },
        { "name": "query", "description": "Views over a recording (rec_…) or experiment (exp_…): summary, frames, threads, hotspots (CPU self time by function|file|package with full stacks), alloc (bytes allocated by function|file|package with full stacks), layout (forced layouts and style recalcs by the call that forced them), gc (collections and GC time per run), heap (live objects by class with retained sizes, detached DOM, retainer paths; rec_a..rec_b diffs two recordings by class); those need attribution recordings; sql (Perfetto trace_processor SQL), progress (running experiments).",
          "inputSchema": { "type": "object", "required": ["handle"], "properties": {
            "handle": s("rec_… or exp_… (a unique prefix works)"), "view": s("summary (default), frames, threads, hotspots, alloc, layout, gc, heap, sql, progress"),
            "sql": s("SQL for the sql view"), "by": s("hotspots and alloc grouping: function, file, package"), "top": n("rows (default 15)") } } },
        { "name": "report", "description": "Write a self-contained HTML (or markdown) report from a recording or experiment and return its path, to show in the Review pane.",
          "inputSchema": { "type": "object", "required": ["handle"], "properties": {
            "handle": s("rec_… or exp_…"), "view": s("experiment or recording (default from the handle)"), "format": s("html (default) or md") } } },
    ])
}

/// Turns a tool call into CLI arguments, so MCP and the CLI share one code path.
fn to_args(name: &str, input: &Value) -> Result<(Vec<String>, u64, Option<String>)> {
    let mut a = vec![name.to_string()];
    let get = |k: &str| input.get(k).and_then(|v| v.as_str().map(str::to_string).or_else(|| (v.is_number()).then(|| v.to_string())));
    let mut pending_id = None;
    match name {
        "status" => {
            if let Some(t) = get("target") {
                a.push(t);
            }
        }
        "record" => a.push(get("target").ok_or_else(|| Error::usage("record needs target"))?),
        "bench" => {
            a.push(get("file").ok_or_else(|| Error::usage("bench needs file"))?);
            let id = store::new_id("exp");
            a.push("--id".into());
            a.push(id.clone());
            pending_id = Some(id);
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
            let view = get("view").unwrap_or_else(|| if h.starts_with("exp_") { "experiment".into() } else { "recording".into() });
            a.push(view);
            a.push(h);
        }
        other => return Err(Error::usage(format!("unknown tool {other}"))),
    }
    let skip = ["target", "file", "handle", "view", "sql", "timeout"];
    if let Some(obj) = input.as_object() {
        for (k, v) in obj {
            if skip.contains(&k.as_str()) && !(name == "bench" && k == "target") {
                continue;
            }
            match v {
                Value::Bool(true) => a.push(format!("--{k}")),
                Value::Bool(false) | Value::Null => {}
                Value::String(s) => {
                    a.push(format!("--{k}"));
                    a.push(s.clone());
                }
                other => {
                    a.push(format!("--{k}"));
                    a.push(other.to_string());
                }
            }
        }
    }
    let timeout = input.get("timeout").and_then(Value::as_u64).unwrap_or(600);
    Ok((a, timeout, pending_id))
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
            let text = format!("still running after {timeout} s; it continues in the daemon. Follow it with query {{ handle: \"{id}\", view: \"progress\" }}");
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
    unsafe { std::env::set_var("STROBE_QUIET", "1") };
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

/// The MCP token, created on first use (32 random bytes as hex).
pub fn token() -> Result<String> {
    let p = token_path();
    if let Ok(t) = std::fs::read_to_string(&p)
        && t.trim().len() >= 32
    {
        return Ok(t.trim().to_string());
    }
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..4u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i ^ host::now_ns());
        out += &format!("{:016x}", h.finish());
    }
    std::fs::write(&p, &out)?;
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
    unsafe { std::env::set_var("STROBE_QUIET", "1") };
    let handler: Arc<dyn Fn(http::Request) -> http::Response + Send + Sync> = Arc::new(move |req: http::Request| serve(req, port, &token));
    http::serve(listener, handler);
    Ok(())
}

fn serve(req: http::Request, port: u16, token: &str) -> http::Response {
    let host = req.headers.get("host").map(String::as_str).unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return http::Response::text(403, "bad host");
    }
    if let Some(origin) = req.headers.get("origin")
        && origin != "null"
    {
        return http::Response::text(403, "cross-origin requests are not allowed");
    }
    let path = req.path.split('?').next().unwrap_or("/").to_string();
    if req.method == "GET" && path.starts_with("/report/") {
        let handle = path.trim_start_matches("/report/");
        return match report_html(handle) {
            Ok(html) => http::Response::new(200, "text/html; charset=utf-8", html.into_bytes()),
            Err(e) => http::Response::text(404, &e.message),
        };
    }
    if path == "/health" {
        return http::Response::new(200, "application/json", json!({ "strobe": env!("CARGO_PKG_VERSION") }).to_string().into_bytes());
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
    let (view, data) = if handle.starts_with("exp_") { ("experiment", store::read_json(&dir.join("result.json"))?) } else { ("recording", store::read_json(&dir.join("meta.json"))?) };
    Ok(super::report::html(view, &data))
}

/// Is a daemon answering on this device?
pub fn running() -> bool {
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port())), Duration::from_millis(500)).is_ok()
}
