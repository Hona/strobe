//! Android devices over adb: a client of the adb server's smart-socket protocol on 127.0.0.1:5037
//! (`ANDROID_ADB_SERVER_PORT` moves it), in Rust, with no adb process per request. The `adb`
//! executable runs only to start a server that does not answer (SDK `platform-tools`, found through
//! `ANDROID_HOME`, `ANDROID_SDK_ROOT`, PATH, then the SDK's default folder).
//!
//! A request is its length as four hex digits, then its text; the server answers `OKAY`, or `FAIL`
//! and a length-prefixed message. `host:*` requests talk to the server; `host:transport:<serial>`
//! turns the connection into a stream to that device's adbd, which then takes one service: a shell
//! (`shell,v2,raw:`: framed stdout, stderr and the exit code), `sync:` (files), `reverse:*`.

// The mobile targets (validators, `--mobile`, Android Chrome) build on this; until they land only
// `restore_stale` has a caller.
#![allow(dead_code)]

mod awake;
mod facts;

#[allow(unused_imports)]
pub use awake::{PhoneSlot, StayAwake};
#[allow(unused_imports)]
pub use facts::{Battery, Display, Facts, Thermal};

use crate::host::{paths, spawn};
use crate::model::{Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a request may take before it fails.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The most a shell command's stdout or stderr may hold; one shell packet's largest size.
const SHELL_CAP: usize = 16 << 20;
const PACKET_CAP: usize = 1 << 20;
/// adbd's sync data chunks are at most this big.
const SYNC_CHUNK: usize = 64 * 1024;

/// The adb server strobe talks to.
#[derive(Clone, Copy, Debug)]
pub struct Adb {
    port: u16,
}

/// A device as the server lists it (`host:devices-l`).
#[derive(Clone, Debug, Serialize)]
pub struct Listed {
    pub serial: String,
    pub state: State,
    pub transport: Transport,
    /// The USB port path (`1-4.2`), on a cabled device where the OS's adb reports one (not Windows).
    pub usb: Option<String>,
    pub product: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub transport_id: Option<u64>,
}

/// A device's connection state as the server reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Connected and authorized: it takes requests.
    Device,
    Offline,
    /// The phone has not accepted this computer's key (the "Allow USB debugging?" prompt).
    Unauthorized,
    Authorizing,
    Connecting,
    /// The OS refused access to the USB device (Linux udev rules).
    NoPermissions,
    Recovery,
    Sideload,
    Bootloader,
    Other(String),
}

/// How the device is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Usb,
    /// adb over the network (`adb connect host:port`, or wireless debugging found by mDNS).
    Wireless,
    /// An emulator's own local transport (`emulator-5554`).
    Local,
}

/// What a shell command left: its exit code, and stdout and stderr apart.
#[derive(Clone, Debug)]
pub struct ShellOutput {
    pub code: u8,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ShellOutput {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// A file on the device as `sync` STAT sees it.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Stat {
    pub mode: u32,
    pub size: u64,
    pub mtime: i64,
}

/// One connected device: requests open a connection each (the server multiplexes them).
#[derive(Clone, Debug)]
pub struct Device {
    adb: Adb,
    pub serial: String,
}

/// A forward (host to device) or reverse (device to host) that lasts while this value does. Each is
/// journaled in `<state>/adb/tunnels-<pid>.json`, so a strobe killed while one is open has it removed
/// by the next (`restore_stale`).
pub struct Tunnel {
    device: Device,
    reverse: bool,
    /// The listening side's spec, as given or with the port chosen for `tcp:0`.
    listen: String,
    pub target: String,
}

/// A connection to the server: one request, then (after a transport switch) one device service.
/// `ctx` names what it is for in errors (the device's serial, the request).
struct Conn {
    s: TcpStream,
    ctx: String,
}

fn server_port() -> u16 {
    std::env::var("ANDROID_ADB_SERVER_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(5037)
}

/// The `adb` executable, by absolute path only: the SDK's `platform-tools` under `ANDROID_HOME` or
/// `ANDROID_SDK_ROOT`, then PATH (its relative entries skipped: an `adb` in the current folder is
/// not trusted), then this OS's default SDK folder.
pub fn adb_exe() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    let sdk = |root: PathBuf| root.join("platform-tools").join(exe);
    let env = ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter().filter_map(std::env::var_os).map(PathBuf::from).map(sdk);
    let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join(exe)).collect::<Vec<_>>()).unwrap_or_default();
    let default = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Android").join("Sdk"))
    } else if cfg!(target_os = "macos") {
        Some(super::home().join("Library/Android/sdk"))
    } else {
        Some(super::home().join("Android/Sdk"))
    };
    env.chain(path).chain(default.map(sdk)).find(|p| p.is_absolute() && p.is_file())
}

