//! `strobe.json`: the project's file, over the user's own (SPEC §9.5). Read strictly: a file that
//! does not parse, a key strobe does not read, or a value of the wrong type (a typo or a guard
//! written as an object would be silently ignored) fails with where it is.

use crate::host;
use crate::jsonc::{self, Seg};
use crate::model::{Error, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The project root for `dir`: the nearest ancestor with a strobe.json, else the git root, else `dir`.
pub fn project_root(dir: &Path) -> PathBuf {
    let found = dir.ancestors().find(|d| d.join("strobe.json").is_file());
    if let Some(d) = found {
        return d.to_path_buf();
    }
    host::git::toplevel(dir).unwrap_or_else(|| dir.to_path_buf())
}

/// The project's strobe.json over the user's (`<state>/strobe.json`). `apps` merge per app, so a
/// user can keep profiles for apps they measure from anywhere. The project's file is checked whole:
/// anything wrong in it is a usage error (exit 2) with its path, line and column. The user's file
/// serves every repository on the machine, so a problem in one of its app profiles fails only when
/// that profile is used (`check_app`), and a key it does not know elsewhere only warns.
pub fn load(root: &Path) -> Result<Value> {
    // Read only: loading the configuration makes and changes nothing (`strobe config`).
    let user_path = host::paths::state_path().join("strobe.json");
    let (mut out, problems) = read(&user_path)?;
    for p in problems {
        match p.path.as_slice() {
            [Seg::Key(apps), Seg::Key(id), ..] if apps == "apps" => {
                if let Some(slot) = out.get_mut("apps").and_then(Value::as_object_mut).and_then(|a| a.get_mut(id)) {
                    if !slot.is_object() {
                        *slot = Value::Object(Map::new());
                    }
                    if slot.get(INVALID).is_none() {
                        slot[INVALID] = Value::String(p.message);
                    }
                }
            }
            _ if p.unknown => eprintln!("strobe: warning: {}; strobe ignores it", p.message),
            _ => return Err(p.error("")),
        }
    }
    let (project, problems) = read(&root.join("strobe.json"))?;
    let more = match problems.len() {
        0 | 1 => String::new(),
        2 => "; 1 more problem".to_string(),
        n => format!("; {} more problems", n - 1),
    };
    if let Some(p) = problems.into_iter().next() {
        return Err(p.error(&more));
    }
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

/// Where `load` keeps a user profile's problem until the profile is used.
const INVALID: &str = "$invalid";

/// Fails when app profile `id`, or one it extends, came from the user's strobe.json with a problem.
pub fn check_app(config: &Value, id: &str) -> Result<()> {
    let mut at = id.to_string();
    for _ in 0..9 {
        let p = &config["apps"][&at];
        if let Some(why) = p[INVALID].as_str() {
            return Err(Error::usage(why.to_string()).hint(format!("fix app profile \"{at}\" in your own strobe.json, or run strobe update if strobe gained the key in a newer release")));
        }
        match p["extends"].as_str() {
            Some(parent) => at = parent.to_string(),
            None => break,
        }
    }
    Ok(())
}

/// What is wrong at one place of a strobe.json, said with its path, line and column.
struct Problem {
    path: Vec<Seg>,
    /// A key strobe does not read, rather than a value of the wrong type or a missing key.
    unknown: bool,
    message: String,
    hint: Option<String>,
}

impl Problem {
    fn error(self, more: &str) -> Error {
        let e = Error::usage(format!("{}{more}", self.message));
        match self.hint {
            Some(h) => e.hint(h),
            None => e,
        }
    }
}

/// One strobe.json, parsed; an absent file is empty. A file that does not parse fails here; what
/// parses but is wrong comes back as problems, first in file order.
fn read(path: &Path) -> Result<(Map<String, Value>, Vec<Problem>)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok((Map::new(), Vec::new())) };
    let json = jsonc::strip(&text);
    let at = |line: usize, col: usize| format!("{}:{line}:{col}", path.display());
    let value: Value = serde_json::from_str(&json).map_err(|e| {
        let why = e.to_string();
        Error::usage(format!("{}: {}", at(e.line(), e.column()), why.split(" at line ").next().unwrap_or(&why)))
    })?;
    let mut found = Vec::new();
    check(&value, &TOP, &mut Vec::new(), &mut found);
    let mut problems: Vec<(usize, Problem)> = found
        .into_iter()
        .map(|f| {
            let offset = jsonc::offset(&json, &f.path).unwrap_or(0);
            let (line, col) = jsonc::line_col(&json, offset);
            let message = format!("{}: {}", at(line, col), f.what);
            (offset, Problem { path: f.path, unknown: f.unknown, message, hint: (!f.hint.is_empty()).then_some(f.hint) })
        })
        .collect();
    problems.sort_by_key(|p| p.0);
    let Value::Object(map) = value else { return Err(Error::usage(format!("{}: strobe.json is one JSON object", at(1, 1)))) };
    Ok((map, problems.into_iter().map(|p| p.1).collect()))
}

