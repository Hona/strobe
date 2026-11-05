//! `strobe.json`: the project's file, over the user's own (SPEC §9.5). Read strictly: a file that
//! does not parse, or a key strobe does not read (a typo would be silently ignored), fails with
//! where it is.

use crate::host;
use crate::model::{Error, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The project root for `dir`: the nearest ancestor with a strobe.json, else the git root, else `dir`.
pub fn project_root(dir: &Path) -> PathBuf {
    let found = dir.ancestors().find(|d| d.join("strobe.json").is_file());
    if let Some(d) = found {
        return d.to_path_buf();
    }
    host::output(Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"])).map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf())
}

/// The project's strobe.json over the user's (`<state>/strobe.json`). `apps` merge per app, so a
/// user can keep profiles for apps they measure from anywhere. Either file failing to parse, or
/// naming a key strobe does not read, is a usage error (exit 2) with its path, line and column.
pub fn load(root: &Path) -> Result<Value> {
    let user = read(&host::paths::state_dir().join("strobe.json"))?;
    let project = read(&root.join("strobe.json"))?;
    let mut out = user;
    let mut apps = out.remove("apps").and_then(|a| a.as_object().cloned()).unwrap_or_default();
    for (k, v) in project {
        if k == "apps" {
            apps.extend(v.as_object().cloned().unwrap_or_default());
            continue;
        }
        out.insert(k, v);
    }
    out.insert("apps".into(), Value::Object(apps));
    Ok(Value::Object(out))
}

/// One strobe.json, checked; an absent file is empty.
fn read(path: &Path) -> Result<Map<String, Value>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(Map::new()) };
    // Some Windows editors begin UTF-8 with a byte-order mark, which JSON does not allow.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let at = |line: usize, col: usize| format!("{}:{line}:{col}", path.display());
    let value: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| {
        let why = e.to_string();
        Error::usage(format!("{}: {}", at(e.line(), e.column()), why.split(" at line ").next().unwrap_or(&why)))
    })?;
    let mut unknown = Vec::new();
    check(&value, &TOP, "", &mut unknown);
    let Value::Object(map) = value else { return Err(Error::usage(format!("{}: strobe.json is one JSON object", at(1, 1)))) };
    if let Some((key, place, known)) = unknown.first() {
        let (line, col) = locate(text, key);
        let near = suggest(key, known).map(|k| format!(" (did you mean \"{k}\"?)")).unwrap_or_default();
        let more = match unknown.len() - 1 {
            0 => String::new(),
            1 => "; 1 more unknown key".to_string(),
            n => format!("; {n} more unknown keys"),
        };
        return Err(Error::usage(format!("{}: strobe reads no key \"{key}\" in {place}{near}{more}", at(line, col))).hint(format!("keys there: {}", known.join(", "))));
    }
    Ok(map)
}

