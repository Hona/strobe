//! WebDriver BiDi over a WebSocket (Firefox's built-in remote protocol, no geckodriver). One reader
//! thread; callers block on replies, as over CDP.

use crate::model::{Error, Result};
use crate::target::a11y::keys::{Key, Named};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tungstenite::protocol::{Role, WebSocket, WebSocketConfig};
use tungstenite::{Message, client};

type Pending = Arc<Mutex<HashMap<u64, Sender<Value>>>>;
type Subscribers = Arc<Mutex<Vec<Sender<Value>>>>;

pub struct Bidi {
    writer: Mutex<WebSocket<TcpStream>>,
    next: AtomicU64,
    pending: Pending,
    subscribers: Subscribers,
    closed: Arc<Mutex<Option<String>>>,
}

impl Bidi {
    /// Connects to a `ws://host:port/session` endpoint (a session is made with `session.new`).
    pub fn connect(url: &str) -> Result<Arc<Bidi>> {
        let rest = url.strip_prefix("ws://").ok_or_else(|| Error::msg(format!("not a ws:// url: {url}")))?;
        let host = rest.split('/').next().unwrap_or(rest);
        // Bounded: a browser that took the port and hangs fails the launch (which a bench runs again)
        // instead of holding it.
        let addr = std::net::ToSocketAddrs::to_socket_addrs(host).ok().and_then(|mut a| a.next()).ok_or_else(|| Error::unreachable(format!("bidi {host}: no address")))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(|e| Error::unreachable(format!("bidi {host}: {e}")))?;
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let config = WebSocketConfig { max_message_size: Some(1 << 30), max_frame_size: Some(1 << 30), ..Default::default() };
        let (reader, _) = client::client_with_config(url, stream.try_clone()?, Some(config)).map_err(|e| Error::unreachable(format!("bidi handshake: {e}")))?;
        stream.set_read_timeout(None).ok();
        let writer = WebSocket::from_raw_socket(stream, Role::Client, None);
        let bidi = Arc::new(Bidi {
            writer: Mutex::new(writer),
            next: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(Mutex::new(None)),
        });
        let (pending, subscribers, closed) = (bidi.pending.clone(), bidi.subscribers.clone(), bidi.closed.clone());
        std::thread::Builder::new().name("bidi-reader".into()).spawn(move || read_loop(reader, pending, subscribers, closed))?;
        Ok(bidi)
    }

    pub fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_timeout(method, params, Duration::from_secs(60))
    }

    /// Sends a command and waits for its result.
    pub fn send_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if let Some(why) = self.closed.lock().unwrap().clone() {
            return Err(Error::unreachable(format!("bidi closed: {why}")));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "id": id, "method": method, "params": params });
        if let Err(e) = self.writer.lock().unwrap().send(Message::text(msg.to_string())) {
            self.pending.lock().unwrap().remove(&id);
            return Err(Error::unreachable(format!("bidi send {method}: {e}")));
        }
        let reply = rx.recv_timeout(timeout).map_err(|e| {
            self.pending.lock().unwrap().remove(&id);
            match (e, self.closed.lock().unwrap().clone()) {
                (RecvTimeoutError::Disconnected, Some(why)) => Error::unreachable(format!("bidi {method}: connection closed: {why}")),
                _ => Error::timeout(format!("bidi {method}: no reply in {}s", timeout.as_secs())),
            }
        })?;
        if reply["type"] == "error" {
            let text = reply["message"].as_str().filter(|m| !m.is_empty()).or(reply["error"].as_str()).unwrap_or("error");
            return Err(Error::msg(format!("bidi {method}: {}", text.lines().next().unwrap_or(text))));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Sends a command without waiting for its reply (dropped when it comes): a paced input stream
    /// whose next move must not wait for the page to handle the last one.
    pub fn post(&self, method: &str, params: Value) -> Result<()> {
        if let Some(why) = self.closed.lock().unwrap().clone() {
            return Err(Error::unreachable(format!("bidi closed: {why}")));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let msg = json!({ "id": id, "method": method, "params": params });
        self.writer.lock().unwrap().send(Message::text(msg.to_string())).map_err(|e| Error::unreachable(format!("bidi send {method}: {e}")))
    }

    /// Every event from now on (those the session subscribed to).
    pub fn events(&self) -> Receiver<Value> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn is_closed(&self) -> bool {
        self.closed.lock().unwrap().is_some()
    }
}

fn read_loop(mut ws: WebSocket<TcpStream>, pending: Pending, subscribers: Subscribers, closed: Arc<Mutex<Option<String>>>) {
    let why = loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(e) => break e.to_string(),
        };
        let text = match &msg {
            Message::Text(t) => t.as_str().to_string(),
            Message::Binary(b) => String::from_utf8_lossy(b).to_string(),
            Message::Close(_) => break "closed by the browser".to_string(),
            _ => continue,
        };
        let value = match super::cdp::parse(&text) {
            Ok(v) => v,
            Err(e) => {
                // A reply strobe cannot read fails its command now (its id may be anywhere in the object).
                let id = regex_lite::Regex::new(r#""id"\s*:\s*(\d+)"#).ok().and_then(|re| re.captures(&text)?.get(1)?.as_str().parse::<u64>().ok());
                if let Some(tx) = id.and_then(|id| pending.lock().unwrap().remove(&id)) {
                    let _ = tx.send(json!({ "type": "error", "message": format!("unreadable reply ({} bytes): {e}", text.len()) }));
                }
                continue;
            }
        };
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(value);
            }
            continue;
        }
        subscribers.lock().unwrap().retain(|tx| tx.send(value.clone()).is_ok());
    };
    *closed.lock().unwrap() = Some(why);
    pending.lock().unwrap().clear();
    subscribers.lock().unwrap().clear();
}

