//! iOS devices on a cable: usbmuxd (the OS's multiplexer), the pair record it keeps, lockdownd and
//! its services over mutual TLS, and the battery. Strictly read-only toward the phone: strobe never
//! pairs, unpairs, sets a value, installs anything or sends a command that changes the device; it
//! reads with the trust the user already gave this computer.
//!
//! usbmuxd listens on `/var/run/usbmuxd` (macOS's own; Linux's usbmuxd daemon, which udev starts
//! only while a phone is plugged in) or 127.0.0.1:27015 (Windows: Apple Mobile Device Service);
//! `USBMUXD_SOCKET_ADDRESS` (`UNIX:<path>` or `<host>:<port>`) overrides it, as libusbmuxd's does.
//! Its messages are a 16-byte little-endian header and an XML plist. After `Connect` the socket is a
//! raw tunnel to a port on the device: lockdownd on 62078, whose messages are a 4-byte big-endian
//! length and a plist, then TLS once a session starts. A service lockdownd starts is a second
//! tunnel, under TLS when it says so.

mod tls;

use crate::model::{Error, Result};
use plist::{Dictionary, Value};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde::Serialize;
use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A byte stream to the device: a usbmuxd tunnel, or TLS over one. Every service layer (lockdownd,
/// diagnostics_relay, Web Inspector, os_trace_relay, AFC) reads and writes through it.
pub trait Io: Read + Write + Send {}
impl<T: Read + Write + Send> Io for T {}

/// The longest one read or write may block: the device answers in milliseconds, and a phone that
/// stopped answering must not hang strobe.
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const LOCKDOWN_PORT: u16 = 62078;
#[cfg(unix)]
const SOCKET: &str = "/var/run/usbmuxd";
/// No plist strobe reads from a device comes near this; a larger length is a broken stream.
const MAX_MESSAGE: usize = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Connection {
    #[serde(rename = "usb")]
    Usb,
    #[serde(rename = "wifi")]
    Network,
}

/// One entry of usbmuxd's device list. A phone on the cable that also syncs over Wi-Fi is two
/// entries with one `udid`, each with its own `id`.
#[derive(Clone, Debug)]
pub struct Device {
    /// usbmuxd's handle for this attachment (changes on every replug).
    pub id: u64,
    pub udid: String,
    pub connection: Connection,
}

/// The id to show for a phone: the UDID's last 8 characters. A current UDID is
/// `<8 hex chip id>-<16 hex ECID>`, so its first characters are the same for every phone of a model.
pub fn short_udid(udid: &str) -> String {
    let chars: Vec<char> = udid.chars().collect();
    chars[chars.len().saturating_sub(8)..].iter().collect()
}

// sockets and deadlines

/// The deadline of a tunnel's reads and writes, shared with its socket so it still applies once TLS
/// wraps the socket. (A second handle on the socket would not do: on Windows a duplicated socket's
/// timeouts are its own.)
#[derive(Clone)]
struct Ctl(Arc<Mutex<Option<Instant>>>);

impl Ctl {
    fn set(&self, deadline: Option<Instant>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = deadline;
    }
    /// The timeout of the next read or write: what is left of the deadline, at most `IO_TIMEOUT`.
    fn left(&self) -> std::io::Result<Duration> {
        match *self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(d) => d.checked_duration_since(Instant::now()).filter(|l| !l.is_zero()).map(|l| l.min(IO_TIMEOUT)).ok_or_else(|| std::io::ErrorKind::TimedOut.into()),
            None => Ok(IO_TIMEOUT),
        }
    }
}