/// What strobe reads at one place of strobe.json.
enum Shape {
    /// An object with these keys only.
    Keys(&'static [(&'static str, Shape)]),
    /// An object whose keys are names the user picks (app ids), each holding this shape.
    Named(&'static Shape),
    /// An array of this shape.
    List(&'static Shape),
    /// A value strobe takes as a whole (a string, a path per OS, an env map, a seed file's contents).
    Any,
}

use Shape::{Any, Keys, List, Named};

const BUILD_OS: Shape = Keys(&[("command", Any), ("cwd", Any), ("output", Any), ("env", Any)]);
const GUARD: Shape = Keys(&[("file", Any), ("pid", Any), ("url", Any), ("os", Any)]);
const APP: Shape = Keys(&[
    ("exe", Any),
    ("extends", Any),
    ("kind", Any),
    ("env", Any),
    ("unset", Any),
    ("dirs", Any),
    ("files", Any),
    ("guard", List(&GUARD)),
    ("protocols", Any),
    ("args", Any),
    ("window", Any),
    ("moved", Any),
    ("gpuCache", Any),
]);
const TOP: Shape = Keys(&[
    ("$schema", Any),
    ("build", Keys(&[("command", Any), ("cwd", Any), ("output", Any), ("env", Any), ("windows", BUILD_OS), ("macos", BUILD_OS), ("linux", BUILD_OS)])),
    ("serve", Keys(&[("command", Any), ("cwd", Any), ("ready", Any), ("dir", Any)])),
    ("apps", Named(&APP)),
    ("benches", Any),
    ("metrics", Keys(&[("equivalence", Any)])),
    ("packages", Keys(&[("groups", Any)])),
    ("harness", Any),
    ("symbols", Keys(&[("servers", Any)])),
]);

/// Collects `(key, where, the keys known there)` for every key `shape` does not have.
fn check(v: &Value, shape: &Shape, place: &str, out: &mut Vec<(String, String, Vec<&'static str>)>) {
    let here = |k: &str| if place.is_empty() { k.to_string() } else { format!("{place}.{k}") };
    match (shape, v) {
        (Keys(keys), Value::Object(m)) => {
            for (k, x) in m {
                match keys.iter().find(|(name, _)| name == k) {
                    Some((_, inner)) => check(x, inner, &here(k), out),
                    None => out.push((k.clone(), if place.is_empty() { "strobe.json".into() } else { place.to_string() }, keys.iter().map(|(n, _)| *n).collect())),
                }
            }
        }
        (Named(inner), Value::Object(m)) => m.iter().for_each(|(k, x)| check(x, inner, &here(k), out)),
        (List(inner), Value::Array(a)) => a.iter().enumerate().for_each(|(i, x)| check(x, inner, &format!("{place}[{i}]"), out)),
        _ => {}
    }
}

/// The line and column of `"key":` in the file, for the message (1, 1 when not found).
fn locate(text: &str, key: &str) -> (usize, usize) {
    let needle = serde_json::to_string(key).unwrap_or_default();
    let mut from = 0;
    while let Some(i) = text[from..].find(&needle).map(|i| i + from) {
        if text[i + needle.len()..].trim_start().starts_with(':') {
            let line = text[..i].matches('\n').count() + 1;
            let col = i - text[..i].rfind('\n').map_or(0, |n| n + 1) + 1;
            return (line, col);
        }
        from = i + needle.len();
    }
    (1, 1)
}

/// The known key closest to a mistyped one: other case, or at most two edits away.
fn suggest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let lower = key.to_lowercase();
    known.iter().map(|k| (if k.to_lowercase() == lower { 0 } else { edits(&lower, &k.to_lowercase()) }, *k)).filter(|(d, _)| *d <= 2).min_by_key(|(d, _)| *d).map(|(_, k)| k)
}

fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// JSON with comments: `//` and `/* */` comments and trailing commas become spaces, outside
/// strings only, so line and column in a parse error are the file's own.
fn strip_jsonc(t: &str) -> String {
    let b = t.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, i: usize| {
        if out[i] != b'\n' && out[i] != b'\r' {
            out[i] = b' ';
        }
    };
    let mut i = 0;
    let mut in_string = false;
    while i < b.len() {
        let c = b[i];
        if in_string {
            match c {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else if c == b'"' {
            in_string = true;
        } else if let Some(end) = skip_comment(b, i) {
            for k in i..end {
                blank(&mut out, k);
            }
            i = end;
            continue;
        } else if c == b',' {
            // A comma whose next significant byte closes the object or array.
            let mut k = i + 1;
            while k < b.len() && (b[k].is_ascii_whitespace() || starts_comment(b, k)) {
                k = skip_comment(b, k).unwrap_or(k + 1);
            }
            if matches!(b.get(k), Some(b'}' | b']')) {
                out[i] = b' ';
            }
        }
        i += 1;
    }
    // Only ASCII bytes were replaced, by ASCII: the text is still UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| t.to_string())
}

fn starts_comment(b: &[u8], i: usize) -> bool {
    b[i] == b'/' && matches!(b.get(i + 1), Some(b'/' | b'*'))
}

/// The index after the comment starting at `i`.
fn skip_comment(b: &[u8], i: usize) -> Option<usize> {
    if !starts_comment(b, i) {
        return None;
    }
    if b[i + 1] == b'/' { Some(b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p)) } else { Some(b[i + 2..].windows(2).position(|w| w == b"*/").map_or(b.len(), |p| i + 2 + p + 2)) }
}
