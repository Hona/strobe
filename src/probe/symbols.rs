//! Original names and locations for CPU-profile frames, from the page's own source maps (SPEC §8.3).
//!
//! A production bundle profiles as minified names in hashed chunks (`go` in `file-oSEU8O4T.js`). Right
//! after an attribution block's profiler stops, strobe asks the page to fetch each script's map (so it
//! works for http and for an Electron app's custom protocol alike), decodes it, and writes
//! `cpu-<n>.symbols.json`: `"<url>:<line>:<column>"` (as in the profile, 0-based) to
//! `{ name, source, line, column, file }`: the original name when the map has one, the source path
//! exactly as the map records it, the 1-based original line and column, and the absolute file on
//! disk when strobe knows which folder the origin serves and the file exists. Complete over short:
//! displays may abbreviate, the record does not.

use crate::target::chromium::Page;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Symbols for every frame of `profile` whose script has a source map. `served` is the folder the
/// page's http origin serves (a web build's output), used to find the original files on disk.
pub fn resolve(page: &Page, profile: &Value, served: Option<&Path>) -> Value {
    let mut by_url: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for n in profile["nodes"].as_array().into_iter().flatten() {
        let f = &n["callFrame"];
        let url = f["url"].as_str().unwrap_or("");
        let (line, col) = (f["lineNumber"].as_i64().unwrap_or(-1), f["columnNumber"].as_i64().unwrap_or(-1));
        if url.is_empty() || line < 0 || col < 0 || url.starts_with("chrome") || url.starts_with("devtools") {
            continue;
        }
        by_url.entry(url.to_string()).or_default().push((line, col));
    }
    let mut out = Map::new();
    let mut texts: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for (url, positions) in by_url {
        let Some((map_url, text)) = fetch_map(page, &url) else { continue };
        let Some(map) = SourceMap::parse(&text) else { continue };
        // The map's folder on disk: its URL path under the served folder (http origins only).
        let map_dir = served.zip(url_path(&map_url)).map(|(root, p)| root.join(p.trim_start_matches('/'))).and_then(|p| p.parent().map(Path::to_path_buf));
        for (line, col) in positions {
            let Some(o) = map.lookup(line as usize, col) else { continue };
            let file = map_dir.as_ref().map(|d| normalize(&d.join(strip_scheme(&o.source)))).filter(|p| p.is_file());
            // Without a name in the map, the original declaration on disk usually has one.
            let (name, from) = match (&o.name, &file) {
                (Some(n), _) => (Some(n.clone()), Some("map")),
                (None, Some(f)) => {
                    let lines = texts.entry(f.clone()).or_insert_with(|| std::fs::read_to_string(f).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default());
                    let n = lines.get(o.line as usize).and_then(|l| declared_name(l, o.column as usize));
                    let from = n.as_ref().map(|_| "source");
                    (n, from)
                }
                _ => (None, None),
            };
            out.insert(
                format!("{url}:{line}:{col}"),
                json!({ "name": name, "name_from": from, "source": o.source, "line": o.line + 1, "column": o.column + 1, "file": file.map(|p| p.display().to_string()) }),
            );
        }
    }
    Value::Object(out)
}

/// The name a function declared at `col` (0-based, in characters) of `line` goes by: `function name(`,
/// `name(` (a method), or `name = …` / `name: …` just before an arrow or anonymous function.
fn declared_name(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let ident = |from: usize| -> Option<String> {
        let s: String = chars.get(from..)?.iter().take_while(|c| c.is_alphanumeric() || **c == '_' || **c == '$').collect();
        (!s.is_empty() && !s.chars().next().is_some_and(|c| c.is_ascii_digit())).then_some(s)
    };
    let rest: String = chars.get(col..)?.iter().collect();
    let trimmed = rest.trim_start_matches("async ").trim_start();
    if let Some(after) = trimmed.strip_prefix("function") {
        let skip = chars.len() - after.chars().count();
        let at = skip + after.chars().take_while(|c| c.is_whitespace() || *c == '*').count();
        return ident(at);
    }
    if let Some(n) = ident(col).filter(|n| chars.get(col + n.chars().count()) == Some(&'(') && !matches!(n.as_str(), "if" | "for" | "while" | "switch" | "catch")) {
        return Some(n);
    }
    // `name = (`, `name = async (`, `name: function (`, `name = x =>`: the identifier before `=` or `:`.
    let before: String = chars[..col.min(chars.len())].iter().collect();
    let head = before.trim_end().trim_end_matches("async").trim_end();
    let head = head.strip_suffix('=').or_else(|| head.strip_suffix(':'))?.trim_end();
    let name: String = head.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect::<Vec<_>>().into_iter().rev().collect();
    (!name.is_empty()).then_some(name)
}