enum Stream {
    Tcp(std::net::TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

/// A socket to usbmuxd whose every read and write is bounded by its `Ctl`.
struct Sock {
    stream: Stream,
    ctl: Ctl,
}

impl Sock {
    fn arm(&self) -> std::io::Result<()> {
        let t = Some(self.ctl.left()?);
        match &self.stream {
            Stream::Tcp(s) => s.set_read_timeout(t).and(s.set_write_timeout(t)),
            #[cfg(unix)]
            Stream::Unix(s) => s.set_read_timeout(t).and(s.set_write_timeout(t)),
        }
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm()?;
        match &mut self.stream {
            Stream::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm()?;
        match &mut self.stream {
            Stream::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn late() -> Error {
    Error::timeout("the device did not answer in time")
}

/// An I/O error, a timeout as a timeout.
fn io(e: std::io::Error) -> Error {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => late(),
        _ => Error::msg(e.to_string()),
    }
}

/// Packages whose files mean Linux's usbmuxd is installed (udev starts it when a phone appears).
#[cfg(target_os = "linux")]
const LINUX_USBMUXD: &[&str] = &["/usr/sbin/usbmuxd", "/sbin/usbmuxd", "/usr/bin/usbmuxd", "/lib/udev/rules.d/39-usbmuxd.rules", "/usr/lib/udev/rules.d/39-usbmuxd.rules"];

fn mux_socket(deadline: Option<Instant>) -> Result<(Box<dyn Io>, Ctl)> {
    let tcp = |addr: &str, limit: Duration| -> Result<Stream> {
        let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(addr).map_err(|e| Error::msg(format!("usbmuxd address {addr}: {e}")))?.collect();
        let mut last = format!("{addr} resolves to nothing");
        for sa in addrs {
            match std::net::TcpStream::connect_timeout(&sa, limit) {
                Ok(s) => {
                    s.set_nodelay(true)?;
                    return Ok(Stream::Tcp(s));
                }
                Err(e) => last = format!("nothing answers on {sa}: {e}"),
            }
        }
        Err(Error::msg(last))
    };
    #[cfg(unix)]
    let unix = |path: &str| -> Result<Stream> {
        let s = std::os::unix::net::UnixStream::connect(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => Error::msg(format!("no permission to open {path} (its owner and mode decide who may talk to usbmuxd)")),
            _ => Error::msg(format!("{path}: {e}")),
        })?;
        Ok(Stream::Unix(s))
    };
    let stream = match std::env::var("USBMUXD_SOCKET_ADDRESS").ok().filter(|a| !a.is_empty()) {
        Some(addr) => match addr.strip_prefix("UNIX:") {
            #[cfg(unix)]
            Some(path) => unix(path)?,
            #[cfg(not(unix))]
            Some(path) => return Err(Error::msg(format!("USBMUXD_SOCKET_ADDRESS names a Unix socket ({path}); this OS has usbmuxd on TCP"))),
            None => tcp(&addr, Duration::from_secs(1))?,
        },
        #[cfg(unix)]
        None => unix(SOCKET)?,
        // Windows retries a refused loopback connect for about 2 s; a listening usbmuxd answers at once.
        #[cfg(windows)]
        None => tcp("127.0.0.1:27015", Duration::from_millis(200))?,
    };
    let ctl = Ctl(Arc::new(Mutex::new(deadline)));
    Ok((Box::new(Sock { stream, ctl: ctl.clone() }), ctl))
}

/// Why usbmuxd is unreachable, in the user's terms.
fn no_mux(e: Error) -> Error {
    if e.code == 124 {
        return e;
    }
    let fix = if cfg!(target_os = "macos") {
        "macOS runs it as part of the OS"
    } else if cfg!(windows) {
        "install Apple Devices from the Microsoft Store and keep it open, or iTunes: their Apple Mobile Device Service is usbmuxd"
    } else {
        "install usbmuxd (most distributions package it); udev starts it when a phone is plugged in"
    };
    Error::msg(format!("no usbmuxd ({})", e.message)).hint(fix)
}

/// One request and its reply on a fresh multiplexer connection, which stays open for a `Connect`.
fn mux_request(msg_type: &str, mut fields: Dictionary, deadline: Option<Instant>) -> Result<(Box<dyn Io>, Ctl, Dictionary)> {
    let (mut s, ctl) = mux_socket(deadline).map_err(no_mux)?;
    fields.insert("MessageType".into(), msg_type.into());
    fields.insert("ProgName".into(), "strobe".into());
    fields.insert("ClientVersionString".into(), concat!("strobe-", env!("CARGO_PKG_VERSION")).into());
    fields.insert("kLibUSBMuxVersion".into(), 3u64.into());
    let mut body = Vec::new();
    Value::Dictionary(fields).to_writer_xml(&mut body).map_err(|e| Error::msg(format!("usbmuxd request: {e}")))?;
    let mut msg = Vec::with_capacity(16 + body.len());
    // length, version 1 (plist), message 8 (plist), tag
    for word in [16 + body.len() as u32, 1, 8, 1] {
        msg.extend_from_slice(&word.to_le_bytes());
    }
    msg.extend_from_slice(&body);
    s.write_all(&msg).map_err(io)?;
    let mut head = [0u8; 16];
    s.read_exact(&mut head).map_err(io)?;
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if !(16..=MAX_MESSAGE).contains(&len) {
        return Err(Error::msg(format!("usbmuxd sent a {len}-byte message")));
    }
    let mut body = vec![0u8; len - 16];
    s.read_exact(&mut body).map_err(io)?;
    let reply = parse(&body).map_err(|e| Error::msg(format!("usbmuxd reply to {msg_type}: {}", e.message)))?;
    Ok((s, ctl, reply))
}

/// usbmuxd's `Result` numbers (0 is success).
fn mux_result(reply: &Dictionary, what: &str) -> Result<()> {
    match reply.get("Number").and_then(Value::as_unsigned_integer) {
        Some(0) => Ok(()),
        Some(n) => Err(Error::msg(format!(
            "usbmuxd refused {what}: {}",
            match n {
                1 => "bad command".to_string(),
                2 => "no such device, or no pair record for it".to_string(),
                3 => "connection refused by the device".to_string(),
                6 => "bad protocol version".to_string(),
                n => format!("result {n}"),
            }
        ))),
        None => Err(Error::msg(format!("usbmuxd sent no result for {what}"))),
    }
}

/// The devices usbmuxd sees, cable and Wi-Fi alike. Err only when usbmuxd itself is unreachable.
pub fn list(deadline: Option<Instant>) -> Result<Vec<Device>> {
    // Linux's usbmuxd has no socket until udev starts it for a plugged-in phone.
    #[cfg(target_os = "linux")]
    if std::env::var_os("USBMUXD_SOCKET_ADDRESS").is_none() && !std::path::Path::new(SOCKET).exists() && LINUX_USBMUXD.iter().any(|p| std::path::Path::new(p).exists()) {
        return Ok(Vec::new());
    }
    let (_, _, reply) = mux_request("ListDevices", Dictionary::new(), deadline)?;
    let mut out = Vec::new();
    for d in reply.get("DeviceList").and_then(Value::as_array).into_iter().flatten() {
        let Some(p) = d.as_dictionary().and_then(|d| d.get("Properties")).and_then(Value::as_dictionary) else { continue };
        let (Some(id), Some(udid)) = (p.get("DeviceID").and_then(Value::as_unsigned_integer), p.get("SerialNumber").and_then(Value::as_string)) else { continue };
        let connection = match p.get("ConnectionType").and_then(Value::as_string) {
            Some("USB") => Connection::Usb,
            Some("Network") => Connection::Network,
            _ => continue,
        };
        out.push(Device { id, udid: udid.to_string(), connection });
    }
    Ok(out)
}

/// A raw tunnel to `port` on the device.
fn mux_connect(dev: &Device, port: u16, deadline: Option<Instant>) -> Result<(Box<dyn Io>, Ctl)> {
    let mut f = Dictionary::new();
    f.insert("DeviceID".into(), dev.id.into());
    // usbmuxd wants the port in network byte order, read as a native integer.
    f.insert("PortNumber".into(), u64::from(port.swap_bytes()).into());
    let (s, ctl, reply) = mux_request("Connect", f, deadline)?;
    mux_result(&reply, &format!("a connection to port {port}"))?;
    Ok((s, ctl))
}

/// What this computer holds to talk to a device it was trusted by: its host identity and the
/// certificates exchanged when the user tapped Trust. usbmuxd hands it to any local user.
pub struct PairRecord {
    host_id: String,
    system_buid: String,
    host_cert: CertificateDer<'static>,
    host_key: PrivateKeyDer<'static>,
    device_cert: CertificateDer<'static>,
}

/// The pair record for `udid`. Err when this computer was never trusted by the device.
pub fn pair_record(udid: &str, deadline: Option<Instant>) -> Result<PairRecord> {
    let mut f = Dictionary::new();
    f.insert("PairRecordID".into(), udid.into());
    let (_, _, reply) = mux_request("ReadPairRecord", f, deadline)?;
    let Some(data) = reply.get("PairRecordData").and_then(Value::as_data) else {
        mux_result(&reply, "the pair record")?;
        return Err(Error::msg("usbmuxd sent no pair record"));
    };
    let rec = parse(data)?;
    let text = |k: &str| rec.get(k).and_then(Value::as_string).map(str::to_string).ok_or_else(|| Error::msg(format!("the pair record has no {k}")));
    let pem = |k: &str| rec.get(k).and_then(Value::as_data).ok_or_else(|| Error::msg(format!("the pair record has no {k}")));
    let cert = |k: &str| CertificateDer::from_pem_slice(pem(k)?).map(|c| c.into_owned()).map_err(|e| Error::msg(format!("the pair record's {k}: {e}")));
    Ok(PairRecord {
        host_id: text("HostID")?,
        system_buid: text("SystemBUID")?,
        host_cert: cert("HostCertificate")?,
        host_key: PrivateKeyDer::from_pem_slice(pem("HostPrivateKey")?).map_err(|e| Error::msg(format!("the pair record's HostPrivateKey: {e}")))?,
        device_cert: cert("DeviceCertificate")?,
    })
}

fn not_paired(udid: &str, e: Error) -> Error {
    Error::msg(format!("this computer is not trusted by the device ({})", e.message))
        .hint(format!("unlock the phone ({}), plug it in and tap Trust, then enter its passcode; strobe never pairs by itself", short_udid(udid)))
}

// plist framing

fn parse(bytes: &[u8]) -> Result<Dictionary> {
    Value::from_reader(Cursor::new(bytes)).map_err(|e| Error::msg(format!("bad plist from the device: {e}")))?.into_dictionary().ok_or_else(|| Error::msg("the device sent a plist that is not a dictionary"))
}

/// Writes `value` as one framed message: a 4-byte big-endian length, then the plist (XML, or
/// binary where the service speaks binary, such as Web Inspector).
pub fn send_plist(stream: &mut dyn Io, value: &Value, binary: bool) -> Result<()> {
    let mut body = Vec::new();
    if binary { value.to_writer_binary(&mut body) } else { value.to_writer_xml(&mut body) }.map_err(|e| Error::msg(format!("plist: {e}")))?;
    let len = u32::try_from(body.len()).map_err(|_| Error::msg("plist too large to send"))?;
    let mut msg = len.to_be_bytes().to_vec();
    msg.extend_from_slice(&body);
    stream.write_all(&msg).map_err(io)?;
    stream.flush().map_err(io)
}

/// Reads one framed message (either plist format).
pub fn recv_plist(stream: &mut dyn Io) -> Result<Dictionary> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).map_err(io)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(Error::msg(format!("the device sent a {len}-byte message")));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).map_err(io)?;
    parse(&body)
}

// TLS

fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(tls::provider())).clone()
}

