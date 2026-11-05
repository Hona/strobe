//! `strobe look <url>`: a page in a headless browser, nothing measured: its title, address, headings,
//! visible text, console messages and uncaught errors, and a screenshot. For checking that a UI
//! loads and says what it should (an agent looking at a page while the session is locked) without
//! writing a browser script.

use super::{Output, fmt, progress};
use crate::cli::{Args, duration_ms};
use crate::model::{Error, Result};
use crate::target::a11y::{Dialect, Selector};
use crate::target::chromium::{self, Window};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// The app's executable is not there: says so, and names the newest build of it a bench made (a
/// directory or git-ref variant's build output, kept in strobe's `builds/out`), to look at instead.
fn missing(app: &crate::target::isolate::App, profile: Option<&str>) -> Error {
    let name = app.exe.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    let newest = std::fs::read_dir(crate::host::paths::state_dir().join("builds").join("out"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|d| d.path().join(&name))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max();
    let e = Error::usage(format!("{} does not exist, so there is no {} to look at", app.exe.display(), app.id));
    let with = profile.map(|id| format!(" --app {id}")).unwrap_or_default();
    match newest {
        Some((_, p)) => e.hint(format!("the newest build of it a bench made is {}: strobe look exe:{}{with}", p.display(), p.display())),
        None => e.hint("install the app, or build it (a bench of a directory or git ref builds it with strobe.json's build), then look at exe:<path> --app <id>"),
    }
}

/// `strobe look app:<id> | exe:<path>`: the native app launched in an isolated home as a bench
/// launches it, nothing measured, then closed: its windows' titles and what its accessibility tree
/// shows (the window names, open dialogs and headings a case's `expect` checks, and the keyed
/// elements selectors name). The tree reads on a locked session too, so an `expect` can be checked
/// before the next unlock.
fn native(args: &Args, target: &str) -> Result<Output> {
    use crate::target::{isolate, native as nt};
    let config = crate::config::load(&crate::config::project_root(&std::env::current_dir()?))?;
    let mut app = match (target.strip_prefix("app:"), target.strip_prefix("exe:"), args.get("app")) {
        (Some(id), _, _) => isolate::app(id, &config)?,
        (_, Some(path), Some(id)) => isolate::app(id, &config)?.with_exe(Path::new(path)),
        (_, Some(path), None) => isolate::App::bare(Path::new(path)),
        _ => return Err(Error::usage("look app:<id> or exe:<path>")),
    };
    app.kind = "native".into();
    // `--build`: this checkout's own build of the app (strobe.json `build`), as a bench of it runs.
    if args.has("build") {
        if target.starts_with("exe:") {
            return Err(Error::usage("--build builds the app of an app:<id> profile from this checkout; exe:<path> names the executable already"));
        }
        app = app.with_exe(&super::bench::build_native(&std::env::current_dir()?)?);
    }
    if !app.exe.is_file() {
        return Err(missing(&app, target.strip_prefix("app:").or(args.get("app"))));
    }
    let wait = Duration::from_millis(args.get("wait").map(duration_ms).transpose()?.unwrap_or(2000));
    let timeout = Duration::from_millis(args.get("timeout").map(duration_ms).transpose()?.unwrap_or(60_000));
    let keys = args.num("keys")?.unwrap_or(40.0).max(0.0) as usize;
    for l in crate::target::isolate::reap_dead_owners() {
        progress(l);
    }
    let home = isolate::prepare_home(&app, args.get("home").map(Path::new), None)?;
    progress(format!("look: launching {} in an isolated home", app.exe.display()));
    let (mut n, guard) = match nt::launch(&app, &home, &[], false, None) {
        Ok(x) => x,
        Err(e) => {
            isolate::release_home(&home.dir);
            isolate::remove_home(&home.dir);
            return Err(e);
        }
    };
    let result = (|| -> Result<Output> {
        let started = Instant::now();
        let mut problems: Vec<String> = Vec::new();
        let main = if app.windowless {
            None
        } else {
            let w = n.wait_window(timeout, &guard)?;
            Some(w.hwnd)
        };
        let window_ms = started.elapsed().as_secs_f64() * 1e3;
        let session = main.map(|m| crate::driver::native::Session::open(&n.pids(), m));
        if let (Some(Ok(se)), Some(sel)) = (&session, args.get("until"))
            && !se.shows(&Value::String(sel.to_string()), timeout)?
        {
            problems.push(format!("{sel:?} never showed in {} s", timeout.as_secs()));
        }
        std::thread::sleep(wait);
        let windows = crate::host::top_windows(&n.pids());
        let mut titles: Vec<String> = windows.iter().filter(|w| Some(w.0) == main).chain(windows.iter().filter(|w| Some(w.0) != main)).map(|w| w.1.trim().to_string()).filter(|t| !t.is_empty()).collect();
        let (mut headings, mut keyed) = (Vec::new(), Vec::new());
        match &session {
            Some(Ok(se)) => {
                match se.screen(Duration::from_secs(5)) {
                    Ok((names, h)) => {
                        for t in names {
                            if !titles.contains(&t) {
                                titles.push(t);
                            }
                        }
                        headings = h;
                    }
                    Err(e) => problems.push(format!("accessibility: {}", e.message)),
                }
                keyed = se.keys(keys).unwrap_or_default();
            }
            Some(Err(e)) => problems.push(format!("accessibility: {}", e.message)),
            None => problems.push("the app has no window (a windowless profile): nothing to read".into()),
        }
        let locked = crate::host::session_locked();
        let mut text = format!("{} · {}\n", titles.first().map(String::as_str).unwrap_or("(no title)"), app.exe.display());
        text += &format!("window   {}{}\n", if main.is_some() { format!("after {}", fmt::value(window_ms, "ms")) } else { "none".into() }, if locked { " · session locked (the tree still reads; pixels and input do not)" } else { "" });
        for p in &problems {
            text += &format!("!        {p}\n");
        }
        for (i, t) in titles.iter().enumerate() {
            text += &format!("{}{t}\n", if i == 0 { "titles   " } else { "         " });
        }
        for (i, h) in headings.iter().enumerate() {
            text += &format!("{}{h}\n", if i == 0 { "headings " } else { "         " });
        }
        if headings.is_empty() && matches!(session, Some(Ok(_))) {
            text += "headings none: the tree marks no open dialog or heading, so an `expect.heading` would refuse this screen\n";
        }
        if !keyed.is_empty() {
            text += &format!("keys     {} visible keyed elements{}\n", keyed.len(), if keyed.len() == keys { format!(" (the first {keys}; --keys N for more)") } else { String::new() });
            for (path, role, name) in &keyed {
                text += &format!("  {path:<32} {role:<12} {name}\n");
            }
        }
        text += "expect   a case's `expect: { title, heading }` matches one of the titles, and one of the headings\n";
        let json = json!({
            "target": target, "exe": app.exe, "window_ms": main.map(|_| window_ms), "session_locked": locked, "titles": titles, "headings": headings,
            "keys": keyed.iter().map(|(p, r, n)| json!({ "path": p, "role": r, "name": n })).collect::<Vec<_>>(), "problems": problems,
        });
        let mut o = Output::new(text, json);
        if !problems.is_empty() {
            o.code = 124;
        }
        Ok(o)
    })();
    n.close(true);
    result
}

pub fn run(args: &Args) -> Result<Output> {
    let target = args.pos.first().ok_or_else(|| Error::usage("look needs a page or an app: strobe look <url | file | app:<id> | exe:<path>> [--until <css>] [--wait 2s] [--out shot.png]"))?;
    if target.starts_with("app:") || target.starts_with("exe:") {
        return native(args, target);
    }
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
    for l in crate::target::isolate::reap_dead_owners() {
        progress(l);
    }
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
        // The same selectors and probe as a bench's page calls and `expect`, so what look says is what they see.
        if let Some(sel) = args.get("until") {
            let sel = Selector::parse(&Value::String(sel.to_string()), Dialect::Page)?;
            if !crate::driver::shows(&page, &sel, timeout)? {
                problems.push(format!("{:?} never showed in {} s", sel.source, timeout.as_secs()));
            }
        }
        std::thread::sleep(Duration::from_millis(wait));
        let info = crate::driver::screen(&page, 6, chars, Duration::from_secs(10))?;
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
        let headings: Vec<&str> = info["headings"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        for (i, h) in headings.iter().enumerate() {
            text += &format!("{}{h}\n", if i == 0 { "headings " } else { "         " });
        }
        if headings.is_empty() {
            text += "headings none: the page shows no open dialog, h1, h2 or level 1-2 role=heading, so an `expect.heading` would refuse it\n";
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
        text += "expect   a case's `expect: { title, heading }` matches the title, and one of the headings\n";
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
