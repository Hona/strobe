//! W3C WebDriver Classic over HTTP (SPEC §13.1): a driver server on loopback (safaridriver,
//! WebKitWebDriver, WPEWebDriver), one connection per command, std `TcpStream` only. A driver runs one
//! command of a session at a time: a command strobe gave up on still holds the session until the
//! browser finishes it.

use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A WebDriver error as the driver names it (`javascript error`, `session not created`, …) and its
/// message.
#[derive(Debug)]
pub struct Failure {
    pub error: String,
    pub message: String,
}

/// A command's outcome: the driver's `value`, or the WebDriver error it answered with. Transport
/// failures (no driver, no reply in time) are the outer `Result`.
pub type Reply = std::result::Result<Value, Failure>;

/// A driver server on `127.0.0.1:<port>`.
#[derive(Clone)]
pub struct Driver {
    port: u16,
}

/// One session of a driver.
pub struct Session {
    driver: Driver,
    pub id: String,
    /// What the driver said the session runs (`browserName`, `browserVersion`, its own keys).
    pub capabilities: Value,
}

impl Driver {
    pub fn new(port: u16) -> Driver {
        Driver { port }
    }

    /// Waits until the driver answers `/status` (it listens), up to `timeout`.
    pub fn wait_ready(&self, timeout: Duration, alive: &mut dyn FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.call("GET", "/status", None, Duration::from_secs(2)).is_ok() {
                return Ok(());
            }
            if !alive() {
                return Err(Error::unreachable("the WebDriver server exited before it listened"));
            }
            if Instant::now() > deadline {
                return Err(Error::unreachable(format!("the WebDriver server did not listen on port {} in {} s", self.port, timeout.as_secs())));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// `POST /session` with these `alwaysMatch` capabilities.
    pub fn new_session(&self, capabilities: Value, timeout: Duration) -> Result<std::result::Result<Session, Failure>> {
        Ok(self.call("POST", "/session", Some(&json!({ "capabilities": { "alwaysMatch": capabilities } })), timeout)?.map(|v| Session {
            driver: self.clone(),
            id: v["sessionId"].as_str().unwrap_or("").to_string(),
            capabilities: v["capabilities"].clone(),
        }))
    }

    /// One command: an HTTP request on a fresh connection, its JSON `value` or WebDriver error back.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<Reply> {
        let what = || format!("webdriver {method} {path}");
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).map_err(|e| Error::unreachable(format!("{}: {e}", what())))?;
        stream.set_nodelay(true).ok();
        stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
        // A read waits as long as the command may take, and a little longer for the reply itself.
        stream.set_read_timeout(Some(timeout + Duration::from_secs(1))).ok();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/json\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.port,
            body.len()
        );
        stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes())).map_err(|e| Error::unreachable(format!("{}: {e}", what())))?;
        let started = Instant::now();
        let (status, text) = read_response(stream).map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Error::timeout(format!("{}: no reply in {} s", what(), started.elapsed().as_secs())),
            _ => Error::unreachable(format!("{}: {e}", what())),
        })?;
        let v: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).map_err(|e| Error::msg(format!("{}: not JSON ({e}): {}", what(), text.chars().take(200).collect::<String>())))?
        };
        let value = v.get("value").cloned().unwrap_or(Value::Null);
        if status >= 400 || value.get("error").and_then(Value::as_str).is_some() {
            return Ok(Err(Failure {
                error: value["error"].as_str().unwrap_or("unknown error").to_string(),
                message: value["message"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string(),
            }));
        }
        Ok(Ok(value))
    }
}

/// The status code and body of one HTTP/1.1 response (Content-Length, chunked, or to the end).
fn read_response(stream: TcpStream) -> std::io::Result<(u16, String)> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("not an HTTP response: {line:?}")))?;
    let (mut length, mut chunked) = (None, false);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = v.trim().parse::<usize>().ok(),
                "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            r.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut chunk = vec![0u8; n + 2];
            r.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(n) = length {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

impl Failure {
    pub fn text(&self) -> String {
        if self.message.is_empty() { self.error.clone() } else { format!("{}: {}", self.error, self.message) }
    }
}

impl Session {
    /// A command of this session; a WebDriver error becomes an error naming the command.
    pub fn send(&self, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<Value> {
        let full = format!("/session/{}{path}", self.id);
        self.driver.call(method, &full, body, timeout)?.map_err(|f| Error::msg(format!("webdriver {}: {}", if path.is_empty() { "session" } else { path.trim_start_matches('/') }, f.text())))
    }

    /// The same, keeping the WebDriver error typed for the caller to tell apart.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<Reply> {
        self.driver.call(method, &format!("/session/{}{path}", self.id), body, timeout)
    }

    /// `script` as a function body run asynchronously: its last argument is the callback it answers with.
    pub fn execute_async(&self, script: &str, timeout: Duration) -> Result<Reply> {
        self.call("POST", "/execute/async", Some(&json!({ "script": script, "args": [] })), timeout)
    }

    /// Input actions (`POST /actions`), back once the browser has dispatched them all.
    pub fn actions(&self, actions: &Value, timeout: Duration) -> Result<()> {
        self.send("POST", "/actions", Some(&json!({ "actions": actions })), timeout).map(|_| ())
    }

    /// The top-level viewport as PNG bytes.
    pub fn screenshot(&self, timeout: Duration) -> Result<Vec<u8>> {
        let v = self.send("GET", "/screenshot", None, timeout)?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, v.as_str().unwrap_or("")).map_err(|e| Error::msg(format!("screenshot: {e}")))
    }

    /// The window's place and outer size, as the driver reads it: (x, y, width, height).
    pub fn window_rect(&self) -> Result<(f64, f64, f64, f64)> {
        let v = self.send("GET", "/window/rect", None, Duration::from_secs(10))?;
        let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
        Ok((n("x"), n("y"), n("width"), n("height")))
    }

    pub fn set_window_rect(&self, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        self.send("POST", "/window/rect", Some(&json!({ "x": x, "y": y, "width": width, "height": height })), Duration::from_secs(10)).map(|_| ())
    }

    /// Ends the session (the driver closes its windows).
    pub fn delete(&self, timeout: Duration) -> Result<()> {
        self.send("DELETE", "", None, timeout).map(|_| ())
    }
}