/// What strobe reads at one place of strobe.json.
enum Shape {
    /// An object with these keys only; `Req` marks the ones it must have.
    Keys(&'static [(&'static str, Shape)]),
    /// An object whose keys are names the user picks (app ids), each holding this shape.
    Named(&'static Shape),
    /// An array of this shape.
    List(&'static Shape),
    Req(&'static Shape),
    Str,
    Bool,
    /// An array of strings.
    Strings,
    /// An object of strings (an environment).
    StrMap,
    /// An object strobe takes whole (a home's seed files).
    Object,
    /// A path, or one per OS (`{ "windows": …, "macos": …, "linux": … }`).
    PerOs,
    Enum(&'static [&'static str]),
    /// A percentage as text: `"2%"` or `"2"`.
    Percent,
}

use Shape::{Bool, Enum, Keys, List, Named, Object, PerOs, Percent, Req, Str, StrMap, Strings};

const OSES: &[&str] = &["windows", "macos", "linux"];
const BUILD_OS: Shape = Keys(&[("command", Str), ("cwd", Str), ("output", PerOs), ("env", StrMap)]);
const GUARD: Shape = Keys(&[("file", Req(&Str)), ("pid", Str), ("url", Str), ("os", Enum(OSES))]);
/// `apps.<id>.motion`: what the app's owner declares makes it prefer reduced motion (SPEC 13.1).
const MOTION: Shape = Keys(&[("reduce", Keys(&[("args", Strings), ("env", StrMap), ("files", Object)]))]);
const APP: Shape = Keys(&[
    ("exe", PerOs),
    ("extends", Str),
    ("kind", Enum(crate::target::isolate::AppKind::NAMES)),
    ("env", StrMap),
    ("unset", Strings),
    ("dirs", Strings),
    ("files", Object),
    ("guard", List(&GUARD)),
    ("protocols", Strings),
    ("args", Strings),
    ("window", Bool),
    ("moved", Strings),
    ("gpuCache", Enum(&["warm", "cold"])),
    ("motion", MOTION),
]);
const TOP: Shape = Keys(&[
    ("$schema", Str),
    ("build", Keys(&[("command", Str), ("cwd", Str), ("output", PerOs), ("env", StrMap), ("windows", BUILD_OS), ("macos", BUILD_OS), ("linux", BUILD_OS)])),
    ("serve", Keys(&[("command", Str), ("cwd", Str), ("ready", Str), ("dir", Str)])),
    ("apps", Named(&APP)),
    // Reserved: the project's bench files, for tools; strobe does not read it yet (SPEC 9.5).
    ("benches", Strings),
    ("metrics", Keys(&[("equivalence", Percent)])),
    ("harness", Strings),
    ("symbols", Keys(&[("servers", Strings)])),
]);

struct Found {
    path: Vec<Seg>,
    unknown: bool,
    what: String,
    hint: String,
}

fn place(path: &[Seg]) -> String {
    let mut s = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) if s.is_empty() => s += k,
            Seg::Key(k) => s += &format!(".{k}"),
            Seg::Index(i) => s += &format!("[{i}]"),
        }
    }
    if s.is_empty() { "strobe.json".into() } else { s }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "true or false",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn expected(shape: &Shape) -> String {
    match shape {
        Keys(keys) => format!("an object ({})", keys.iter().map(|k| k.0).collect::<Vec<_>>().join(", ")),
        Named(_) | Object => "an object".into(),
        List(_) => "an array".into(),
        Req(inner) => expected(inner),
        Str => "a string".into(),
        Bool => "true or false".into(),
        Strings => "an array of strings".into(),
        StrMap => "an object of strings".into(),
        PerOs => "a path, or one per OS ({ \"windows\": …, \"macos\": …, \"linux\": … })".into(),
        Enum(names) => format!("one of {}", names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ")),
        Percent => "a percentage such as \"2%\"".into(),
    }
}

/// Collects every problem of `v` against `shape` at `path`.
fn check(v: &Value, shape: &Shape, path: &mut Vec<Seg>, out: &mut Vec<Found>) {
    let ok = match (shape, v) {
        (Req(inner), _) => return check(v, inner, path, out),
        (Keys(_) | Named(_) | Object | StrMap, Value::Object(_)) | (List(_) | Strings, Value::Array(_)) | (Str | Enum(_) | Percent, Value::String(_)) | (Bool, Value::Bool(_)) => true,
        (PerOs, Value::String(_) | Value::Object(_)) => true,
        _ => false,
    };
    if !ok {
        return wrong(out, path, format!("{} must be {}, not {}", place(path), expected(shape), type_name(v)));
    }
    match (shape, v) {
        (Keys(keys), Value::Object(m)) => {
            for (k, x) in m {
                path.push(Seg::Key(k.clone()));
                match keys.iter().find(|(name, _)| name == k) {
                    Some((_, inner)) => check(x, inner, path, out),
                    None => unknown(k, &place(&path[..path.len() - 1]), &keys.iter().map(|k| k.0).collect::<Vec<_>>(), path, out),
                }
                path.pop();
            }
            for (name, inner) in keys.iter() {
                if matches!(inner, Req(_)) && !m.contains_key(*name) {
                    wrong(out, path, format!("{} needs \"{name}\"", place(path)));
                }
            }
        }
        (Named(inner), Value::Object(m)) => m.iter().for_each(|(k, x)| {
            path.push(Seg::Key(k.clone()));
            check(x, inner, path, out);
            path.pop();
        }),
        (List(inner), Value::Array(a)) => a.iter().enumerate().for_each(|(i, x)| {
            path.push(Seg::Index(i));
            check(x, inner, path, out);
            path.pop();
        }),
        (Strings, Value::Array(a)) => {
            if let Some(i) = a.iter().position(|x| !x.is_string()) {
                path.push(Seg::Index(i));
                wrong(out, path, format!("{} must be a string, not {}", place(path), type_name(&a[i])));
                path.pop();
            }
        }
        (StrMap, Value::Object(m)) => {
            if let Some((k, x)) = m.iter().find(|(_, x)| !x.is_string()) {
                path.push(Seg::Key(k.clone()));
                wrong(out, path, format!("{} must be a string, not {}", place(path), type_name(x)));
                path.pop();
            }
        }
        (PerOs, Value::Object(m)) => {
            for (k, x) in m {
                path.push(Seg::Key(k.clone()));
                if !OSES.contains(&k.as_str()) {
                    unknown(k, &place(&path[..path.len() - 1]), OSES, path, out);
                } else if !x.is_string() {
                    wrong(out, path, format!("{} must be a path, not {}", place(path), type_name(x)));
                }
                path.pop();
            }
        }
        (Enum(names), Value::String(s)) if !names.contains(&s.as_str()) => wrong(out, path, format!("{} must be {}, not \"{s}\"", place(path), expected(shape))),
        (Percent, Value::String(s)) if !s.trim_end_matches('%').parse::<f64>().is_ok_and(|x| x.is_finite() && x >= 0.0) => {
            wrong(out, path, format!("{} must be {}, not \"{s}\"", place(path), expected(shape)))
        }
        _ => {}
    }
}

fn wrong(out: &mut Vec<Found>, path: &[Seg], what: String) {
    out.push(Found { path: path.to_vec(), unknown: false, what, hint: String::new() });
}

fn unknown(key: &str, place: &str, known: &[&str], path: &[Seg], out: &mut Vec<Found>) {
    let near = suggest(key, known).map(|k| format!(" (did you mean \"{k}\"?)")).unwrap_or_default();
    out.push(Found {
        path: path.to_vec(),
        unknown: true,
        what: format!("strobe reads no key \"{key}\" in {place}{near}"),
        hint: format!("keys there: {}; or run strobe update if the key came with a newer release", known.join(", ")),
    });
}

/// The known key closest to a mistyped one: other case, an OS's other name, or at most two edits away.
fn suggest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let lower = key.to_lowercase();
    let alias = match lower.as_str() {
        "darwin" | "mac" | "osx" => "macos",
        "win32" | "win" | "win64" => "windows",
        other => other,
    };
    known
        .iter()
        .map(|k| (if k.to_lowercase() == alias { 0 } else { edits(&lower, &k.to_lowercase()) }, *k))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
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
