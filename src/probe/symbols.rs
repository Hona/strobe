//! Original names and locations for profile frames, from the page's own source maps (SPEC §8.3).
//!
//! A production bundle profiles as minified names in hashed chunks (`go` in `file-oSEU8O4T.js`). Right
//! after an attribution block's profilers stop, while the target still serves its scripts, strobe asks
//! the page to fetch each profiled script's source map (so it works for http and for an Electron app's
//! custom protocol alike) and stores the maps in the recording (`maps/index.json` and gzip files). Any
//! view can then turn any frame (CPU samples, allocation sites, layout stacks) into its original name,
//! the absolute file on disk, line and column. Complete over short: displays may abbreviate, records
//! do not.

use crate::model::Result;
use crate::target::web::WebPage;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Script URLs in a V8 profile (`.cpuprofile` node list or a sampling heap profile tree).
pub fn urls(profile: &Value, out: &mut BTreeSet<String>) {
    let mut visit = |f: &Value| {
        if let Some(u) = f["url"].as_str().filter(|u| !u.is_empty() && !u.starts_with("chrome") && !u.starts_with("devtools")) {
            out.insert(u.to_string());
        }
    };
    for n in profile["nodes"].as_array().into_iter().flatten() {
        visit(&n["callFrame"]);
    }
    let mut stack: Vec<&Value> = profile.get("head").into_iter().collect();
    while let Some(n) = stack.pop() {
        visit(&n["callFrame"]);
        stack.extend(n["children"].as_array().into_iter().flatten());
    }
}