impl Conn {
    fn open(port: u16, timeout: Option<Duration>, ctx: &str) -> Result<Conn> {
        let open = || -> std::io::Result<TcpStream> {
            let s = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_secs(2))?;
            s.set_nodelay(true)?;
            s.set_read_timeout(timeout)?;
            s.set_write_timeout(Some(DEFAULT_TIMEOUT))?;
            Ok(s)
        };
        let s = open().map_err(|e| Error::unreachable(format!("no adb server answers on 127.0.0.1:{port}: {e}")).hint("start it with `adb start-server`"))?;
        Ok(Conn { s, ctx: ctx.to_string() })
    }

    fn error(&self, e: std::io::Error) -> Error {
        match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Error::timeout(format!("{}: no answer in time", self.ctx)),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe => {
                Error::unreachable(format!("{}: the connection closed (the device or the adb server went away?)", self.ctx))
            }
            _ => Error::msg(format!("{}: {e}", self.ctx)),
        }
    }

    fn write(&mut self, b: &[u8]) -> Result<()> {
        self.s.write_all(b).map_err(|e| self.error(e))
    }

    fn send(&mut self, req: &str) -> Result<()> {
        if req.len() > 0xffff {
            return Err(Error::msg(format!("{}: adb request too long ({} bytes)", self.ctx, req.len())));
        }
        self.write(format!("{:04x}{req}", req.len()).as_bytes())
    }

    fn read_into(&mut self, b: &mut [u8]) -> Result<()> {
        self.s.read_exact(b).map_err(|e| self.error(e))
    }

    fn read_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        self.read_into(&mut b)?;
        Ok(b)
    }

    /// `OKAY`, or the server's `FAIL` message as an error.
    fn status(&mut self) -> Result<()> {
        let s = self.read_n(4)?;
        match &s[..] {
            b"OKAY" => Ok(()),
            b"FAIL" => {
                let why = self.read_string()?;
                Err(Error::msg(format!("{}: {why}", self.ctx)))
            }
            other => Err(Error::msg(format!("{}: unexpected reply {:?}", self.ctx, String::from_utf8_lossy(other)))),
        }
    }

    fn request(&mut self, req: &str) -> Result<()> {
        self.send(req)?;
        self.status()
    }

    /// A length-prefixed string (four hex digits: at most 64 KiB).
    fn read_string(&mut self) -> Result<String> {
        let len = self.read_n(4)?;
        let len = usize::from_str_radix(std::str::from_utf8(&len).unwrap_or(""), 16).map_err(|_| Error::msg(format!("{}: bad length {:?}", self.ctx, String::from_utf8_lossy(&len))))?;
        Ok(String::from_utf8_lossy(&self.read_n(len)?).into_owned())
    }

    /// Sets the read timeout to what is left until `deadline`; fails once it has passed.
    fn until(&mut self, deadline: Instant, what: &str) -> Result<()> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::timeout(format!("{}: {what} took too long", self.ctx)));
        }
        self.s.set_read_timeout(Some(left)).map_err(|e| self.error(e))
    }
}

