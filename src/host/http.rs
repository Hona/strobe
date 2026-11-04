//! A minimal HTTP/1.1 server for the daemon (MCP, reports) and the calibration page. Hand-rolled, like mesh.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// A Content-Security-Policy header, enforced on top of any the page sets itself.
    pub csp: Option<String>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response { status, content_type, body: body.into(), csp: None }
    }
    pub fn text(status: u16, body: &str) -> Response {
        Response::new(status, "text/plain; charset=utf-8", body.as_bytes().to_vec())
    }
}

const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    let mut size = 0;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).ok()?;
        size += n;
        if n == 0 || size > MAX_HEADERS {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return None;
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(Request { method, path, headers, body })
}

pub fn write_response(stream: &mut TcpStream, r: &Response) {
    let reason = match r.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
        _ => "Error",
    };
    let csp = r.csp.as_ref().map(|c| format!("Content-Security-Policy: {c}\r\n")).unwrap_or_default();
    let head = format!("HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{csp}Connection: close\r\n\r\n", r.status, r.content_type, r.body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&r.body);
    let _ = stream.flush();
}

/// Serves `handler` on a listener, one thread per connection.
pub fn serve(listener: TcpListener, handler: Arc<dyn Fn(Request) -> Response + Send + Sync>) {
    for stream in listener.incoming().flatten() {
        let handler = handler.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            if let Some(req) = read_request(&mut stream) {
                let resp = handler(req);
                write_response(&mut stream, &resp);
            }
        });
    }
}

/// Starts a static server for in-memory files on a free port; returns the port.
pub fn serve_static(files: Vec<(&'static str, &'static str, &'static str)>) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let handler: Arc<dyn Fn(Request) -> Response + Send + Sync> = Arc::new(move |req: Request| {
        let path = req.path.split('?').next().unwrap_or("/");
        match files.iter().find(|f| f.0 == path) {
            Some(f) => Response::new(200, f.1, f.2.as_bytes().to_vec()),
            None => Response::text(404, "not found"),
        }
    });
    std::thread::spawn(move || serve(listener, handler));
    Ok(port)
}

/// Compares in constant time.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
