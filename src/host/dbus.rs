//! A small blocking D-Bus client (Linux): one connection, method calls with basic arguments and
//! their replies decoded into `Value`. Enough for logind's lock state and the accessibility bus
//! (AT-SPI); signals are skipped. Little-endian wire format, EXTERNAL authentication.

use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Byte(u8),
    Int(i64),
    UInt(u64),
    Double(f64),
    /// Strings, object paths and signatures.
    Str(String),
    Array(Vec<Value>),
    Struct(Vec<Value>),
    Variant(Box<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Variant(v) => v.as_str(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::UInt(u) => i64::try_from(*u).ok(),
            Value::Byte(b) => Some(*b as i64),
            Value::Variant(v) => v.as_i64(),
            _ => None,
        }
    }

    pub fn fields(&self) -> &[Value] {
        match self {
            Value::Struct(f) | Value::Array(f) => f,
            Value::Variant(v) => v.fields(),
            _ => &[],
        }
    }
}

/// One argument of a call, with its D-Bus type.
pub enum Arg<'a> {
    Str(&'a str),
    Path(&'a str),
    I32(i32),
    U32(u32),
    Bool(bool),
}

impl Arg<'_> {
    fn sig(&self) -> char {
        match self {
            Arg::Str(_) => 's',
            Arg::Path(_) => 'o',
            Arg::I32(_) => 'i',
            Arg::U32(_) => 'u',
            Arg::Bool(_) => 'b',
        }
    }
}

struct Message {
    kind: u8,
    reply_to: Option<u32>,
    sig: String,
    body: Vec<u8>,
    err: Option<String>,
}

pub struct Conn {
    stream: UnixStream,
    serial: u32,
}

fn pad(buf: &mut Vec<u8>, align: usize) {
    while !buf.len().is_multiple_of(align) {
        buf.push(0);
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    pad(buf, 4);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn put_sig(buf: &mut Vec<u8>, s: &str) {
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

impl Conn {
    /// The system bus (`DBUS_SYSTEM_BUS_ADDRESS`, else the standard socket).
    pub fn system() -> Result<Conn> {
        let addr = std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap_or_else(|_| "unix:path=/run/dbus/system_bus_socket".into());
        Conn::open(&addr)
    }

    /// The user's session bus (`DBUS_SESSION_BUS_ADDRESS`, else `$XDG_RUNTIME_DIR/bus`).
    pub fn session() -> Result<Conn> {
        let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|a| !a.is_empty()).unwrap_or_else(|| {
            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
            format!("unix:path={dir}/bus")
        });
        Conn::open(&addr)
    }

    /// A `unix:path=…` or `unix:abstract=…` address (the first of a `;` list that connects).
    pub fn open(address: &str) -> Result<Conn> {
        let mut last = Error::new(ErrorKind::NotFound, format!("no usable D-Bus address in {address}"));
        for a in address.split(';') {
            let Some(params) = a.strip_prefix("unix:") else { continue };
            let get = |k: &str| params.split(',').find_map(|p| p.strip_prefix(k)).map(str::to_string);
            let stream = if let Some(p) = get("path=") {
                UnixStream::connect(p)
            } else if let Some(name) = get("abstract=") {
                use std::os::linux::net::SocketAddrExt;
                std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).and_then(|sa| UnixStream::connect_addr(&sa))
            } else {
                continue;
            };
            match stream.and_then(Conn::auth) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn auth(mut stream: UnixStream) -> Result<Conn> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let uid: String = unsafe { libc::getuid() }.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())?;
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            stream.read_exact(&mut b)?;
            line.push(b[0]);
            if line.len() > 512 {
                break;
            }
        }
        if !line.starts_with(b"OK ") {
            return Err(Error::new(ErrorKind::PermissionDenied, "D-Bus refused authentication"));
        }
        stream.write_all(b"BEGIN\r\n")?;
        let mut c = Conn { stream, serial: 0 };
        c.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", &[])?;
        Ok(c)
    }

    /// Calls a method and returns its reply's body, one value per out argument.
    pub fn call(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[Arg]) -> Result<Vec<Value>> {
        self.serial += 1;
        let serial = self.serial;
        let sig: String = args.iter().map(Arg::sig).collect();
        let mut body = Vec::new();
        for a in args {
            match a {
                Arg::Str(s) | Arg::Path(s) => put_str(&mut body, s),
                Arg::I32(v) => put_u32(&mut body, *v as u32),
                Arg::U32(v) => put_u32(&mut body, *v),
                Arg::Bool(v) => put_u32(&mut body, *v as u32),
            }
        }
        let mut fields = Vec::new();
        let mut field = |code: u8, sig: &str, value: &str| {
            pad(&mut fields, 8);
            fields.push(code);
            put_sig(&mut fields, sig);
            if sig == "g" {
                put_sig(&mut fields, value);
            } else {
                put_str(&mut fields, value);
            }
        };
        field(1, "o", path);
        field(2, "s", iface);
        field(3, "s", member);
        field(6, "s", dest);
        if !sig.is_empty() {
            field(8, "g", &sig);
        }
        let mut msg = vec![b'l', 1, 0, 1];
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad(&mut msg, 8);
        msg.extend_from_slice(&body);
        self.stream.write_all(&msg)?;
        loop {
            let Message { kind, reply_to, sig, body, err } = self.read_message()?;
            if reply_to != Some(serial) {
                continue;
            }
            let values = decode(&sig, &body)?;
            return match kind {
                2 => Ok(values),
                _ => Err(Error::other(format!("{}: {}", err.unwrap_or_default(), values.first().and_then(Value::as_str).unwrap_or("")))),
            };
        }
    }

    /// `org.freedesktop.DBus.Properties.Get`, unwrapped from its variant.
    pub fn get_property(&mut self, dest: &str, path: &str, iface: &str, name: &str) -> Result<Value> {
        let mut r = self.call(dest, path, "org.freedesktop.DBus.Properties", "Get", &[Arg::Str(iface), Arg::Str(name)])?;
        match r.pop() {
            Some(Value::Variant(v)) => Ok(*v),
            Some(v) => Ok(v),
            None => Err(Error::other("empty property reply")),
        }
    }

    fn read_message(&mut self) -> Result<Message> {
        let mut head = [0u8; 16];
        self.stream.read_exact(&mut head)?;
        if head[0] != b'l' {
            return Err(Error::other("big-endian D-Bus message"));
        }
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap_or_default()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap_or_default()) as usize;
        let padded = fields_len.div_ceil(8) * 8;
        let mut rest = vec![0u8; padded + body_len];
        self.stream.read_exact(&mut rest)?;
        // Header fields are decoded with offsets relative to the message start (alignment).
        let mut whole = head.to_vec();
        whole.extend_from_slice(&rest[..fields_len]);
        let mut r = Reader { b: &whole, at: 16 };
        let (mut reply_to, mut sig, mut err) = (None, String::new(), None);
        while r.at < whole.len() {
            r.align(8);
            if r.at >= whole.len() {
                break;
            }
            let code = r.byte()?;
            let s = r.signature()?;
            let v = r.value(&mut s.chars().peekable())?;
            match (code, v) {
                (5, Value::UInt(n)) => reply_to = Some(n as u32),
                (8, Value::Str(g)) => sig = g,
                (4, Value::Str(e)) => err = Some(e),
                _ => {}
            }
        }
        Ok(Message { kind: head[1], reply_to, sig, body: rest[padded..].to_vec(), err })
    }
}

