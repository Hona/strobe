//! The native bridge (SPEC §13.7, protocol `native-bridge-v1`): strobe listens on loopback, a native
//! app whose UI toolkit implements the bridge connects out (`STROBE_BRIDGE`, `STROBE_BRIDGE_TOKEN`),
//! and strobe drives it and reads its run records over newline-delimited JSON.

use crate::host;
use crate::model::{Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Where a launch's bridge connects: a loopback port and a token only that launch gets.
pub struct Listener {
    listener: TcpListener,
    pub port: u16,
    pub token: String,
}

/// The app's first line: who it is and how it runs.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Hello {
    pub pid: u32,
    pub toolkit: String,
    pub toolkit_version: String,
    /// `window` (real windows, real presentation) or `headless` (offscreen, virtual clock).
    pub mode: String,
    pub debug_build: bool,
    pub clock: String,
    pub now_ns: u64,
}

/// A connected bridge. Requests and replies are matched by id; the app's unsolicited events are kept.
pub struct Bridge {
    conn: TcpStream,
    lines: Receiver<Value>,
    next: Cell<u64>,
    pub hello: Hello,
    pub events: RefCell<Vec<Value>>,
}

/// The clock name a bridge must report on this OS: the one `host::now_ns` reads.
pub const CLOCK: &str = if cfg!(windows) {
    "qpc"
} else if cfg!(target_os = "macos") {
    "mach"
} else {
    "monotonic"
};

/// 32 hex characters from the OS's random keys (std's hash seeds).
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2u64)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(host::now_ns() ^ i);
            format!("{:016x}", h.finish())
        })
        .collect()
}

impl Listener {
    pub fn open() -> Result<Listener> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        Ok(Listener { port: listener.local_addr()?.port(), listener, token: token() })
    }

    /// The environment that tells the app where to connect.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("STROBE_BRIDGE".into(), format!("127.0.0.1:{}", self.port)), ("STROBE_BRIDGE_TOKEN".into(), self.token.clone())]
    }

    /// The app's connection, once it says hello with the right token, waiting up to `timeout` while
    /// `alive` holds. A connection with a wrong token or no hello is closed and strobe keeps waiting.
    pub fn accept(&self, timeout: Duration, alive: &dyn Fn() -> bool) -> Result<Bridge> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((conn, _)) => {
                    if let Some(b) = self.greet(conn)? {
                        return Ok(b);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(Error::msg(format!("bridge: {e}"))),
            }
            if !alive() {
                return Err(Error::msg("the app exited before its bridge connected"));
            }
            if Instant::now() > deadline {
                return Err(Error::unreachable(format!("the app's bridge did not connect in {} s", timeout.as_secs()))
                    .hint("build the app with its UI toolkit's strobe bridge (a Cargo feature, for example); without it strobe measures the app only from outside"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Accepts a connection already waiting, without blocking.
    pub fn try_accept(&self) -> Result<Option<Bridge>> {
        match self.listener.accept() {
            Ok((conn, _)) => self.greet(conn),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(Error::msg(format!("bridge: {e}"))),
        }
    }

    fn greet(&self, conn: TcpStream) -> Result<Option<Bridge>> {
        conn.set_nonblocking(false)?;
        conn.set_nodelay(true)?;
        conn.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut first = String::new();
        if reader.read_line(&mut first).is_err() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(first.trim()).unwrap_or(Value::Null);
        if v["hello"] != 1 || v["token"].as_str() != Some(self.token.as_str()) {
            let _ = conn.shutdown(std::net::Shutdown::Both);
            return Ok(None);
        }
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let hello = Hello {
            pid: v["pid"].as_u64().unwrap_or(0) as u32,
            toolkit: s("toolkit"),
            toolkit_version: s("toolkit_version"),
            mode: s("mode"),
            debug_build: v["debug_build"] == true,
            clock: s("clock"),
            now_ns: v["now_ns"].as_u64().unwrap_or(0),
        };
        conn.set_read_timeout(None)?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in reader.lines().map_while(std::result::Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Ok(Some(Bridge { conn, lines, next: Cell::new(1), hello, events: RefCell::new(Vec::new()) }))
    }
}

impl Bridge {
    /// Sends `op` and waits for its reply up to `timeout`. A reply with `err` fails with the app's
    /// code and message; `unsupported` names the op.
    pub fn request(&self, op: &str, args: Value, timeout: Duration) -> Result<Value> {
        let id = self.next.get();
        self.next.set(id + 1);
        let mut msg = json!({ "id": id, "op": op });
        if !args.is_null() {
            msg["args"] = args;
        }
        (&self.conn).write_all(format!("{msg}\n").as_bytes()).map_err(|e| Error::msg(format!("bridge {op}: the app closed its bridge ({e})")))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = match self.lines.recv_timeout(left) {
                Ok(v) => v,
                Err(RecvTimeoutError::Timeout) => return Err(Error { code: 124, message: format!("bridge {op}: the app did not answer in {} s", timeout.as_secs()), hint: None }),
                Err(RecvTimeoutError::Disconnected) => return Err(Error::msg(format!("bridge {op}: the app closed its bridge (did it exit?)"))),
            };
            if v.get("event").is_some() {
                self.events.borrow_mut().push(v);
                continue;
            }
            if v["id"] != json!(id) {
                continue;
            }
            if let Some(e) = v.get("err") {
                let code = e["code"].as_str().unwrap_or("internal");
                let message = e["message"].as_str().unwrap_or("");
                return Err(match code {
                    "unsupported" => Error::usage(format!("the app's bridge does not support {op}{}", if message.is_empty() { String::new() } else { format!(": {message}") })),
                    "timeout" | "not-found" => Error { code: 124, message: format!("{op}: {message}"), hint: None },
                    _ => Error::msg(format!("bridge {op}: {code}{}", if message.is_empty() { String::new() } else { format!(": {message}") })),
                });
            }
            return Ok(v["ok"].clone());
        }
    }

    /// Asks the app to quit through its own quit path; returns once it answered (or did not, in 2 s).
    pub fn quit(&self) {
        let _ = self.request("quit", Value::Null, Duration::from_secs(2));
    }
}