/// Trusts exactly the device of the pair record: its certificate's SubjectPublicKeyInfo must be
/// the one the device was given when the user trusted this computer (pairing certificates are
/// self-issued, so there is no chain to check).
#[derive(Debug)]
struct PinnedDevice {
    spki: Vec<u8>,
}

impl ServerCertVerifier for PinnedDevice {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match spki(end_entity) {
            Some(k) if k.whole == self.spki.as_slice() => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::General("the device's certificate is not the one in this computer's pair record".into())),
        }
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(message, cert, dss)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tls::VERIFY_SCHEMES.to_vec()
    }
}

fn verify_signature(message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
    match spki(cert) {
        Some(k) if tls::verify(dss.scheme, k.key, message, dss.signature()) => Ok(HandshakeSignatureValid::assertion()),
        _ => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature)),
    }
}

/// One DER element: its tag, its contents and what follows it.
fn tlv(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, d) = d.split_first()?;
    let (&first, mut d) = d.split_first()?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || d.len() < n {
            return None;
        }
        let len = d[..n].iter().fold(0usize, |l, &b| (l << 8) | b as usize);
        d = &d[n..];
        len
    };
    (d.len() >= len).then(|| (tag, &d[..len], &d[len..]))
}

/// A certificate's SubjectPublicKeyInfo: the whole element as encoded, and its key bits.
struct Spki<'a> {
    whole: &'a [u8],
    key: &'a [u8],
}

