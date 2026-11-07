//! strobe's own reads of a page's scripts and source maps (SPEC §8.3), with the limits a page cannot
//! talk strobe past: a file only for a page that is a file itself, never a local or private address for
//! a page on the internet, a map only from where its script came from, no redirect, and bounded bodies.

use super::Fetched;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// The most strobe reads of one script or map.
const CAP: usize = 128 << 20;

/// What reading an address found.
enum Got {
    Body(Vec<u8>),
    /// The server said there is none (404, 410), or no such file.
    Missing,
    /// Not readable by strobe (refused, cookie-gated, unreachable, too large, not allowed): the page may
    /// still read it.
    Failed,
}

/// Each http(s) or file script's map, read by strobe, 8 at a time: `Some(None)` where the script has
/// none, `None` where strobe cannot read it (a custom protocol, a server that refused strobe), which the
/// page then fetches. `file_page`: the page itself was a file, so its file scripts may be read.
pub(super) fn read_maps(urls: &[&String], served: Option<&Path>, file_page: bool) -> Vec<Option<Option<Fetched>>> {
    let mut out = Vec::with_capacity(urls.len());
    for batch in urls.chunks(8) {
        std::thread::scope(|s| {
            let threads: Vec<_> = batch.iter().map(|u| s.spawn(move || read_map(u, served, file_page))).collect();
            out.extend(threads.into_iter().map(|t| t.join().ok().flatten()));
        });
    }
    out
}

/// One script's map as the page's own fetch would find it: the last `sourceMappingURL` comment (a URL
/// relative to the script, or an inline `data:` map), else a hidden map beside the script
/// (`<script>.map`, STROBE_SOURCEMAP=hidden). A map is a JSON object, never another body (an app's
/// index page for an unknown path). A map strobe serves from a folder is read there when the script
/// there is the one served, byte for byte (a server need not serve maps).
fn read_map(url: &str, served: Option<&Path>, file_page: bool) -> Option<Option<Fetched>> {
    if !clean(url) {
        return Some(None);
    }
    let file = scheme(url) == "file";
    if !file && !matches!(scheme(url), "http" | "https") {
        return None;
    }
    if file && !file_page {
        return Some(None);
    }
    let local = loopback(url);
    let text = match get(url, local) {
        Got::Body(b) => b,
        Got::Missing => return Some(None),
        Got::Failed => return None,
    };
    let s = String::from_utf8_lossy(&text);
    let at = s.rfind("//# sourceMappingURL=").into_iter().chain(s.rfind("//@ sourceMappingURL=")).max();
    let named = at.and_then(|i| s[i + 21..].split(char::is_whitespace).next()).filter(|m| !m.is_empty());
    let href = match named {
        Some(m) => join(url, m),
        None => format!("{}.map", url.split(['?', '#']).next().unwrap_or(url)),
    };
    let map = match href.strip_prefix("data:") {
        Some(data) => inline(data),
        // A map where its script came from: an http(s) script's never names a file.
        None if !clean(&href) || (scheme(&href) == "file") != file || !matches!(scheme(&href), "http" | "https" | "file") => None,
        None => match on_disk(url, &href, &text, served) {
            Some(m) => Some(m),
            None => match get(&href, local) {
                Got::Body(b) => Some(b),
                Got::Missing => None,
                Got::Failed => return None,
            },
        },
    };
    let map = map.and_then(|m| String::from_utf8(m).ok()).filter(|m| m.trim_start_matches('\u{feff}').trim_start().starts_with('{'));
    let Some(map) = map else { return Some(None) };
    // Line starts in UTF-16 code units, as the page's own offsets count.
    let mut lines = vec![0i64];
    let mut n = 0i64;
    for c in s.chars() {
        n += c.len_utf16() as i64;
        if c == '\n' {
            lines.push(n);
        }
    }
    Some(Some(Fetched { url: href, text: map, lines }))
}

/// The map from the folder strobe serves, for a script strobe serves (a loopback address) whose map is on
/// its own origin, when the script in that folder is byte for byte the one served. Paths stay inside the
/// folder.
fn on_disk(url: &str, href: &str, served_text: &[u8], served: Option<&Path>) -> Option<Vec<u8>> {
    let root = served.filter(|_| loopback(url) && scheme(url) != "file" && origin(url) == origin(href))?;
    let script = inside(root, url)?;
    if std::fs::metadata(&script).ok()?.len() as usize != served_text.len() || std::fs::read(&script).ok()? != served_text {
        return None;
    }
    let map = inside(root, href)?;
    (std::fs::metadata(&map).ok()?.len() as usize <= CAP).then(|| std::fs::read(map).ok()).flatten()
}