/// Decodes a body with offsets from its own start (bodies start 8-aligned).
pub fn decode(sig: &str, body: &[u8]) -> Result<Vec<Value>> {
    let mut r = Reader { b: body, at: 0 };
    let mut chars = sig.chars().peekable();
    let mut out = Vec::new();
    while chars.peek().is_some() {
        out.push(r.value(&mut chars)?);
    }
    Ok(out)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

fn short() -> Error {
    Error::new(ErrorKind::UnexpectedEof, "short D-Bus message")
}

impl Reader<'_> {
    fn align(&mut self, n: usize) {
        self.at = self.at.div_ceil(n) * n;
    }

    fn take(&mut self, n: usize, align: usize) -> Result<&[u8]> {
        self.align(align);
        let s = self.b.get(self.at..self.at + n).ok_or_else(short)?;
        self.at += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1, 1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4, 4)?.try_into().map_err(|_| short())?))
    }

    fn signature(&mut self) -> Result<String> {
        let n = self.byte()? as usize;
        let s = String::from_utf8_lossy(self.take(n + 1, 1)?).trim_end_matches('\0').to_string();
        Ok(s)
    }

    /// One complete type from `sig`, consumed.
    fn value(&mut self, sig: &mut std::iter::Peekable<std::str::Chars>) -> Result<Value> {
        let c = sig.next().ok_or_else(short)?;
        Ok(match c {
            'y' => Value::Byte(self.byte()?),
            'b' => Value::Bool(self.u32()? != 0),
            'n' => Value::Int(i16::from_le_bytes(self.take(2, 2)?.try_into().map_err(|_| short())?) as i64),
            'q' => Value::UInt(u16::from_le_bytes(self.take(2, 2)?.try_into().map_err(|_| short())?) as u64),
            'i' => Value::Int(self.u32()? as i32 as i64),
            'u' | 'h' => Value::UInt(self.u32()? as u64),
            'x' => Value::Int(i64::from_le_bytes(self.take(8, 8)?.try_into().map_err(|_| short())?)),
            't' => Value::UInt(u64::from_le_bytes(self.take(8, 8)?.try_into().map_err(|_| short())?)),
            'd' => Value::Double(f64::from_le_bytes(self.take(8, 8)?.try_into().map_err(|_| short())?)),
            's' | 'o' => {
                let n = self.u32()? as usize;
                let s = String::from_utf8_lossy(self.take(n, 1)?).to_string();
                self.take(1, 1)?;
                Value::Str(s)
            }
            'g' => Value::Str(self.signature()?),
            'v' => {
                let s = self.signature()?;
                Value::Variant(Box::new(self.value(&mut s.chars().peekable())?))
            }
            'a' => {
                let elem = element(sig)?;
                let n = self.u32()? as usize;
                let first = elem.chars().next().unwrap_or('y');
                self.align(match first {
                    'x' | 't' | 'd' | '(' | '{' => 8,
                    'n' | 'q' => 2,
                    'y' | 'g' | 'v' => 1,
                    _ => 4,
                });
                let end = self.at + n;
                let mut items = Vec::new();
                while self.at < end {
                    items.push(self.value(&mut elem.chars().peekable())?);
                }
                Value::Array(items)
            }
            '(' | '{' => {
                self.align(8);
                let close = if c == '(' { ')' } else { '}' };
                let mut items = Vec::new();
                while sig.peek().is_some_and(|&n| n != close) {
                    items.push(self.value(sig)?);
                }
                sig.next();
                Value::Struct(items)
            }
            other => return Err(Error::other(format!("D-Bus type {other} not handled"))),
        })
    }
}

/// The complete type that starts `sig` (an array's element), consumed from it.
fn element(sig: &mut std::iter::Peekable<std::str::Chars>) -> Result<String> {
    let mut out = String::new();
    let mut depth = 0i32;
    loop {
        let c = sig.next().ok_or_else(short)?;
        out.push(c);
        match c {
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            'a' => continue,
            _ => {}
        }
        if depth == 0 {
            return Ok(out);
        }
    }
}