/// Reads the key with a minimal DER walk rather than a full X.509 parser, which refuses some
/// pairing certificates' encodings that matter nothing here (the key is pinned).
fn spki(cert: &[u8]) -> Option<Spki<'_>> {
    let (0x30, cert, _) = tlv(cert)? else { return None };
    let (0x30, mut tbs, _) = tlv(cert)? else { return None };
    if tbs.first() == Some(&0xa0) {
        tbs = tlv(tbs)?.2; // version
    }
    for _ in 0..5 {
        tbs = tlv(tbs)?.2; // serial, signature, issuer, validity, subject
    }
    let (0x30, info, rest) = tlv(tbs)? else { return None };
    let whole = &tbs[..tbs.len() - rest.len()];
    let (0x30, _, bits) = tlv(info)? else { return None };
    // The key is a BIT STRING with no unused bits, and the last thing in the element.
    let (0x03, bits, after) = tlv(bits)? else { return None };
    let (0, key) = bits.split_first()? else { return None };
    after.is_empty().then_some(Spki { whole, key })
}

fn tls_config(pair: &PairRecord) -> Result<Arc<ClientConfig>> {
    let spki = spki(&pair.device_cert).ok_or_else(|| Error::msg("the pair record's DeviceCertificate has no readable public key"))?.whole.to_vec();
    let tls = |e: rustls::Error| Error::msg(format!("TLS setup: {e}"));
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(tls)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedDevice { spki }))
        .with_client_auth_cert(vec![pair.host_cert.clone()], pair.host_key.clone_key())
        .map_err(tls)?;
    cfg.enable_sni = false;
    Ok(Arc::new(cfg))
}