impl Adb {
    /// The running server, started (`adb start-server`) whenever none answers.
    pub fn connect() -> Result<Adb> {
        static START: Mutex<()> = Mutex::new(());
        let adb = Adb { port: server_port() };
        if adb.version().is_ok() {
            return Ok(adb);
        }
        let _one = START.lock().unwrap_or_else(|e| e.into_inner());
        // Another thread may have started it while this one waited.
        if adb.version().is_ok() {
            return Ok(adb);
        }
        let exe = adb_exe().ok_or_else(|| {
            Error::unreachable(format!("no adb server answers on 127.0.0.1:{} and no adb executable was found", adb.port))
                .hint("install the Android SDK platform-tools and set ANDROID_HOME, or put adb on PATH")
        })?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("start-server").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console flashes up from the windowless strobe.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| Error::msg(format!("could not run {}: {e}", exe.display())))?;
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if adb.version().is_ok() {
                let _ = child.try_wait();
                return Ok(adb);
            }
            if let Ok(Some(status)) = child.try_wait()
                && !status.success()
            {
                return Err(Error::unreachable(format!("{} start-server failed ({status})", exe.display())).hint("run `adb start-server` to see why"));
            }
            if Instant::now() >= until {
                return Err(Error::unreachable(format!("{} start-server ran, but no server answered within 10 s", exe.display())).hint("run `adb start-server` to see why"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The server when one answers now; never starts one.
    fn running() -> Option<Adb> {
        let adb = Adb { port: server_port() };
        adb.version().is_ok().then_some(adb)
    }

    fn conn(&self, ctx: &str) -> Result<Conn> {
        Conn::open(self.port, Some(DEFAULT_TIMEOUT), ctx)
    }

    /// A `host:` request whose answer is one length-prefixed string.
    fn query(&self, req: &str) -> Result<String> {
        let mut c = self.conn(&format!("adb {req}"))?;
        c.request(req)?;
        c.read_string()
    }

    /// The server's protocol version (`host:version`).
    pub fn version(&self) -> Result<u32> {
        let v = self.query("host:version")?;
        u32::from_str_radix(v.trim(), 16).map_err(|_| Error::msg(format!("adb: bad version {v:?}")))
    }

    /// Every device the server knows, in any state.
    pub fn devices(&self) -> Result<Vec<Listed>> {
        Ok(parse_devices(&self.query("host:devices-l")?))
    }

    /// The device list each time it changes (`host:track-devices-l`), the current one first. It ends
    /// after its first error.
    pub fn track_devices(&self) -> Result<impl Iterator<Item = Result<Vec<Listed>>>> {
        let mut c = Conn::open(self.port, None, "adb host:track-devices-l")?;
        c.request("host:track-devices-l")?;
        let mut c = Some(c);
        Ok(std::iter::from_fn(move || {
            let r = c.as_mut()?.read_string().map(|t| parse_devices(&t));
            if r.is_err() {
                c = None;
            }
            Some(r)
        }))
    }

    /// The device to use: `serial` (else `ANDROID_SERIAL`), or the one connected device. A phone
    /// listed twice (USB and wireless debugging) is the one phone, used over USB.
    pub fn pick(&self, serial: Option<&str>) -> Result<Device> {
        let env = std::env::var("ANDROID_SERIAL").ok().filter(|s| !s.is_empty());
        let want = serial.map(str::to_string).or(env);
        let all = self.devices()?;
        let named = |l: &Listed| format!("{} ({})", l.serial, state_text(&l.state));
        let list = || all.iter().map(named).collect::<Vec<_>>().join(", ");
        let one = match &want {
            Some(s) => all.iter().find(|l| &l.serial == s).ok_or_else(|| {
                Error::unreachable(format!("no device {s} is attached{}", if all.is_empty() { String::new() } else { format!(" (attached: {})", list()) }))
            })?,
            None => {
                let ready: Vec<&Listed> = all.iter().filter(|l| l.state == State::Device).collect();
                match ready.as_slice() {
                    [one] => *one,
                    [] => match all.first() {
                        None => return Err(Error::unreachable("no Android device is attached").hint("connect a phone with USB debugging on, or start an emulator; `adb devices` lists them")),
                        Some(l) => l,
                    },
                    many => {
                        let hw: Vec<Option<String>> = many.iter().map(|l| self.device(&l.serial).hardware_serial()).collect();
                        let same = hw[0].is_some() && hw.iter().all(|h| *h == hw[0]);
                        match many.iter().find(|l| l.transport == Transport::Usb) {
                            Some(usb) if same => *usb,
                            _ => {
                                return Err(Error::usage(format!("{} devices are attached: {}", many.len(), many.iter().map(|l| named(l)).collect::<Vec<_>>().join(", "))).hint("name one by its serial"));
                            }
                        }
                    }
                }
            }
        };
        let why = state_text(&one.state);
        match one.state {
            State::Device => Ok(self.device(&one.serial)),
            State::Unauthorized => Err(Error::new(3, format!("{} is unauthorized", one.serial)).hint("unlock the phone and accept the \"Allow USB debugging?\" prompt")),
            State::NoPermissions => Err(Error::new(3, format!("{} is {why}", one.serial)).hint("the OS refused the USB device: add a udev rule for it, then reconnect")),
            _ => Err(Error::unreachable(format!("{} is {why}", one.serial)).hint("reconnect it, or wait until it finishes booting")),
        }
    }

    /// A device by serial, without checking it is attached.
    pub fn device(&self, serial: &str) -> Device {
        Device { adb: *self, serial: serial.to_string() }
    }
}

fn state_text(s: &State) -> String {
    match s {
        State::Device => "connected".into(),
        State::Offline => "offline".into(),
        State::Unauthorized => "unauthorized".into(),
        State::Authorizing => "authorizing".into(),
        State::Connecting => "connecting".into(),
        State::NoPermissions => "without USB permissions".into(),
        State::Recovery => "in recovery".into(),
        State::Sideload => "in sideload".into(),
        State::Bootloader => "in the bootloader".into(),
        State::Other(o) => o.clone(),
    }
}

/// An emulator's serial names it; a network device's serial is `host:port` or an mDNS service
/// (`adb-<id>._adb-tls-connect._tcp`); a cabled one has a USB path, except on Windows, whose adb
/// lists none: what is neither is a cabled phone.
fn classify(serial: &str, usb: bool) -> Transport {
    if serial.starts_with("emulator-") {
        Transport::Local
    } else if usb {
        Transport::Usb
    } else if serial.contains(':') || serial.contains("._adb") || serial.ends_with("._tcp") {
        Transport::Wireless
    } else {
        Transport::Usb
    }
}

/// `host:devices-l` lines: `<serial> <state> [usb:<path>] product:<p> model:<m> device:<d> transport_id:<n>`.
/// A state of two words (`no permissions (…)`) runs until the first `key:value`.
fn parse_devices(text: &str) -> Vec<Listed> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let serial = words.next()?.to_string();
            let rest: Vec<&str> = words.collect();
            let kv = |w: &&str| w.split_once(':').is_some_and(|(k, _)| !k.is_empty() && k.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
            let split = rest.iter().position(|w| kv(w) && !w.starts_with("permissions")).unwrap_or(rest.len());
            let state_words = rest[..split].join(" ");
            let field = |k: &str| rest[split..].iter().find_map(|w| w.strip_prefix(k).and_then(|v| v.strip_prefix(':'))).map(str::to_string);
            let state = match state_words.as_str() {
                "device" => State::Device,
                "offline" => State::Offline,
                "unauthorized" => State::Unauthorized,
                "authorizing" => State::Authorizing,
                "connecting" => State::Connecting,
                "recovery" => State::Recovery,
                "sideload" => State::Sideload,
                "bootloader" => State::Bootloader,
                s if s.starts_with("no permissions") => State::NoPermissions,
                s => State::Other(s.to_string()),
            };
            let usb = field("usb");
            let transport = classify(&serial, usb.is_some());
            Some(Listed { usb, transport, product: field("product"), model: field("model"), device: field("device"), transport_id: field("transport_id").and_then(|t| t.parse().ok()), state, serial })
        })
        .collect()
}

/// `s` as one word for the device's `sh`.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn short(cmd: &str) -> String {
    let first = cmd.lines().next().unwrap_or("");
    if first.chars().count() > 60 || first.len() < cmd.len() { format!("{}…", first.chars().take(60).collect::<String>()) } else { first.to_string() }
}