/// An http URL's path as a file under `root`, or None when it would leave it.
fn inside(root: &Path, url: &str) -> Option<PathBuf> {
    let path = percent_decode(super::url_path(url)?.trim_start_matches('/'));
    let rel = Path::new(&path);
    rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)).then(|| root.join(rel))
}

/// A script or map by its address: a file, an http URL over a plain connection, an https URL through
/// curl; no redirect followed. `local`: the address may be on this machine or a private network (the
/// page strobe opened is); else such an address is refused, whatever name leads there.
fn get(url: &str, local: bool) -> Got {
    if !clean(url) {
        return Got::Failed;
    }
    match scheme(url) {
        "file" => {
            let raw = url[7..].split(['?', '#']).next().unwrap_or("");
            let path = percent_decode(raw);
            // `file:///C:/x` on Windows.
            let path = if path.as_bytes().get(2) == Some(&b':') { &path[1..] } else { &path[..] };
            match std::fs::metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Got::Missing,
                Ok(m) if m.is_file() && (m.len() as usize) <= CAP => std::fs::read(path).map_or(Got::Failed, Got::Body),
                _ => Got::Failed,
            }
        }
        "https" => https(url, local).unwrap_or(Got::Failed),
        "http" => http(url, local).unwrap_or(Got::Failed),
        _ => Got::Failed,
    }
}

/// The host, port and path of an http(s) URL (the port the scheme's when none is given).
fn parts(url: &str) -> Option<(String, u16, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let (auth, path) = rest.find(['/', '?', '#']).map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let path = path.split('#').next().unwrap_or("/");
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    if auth.contains('@') {
        return None;
    }
    let (host, port) = match auth.rsplit_once(':').filter(|(h, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && (!h.starts_with('[') || h.ends_with(']'))) {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (auth, if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 }),
    };
    Some((host.to_string(), port, path))
}

/// The addresses a host leads to that strobe may connect to: for a page that is not on this machine,
/// none on this machine, a link-local or a private network.
fn addresses(host: &str, port: u16, local: bool) -> Option<Vec<SocketAddr>> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let all: Vec<SocketAddr> = (bare, port).to_socket_addrs().ok()?.collect();
    let ok: Vec<SocketAddr> = all.iter().copied().filter(|a| local || public(a.ip())).collect();
    // One address strobe may not use refuses the name: it could lead anywhere next time.
    (!ok.is_empty() && ok.len() == all.len()).then_some(ok)
}

fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => !(v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_broadcast()),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(v4) => public(IpAddr::V4(v4)),
            None => !(v.is_loopback() || v.is_unspecified() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80),
        },
    }
}