/// TLS over `stream` with this computer's pairing identity, handshake done. Returns the stream and
/// the negotiated version and suite.
fn tls_wrap(stream: Box<dyn Io>, cfg: &Arc<ClientConfig>) -> Result<(Box<dyn Io>, String)> {
    let name = ServerName::try_from("device").expect("static name");
    let conn = ClientConnection::new(cfg.clone(), name).map_err(|e| Error::msg(format!("TLS: {e}")))?;
    let mut s = StreamOwned::new(conn, stream);
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock).map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => late(),
            _ => Error::msg(format!("TLS handshake with the device: {e}")),
        })?;
    }
    let version = s.conn.protocol_version().map(|v| format!("{v:?}")).unwrap_or_default();
    let suite = s.conn.negotiated_cipher_suite().map(|c| format!("{:?}", c.suite())).unwrap_or_default();
    Ok((Box::new(s), format!("{version} {suite}")))
}

// lockdownd

/// A connection to lockdownd, optionally with a session (TLS, needed for most values and every
/// service). Every request is bounded by its deadline (when set) and `IO_TIMEOUT`.
pub struct Lockdown {
    stream: Box<dyn Io>,
    ctl: Ctl,
    device: Device,
    deadline: Option<Instant>,
    session: Option<(String, Arc<ClientConfig>)>,
    /// The session's TLS version and cipher suite.
    pub tls: Option<String>,
}