/// The lists `list-forward` prints (`<serial> <listen> <target>`), of `serial` only where given: the
/// server's list holds every device's forwards.
fn parse_list(text: &str, serial: Option<&str>) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| match l.split_whitespace().collect::<Vec<_>>().as_slice() {
            [s, listen, target] if serial.is_none_or(|want| want == *s) => Some((listen.to_string(), target.to_string())),
            _ => None,
        })
        .collect()
}

impl Device {
    /// A connection switched to this device, ready for one service.
    fn transport(&self, timeout: Option<Duration>, what: &str) -> Result<Conn> {
        let mut c = Conn::open(self.adb.port, timeout, &format!("{}: {what}", self.serial))?;
        c.request(&format!("host:transport:{}", self.serial))?;
        Ok(c)
    }

    /// What this device's adbd supports (`shell_v2`, `cmd`, `stat_v2`, …).
    pub fn features(&self) -> Result<Vec<String>> {
        Ok(self.adb.query(&format!("host-serial:{}:features", self.serial))?.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect())
    }

    /// The phone's own serial (`ro.serialno`, else `ro.boot.serialno`): the same over USB and Wi-Fi.
    pub fn hardware_serial(&self) -> Option<String> {
        let s = self.sh("getprop ro.serialno; getprop ro.boot.serialno").ok()?;
        s.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
    }

