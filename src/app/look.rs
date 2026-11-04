//! `strobe look <url>`: a page in a headless browser, nothing measured: its title, address, headings,
//! visible text, console messages and uncaught errors, and a screenshot. For checking that a UI
//! loads and says what it should (an agent looking at a page while the session is locked) without
//! writing a browser script.

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::model::{Error, Result};
use crate::target::chromium::{self, Window};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the page shows: title, address, headings and its visible text, at most `chars` of it.
fn read_page(chars: usize) -> String {
    format!(
        r#"(() => {{
  const shown = (e) => {{ const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== "hidden" }}
  const headings = [...document.querySelectorAll("h1, h2, h3, [role=heading], dialog[open], [role=dialog]")].filter(shown)
    .map((e) => ({{ tag: e.getAttribute("role") || e.tagName.toLowerCase(), text: (e.getAttribute("aria-label") || e.innerText || "").trim().replace(/\s+/g, " ").slice(0, 120) }}))
    .filter((h) => h.text).slice(0, 30)
  const text = (document.body?.innerText ?? "").replace(/\n{{3,}}/g, "\n\n").trim()
  return JSON.stringify({{ title: document.title, url: location.href, headings, text: text.slice(0, {chars}), chars: text.length, ready: document.readyState }})
}})()"#
    )
}

/// A local file is a `file://` address; anything else goes as given.
fn address(target: &str) -> String {
    let p = Path::new(target);
    if !target.contains("://") && !target.starts_with("about:") && !target.starts_with("data:") && p.exists() {
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = abs.display().to_string().replace('\\', "/");
        let s = s.trim_start_matches("//?/");
        return if s.starts_with('/') { format!("file://{s}") } else { format!("file:///{s}") };
    }
    target.to_string()
}