/// The script's map: its `sourceMappingURL` (a URL or an inline `data:` map) and text, fetched by the page.
fn fetch_map(page: &Page, url: &str) -> Option<(String, String)> {
    let js = format!(
        r#"(async (u) => {{
  try {{
    const s = await (await fetch(u)).text()
    const m = /\/\/[#@] sourceMappingURL=(\S+)\s*$/.exec(s.slice(-600))
    if (!m) return null
    const href = new URL(m[1], u).href
    const r = await fetch(href)
    return r.ok ? {{ url: href, text: await r.text() }} : null
  }} catch {{ return null }}
}})({})"#,
        json!(url)
    );
    let v = page.eval_timeout(&js, Duration::from_secs(30)).ok()?;
    Some((v["url"].as_str()?.to_string(), v["text"].as_str()?.to_string()))
}

/// The path of an http(s) URL, without query or fragment.
fn url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    Some(path.split(['?', '#']).next().unwrap_or(path).to_string())
}

/// `webpack://app/./src/x.ts` → `./src/x.ts`; other sources unchanged.
fn strip_scheme(source: &str) -> &str {
    match source.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|x| x.1).unwrap_or(rest),
        None => source,
    }
}

/// Resolves `.` and `..` without touching the file system (the file may not exist).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

struct Original {
    source: String,
    line: i64,
    column: i64,
    name: Option<String>,
}

struct SourceMap {
    sources: Vec<String>,
    names: Vec<String>,
    /// Per generated line: segments `[generated column, source, source line, source column, name or -1]`,
    /// in column order.
    lines: Vec<Vec<[i64; 5]>>,
}

impl SourceMap {
    fn parse(text: &str) -> Option<SourceMap> {
        let v: Value = serde_json::from_str(text).ok()?;
        let root = v["sourceRoot"].as_str().unwrap_or("");
        let strings = |k: &str| -> Vec<String> { v[k].as_array().map(|a| a.iter().map(|s| s.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default() };
        let sources = strings("sources").into_iter().map(|s| format!("{root}{s}")).collect();
        let names = strings("names");
        let (mut src, mut src_line, mut src_col, mut name) = (0i64, 0i64, 0i64, 0i64);
        let lines = v["mappings"]
            .as_str()?
            .split(';')
            .map(|line| {
                let mut col = 0i64;
                line.split(',')
                    .filter_map(|seg| {
                        let f = vlq(seg);
                        if f.is_empty() {
                            return None;
                        }
                        col += f[0];
                        if f.len() < 4 {
                            return None;
                        }
                        src += f[1];
                        src_line += f[2];
                        src_col += f[3];
                        let n = if f.len() >= 5 {
                            name += f[4];
                            name
                        } else {
                            -1
                        };
                        Some([col, src, src_line, src_col, n])
                    })
                    .collect()
            })
            .collect();
        Some(SourceMap { sources, names, lines })
    }

    /// The original location (0-based) and name at a generated position. V8 places a function at its
    /// start (`function` or the parameter list), while minifiers name the identifier segment just
    /// after it, so a name up to 40 columns on in the same source is taken too.
    fn lookup(&self, line: usize, col: i64) -> Option<Original> {
        let segs = self.lines.get(line)?;
        let i = segs.partition_point(|s| s[0] <= col);
        let s = segs.get(i.checked_sub(1)?)?;
        let source = self.sources.get(usize::try_from(s[1]).ok()?)?.clone();
        let named = |seg: &[i64; 5]| usize::try_from(seg[4]).ok().and_then(|n| self.names.get(n)).cloned();
        let name = named(s).or_else(|| segs[i..].iter().take_while(|n| n[0] <= col + 40 && n[1] == s[1]).find_map(named));
        Some(Original { source, line: s[2], column: s[3], name })
    }
}

/// Base64 VLQ: 5 data bits per character, bit 6 continues, the lowest bit of a value is its sign.
fn vlq(seg: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let (mut value, mut shift) = (0i64, 0u32);
    for c in seg.bytes() {
        let d = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Vec::new(),
        } as i64;
        value += (d & 31) << shift;
        if d & 32 != 0 {
            shift += 5;
            continue;
        }
        out.push(if value & 1 == 1 { -(value >> 1) } else { value >> 1 });
        value = 0;
        shift = 0;
    }
    out
}
