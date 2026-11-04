//! A small blocking D-Bus client (Linux): one connection, method calls with basic arguments and
//! their replies decoded into `Value`, calls sent ahead of their replies (a tree read pipelines
//! hundreds), signals queued for whoever reads them, and file descriptors in replies (logind's
//! inhibitor). Enough for logind and the accessibility bus (AT-SPI). Little-endian wire format,
//! EXTERNAL authentication.

use std::collections::{HashMap, VecDeque};
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
            Value::Bool(b) => Some(*b as i64),
            Value::Variant(v) => v.as_i64(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Double(d) => Some(*d),
            Value::Variant(v) => v.as_f64(),
            other => other.as_i64().map(|i| i as f64),
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
    F64(f64),
    /// `as`
    Strs(&'a [&'a str]),
    /// `v`
    Var(Box<Arg<'a>>),
}

impl Arg<'_> {
    fn sig(&self) -> String {
        match self {
            Arg::Str(_) => "s".into(),
            Arg::Path(_) => "o".into(),
            Arg::I32(_) => "i".into(),
            Arg::U32(_) => "u".into(),
            Arg::Bool(_) => "b".into(),
            Arg::F64(_) => "d".into(),
            Arg::Strs(_) => "as".into(),
            Arg::Var(_) => "v".into(),
        }
    }

    fn put(&self, body: &mut Vec<u8>) {
        match self {
            Arg::Str(s) | Arg::Path(s) => put_str(body, s),
            Arg::I32(v) => put_u32(body, *v as u32),
            Arg::U32(v) => put_u32(body, *v),
            Arg::Bool(v) => put_u32(body, *v as u32),
            Arg::F64(v) => {
                pad(body, 8);
                body.extend_from_slice(&v.to_le_bytes());
            }
            Arg::Strs(list) => {
                pad(body, 4);
                let at = body.len();
                body.extend_from_slice(&[0; 4]);
                let start = body.len();
                for s in *list {
                    put_str(body, s);
                }
                let n = (body.len() - start) as u32;
                body[at..at + 4].copy_from_slice(&n.to_le_bytes());
            }
            Arg::Var(inner) => {
                put_sig(body, &inner.sig());
                inner.put(body);
            }
        }
    }
}

/// A signal: who sent it, from which object, and its body.
#[derive(Clone, Debug)]
pub struct Signal {
    pub sender: String,
    pub path: String,
    pub iface: String,
    pub member: String,
    pub body: Vec<Value>,
}

struct Message {
    kind: u8,
    reply_to: Option<u32>,
    sig: String,
    body: Vec<u8>,
    err: Option<String>,
    sender: String,
    path: String,
    iface: String,
    member: String,
    fds: Vec<OwnedFd>,
}