fn console_text(args: &Value) -> String {
    args.as_array()
        .into_iter()
        .flatten()
        .map(|a| match a["value"].as_str() {
            Some(s) => s.to_string(),
            None if !a["value"].is_null() => a["value"].to_string(),
            None => a["description"].as_str().or(a["type"].as_str()).unwrap_or("").to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run(args: &Args) -> Result<Output> {
    let target = args.pos.first().ok_or_else(|| Error::usage("look needs a page: strobe look <url | file> [--until <css>] [--wait 2s] [--out shot.png]"))?;
    let url = address(target);
    let (width, height) = match args.get("size") {
        Some(s) => {
            let (w, h) = s.split_once(['x', 'X', '×']).ok_or_else(|| Error::usage(format!("--size WIDTHxHEIGHT, not {s}")))?;
            (w.trim().parse().map_err(|_| Error::usage(format!("--size: {w}")))?, h.trim().parse().map_err(|_| Error::usage(format!("--size: {h}")))?)
        }
        None => (1280u32, 800u32),
    };
    let wait = args.get("wait").map(duration_ms).transpose()?.unwrap_or(1000);
    let chars = args.num("chars")?.unwrap_or(4000.0).max(0.0) as usize;
    let timeout = Duration::from_millis(args.get("timeout").map(duration_ms).transpose()?.unwrap_or(30_000));
    let window = Window { x: 0, y: 0, width, height, scale: args.num("scale")? };
    let browser = chromium::launch(&window, !args.has("headed"), None, &[])?;
    let result = (|| -> Result<Output> {
        let page = browser.page()?;
        let events = browser.cdp.events();
        page.send("Runtime.enable", json!({}))?;
        page.send("Log.enable", json!({}))?;
        if !args.has("headed") {
            page.send("Emulation.setDeviceMetricsOverride", json!({ "width": width, "height": height, "deviceScaleFactor": window.scale.unwrap_or(1.0), "mobile": false }))?;
        }
        progress(format!("look {url}"));
        let started = Instant::now();
        let loaded = page.navigate(&url, timeout);
        let load_ms = started.elapsed().as_secs_f64() * 1e3;
        let mut problems: Vec<String> = Vec::new();
        match loaded {
            Err(e) if e.code == 255 => return Err(e),
            Err(e) => problems.push(format!("the load event never came: {}", e.message)),
            Ok(()) => {}
        }
        if let Some(sel) = args.get("until") {
            let expr = format!(
                "(async () => {{ const end = Date.now() + {}; while (Date.now() < end) {{ const e = document.querySelector({}); if (e) {{ const r = e.getBoundingClientRect(); if (r.width > 0 && r.height > 0) return true }} await new Promise((r) => setTimeout(r, 50)) }} return false }})()",
                timeout.as_millis(),
                json!(sel)
            );
            if page.eval_timeout(&expr, timeout + Duration::from_secs(5))? != Value::Bool(true) {
                problems.push(format!("{sel:?} never showed in {} s", timeout.as_secs()));
            }
        }
        std::thread::sleep(Duration::from_millis(wait));
        let info: Value = page.eval_timeout(&read_page(chars), Duration::from_secs(10))?.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        // Everything the page said while it loaded and settled.
        let mut console: Vec<Value> = Vec::new();
        for ev in events.try_iter().filter(|e| e["sessionId"].as_str() == Some(page.session.as_str())) {
            let p = &ev["params"];
            match ev["method"].as_str().unwrap_or("") {
                "Runtime.consoleAPICalled" => console.push(json!({ "level": p["type"], "text": console_text(&p["args"]) })),
                "Runtime.exceptionThrown" => {
                    let d = &p["exceptionDetails"];
                    let text = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("exception").lines().next().unwrap_or("").to_string();
                    console.push(json!({ "level": "exception", "text": text, "where": format!("{}:{}", d["url"].as_str().unwrap_or(""), d["lineNumber"].as_u64().unwrap_or(0) + 1) }));
                }
                "Log.entryAdded" => console.push(json!({ "level": p["entry"]["level"], "text": p["entry"]["text"], "where": p["entry"]["url"] })),
                _ => {}
            }
        }
        use base64::Engine;
        let shot = page.send_timeout("Page.captureScreenshot", json!({ "format": "png", "captureBeyondViewport": args.has("full"), }), Duration::from_secs(15))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(shot["data"].as_str().unwrap_or("")).map_err(|e| Error::msg(format!("screenshot: {e}")))?;
        let out: PathBuf = match args.get("out") {
            Some(p) => PathBuf::from(p),
            None => crate::host::paths::run().join(format!("look-{}.png", crate::host::now_ns())),
        };
        if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&out, &bytes)?;
        let errors = console.iter().filter(|c| matches!(c["level"].as_str(), Some("error" | "exception"))).count();
        let mut text = format!("{} · {}\n", info["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(no title)"), info["url"].as_str().unwrap_or(&url));
        text += &format!("load     {} · {} · {width}×{height}\n", fmt::value(load_ms, "ms"), info["ready"].as_str().unwrap_or("?"));
        for p in &problems {
            text += &format!("!        {p}\n");
        }
        let headings: Vec<String> = info["headings"].as_array().into_iter().flatten().map(|h| format!("{} {}", h["tag"].as_str().unwrap_or(""), h["text"].as_str().unwrap_or(""))).collect();
        for (i, h) in headings.iter().enumerate() {
            text += &format!("{}{h}\n", if i == 0 { "headings " } else { "         " });
        }
        text += &format!("console  {} messages, {errors} errors\n", console.len());
        for c in console.iter().take(20) {
            let at = c["where"].as_str().filter(|w| !w.is_empty()).map(|w| format!("  ({w})")).unwrap_or_default();
            text += &format!("  {:<10}{}{at}\n", c["level"].as_str().unwrap_or(""), c["text"].as_str().unwrap_or("").lines().next().unwrap_or(""));
        }
        if console.len() > 20 {
            text += &format!("  … {} more (--json lists them all)\n", console.len() - 20);
        }
        let body = info["text"].as_str().unwrap_or("");
        let total = info["chars"].as_u64().unwrap_or(0);
        text += &format!("text     {total} characters{}\n", if (total as usize) > body.len() { format!(", the first {} (--chars N for more)", body.chars().count()) } else { String::new() });
        for l in body.lines() {
            text += &format!("  {l}\n");
        }
        text += &format!("shot     {}\n", out.display());
        let json = json!({ "url": info["url"], "title": info["title"], "load_ms": load_ms, "ready": info["ready"], "problems": problems, "headings": info["headings"], "console": console, "errors": errors, "text": body, "chars": total, "screenshot": out });
        let mut o = Output::new(text, json);
        // The page never loaded, or never showed `--until`: a timeout. Its own console errors are the
        // page's, and listed.
        if !problems.is_empty() {
            o.code = 124;
        }
        Ok(o)
    })();
    browser.close();
    result
}