impl Lockdown {
    pub fn connect(device: &Device, deadline: Option<Instant>) -> Result<Self> {
        let (stream, ctl) = mux_connect(device, LOCKDOWN_PORT, deadline)?;
        let mut l = Lockdown { stream, ctl, device: device.clone(), deadline, session: None, tls: None };
        let r = l.request("QueryType", Dictionary::new())?;
        match r.get("Type").and_then(Value::as_string) {
            Some("com.apple.mobile.lockdown") => Ok(l),
            other => Err(Error::msg(format!("port {LOCKDOWN_PORT} is not lockdownd (it answered {other:?})"))),
        }
    }

    /// One request and its reply. A broken stream ends the session: nothing more can be said on it.
    fn request(&mut self, request: &str, mut fields: Dictionary) -> Result<Dictionary> {
        fields.insert("Label".into(), "strobe".into());
        fields.insert("Request".into(), request.into());
        let r = send_plist(&mut *self.stream, &Value::Dictionary(fields), false).and_then(|()| recv_plist(&mut *self.stream));
        let r = r.inspect_err(|_| self.session = None)?;
        if let Some(e) = r.get("Error").and_then(Value::as_string) {
            return Err(Error::msg(format!("lockdownd refused {request}: {e}")));
        }
        Ok(r)
    }

    /// One value, or a whole domain with `key` None (the default domain then holds the device's
    /// identity and state in one reply). Before a session only the identity (ProductType,
    /// ProductVersion and the like) answers.
    pub fn get_value(&mut self, domain: Option<&str>, key: Option<&str>) -> Result<Option<Value>> {
        let mut f = Dictionary::new();
        if let Some(d) = domain {
            f.insert("Domain".into(), d.into());
        }
        if let Some(k) = key {
            f.insert("Key".into(), k.into());
        }
        Ok(self.request("GetValue", f)?.remove("Value"))
    }

    /// Starts a session with this computer's pair record and switches the connection to TLS.
    pub fn start_session(&mut self, pair: &PairRecord) -> Result<()> {
        let mut f = Dictionary::new();
        f.insert("HostID".into(), pair.host_id.as_str().into());
        f.insert("SystemBUID".into(), pair.system_buid.as_str().into());
        let r = self.request("StartSession", f).map_err(|e| if e.message.contains("InvalidHostID") { not_paired(&self.device.udid, e) } else { e })?;
        let id = r.get("SessionID").and_then(Value::as_string).ok_or_else(|| Error::msg("lockdownd started a session without an id"))?.to_string();
        let cfg = tls_config(pair)?;
        self.session = Some((id, cfg.clone()));
        if r.get("EnableSessionSSL").and_then(Value::as_boolean).unwrap_or(false) {
            let plain = std::mem::replace(&mut self.stream, Box::new(std::io::empty()));
            let (stream, what) = tls_wrap(plain, &cfg).inspect_err(|_| self.session = None)?;
            self.stream = stream;
            self.tls = Some(what);
        }
        Ok(())
    }

    /// Starts `name` and connects to it (TLS when lockdownd asks). Err carries lockdownd's reason:
    /// `InvalidService` when this iOS offers no such service over lockdownd, `PasswordProtected`
    /// while the phone is locked for a service that needs it unlocked. The service starts with this
    /// connection's deadline.
    pub fn start_service(&mut self, name: &str) -> Result<Service> {
        let Some((_, cfg)) = self.session.clone() else { return Err(Error::msg(format!("{name}: lockdownd starts services only in a session"))) };
        let mut f = Dictionary::new();
        f.insert("Service".into(), name.into());
        let r = self.request("StartService", f)?;
        let port = r.get("Port").and_then(Value::as_unsigned_integer).and_then(|p| u16::try_from(p).ok()).ok_or_else(|| Error::msg(format!("lockdownd started {name} without a port")))?;
        let (stream, ctl) = mux_connect(&self.device, port, self.deadline)?;
        let (stream, tls) = if r.get("EnableServiceSSL").and_then(Value::as_boolean).unwrap_or(false) {
            let (stream, what) = tls_wrap(stream, &cfg)?;
            (stream, Some(what))
        } else {
            (stream, None)
        };
        Ok(Service { name: name.to_string(), port, tls, stream, ctl })
    }
}