/// A BiDi `RemoteValue` as plain JSON, the way CDP's `returnByValue` gives it: `undefined`, `NaN` and
/// the infinities read as null (JSON has none of them), `-0` as 0, objects and maps as objects (their
/// string keys), arrays and sets as arrays, a date as its string, anything else (a node, a window, a
/// function) as null.
pub fn value(v: &Value) -> Value {
    let items = |v: &Value| v["value"].as_array().cloned().unwrap_or_default();
    match v["type"].as_str().unwrap_or("") {
        "string" | "date" => v["value"].clone(),
        "boolean" => v["value"].clone(),
        "number" => match &v["value"] {
            Value::String(s) if s == "-0" => json!(0.0),
            Value::String(_) => Value::Null,
            n => n.clone(),
        },
        "bigint" => v["value"].as_str().and_then(|s| s.parse::<i64>().ok()).map_or_else(|| v["value"].clone(), |n| json!(n)),
        "array" | "set" => Value::Array(items(v).iter().map(value).collect()),
        "object" | "map" => {
            let mut out = Map::new();
            for pair in items(v) {
                let key = match &pair[0] {
                    Value::String(s) => s.clone(),
                    k => value(k).as_str().map(str::to_string).unwrap_or_else(|| value(k).to_string()),
                };
                out.insert(key, value(&pair[1]));
            }
            Value::Object(out)
        }
        _ => Value::Null,
    }
}

/// A key as WebDriver input names it: a character, or a named key's private-use code point
/// (W3C WebDriver's key table). F13 to F24, CapsLock and ContextMenu have none: a usage error.
pub fn key_value(key: Key) -> Result<String> {
    let code = match key {
        Key::Char(c) => return Ok(c.to_string()),
        Key::Named(n) => match n {
            Named::Enter => '\u{E007}',
            Named::Tab => '\u{E004}',
            Named::Escape => '\u{E00C}',
            Named::Backspace => '\u{E003}',
            Named::Delete => '\u{E017}',
            Named::Insert => '\u{E016}',
            Named::Home => '\u{E011}',
            Named::End => '\u{E010}',
            Named::PageUp => '\u{E00E}',
            Named::PageDown => '\u{E00F}',
            Named::ArrowUp => '\u{E013}',
            Named::ArrowDown => '\u{E015}',
            Named::ArrowLeft => '\u{E012}',
            Named::ArrowRight => '\u{E014}',
            Named::Space => ' ',
            Named::Shift => '\u{E008}',
            Named::Control => '\u{E009}',
            Named::Alt => '\u{E00A}',
            Named::Meta => '\u{E03D}',
            Named::F(k @ 1..=12) => char::from_u32(0xE031 + k as u32 - 1).unwrap_or('\u{E031}'),
            Named::Letter(c) | Named::Digit(c) => (c as char).to_ascii_lowercase(),
            Named::F(_) | Named::CapsLock | Named::ContextMenu => return Err(Error::usage(format!("{n:?}: WebDriver input has no such key, so a Firefox page cannot be sent it"))),
        },
    };
    Ok(code.to_string())
}
