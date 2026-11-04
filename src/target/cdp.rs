//! Chrome DevTools Protocol over a WebSocket. One reader thread; callers block on replies.

use crate::model::{Error, Result};
use serde_json::{Value, json};
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

pub struct Cdp {
    writer: Mutex<WebSocket<TcpStream>>,
    next: AtomicU64,
    pending: Pending,
    subscribers: Subscribers,
    closed: Arc<Mutex<Option<String>>>,
}

impl Cdp {
    /// Connects to a `ws://host:port/path` DevTools endpoint.
    pub fn connect(url: &str) -> Result<Arc<Cdp>> {
        let rest = url.strip_prefix("ws://").ok_or_else(|| Error::msg(format!("not a ws:// url: {url}")))?;
        let host = rest.split('/').next().unwrap_or(rest);
        let stream = TcpStream::connect(host).map_err(|e| Error::unreachable(format!("cdp {host}: {e}")))?;
        stream.set_nodelay(true).ok();
        // A CPU profile of a page that spins comes back as one message over tungstenite's 16 MiB default, and a
        // refused message closes the connection.
        let config = WebSocketConfig { max_message_size: Some(1 << 30), max_frame_size: Some(1 << 30), ..Default::default() };
        let (reader, _) = client::client_with_config(url, stream.try_clone()?, Some(config)).map_err(|e| Error::unreachable(format!("cdp handshake: {e}")))?;
        let writer = WebSocket::from_raw_socket(stream, Role::Client, None);
        let cdp = Arc::new(Cdp {
            writer: Mutex::new(writer),
            next: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(Mutex::new(None)),
        });
        let (pending, subscribers, closed) = (cdp.pending.clone(), cdp.subscribers.clone(), cdp.closed.clone());
        std::thread::Builder::new().name("cdp-reader".into()).spawn(move || read_loop(reader, pending, subscribers, closed))?;
        Ok(cdp)
    }

    /// Sends a command (to the browser, or to a session) and waits for its result.
    pub fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.send_timeout(method, params, session, Duration::from_secs(60))
    }

    pub fn send_timeout(&self, method: &str, params: Value, session: Option<&str>, timeout: Duration) -> Result<Value> {
        if let Some(why) = self.closed.lock().unwrap().clone() {
            return Err(Error::unreachable(format!("cdp closed: {why}")));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.writer.lock().unwrap().send(Message::text(msg.to_string())).map_err(|e| Error::unreachable(format!("cdp send {method}: {e}")))?;
        let reply = rx.recv_timeout(timeout).map_err(|e| {
            self.pending.lock().unwrap().remove(&id);
            match (e, self.closed.lock().unwrap().clone()) {
                (RecvTimeoutError::Disconnected, Some(why)) => Error::unreachable(format!("cdp {method}: connection closed: {why}")),
                _ => Error { code: 124, message: format!("cdp {method}: no reply in {}s", timeout.as_secs()), hint: None },
            }
        })?;
        if let Some(err) = reply.get("error") {
            let text = err.get("message").and_then(Value::as_str).unwrap_or("error");
            return Err(Error::msg(format!("cdp {method}: {text}")));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Sends a command without waiting for its reply (dropped when it comes): a paced input stream
    /// whose next event must not wait for the renderer to handle the last one.
    pub fn post(&self, method: &str, params: Value, session: Option<&str>) -> Result<()> {
        if let Some(why) = self.closed.lock().unwrap().clone() {
            return Err(Error::unreachable(format!("cdp closed: {why}")));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.writer.lock().unwrap().send(Message::text(msg.to_string())).map_err(|e| Error::unreachable(format!("cdp send {method}: {e}")))
    }

    /// Every event from now on (all sessions).
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
        let value = match parse(&text) {
            Ok(v) => v,
            Err(e) => {
                // A reply strobe cannot read must fail its command now, not leave it waiting out its timeout.
                if let Some(id) = text.strip_prefix("{\"id\":").and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|d| d.parse::<u64>().ok())
                    && let Some(tx) = pending.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(json!({ "id": id, "error": { "message": format!("unreadable reply ({} bytes): {e}", text.len()) } }));
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
    // Wake every waiter, for replies and for events: neither will come. A waiter left on an event once held a
    // recording for its whole timeout after the connection closed.
    pending.lock().unwrap().clear();
    subscribers.lock().unwrap().clear();
}

/// Parses a CDP message without serde_json's 128-level nesting limit: a sampling heap profile nests
/// two levels per stack frame, and stacks run deeper than 64 frames.
pub fn parse(text: &str) -> serde_json::Result<Value> {
    use serde::Deserialize;
    let mut de = serde_json::Deserializer::from_str(text);
    de.disable_recursion_limit();
    Value::deserialize(&mut de)
}

/// Waits for an event matching `method` (and an optional session) on a subscription.
pub fn wait_event(rx: &Receiver<Value>, method: &str, session: Option<&str>, timeout: Duration) -> Result<Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(Error { code: 124, message: format!("no {method} in {}s", timeout.as_secs()), hint: None });
        }
        let ev = rx.recv_timeout(left).map_err(|e| match e {
            RecvTimeoutError::Disconnected => Error::unreachable(format!("no {method}: the DevTools connection closed")),
            RecvTimeoutError::Timeout => Error { code: 124, message: format!("no {method} in {}s", timeout.as_secs()), hint: None },
        })?;
        if ev.get("method").and_then(Value::as_str) != Some(method) {
            continue;
        }
        if let Some(s) = session
            && ev.get("sessionId").and_then(Value::as_str) != Some(s)
        {
            continue;
        }
        return Ok(ev.get("params").cloned().unwrap_or(Value::Null));
    }
}