/// Saves the source maps of every script the page has run, and of `scripts` (profile URLs), into
/// `dir/maps/`, with each script's function ranges: a stack frame that is a call site (layout and style
/// stacks in the trace) is then named after the function containing it, not the identifier at the
/// call. `served` is the folder the page's http origin serves (a web build's output); with it, original
/// sources resolve to files on disk. Returns how many maps were stored.
pub fn store_maps(page: &WebPage, scripts: &BTreeSet<String>, dir: &Path, served: Option<&Path>) -> Result<usize> {
    let maps = dir.join("maps");
    std::fs::create_dir_all(&maps)?;
    // V8's own function ranges per script (UTF-16 offsets), without starting anything that would change
    // how the page runs. Firefox's frames carry their function's start, so they need none.
    let coverage = match page.cdp() {
        Some(p) => {
            p.send("Profiler.enable", json!({}))?;
            p.send_timeout("Profiler.getBestEffortCoverage", json!({}), Duration::from_secs(60)).unwrap_or(Value::Null)
        }
        None => Value::Null,
    };
    let mut ranges: BTreeMap<String, Vec<(i64, i64, String)>> = BTreeMap::new();
    for s in coverage["result"].as_array().into_iter().flatten() {
        let Some(url) = s["url"].as_str().filter(|u| !u.is_empty()) else { continue };
        let fns = ranges.entry(url.to_string()).or_default();
        for f in s["functions"].as_array().into_iter().flatten() {
            let r = &f["ranges"][0];
            if let (Some(a), Some(b)) = (r["startOffset"].as_i64(), r["endOffset"].as_i64()) {
                fns.push((a, b, f["functionName"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    let all: BTreeSet<String> = scripts.iter().cloned().chain(ranges.keys().filter(|u| !u.starts_with("chrome") && !u.starts_with("devtools")).cloned()).collect();
    let mut index = Map::new();
    let all: Vec<&String> = all.iter().collect();
    // The page fetches 16 scripts and their maps at a time: one round trip per batch, not per script.
    let fetched: Vec<(usize, &String, Fetched)> =
        all.chunks(16).enumerate().flat_map(|(b, urls)| fetch_maps(page, urls).into_iter().enumerate().filter_map(move |(j, f)| Some((b * 16 + j, f?)))).map(|(i, f)| (i, all[i], f)).collect();
    for (i, url, fetched) in fetched {
        let file = format!("maps/{i}.map.gz");
        write_gz(&dir.join(&file), fetched.text.as_bytes())?;
        let mut fns = ranges.remove(url).unwrap_or_default();
        fns.sort();
        fns.dedup();
        let scopes = format!("maps/{i}.scopes.json.gz");
        let body = json!({ "lines": fetched.lines, "functions": fns.iter().map(|(a, b, n)| json!([a, b, n])).collect::<Vec<_>>() });
        write_gz(&dir.join(&scopes), body.to_string().as_bytes())?;
        // The map's folder on disk: its URL path under the served folder (http origins only).
        let map_dir = served.zip(url_path(&fetched.url)).map(|(root, p)| root.join(p.trim_start_matches('/'))).and_then(|p| p.parent().map(|d| d.display().to_string()));
        index.insert(url.clone(), json!({ "map": file, "map_url": fetched.url, "map_dir": map_dir, "scopes": scopes }));
    }
    let n = index.len();
    crate::store::write_json(&maps.join("index.json"), &Value::Object(index))?;
    Ok(n)
}

fn write_gz(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes)?;
    crate::store::write_atomic(path, &gz.finish()?)
}

struct Fetched {
    url: String,
    text: String,
    /// The script's line start offsets (UTF-16), to turn a line and column into a function range offset.
    lines: Vec<i64>,
}

/// Each script's map: its `sourceMappingURL` (a URL or an inline `data:` map) and text, fetched by the
/// page, in parallel; `None` where a script has none or the fetch failed.
fn fetch_maps(page: &WebPage, urls: &[&String]) -> Vec<Option<Fetched>> {
    let js = format!(
        r#"Promise.all({}.map(async (u) => {{
  try {{
    const s = await (await fetch(u)).text()
    // The last such comment, however long: an inline data: map is the whole map in base64.
    const at = Math.max(s.lastIndexOf('//# sourceMappingURL='), s.lastIndexOf('//@ sourceMappingURL='))
    const m = at < 0 ? null : /^\/\/[#@] sourceMappingURL=(\S+)/.exec(s.slice(at))
    // Without the comment, a hidden map (STROBE_SOURCEMAP=hidden) sits beside its script as <script>.map.
    const href = m ? new URL(m[1], u).href : u.split(/[?#]/)[0] + '.map'
    const r = await fetch(href)
    if (!r.ok || (!m && !/^\s*\{{/.test(await r.clone().text()))) return null
    const lines = [0]
    for (let i = s.indexOf('\n'); i >= 0; i = s.indexOf('\n', i + 1)) lines.push(i + 1)
    return {{ url: href, text: await r.text(), lines }}
  }} catch {{ return null }}
}}))"#,
        json!(urls)
    );
    let v = page.eval_timeout(&js, Duration::from_secs(60)).unwrap_or(Value::Null);
    let one = |v: &Value| Some(Fetched { url: v["url"].as_str()?.to_string(), text: v["text"].as_str()?.to_string(), lines: v["lines"].as_array()?.iter().filter_map(Value::as_i64).collect() });
    (0..urls.len()).map(|i| one(&v[i])).collect()
}

/// One frame, located as completely as strobe can: the original name, the absolute file on disk when
/// known (else the source path the map records, else the script URL), 1-based line and column.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frame {
    pub name: String,
    pub file: String,
    pub line: i64,
    pub column: i64,
    pub url: String,
    pub mapped: bool,
    /// Where `name` came from: "map", "source" (the declaration on disk), or None (the profile's own).
    pub name_from: Option<String>,
    /// Native frames know their package from symbols (a crate, `std`, an OS module, a GPU driver).
    pub package: Option<String>,
}

impl Frame {
    /// `file:line:column`, or `(native)` for frames without a script or a source file.
    pub fn location(&self) -> String {
        if self.url.is_empty() && self.file.is_empty() {
            return "(native)".into();
        }
        match (self.line, self.column) {
            (l, c) if l > 0 && c > 0 => format!("{}:{l}:{c}", self.file),
            (l, _) if l > 0 => format!("{}:{l}", self.file),
            _ => self.file.clone(),
        }
    }

    pub fn json(&self) -> Value {
        let mut v = json!({ "name": self.name, "name_from": self.name_from, "file": self.file, "line": self.line, "column": self.column, "url": self.url, "source_mapped": self.mapped });
        if let Some(p) = &self.package {
            v["package"] = json!(p);
        }
        v
    }
}

/// Resolves frames through a recording's stored maps (loaded lazily, one decode per script).
pub struct Symbolizer {
    dir: PathBuf,
    index: Value,
    maps: RefCell<BTreeMap<String, Option<SourceMap>>>,
    texts: RefCell<BTreeMap<PathBuf, Vec<String>>>,
    scopes: RefCell<BTreeMap<String, Option<Scopes>>>,
}

/// A script's line starts and V8's function ranges (start, end, minified name), sorted by start.
struct Scopes {
    lines: Vec<i64>,
    functions: Vec<(i64, i64, String)>,
}

impl Symbolizer {
    pub fn open(dir: &Path) -> Symbolizer {
        let index = crate::store::read_json(&dir.join("maps").join("index.json")).unwrap_or(Value::Null);
        Symbolizer { dir: dir.to_path_buf(), index, maps: RefCell::new(BTreeMap::new()), texts: RefCell::new(BTreeMap::new()), scopes: RefCell::new(BTreeMap::new()) }
    }

    /// A call site from a trace stack (`url`, 0-based line and column, the minified function name): the
    /// original file, line and column of the call itself, named after the innermost function that
    /// contains it, resolved the way `frame` resolves a function start.
    pub fn call_site(&self, url: &str, line0: i64, col0: i64, minified: &str) -> Frame {
        let at = self.locate(url, line0, col0, minified, false);
        let enclosing = self.enclosing(url, line0, col0);
        let named = enclosing.map(|(l, c, n)| self.frame(url, l, c, if minified.is_empty() { &n } else { minified }));
        match named {
            Some(f) => Frame { name: f.name, name_from: f.name_from, ..at },
            None => Frame { name: if minified.is_empty() { "(anonymous)".into() } else { minified.to_string() }, name_from: None, ..at },
        }
    }

    /// A call site whose function's start is known (Firefox's frames carry it): located at the call,
    /// named as `frame` names that function.
    pub fn call_in(&self, url: &str, line0: i64, col0: i64, fn_line0: i64, fn_col0: i64, minified: &str) -> Frame {
        let f = self.frame(url, fn_line0, fn_col0, minified);
        Frame { name: f.name, name_from: f.name_from, ..self.locate(url, line0, col0, minified, false) }
    }

    /// The start (0-based line and column) and minified name of the innermost function around a position.
    fn enclosing(&self, url: &str, line0: i64, col0: i64) -> Option<(i64, i64, String)> {
        let file = self.index[url]["scopes"].as_str()?;
        let mut cache = self.scopes.borrow_mut();
        let scopes = cache
            .entry(url.to_string())
            .or_insert_with(|| {
                let mut text = String::new();
                flate2::read::GzDecoder::new(std::fs::File::open(self.dir.join(file)).ok()?).read_to_string(&mut text).ok()?;
                let v: Value = serde_json::from_str(&text).ok()?;
                let lines = v["lines"].as_array()?.iter().filter_map(Value::as_i64).collect();
                let functions = v["functions"].as_array()?.iter().filter_map(|f| Some((f[0].as_i64()?, f[1].as_i64()?, f[2].as_str()?.to_string()))).collect();
                Some(Scopes { lines, functions })
            })
            .as_ref()?;
        let offset = scopes.lines.get(usize::try_from(line0).ok()?)? + col0;
        // Ranges nest, so the containing range that starts last is the innermost.
        let end = scopes.functions.partition_point(|f| f.0 <= offset);
        let f = scopes.functions[..end].iter().rev().find(|f| offset < f.1)?;
        let line = scopes.lines.partition_point(|s| *s <= f.0).checked_sub(1)?;
        Some((line as i64, f.0 - scopes.lines[line], f.2.clone()))
    }

    /// A frame from a V8 call frame (CPU and allocation profiles place a function at its start): `url`,
    /// 0-based `line` and `col`, and the profile's own name.
    pub fn frame(&self, url: &str, line0: i64, col0: i64, minified: &str) -> Frame {
        self.locate(url, line0, col0, minified, true)
    }

    /// A position's original location; `function_start` says it is where a function starts, so its
    /// declaration may name it (a call site's position names nothing).
    fn locate(&self, url: &str, line0: i64, col0: i64, minified: &str, function_start: bool) -> Frame {
        let minified = if minified.is_empty() { "(anonymous)" } else { minified };
        let plain = Frame { name: minified.to_string(), file: url.to_string(), line: line0 + 1, column: col0 + 1, url: url.to_string(), mapped: false, name_from: None, package: None };
        if url.is_empty() || line0 < 0 || col0 < 0 {
            return plain;
        }
        let entry = &self.index[url];
        if !entry.is_object() {
            return plain;
        }
        let mut maps = self.maps.borrow_mut();
        let map = maps.entry(url.to_string()).or_insert_with(|| {
            let mut text = String::new();
            let f = std::fs::File::open(self.dir.join(entry["map"].as_str().unwrap_or(""))).ok()?;
            flate2::read::GzDecoder::new(f).read_to_string(&mut text).ok()?;
            SourceMap::parse(&text)
        });
        let Some(o) = map.as_ref().and_then(|m| m.lookup(line0 as usize, col0, if minified.starts_with('(') { "" } else { minified })) else { return plain };
        let file = entry["map_dir"].as_str().map(|d| normalize(&Path::new(d).join(strip_scheme(&o.source)))).filter(|p| p.is_file());
        // The original declaration decides when strobe can read it (on disk, else the source the map
        // carries): at an arrow function's position the map's name is often its first parameter
        // (`(event) =>` minified to `e=>`), and some bundlers write no names at all.
        let line = function_start.then(|| self.source_line(file.as_deref(), url, map.as_ref(), &o)).flatten();
        let declared = line.as_deref().map(|l| declared_name(l, o.column as usize));
        let (name, from) = match (declared, &o.name) {
            (Some(Declared::Name(n)), _) => (n, Some("source")),
            // Only a function the source shows to be anonymous (a callback argument, or an arrow whose
            // map name is its first parameter: `(event) =>` minified to `e=>`) overrides the map's name.
            // An unnamed function keeps the map's name only where the map names its own position and
            // that name is not its first parameter's; a name found further on the line is another's.
            (Some(Declared::Anonymous), Some(n)) if o.name_exact && line.as_deref().and_then(|l| first_parameter(l, o.column as usize)).as_deref() != Some(n.as_str()) => (n.clone(), Some("map")),
            (Some(Declared::Callback | Declared::Anonymous), _) => ("(anonymous)".to_string(), Some("source")),
            (_, Some(n)) => (n.clone(), Some("map")),
            _ => (minified.to_string(), None),
        };
        Frame {
            name,
            file: file.map(|p| p.display().to_string()).unwrap_or(o.source),
            line: o.line + 1,
            column: o.column + 1,
            url: url.to_string(),
            mapped: true,
            name_from: from.map(str::to_string),
            package: None,
        }
    }

    /// The original line a mapped position is on: from the file on disk, else from the source the map
    /// carries (`sourcesContent`). Each source is split into lines once.
    fn source_line(&self, file: Option<&Path>, url: &str, map: Option<&SourceMap>, o: &Original) -> Option<String> {
        let key = file.map_or_else(|| PathBuf::from(format!("{url}\u{0}{}", o.src)), Path::to_path_buf);
        let mut texts = self.texts.borrow_mut();
        let lines = texts.entry(key).or_insert_with(|| {
            let text = match file {
                Some(f) => std::fs::read_to_string(f).ok(),
                None => map.and_then(|m| m.contents.get(o.src)?.clone()),
            };
            text.map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default()
        });
        lines.get(usize::try_from(o.line).ok()?).cloned()
    }

    /// A frame from a V8 `callFrame` object.
    pub fn call_frame(&self, f: &Value) -> Frame {
        self.frame(f["url"].as_str().unwrap_or(""), f["lineNumber"].as_i64().unwrap_or(-1), f["columnNumber"].as_i64().unwrap_or(-1), f["functionName"].as_str().unwrap_or(""))
    }
}

/// What the original source says about the function that starts at a position.
enum Declared {
    Name(String),
    /// Shown to be anonymous: a callback in an argument list, or an arrow positioned at its parameter.
    Callback,
    /// No name in the source, but not shown to be anonymous: the map's name, if any, stands.
    Anonymous,
    /// Not a function start strobe can read; the map's name, else the profile's, stands.
    Unknown,
}

/// The first parameter of the function that starts at `col` (`async`, `function`, `*` skipped, then its
/// parameter list, or an arrow's lone parameter right there).
fn first_parameter(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let ident = |from: usize| -> String { chars.get(from..).unwrap_or(&[]).iter().take_while(|c| c.is_alphanumeric() || **c == '_' || **c == '$').collect() };
    let mut at = col;
    loop {
        while chars.get(at).is_some_and(|c| c.is_whitespace() || *c == '*') {
            at += 1;
        }
        match ident(at).as_str() {
            "async" | "function" => at += ident(at).chars().count(),
            _ => break,
        }
    }
    if chars.get(at) == Some(&'(') {
        at += 1;
        while chars.get(at).is_some_and(|c| c.is_whitespace()) {
            at += 1;
        }
    }
    Some(ident(at)).filter(|s| !s.is_empty())
}

const KEYWORDS: [&str; 12] = ["function", "async", "if", "for", "while", "switch", "catch", "return", "new", "await", "typeof", "yield"];

/// The name the function starting at `col` (0-based, in characters) of `line` goes by: `function name(`,
/// a method's `name(`, `name = (…) =>`, an object key `name: function`, a JSX handler `onX={(…) =>`.
fn declared_name(line: &str, col: usize) -> Declared {
    let chars: Vec<char> = line.chars().collect();
    let is_id = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let ident_at = |from: usize| -> Option<String> {
        let s: String = chars.get(from..)?.iter().take_while(|c| is_id(**c)).collect();
        (!s.is_empty() && !s.starts_with(|c: char| c.is_ascii_digit())).then_some(s)
    };
    let skip_ws = |from: usize| (from..chars.len()).find(|i| !chars[*i].is_whitespace()).unwrap_or(chars.len());
    // A function that is no assignment's: a callback when it is an argument (after `(` or `,`).
    let unnamed = |upto: usize| {
        let prev = (0..upto.min(chars.len())).rev().find(|i| !chars[*i].is_whitespace()).map(|i| chars[i]);
        if matches!(prev, Some('(' | ',')) { Declared::Callback } else { Declared::Anonymous }
    };
    let before = || assigned_name(without_generics(&chars[..col.min(chars.len())])).map_or_else(|| unnamed(col), Declared::Name);
    let mut at = col;
    if ident_at(at).as_deref() == Some("async") {
        at = skip_ws(at + 5);
    }
    if ident_at(at).as_deref() == Some("function") {
        let after = skip_ws(at + 8);
        let after = if chars.get(after) == Some(&'*') { skip_ws(after + 1) } else { after };
        return ident_at(after).map_or_else(before, Declared::Name);
    }
    if let Some(n) = ident_at(at) {
        // A parameter (`(e) =>`, `(a, b) =>`): the function is the arrow whose list opens before it.
        if let Some(open) = param_list_start(&chars, at) {
            return assigned_name(without_generics(&chars[..open])).map_or(Declared::Callback, Declared::Name);
        }
        let next = skip_ws(at + n.chars().count());
        return match (chars.get(next), chars.get(next + 1)) {
            // `x => …`: a one-parameter arrow, positioned at its parameter.
            (Some('='), Some('>')) => assigned_name(without_generics(&chars[..col.min(chars.len())])).map_or(Declared::Callback, Declared::Name),
            (Some('('), _) | (Some('<'), _) if !KEYWORDS.contains(&n.as_str()) => declares(&chars[..at], n),
            // `name = …`, positioned at its name. (V8 never places a function at an object key.)
            (Some('='), Some(c)) if *c != '=' => Declared::Name(n),
            _ => Declared::Unknown,
        };
    }
    if matches!(chars.get(at), Some('(') | Some('<')) {
        // `function name(` or a method `name(` (or `name<T>(`): the identifier right before the
        // parameter list and its type parameters.
        let head: String = without_generics(&chars[..at]).iter().collect();
        let name: String = head.trim_end().chars().rev().take_while(|c| is_id(*c)).collect::<Vec<_>>().into_iter().rev().collect();
        if !name.is_empty() && !KEYWORDS.contains(&name.as_str()) {
            let start = head.trim_end().chars().count() - name.chars().count();
            return declares(&chars[..start], name);
        }
        return before();
    }
    Declared::Unknown
}

/// `name(` declares a function after `function`, at a statement or member start, or after a method
/// modifier; anywhere else it is a call, and the function there is an anonymous callback in it.
fn declares(lead: &[char], name: String) -> Declared {
    let text: String = lead.iter().collect();
    let text = text.trim_end();
    let word: String = text.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect::<Vec<_>>().into_iter().rev().collect();
    // TypeScript's member modifiers declare as `static` does.
    let modifier = matches!(word.as_str(), "function" | "async" | "static" | "get" | "set" | "private" | "public" | "protected" | "override" | "readonly" | "export" | "abstract" | "declare");
    let declaration = modifier || text.is_empty() || text.ends_with(['{', ';', '}', ',', '*']);
    // A call's callee: the function there is an argument of the call (the map placed it at the callee).
    if declaration { Declared::Name(name) } else { Declared::Callback }
}

/// Where the parameter list opens when the identifier at `at` is a parameter: right after `(`, or after
/// `,` inside an open `(`.
fn param_list_start(chars: &[char], at: usize) -> Option<usize> {
    let prev = (0..at).rev().find(|i| !chars[*i].is_whitespace())?;
    match chars[prev] {
        '(' => Some(prev),
        ',' => {
            let mut depth = 0;
            for i in (0..prev).rev() {
                match chars[i] {
                    ')' | ']' | '}' => depth += 1,
                    '(' if depth == 0 => return Some(i),
                    '[' | '{' if depth == 0 => return None,
                    '(' | '[' | '{' => depth -= 1,
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// The name an anonymous function expression that starts after `before` is assigned to: `name = `,
/// `name = async `, an object key `name: ` (never the `:` of a ternary), a JSX attribute `onX={`.
fn assigned_name(before: &[char]) -> Option<String> {
    let text: String = before.iter().collect();
    let head = text.trim_end();
    let head = head.strip_suffix("async").unwrap_or(head).trim_end();
    let head = head.strip_suffix('{').unwrap_or(head).trim_end();
    let (rest, key) = match (head.strip_suffix('='), head.strip_suffix(':')) {
        // `==`, `!=`, `<=`, `>=` compare; they assign nothing.
        (Some(h), _) if h.ends_with(['=', '!', '<', '>']) && type_annotation(h.trim_end()).is_none() => return None,
        // `name: Type = …` (TypeScript): the name is before the annotation.
        (Some(h), _) => (type_annotation(h.trim_end()).unwrap_or(h).trim_end().trim_end_matches('?').trim_end(), false),
        (None, Some(h)) => (h.trim_end(), true),
        _ => return None,
    };
    let name: String = rest.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect::<Vec<_>>().into_iter().rev().collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    // An object key follows `{`, `,` or a line start; `cond ? a : () =>` is a ternary, not a key.
    let lead = rest[..rest.len() - name.len()].trim_end();
    (!key || lead.is_empty() || lead.ends_with(['{', ','])).then_some(name)
}
/// `name: Type` (or `name?: Type<A, B>`) before an `=`: the text before the annotation's colon, the
/// colon at the outermost level and right after an identifier (or its `?`).
fn type_annotation(head: &str) -> Option<&str> {
    let mut depth = 0i32;
    for (i, c) in head.char_indices().rev() {
        match c {
            '>' | ')' | ']' | '}' => depth += 1,
            '<' | '(' | '[' | '{' => depth -= 1,
            ':' if depth == 0 => {
                let before = head[..i].trim_end().trim_end_matches('?');
                let annotated = before.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$') && !head[..i].ends_with(':');
                return annotated.then_some(&head[..i]);
            }
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    None
}

/// A slice without the type parameters it ends with (`foo<T, U>` → `foo`; an arrow's `=>` is kept).
fn without_generics(chars: &[char]) -> &[char] {
    let mut end = chars.len();
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if end < 2 || chars[end - 1] != '>' || chars[end - 2] == '=' {
        return chars;
    }
    let mut depth = 0;
    for i in (0..end).rev() {
        match chars[i] {
            '>' => depth += 1,
            '<' => {
                depth -= 1;
                if depth == 0 {
                    return &chars[..i];
                }
            }
            _ => {}
        }
    }
    chars
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
    /// The source's index in the map.
    src: usize,
    line: i64,
    column: i64,
    name: Option<String>,
    /// The name is the position's own segment's (or the one ending at the minified name), not one
    /// found further on the line.
    name_exact: bool,
}

struct SourceMap {
    sources: Vec<String>,
    /// `sourcesContent`: each source's original text, where the map carries it.
    contents: Vec<Option<String>>,
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
        let contents = v["sourcesContent"].as_array().map(|a| a.iter().map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
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
        Some(SourceMap { sources, contents, names, lines })
    }

    /// The original location (0-based) and name at a generated position. V8 places a function at its
    /// start: the `function` keyword (coverage ranges), or the parameter list (profiles), where the
    /// generated name `minified` ends right before it. So the name is the segment's own, else the
    /// segment that ends exactly where `minified` does, else one up to 40 columns on in the same source.
    fn lookup(&self, line: usize, col: i64, minified: &str) -> Option<Original> {
        let segs = self.lines.get(line)?;
        let i = segs.partition_point(|s| s[0] <= col);
        let s = segs.get(i.checked_sub(1)?)?;
        let src = usize::try_from(s[1]).ok()?;
        let source = self.sources.get(src)?.clone();
        let named = |seg: &[i64; 5]| usize::try_from(seg[4]).ok().and_then(|n| self.names.get(n)).cloned();
        let width = minified.encode_utf16().count() as i64;
        let before = (width > 0).then(|| segs[..i].iter().rev().find(|n| n[0] + width == col && n[1] == s[1])).flatten().and_then(named);
        let exact = named(s).or(before);
        let name_exact = exact.is_some();
        let name = exact.or_else(|| segs[i..].iter().take_while(|n| n[0] <= col + 40 && n[1] == s[1]).find_map(named));
        Some(Original { source, src, line: s[2], column: s[3], name, name_exact })
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