fn https(url: &str, local: bool) -> Option<Got> {
    let (host, port, _) = parts(url)?;
    let addr = *addresses(&host, port, local)?.first()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let ip = match addr.ip() {
        IpAddr::V6(v) => format!("[{v}]"),
        v => v.to_string(),
    };
    let out = std::process::Command::new(if cfg!(windows) { "curl.exe" } else { "curl" })
        .args(["-sS", "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "0", "--connect-timeout", "5", "--max-time", "30", "--max-filesize"])
        .arg(CAP.to_string())
        .arg("--resolve")
        .arg(format!("{bare}:{port}:{ip}"))
        .args(["-w", "\n%{http_code}", "-o", "-", "--", url])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return Some(Got::Failed);
    }
    let mut body = out.stdout;
    let nl = body.iter().rposition(|b| *b == b'\n')?;
    let code = String::from_utf8_lossy(&body[nl + 1..]).trim().to_string();
    body.truncate(nl);
    Some(match code.as_str() {
        "200" if body.len() <= CAP => Got::Body(body),
        "404" | "410" => Got::Missing,
        _ => Got::Failed,
    })
}

fn http(url: &str, local: bool) -> Option<Got> {
    let (host, port, path) = parts(url)?;
    let addr = *addresses(&host, port, local)?.first()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let host_header = if port == 80 { host.clone() } else { format!("{host}:{port}") };
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nAccept-Encoding: gzip, identity\r\nConnection: close\r\n\r\n").ok()?;
    // Read by the reply's length (or its chunks), not to the end of the stream: a server may keep the
    // connection open after the reply.
    let mut buf = vec![0u8; 1 << 16];
    let mut fill = |into: &mut Vec<u8>| -> Option<bool> {
        let n = s.read(&mut buf).ok()?;
        into.extend_from_slice(&buf[..n]);
        (into.len() <= CAP + (1 << 16)).then_some(n > 0)
    };
    let mut raw = Vec::new();
    let head_end = loop {
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if raw.len() > 1 << 16 || !fill(&mut raw)? {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
    match head.split_whitespace().nth(1) {
        Some("200") => {}
        Some("404" | "410") => return Some(Got::Missing),
        _ => return Some(Got::Failed),
    }
    let header = |name: &str| head.lines().find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_string()));
    let mut rest = raw.split_off(head_end);
    let body = if header("transfer-encoding:").is_some_and(|e| e.contains("chunked")) {
        let (mut at, mut out) = (0usize, Vec::new());
        while !dechunk(&rest, &mut at, &mut out)? {
            if out.len() > CAP || !fill(&mut rest)? {
                return None;
            }
        }
        out
    } else if let Some(len) = header("content-length:").and_then(|v| v.parse::<usize>().ok()) {
        if len > CAP {
            return Some(Got::Failed);
        }
        while rest.len() < len {
            if !fill(&mut rest)? {
                return None;
            }
        }
        rest.truncate(len);
        rest
    } else {
        while fill(&mut rest)? {}
        rest
    };
    Some(match header("content-encoding:").as_deref() {
        None | Some("identity") => Got::Body(body),
        Some("gzip" | "x-gzip") => {
            let mut out = Vec::new();
            match flate2::read::GzDecoder::new(&body[..]).take(CAP as u64 + 1).read_to_end(&mut out) {
                Ok(_) if out.len() <= CAP => Got::Body(out),
                _ => Got::Failed,
            }
        }
        Some(_) => Got::Failed,
    })
}

/// Decodes the chunks that have come since `at` into `out`: Some(true) once the last chunk came,
/// Some(false) while more must come, None for a body that is no chunked body. Each byte is looked at
/// once, whatever the reads' sizes.
fn dechunk(raw: &[u8], at: &mut usize, out: &mut Vec<u8>) -> Option<bool> {
    loop {
        let rest = &raw[*at..];
        let Some(eol) = rest.windows(2).take(64).position(|w| w == b"\r\n") else { return (rest.len() < 64).then_some(false) };
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).ok()?.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(true);
        }
        let need = eol.checked_add(2 + size + 2)?;
        if rest.len() < need {
            return Some(false);
        }
        if &rest[need - 2..need] != b"\r\n" {
            return None;
        }
        out.extend_from_slice(&rest[eol + 2..need - 2]);
        *at += need;
    }
}

/// An inline `data:` map's bytes (`application/json;base64,…`, or percent-encoded).
fn inline(data: &str) -> Option<Vec<u8>> {
    let (kind, body) = data.split_once(',')?;
    if kind.ends_with(";base64") {
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD.decode(body.trim()).ok().filter(|b| b.len() <= CAP);
    }
    Some(percent_decode(body).into_bytes())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A reference resolved against the script's address, as `new URL(rel, base)` resolves it.
fn join(base: &str, rel: &str) -> String {
    if rel.contains("://") || rel.starts_with("data:") {
        return rel.to_string();
    }
    let base = base.split(['?', '#']).next().unwrap_or(base);
    let (scheme, rest) = base.split_once("://").unwrap_or(("", base));
    if let Some(r) = rel.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    let (host, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (rel, query) = rel.find(['?', '#']).map_or((rel, ""), |i| (&rel[..i], &rel[i..]));
    let joined = if rel.starts_with('/') { rel.to_string() } else { format!("{}{rel}", &path[..path.rfind('/').map_or(0, |i| i + 1)]) };
    let mut segs: Vec<&str> = Vec::new();
    for seg in joined.split('/').skip(1) {
        match seg {
            "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    format!("{scheme}://{host}/{}{query}", segs.join("/"))
}

/// No whitespace or control character anywhere: a URL a page wrote to mislead strobe is no URL.
fn clean(url: &str) -> bool {
    !url.is_empty() && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn scheme(url: &str) -> &str {
    match url.split_once(':') {
        Some((s, _)) if s.eq_ignore_ascii_case("http") => "http",
        Some((s, _)) if s.eq_ignore_ascii_case("https") => "https",
        Some((s, _)) if s.eq_ignore_ascii_case("file") => "file",
        Some((s, _)) => s,
        None => "",
    }
}

/// `scheme://host:port`, lowercase, the port always written.
fn origin(url: &str) -> Option<String> {
    let (host, port, _) = parts(url)?;
    Some(format!("{}://{}:{port}", scheme(url), host.to_ascii_lowercase()))
}

/// An address on this machine (what strobe serves).
pub(super) fn loopback(url: &str) -> bool {
    matches!(parts(url).map(|p| p.0.to_ascii_lowercase()).as_deref(), Some("127.0.0.1" | "localhost" | "[::1]"))
}