pub struct Conn {
    stream: UnixStream,
    serial: u32,
    /// Replies that arrived while another was awaited, by the serial they answer.
    pending: HashMap<u32, Message>,
    signals: VecDeque<Signal>,
    /// File descriptor passing was agreed: reads go through `recvmsg`.
    fds: bool,
    /// The bus's unique name for this connection.
    pub name: String,
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
        Conn::open(&system_address(), false)
    }

    /// The system bus, able to receive file descriptors.
    pub fn system_with_fds() -> Result<Conn> {
        Conn::open(&system_address(), true)
    }

    /// The user's session bus (`DBUS_SESSION_BUS_ADDRESS`, else `$XDG_RUNTIME_DIR/bus`).
    pub fn session() -> Result<Conn> {
        let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|a| !a.is_empty()).unwrap_or_else(|| {
            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
            format!("unix:path={dir}/bus")
        });
        Conn::open(&addr, false)
    }

    /// A `unix:path=…` or `unix:abstract=…` address (the first of a `;` list that connects).
    pub fn open(address: &str, fds: bool) -> Result<Conn> {
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
            match stream.and_then(|s| Conn::auth(s, fds)) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn auth(mut stream: UnixStream, fds: bool) -> Result<Conn> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let uid: String = unsafe { libc::getuid() }.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())?;
        let line = read_line(&mut stream)?;
        if !line.starts_with(b"OK ") {
            return Err(Error::new(ErrorKind::PermissionDenied, "D-Bus refused authentication"));
        }
        let mut agreed = false;
        if fds {
            stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
            agreed = read_line(&mut stream)?.starts_with(b"AGREE_UNIX_FD");
        }
        stream.write_all(b"BEGIN\r\n")?;
        let mut c = Conn { stream, serial: 0, pending: HashMap::new(), signals: VecDeque::new(), fds: agreed, name: String::new() };
        let hello = c.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", &[])?;
        c.name = hello.first().and_then(Value::as_str).unwrap_or("").to_string();
        Ok(c)
    }

    /// How long a read may wait for the bus before the call fails.
    pub fn set_timeout(&self, d: Duration) -> Result<()> {
        self.stream.set_read_timeout(Some(d))
    }

    /// Sends a method call without waiting; its reply comes from `reply`.
    pub fn send(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[Arg]) -> Result<u32> {
        self.serial += 1;
        let serial = self.serial;
        let sig: String = args.iter().map(Arg::sig).collect();
        let mut body = Vec::new();
        for a in args {
            a.put(&mut body);
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
        if !iface.is_empty() {
            field(2, "s", iface);
        }
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
        Ok(serial)
    }

    fn wait_reply(&mut self, serial: u32) -> Result<Message> {
        loop {
            if let Some(m) = self.pending.remove(&serial) {
                return Ok(m);
            }
            let m = self.read_message()?;
            match (m.kind, m.reply_to) {
                (4, _) => self.queue_signal(m),
                (_, Some(r)) if r == serial => return Ok(m),
                (_, Some(r)) => {
                    self.pending.insert(r, m);
                }
                _ => {}
            }
        }
    }

    /// The reply to a call `send` made: its body, one value per out argument.
    pub fn reply(&mut self, serial: u32) -> Result<Vec<Value>> {
        let m = self.wait_reply(serial)?;
        let values = decode(&m.sig, &m.body)?;
        match m.kind {
            2 => Ok(values),
            _ => Err(Error::other(format!("{}: {}", m.err.unwrap_or_default(), values.first().and_then(Value::as_str).unwrap_or("")))),
        }
    }

    /// Calls a method and returns its reply's body, one value per out argument.
    pub fn call(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[Arg]) -> Result<Vec<Value>> {
        let s = self.send(dest, path, iface, member, args)?;
        self.reply(s)
    }

    /// Calls a method whose reply carries file descriptors (needs `system_with_fds`).
    pub fn call_fds(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[Arg]) -> Result<(Vec<Value>, Vec<OwnedFd>)> {
        let s = self.send(dest, path, iface, member, args)?;
        let m = self.wait_reply(s)?;
        let values = decode(&m.sig, &m.body)?;
        match m.kind {
            2 => Ok((values, m.fds)),
            _ => Err(Error::other(format!("{}: {}", m.err.unwrap_or_default(), values.first().and_then(Value::as_str).unwrap_or("")))),
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

    /// Receives signals that match `rule` (`type='signal',interface='…'`).
    pub fn add_match(&mut self, rule: &str) -> Result<()> {
        self.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", &[Arg::Str(rule)]).map(|_| ())
    }

    fn queue_signal(&mut self, m: Message) {
        if let Ok(body) = decode(&m.sig, &m.body) {
            self.signals.push_back(Signal { sender: m.sender, path: m.path, iface: m.iface, member: m.member, body });
            if self.signals.len() > 65_536 {
                self.signals.pop_front();
            }
        }
    }

    /// The next signal, waiting up to `wait` for one (replies that arrive meanwhile are kept).
    pub fn signal(&mut self, wait: Duration) -> Result<Option<Signal>> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Some(s) = self.signals.pop_front() {
                return Ok(Some(s));
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if !self.readable(left)? {
                return Ok(None);
            }
            let m = self.read_message()?;
            match (m.kind, m.reply_to) {
                (4, _) => self.queue_signal(m),
                (_, Some(r)) => {
                    self.pending.insert(r, m);
                }
                _ => {}
            }
        }
    }

    /// Whether a message starts arriving within `wait`.
    fn readable(&self, wait: Duration) -> Result<bool> {
        let mut p = libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut p, 1, wait.as_millis().min(i32::MAX as u128) as i32) };
        if n < 0 {
            let e = Error::last_os_error();
            return if e.kind() == ErrorKind::Interrupted { Ok(false) } else { Err(e) };
        }
        if n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR) != 0 && p.revents & libc::POLLIN == 0 {
            return Err(Error::new(ErrorKind::BrokenPipe, "the bus closed the connection"));
        }
        Ok(n > 0)
    }

    fn read_exact(&mut self, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> Result<()> {
        if !self.fds {
            return self.stream.read_exact(buf);
        }
        let mut got = 0;
        while got < buf.len() {
            let mut iov = libc::iovec { iov_base: buf[got..].as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() - got };
            let mut space = [0u64; 32];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = space.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = std::mem::size_of_val(&space) as _;
            let n = unsafe { libc::recvmsg(self.stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
            if n < 0 {
                let e = Error::last_os_error();
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "the bus closed the connection"));
            }
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let data = libc::CMSG_DATA(c) as *const i32;
                        let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                        for k in 0..count {
                            fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(k))));
                        }
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            got += n as usize;
        }
        Ok(())
    }

    fn read_message(&mut self) -> Result<Message> {
        let mut fds = Vec::new();
        let mut head = [0u8; 16];
        self.read_exact(&mut head, &mut fds)?;
        if head[0] != b'l' {
            return Err(Error::other("big-endian D-Bus message"));
        }
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap_or_default()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap_or_default()) as usize;
        let padded = fields_len.div_ceil(8) * 8;
        let mut rest = vec![0u8; padded + body_len];
        self.read_exact(&mut rest, &mut fds)?;
        // Header fields are decoded with offsets relative to the message start (alignment).
        let mut whole = head.to_vec();
        whole.extend_from_slice(&rest[..fields_len]);
        let mut r = Reader { b: &whole, at: 16 };
        let mut m = Message { kind: head[1], reply_to: None, sig: String::new(), body: Vec::new(), err: None, sender: String::new(), path: String::new(), iface: String::new(), member: String::new(), fds };
        while r.at < whole.len() {
            r.align(8);
            if r.at >= whole.len() {
                break;
            }
            let code = r.byte()?;
            let s = r.signature()?;
            let v = r.value(&mut s.chars().peekable())?;
            match (code, v) {
                (1, Value::Str(p)) => m.path = p,
                (2, Value::Str(i)) => m.iface = i,
                (3, Value::Str(n)) => m.member = n,
                (4, Value::Str(e)) => m.err = Some(e),
                (5, Value::UInt(n)) => m.reply_to = Some(n as u32),
                (7, Value::Str(s)) => m.sender = s,
                (8, Value::Str(g)) => m.sig = g,
                _ => {}
            }
        }
        m.body = rest[padded..].to_vec();
        Ok(m)
    }
}

fn system_address() -> String {
    std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap_or_else(|_| "unix:path=/run/dbus/system_bus_socket".into())
}

fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        stream.read_exact(&mut b)?;
        line.push(b[0]);
        if line.len() > 512 {
            break;
        }
    }
    Ok(line)
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