    /// Opens a shell running `cmd` without a terminal.
    fn open_shell(&self, cmd: &str, timeout: Duration) -> Result<Conn> {
        let mut c = self.transport(Some(timeout), &format!("`{}`", short(cmd)))?;
        c.send(&format!("shell,v2,raw:{cmd}"))?;
        c.status().map_err(|e| Error::msg(format!("{}: the shell did not start: {}", self.serial, e.message)).hint("strobe needs adbd's shell protocol v2 (Android 7 and later)"))?;
        Ok(c)
    }

    /// Runs `cmd` in the device's `sh` without a terminal, within 30 s.
    pub fn shell(&self, cmd: &str) -> Result<ShellOutput> {
        self.shell_for(cmd, DEFAULT_TIMEOUT)
    }

    /// `shell` that fails once `timeout` has passed. Its stdin is closed at once: a command that
    /// reads it sees its end. Its stdout and stderr hold at most 16 MiB each.
    pub fn shell_for(&self, cmd: &str, timeout: Duration) -> Result<ShellOutput> {
        let deadline = Instant::now() + timeout;
        let mut c = self.open_shell(cmd, timeout)?;
        // Packet: id (0 stdin, 1 stdout, 2 stderr, 3 exit, 4 close stdin), length (u32 LE), data.
        c.write(&[4, 0, 0, 0, 0])?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            match read_packet(&mut c, deadline)? {
                (1, data) => stdout.extend_from_slice(&data),
                (2, data) => stderr.extend_from_slice(&data),
                (3, data) => return Ok(ShellOutput { code: data.first().copied().unwrap_or(255), stdout, stderr }),
                _ => {}
            }
            if stdout.len() > SHELL_CAP || stderr.len() > SHELL_CAP {
                return Err(Error::msg(format!("{}: printed more than {} MiB", c.ctx, SHELL_CAP >> 20)));
            }
        }
    }

    /// `shell`, its stdout as text when it exits 0, else an error with its stderr.
    pub fn sh(&self, cmd: &str) -> Result<String> {
        let o = self.shell(cmd)?;
        if !o.ok() {
            let err = o.stderr_text();
            let why = err.trim().lines().last().unwrap_or("");
            return Err(Error::msg(format!("{}: `{}` exited {}{}", self.serial, short(cmd), o.code, if why.is_empty() { String::new() } else { format!(": {why}") })));
        }
        Ok(o.stdout_text())
    }

    /// Every system property (`getprop`).
    pub fn props(&self) -> Result<std::collections::BTreeMap<String, String>> {
        Ok(facts::parse_props(&self.sh("getprop")?))
    }

    /// Forwards host `local` to device `remote` (`tcp:<port>`, `tcp:0` for a free port,
    /// `localabstract:<name>`, `localfilesystem:<path>`, …) until the tunnel is dropped.
    pub fn forward(&self, local: &str, remote: &str) -> Result<Tunnel> {
        let mut c = self.adb.conn(&format!("{}: forward {local} to {remote}", self.serial))?;
        // On the host: one OKAY for the connection, one for the forward, then the port of `tcp:0`.
        c.request(&format!("host-serial:{}:forward:{local};{remote}", self.serial))?;
        c.status()?;
        let listen = if local == "tcp:0" { format!("tcp:{}", c.read_string()?.trim()) } else { local.to_string() };
        Ok(Tunnel::journaled(self, false, listen, remote))
    }

    /// Forwards device `remote` (what the device's apps connect to) to host `local` until the tunnel
    /// is dropped: a page on the phone reaching a server on this computer.
    pub fn reverse(&self, remote: &str, local: &str) -> Result<Tunnel> {
        let mut c = self.transport(Some(DEFAULT_TIMEOUT), &format!("reverse {remote} to {local}"))?;
        // OKAY for the connection, OKAY for the reverse, then the device's port (also for a fixed one).
        c.request(&format!("reverse:forward:{remote};{local}"))?;
        c.status()?;
        let listen = if remote == "tcp:0" { format!("tcp:{}", c.read_string()?.trim()) } else { remote.to_string() };
        Ok(Tunnel::journaled(self, true, listen, local))
    }

    /// This device's forwards (host to device): (local, remote).
    pub fn forwards(&self) -> Result<Vec<(String, String)>> {
        Ok(parse_list(&self.adb.query(&format!("host-serial:{}:list-forward", self.serial))?, Some(&self.serial)))
    }

    /// This device's reverses (device to host): (remote, local).
    pub fn reverses(&self) -> Result<Vec<(String, String)>> {
        let mut c = self.transport(Some(DEFAULT_TIMEOUT), "reverse --list")?;
        // One OKAY (the connection), then the list: no status OKAY as a forward has.
        c.request("reverse:list-forward")?;
        let text = c.read_string()?;
        // adbd knows only its own (it names the host side `host-<transport id>`).
        Ok(parse_list(&text, None))
    }

    fn remove(&self, reverse: bool, listen: &str) -> Result<()> {
        let mut c = if reverse {
            let mut c = self.transport(Some(DEFAULT_TIMEOUT), &format!("remove reverse {listen}"))?;
            c.request(&format!("reverse:killforward:{listen}"))?;
            c
        } else {
            let mut c = self.adb.conn(&format!("{}: remove forward {listen}", self.serial))?;
            c.request(&format!("host-serial:{}:killforward:{listen}", self.serial))?;
            c
        };
        c.status()
    }

    /// A `sync:` session.
    fn sync(&self, what: &str) -> Result<Conn> {
        let mut c = self.transport(Some(DEFAULT_TIMEOUT), what)?;
        c.request("sync:")?;
        Ok(c)
    }

    /// Writes `data` to `remote` on the device. adbd keeps only whether it is executable: the file is
    /// `0777` or `0666` (outside the system partitions).
    pub fn push(&self, data: &[u8], remote: &str, executable: bool) -> Result<()> {
        let mut c = self.sync(&format!("push {remote}"))?;
        // Mode is the file type (regular) and permissions, in decimal after the path.
        let mode = 0o100000 | if executable { 0o755 } else { 0o644 };
        let mtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
        let sent = (|| {
            sync_request(&mut c, b"SEND", format!("{remote},{mode}").as_bytes())?;
            for chunk in data.chunks(SYNC_CHUNK) {
                sync_request(&mut c, b"DATA", chunk)?;
            }
            c.write(b"DONE")?;
            c.write(&mtime.to_le_bytes())
        })();
        // adbd stops reading once it fails (a read-only folder): its FAIL says why, which beats the
        // broken pipe the write saw.
        let reply = sync_head(&mut c).and_then(|(id, len)| Ok((id, String::from_utf8_lossy(&c.read_n(len.min(SYNC_CHUNK))?).into_owned())));
        let _ = sync_request(&mut c, b"QUIT", b"");
        match (reply, sent) {
            (Ok((id, _)), Ok(())) if &id == b"OKAY" => Ok(()),
            (Ok((id, msg)), _) if &id == b"FAIL" => Err(Error::msg(format!("{}: push to {remote}: {msg}", self.serial))),
            (_, Err(e)) | (Err(e), Ok(())) => Err(e),
            (Ok((id, _)), Ok(())) => Err(Error::msg(format!("{}: push to {remote}: unexpected {:?}", self.serial, String::from_utf8_lossy(&id)))),
        }
    }

    /// Reads `remote` from the device into `out`; returns its size.
    pub fn pull(&self, remote: &str, out: &mut dyn Write) -> Result<u64> {
        let mut c = self.sync(&format!("pull {remote}"))?;
        sync_request(&mut c, b"RECV", remote.as_bytes())?;
        let mut size = 0u64;
        let mut buf = vec![0u8; SYNC_CHUNK];
        loop {
            let (id, len) = sync_head(&mut c)?;
            match &id {
                b"DATA" if len <= SYNC_CHUNK => {
                    c.read_into(&mut buf[..len])?;
                    out.write_all(&buf[..len])?;
                    size += len as u64;
                }
                b"DATA" => return Err(Error::msg(format!("{}: pull {remote}: a {len}-byte chunk (adbd sends at most 64 KiB)", self.serial))),
                b"DONE" => break,
                b"FAIL" => return Err(Error::msg(format!("{}: pull {remote}: {}", self.serial, String::from_utf8_lossy(&c.read_n(len.min(SYNC_CHUNK))?)))),
                other => return Err(Error::msg(format!("{}: pull {remote}: unexpected {:?}", self.serial, String::from_utf8_lossy(other)))),
            }
        }
        let _ = sync_request(&mut c, b"QUIT", b"");
        Ok(size)
    }

    /// `remote`'s mode, size and modification time; None when it does not exist. STAT v2 (64-bit
    /// sizes) where adbd has it.
    pub fn stat(&self, remote: &str) -> Result<Option<Stat>> {
        let v2 = self.features()?.iter().any(|f| f == "stat_v2");
        let mut c = self.sync(&format!("stat {remote}"))?;
        sync_request(&mut c, if v2 { b"STA2" } else { b"STAT" }, remote.as_bytes())?;
        let b = c.read_n(if v2 { 72 } else { 16 })?;
        let _ = sync_request(&mut c, b"QUIT", b"");
        let u32_at = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or([0; 4]));
        let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]));
        if v2 {
            // STA2: id, error, dev, ino, mode, nlink, uid, gid, size, atime, mtime, ctime.
            if &b[..4] != b"STA2" {
                return Err(Error::msg(format!("{}: stat {remote}: unexpected reply", self.serial)));
            }
            return Ok((u32_at(4) == 0).then(|| Stat { mode: u32_at(24), size: u64_at(40), mtime: u64_at(56) as i64 }));
        }
        if &b[..4] != b"STAT" {
            return Err(Error::msg(format!("{}: stat {remote}: unexpected reply", self.serial)));
        }
        Ok((u32_at(4) != 0).then(|| Stat { mode: u32_at(4), size: u32_at(8) as u64, mtime: u32_at(12) as i64 }))
    }

    /// What strobe reads about the device before a run (`facts`).
    pub fn facts(&self) -> Result<Facts> {
        facts::read(self)
    }
}

