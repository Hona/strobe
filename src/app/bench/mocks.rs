//! `ctx.mock(pattern, handler)` on a page whose engine gives the runner no CDP session (Firefox): strobe
//! intercepts the matching requests through WebDriver BiDi (`network.addIntercept`, then
//! `provideResponse` or `continueRequest`), which leaves the HTTP cache on as Chromium's Fetch domain
//! does, and asks the runner's handlers over a loopback socket of their own. Firefox allows one BiDi
//! connection per browser, so the runner cannot intercept itself. BiDi's patterns have no wildcards: the
//! intercept is the narrowest one the glob allows (its literal origin, else the page's every request),
//! and a paused request no glob matches goes on at once.

use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct Mocks {
    relay: RefCell<Option<Relay>>,
}

struct Relay {
    globs: Arc<Mutex<Vec<String>>>,
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Mocks {
    /// The runner's `mock` request: intercept `pattern`'s requests, and say where the runner answers them.
    pub(super) fn add(&self, page: &Page, a: &Value) -> Result<Value> {
        let p = match page {
            Page::Firefox(p) => p,
            Page::Chromium(_) => return Err(Error::msg("internal: a Chromium page's mocks go through the runner's own CDP session")),
        };
        let pattern = a["pattern"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| Error::usage("mock(pattern, handler): pattern is a URL glob"))?.to_string();
        let mut relay = self.relay.borrow_mut();
        if relay.is_none() {
            *relay = Some(start(p)?);
        }
        let r = relay.as_ref().ok_or_else(|| Error::msg("internal: no mock relay"))?;
        r.globs.lock().unwrap().push(pattern.clone());
        let mut intercept = json!({ "phases": ["beforeRequestSent"], "contexts": [p.context] });
        if let Some(u) = url_pattern(&pattern) {
            intercept["urlPatterns"] = json!([u]);
        }
        p.bidi.send("network.addIntercept", intercept)?;
        Ok(json!({ "port": r.port, "token": r.token }))
    }
}

/// The relay thread: paused requests in, the runner's answers out.
fn start(p: &crate::target::firefox::Page) -> Result<Relay> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let token = host::paths::csprng_hex(16)?;
    let (globs, stop) = (Arc::new(Mutex::new(Vec::<String>::new())), Arc::new(AtomicBool::new(false)));
    p.bidi.send("session.subscribe", json!({ "events": ["network.beforeRequestSent"], "contexts": [p.context] }))?;
    let rx = p.bidi.events();
    let (bidi, context, g, s, t) = (p.bidi.clone(), p.context.clone(), globs.clone(), stop.clone(), token.clone());
    std::thread::Builder::new().name("mock-relay".into()).spawn(move || {
        let mut conn: Option<(TcpStream, BufReader<TcpStream>)> = None;
        while !s.load(Ordering::Relaxed) {
            let ev = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => ev,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            let q = &ev["params"];
            if ev["method"] != "network.beforeRequestSent" || q["isBlocked"] != true || q["context"].as_str() != Some(context.as_str()) {
                continue;
            }
            let (id, url) = (q["request"]["request"].as_str().unwrap_or("").to_string(), q["request"]["url"].as_str().unwrap_or("").to_string());
            if !g.lock().unwrap().iter().any(|p| glob(p, &url)) {
                let _ = bidi.post("network.continueRequest", json!({ "request": id }));
                continue;
            }
            let headers: serde_json::Map<String, Value> = q["request"]["headers"].as_array().into_iter().flatten().filter_map(|h| Some((h["name"].as_str()?.to_lowercase(), h["value"]["value"].clone()))).collect();
            let ask = json!({ "id": id, "url": url, "method": q["request"]["method"], "headers": headers });
            let answer = ask_runner(&listener, &mut conn, &t, &ask);
            let _ = match answer {
                Some(a) if a["pass"] == true => bidi.post("network.continueRequest", json!({ "request": id })),
                Some(a) => {
                    let headers: Vec<Value> = a["headers"].as_array().into_iter().flatten().map(|h| json!({ "name": h[0], "value": { "type": "string", "value": h[1] } })).collect();
                    bidi.post(
                        "network.provideResponse",
                        json!({ "request": id, "statusCode": a["status"].as_u64().unwrap_or(200), "reasonPhrase": "", "headers": headers, "body": { "type": "base64", "value": a["body"].as_str().unwrap_or("") } }),
                    )
                }
                // The runner is gone or its handler failed (it logged why): the page sees a failed request.
                None => bidi.post("network.failRequest", json!({ "request": id })),
            };
        }
    })?;
    Ok(Relay { globs, port, token, stop })
}

/// One paused request to the runner and its answer (`{ pass }` or `{ status, headers, body }`), over its
/// relay connection, opened on the first request (the runner connects when its first `mock` returns).
fn ask_runner(listener: &TcpListener, conn: &mut Option<(TcpStream, BufReader<TcpStream>)>, token: &str, ask: &Value) -> Option<Value> {
    if conn.is_none() {
        listener.set_nonblocking(true).ok()?;
        // Until the deadline: a connection that does not say the token in time (another local process)
        // is dropped and the next one is taken.
        let deadline = Instant::now() + Duration::from_secs(10);
        *conn = loop {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => return None,
            };
            stream.set_nonblocking(false).ok()?;
            stream.set_nodelay(true).ok();
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut reader = BufReader::new(stream.try_clone().ok()?);
            let mut first = String::new();
            let said = reader.by_ref().take(256).read_line(&mut first).is_ok() && first.trim() == token;
            if said {
                stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
                break Some((stream, reader));
            }
        };
    }
    let (stream, reader) = conn.as_mut()?;
    let ok = writeln!(stream, "{ask}").is_ok();
    let mut line = String::new();
    if !ok || reader.read_line(&mut line).ok()? == 0 {
        *conn = None;
        return None;
    }
    serde_json::from_str::<Value>(&line).ok().filter(|a| a["error"].is_null())
}

/// `*` any run of characters, `?` one, over the whole URL, as the runner matches its handlers. Greedy
/// with one backtrack point (the last `*`): no exponential case on a URL full of near-matches.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < s.len() {
        if i < p.len() && (p[i] == '?' || p[i] == s[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(st) = star {
            i = st + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// The narrowest BiDi URL pattern that holds every URL the glob matches: the URL itself when it has no
/// wildcard; its origin and path when only the query has one (`…/api/data?id=*`); its origin when that
/// part is literal (`http://127.0.0.1:4173/api/*`); else none (every request of the page pauses, and
/// strobe lets the others go).
fn url_pattern(g: &str) -> Option<Value> {
    if !g.contains(['*', '?']) {
        return Some(json!({ "type": "string", "pattern": g }));
    }
    let (scheme, rest) = g.split_once("://")?;
    let authority = rest.split('/').next()?;
    if scheme.contains(['*', '?']) || authority.contains(['*', '?']) || !rest.contains('/') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    };
    let mut u = json!({ "type": "pattern", "protocol": scheme, "hostname": host.trim_start_matches('[').trim_end_matches(']') });
    if let Some(p) = port {
        u["port"] = json!(p);
    }
    // A `?` with query text after it starts the query; the path before it, literal, is the pattern's.
    let after = &rest[authority.len()..];
    if let Some((path, query)) = after.split_once('?')
        && !path.contains('*')
        && (query.contains('=') || query.contains('*'))
    {
        u["pathname"] = json!(path);
    }
    Some(u)
}