impl Drop for Lockdown {
    fn drop(&mut self) {
        if let Some((id, _)) = self.session.take() {
            self.ctl.set(Some(Instant::now() + Duration::from_millis(200)));
            let mut f = Dictionary::new();
            f.insert("SessionID".into(), id.into());
            let _ = self.request("StopSession", f);
        }
    }
}

/// A started lockdownd service: a byte stream to it (TLS when the service asked), with the
/// lockdownd-style plist framing most services use. Protocols with their own framing (AFC,
/// os_trace_relay's binary entries) read and write it directly.
pub struct Service {
    pub name: String,
    pub port: u16,
    /// The TLS version and cipher suite, None on a plain tunnel.
    pub tls: Option<String>,
    stream: Box<dyn Io>,
    ctl: Ctl,
}

impl Service {
    /// Bounds every later read and write by `deadline` (None: only `IO_TIMEOUT` per read or write).
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.ctl.set(deadline);
    }
    pub fn send(&mut self, value: &Value, binary: bool) -> Result<()> {
        send_plist(&mut *self.stream, value, binary)
    }
    pub fn recv(&mut self) -> Result<Dictionary> {
        recv_plist(&mut *self.stream)
    }
}

impl Read for Service {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for Service {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

// what status shows

/// The battery as the device reports it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Battery {
    pub percent: Option<f64>,
    pub charging: Option<bool>,
    /// Battery temperature, °C.
    pub temperature_c: Option<f64>,
}

fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_signed_integer().map(|i| i as f64).or_else(|| v.as_unsigned_integer().map(|u| u as f64)).or_else(|| v.as_real()))
}

/// Level and charging from lockdownd's battery domain; with `diagnostics`, the temperature from
/// diagnostics_relay (a read of the I/O registry's IOPMPowerSource), whose raw capacities stand in
/// only when lockdownd gave no level (its CurrentCapacity and MaxCapacity mix percent and mAh).
pub fn battery(l: &mut Lockdown, diagnostics: bool) -> Battery {
    let mut b = Battery::default();
    if let Ok(Some(Value::Dictionary(d))) = l.get_value(Some("com.apple.mobile.battery"), None) {
        b.percent = number(d.get("BatteryCurrentCapacity")).filter(|p| (0.0..=100.0).contains(p));
        b.charging = d.get("BatteryIsCharging").and_then(Value::as_boolean);
    }
    if !diagnostics {
        return b;
    }
    let Ok(mut s) = l.start_service("com.apple.mobile.diagnostics_relay") else { return b };
    let mut f = Dictionary::new();
    f.insert("Request".into(), "IORegistry".into());
    f.insert("EntryClass".into(), "IOPMPowerSource".into());
    if s.send(&Value::Dictionary(f), false).is_ok()
        && let Ok(r) = s.recv()
        && r.get("Status").and_then(Value::as_string) == Some("Success")
        && let Some(io) = r.get("Diagnostics").and_then(Value::as_dictionary).and_then(|d| d.get("IORegistry")).and_then(Value::as_dictionary)
    {
        // Hundredths of a degree; anything outside a battery's possible range is not a reading.
        b.temperature_c = number(io.get("Temperature")).map(|t| t / 100.0).filter(|t| *t > 0.0 && *t < 100.0);
        if b.percent.is_none() {
            b.percent = match (number(io.get("AppleRawCurrentCapacity")), number(io.get("AppleRawMaxCapacity"))) {
                (Some(c), Some(m)) if m > 0.0 => Some(100.0 * c / m).filter(|p| (0.0..=100.0).contains(p)),
                _ => None,
            };
        }
        b.charging = b.charging.or_else(|| io.get("IsCharging").and_then(Value::as_boolean));
    }
    let mut bye = Dictionary::new();
    bye.insert("Request".into(), "Goodbye".into());
    let _ = s.send(&Value::Dictionary(bye), false);
    b
}