/// One shell v2 packet before `deadline`: (id, data), at most 1 MiB.
fn read_packet(c: &mut Conn, deadline: Instant) -> Result<(u8, Vec<u8>)> {
    c.until(deadline, "the command")?;
    let head = c.read_n(5)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > PACKET_CAP {
        return Err(Error::msg(format!("{}: a {len}-byte shell packet (at most 1 MiB)", c.ctx)));
    }
    c.until(deadline, "the command")?;
    Ok((head[0], c.read_n(len)?))
}

/// A sync message: id, length (u32 LE), data.
fn sync_request(c: &mut Conn, id: &[u8; 4], data: &[u8]) -> Result<()> {
    c.write(id)?;
    c.write(&(data.len() as u32).to_le_bytes())?;
    c.write(data)
}

fn sync_head(c: &mut Conn) -> Result<([u8; 4], usize)> {
    let h = c.read_n(8)?;
    Ok(([h[0], h[1], h[2], h[3]], u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize))
}

/// The tunnels this strobe holds, as journaled.
static TUNNELS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn tunnels_journal() -> PathBuf {
    journal_dir().join(format!("tunnels-{}.json", std::process::id()))
}

fn write_tunnels(list: &[Value]) {
    if list.is_empty() {
        let _ = std::fs::remove_file(tunnels_journal());
    } else {
        let _ = paths::write_owner_only(&tunnels_journal(), json!({ "owner": spawn::owner_stamp(), "tunnels": list }).to_string().as_bytes());
    }
}

impl Tunnel {
    fn journaled(d: &Device, reverse: bool, listen: String, target: &str) -> Tunnel {
        let mut list = TUNNELS.lock().unwrap_or_else(|e| e.into_inner());
        list.push(json!({ "serial": d.serial, "reverse": reverse, "listen": listen, "target": target }));
        write_tunnels(&list);
        Tunnel { device: d.clone(), reverse, listen, target: target.to_string() }
    }

    /// The listening side: `tcp:<port>` (the chosen port for `tcp:0`), `localabstract:<name>`, ….
    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// The listening TCP port, for a `tcp:` tunnel.
    pub fn port(&self) -> Option<u16> {
        self.listen.strip_prefix("tcp:").and_then(|p| p.parse().ok())
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.device.remove(self.reverse, &self.listen);
        let mut list = TUNNELS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = list.iter().position(|t| t["serial"] == self.device.serial.as_str() && t["reverse"] == self.reverse && t["listen"] == self.listen.as_str()) {
            list.remove(i);
        }
        write_tunnels(&list);
    }
}

/// `<state>/adb`, where the journals live.
fn journal_dir() -> PathBuf {
    paths::sub("adb")
}

/// Whether the strobe a journal names (`<pid>:<start time>`) still runs.
fn owner_alive(owner: &str) -> bool {
    spawn::parse_owner(owner).is_some_and(|(pid, start)| pid == std::process::id() || super::start_time(pid) == Some(start))
}