/// One phone as `strobe status` shows it. Never its name: a personal phone's name is the owner's.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    /// The full UDID, to select the phone as a target; text shows `short_udid`.
    pub udid: String,
    /// The usbmuxd attachments this phone has: `usb`, `wifi` or both.
    pub connections: Vec<Connection>,
    pub product_type: Option<String>,
    pub ios: Option<String>,
    /// On the lock screen with a passcode (lockdownd's PasswordProtected), so pages and services that
    /// need it unlocked wait.
    pub locked: Option<bool>,
    pub battery: Battery,
    /// What stopped strobe from reading the rest (not trusted, no answer in time).
    pub problem: Option<String>,
}

/// Every phone usbmuxd sees, each read over its cable within `budget` in all (a phone only on
/// Wi-Fi is listed, not read); `diagnostics` adds the battery temperature (one more service and
/// handshake). Err only when usbmuxd is unreachable.
pub fn summaries(budget: Duration, diagnostics: bool) -> Result<Vec<Summary>> {
    let deadline = Some(Instant::now() + budget);
    let devices = list(deadline)?;
    let mut out: Vec<Summary> = Vec::new();
    for d in &devices {
        if let Some(s) = out.iter_mut().find(|s| s.udid == d.udid) {
            s.connections.push(d.connection);
            continue;
        }
        out.push(Summary { udid: d.udid.clone(), connections: vec![d.connection], product_type: None, ios: None, locked: None, battery: Battery::default(), problem: None });
    }
    for s in &mut out {
        let Some(dev) = devices.iter().find(|d| d.udid == s.udid && d.connection == Connection::Usb) else {
            s.problem = Some("on Wi-Fi only: strobe reads phones on a cable".into());
            continue;
        };
        if let Err(e) = read_summary(dev, s, deadline, diagnostics) {
            s.problem = Some(format!("{}{}", e.message, e.hint.map(|h| format!(" ({h})")).unwrap_or_default()));
        }
    }
    Ok(out)
}

fn read_summary(dev: &Device, s: &mut Summary, deadline: Option<Instant>, diagnostics: bool) -> Result<()> {
    let mut l = Lockdown::connect(dev, deadline)?;
    let pair = pair_record(&dev.udid, deadline);
    if let Ok(p) = &pair {
        l.start_session(p)?;
    }
    let values = l.get_value(None, None)?.and_then(Value::into_dictionary).unwrap_or_default();
    let text = |k: &str| values.get(k).and_then(Value::as_string).map(str::to_string);
    s.product_type = text("ProductType");
    s.ios = text("ProductVersion");
    if let Err(e) = pair {
        return Err(if e.code == 124 { e } else { not_paired(&dev.udid, e) });
    }
    s.locked = match values.get("PasswordProtected").and_then(Value::as_boolean) {
        Some(b) => Some(b),
        None => l.get_value(None, Some("PasswordProtected"))?.and_then(|v| v.as_boolean()),
    };
    s.battery = battery(&mut l, diagnostics);
    Ok(())
}

/// The one-line form: model, iOS, link, battery, lock state and the short id.
pub fn describe(s: &Summary) -> String {
    let link = match (s.connections.contains(&Connection::Usb), s.connections.contains(&Connection::Network)) {
        (true, true) => "USB + Wi-Fi",
        (true, false) => "USB",
        _ => "Wi-Fi",
    };
    let mut parts = vec![s.product_type.clone().unwrap_or_else(|| "iOS device".into())];
    if let Some(v) = &s.ios {
        parts.push(format!("iOS {v}"));
    }
    parts.push(link.into());
    if let Some(p) = s.battery.percent {
        parts.push(format!("{p:.0}%{}", if s.battery.charging == Some(true) { " charging" } else { "" }));
    }
    if let Some(t) = s.battery.temperature_c {
        parts.push(format!("{t:.1} °C"));
    }
    match s.locked {
        Some(true) => parts.push("locked".into()),
        Some(false) => parts.push("unlocked".into()),
        None => {}
    }
    parts.push(short_udid(&s.udid));
    let mut t = parts.join(" · ");
    if let Some(p) = &s.problem {
        t += &format!(": {p}");
    }
    t
}