/// Puts back what a strobe that no longer runs left on a phone: its stay-awake setting (where the
/// phone still holds what it set) and its forwards and reverses (where they still map what it
/// mapped). Free when there is nothing stale: no folder is made, no adb server asked or started.
/// A phone that is not attached keeps its stay-awake journal until a later call sees it.
pub fn restore_stale() {
    let dir = paths::state_path().join("adb");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let stale: Vec<(PathBuf, String, Value)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || !(name.starts_with("stayon-") || name.starts_with("tunnels-")) {
                return None;
            }
            let v = std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(Value::Null);
            if v["owner"].as_str().is_some_and(owner_alive) {
                return None;
            }
            Some((e.path(), name, v))
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    // Without a server every forward and reverse is gone, and no phone can be reached.
    let adb = Adb::running();
    for (path, name, v) in stale {
        if name.starts_with("tunnels-") {
            if let Some(adb) = adb {
                for t in v["tunnels"].as_array().into_iter().flatten() {
                    let (Some(serial), Some(listen), Some(target)) = (t["serial"].as_str(), t["listen"].as_str(), t["target"].as_str()) else { continue };
                    let d = adb.device(serial);
                    let reverse = t["reverse"] == true;
                    let held = if reverse { d.reverses() } else { d.forwards() };
                    if held.is_ok_and(|l| l.iter().any(|(l, t)| l == listen && t == target)) {
                        let _ = d.remove(reverse, listen);
                    }
                }
            }
            let _ = std::fs::remove_file(&path);
        } else if v.is_null() {
            let _ = std::fs::remove_file(&path);
        } else if let Some(adb) = adb {
            awake::restore_journal(&adb, &path, &v);
        }
    }
}
