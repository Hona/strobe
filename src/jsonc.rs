//! JSON with comments (strobe.json, an agent harness's own config): `//` and `/* */` comments and
//! trailing commas, outside strings. `strip` turns it into JSON byte for byte (comments and those
//! commas become spaces, newlines stay), so an offset or a parse error's line and column in the
//! stripped text is the same place in the file.

/// One step of a path into a JSON value: an object's key or an array's index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// The JSON under the comments. A UTF-8 byte-order mark (some Windows editors write one) goes too.
pub fn strip(text: &str) -> String {
    let t = text.strip_prefix('\u{feff}').map_or_else(|| text.to_string(), |rest| format!(" {rest}"));
    let b = t.as_bytes();
    let mut out = b.to_vec();
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
        } else if let Some(end) = comment_end(b, i) {
            for x in &mut out[i..end] {
                if *x != b'\n' && *x != b'\r' {
                    *x = b' ';
                }
            }
            i = end;
            continue;
        } else if c == b',' {
            // A comma whose next significant byte closes the object or array.
            let mut k = i + 1;
            while k < b.len() && (b[k].is_ascii_whitespace() || comment_end(b, k).is_some()) {
                k = comment_end(b, k).unwrap_or(k + 1);
            }
            if matches!(b.get(k), Some(b'}' | b']')) {
                out[i] = b' ';
            }
        }
        i += 1;
    }
    // Only ASCII bytes (and a mark, as one space) were replaced, by ASCII: still UTF-8.
    String::from_utf8(out).unwrap_or(t)
}

/// Where the comment starting at `i` ends, or `None` when none starts there.
fn comment_end(b: &[u8], i: usize) -> Option<usize> {
    match (b.get(i), b.get(i + 1)) {
        (Some(b'/'), Some(b'/')) => Some(b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p)),
        (Some(b'/'), Some(b'*')) => Some(b[i + 2..].windows(2).position(|w| w == b"*/").map_or(b.len(), |p| i + 2 + p + 2)),
        _ => None,
    }
}

/// The byte offset in `json` (valid JSON, such as `strip`'s output) of what `path` names: an
/// object key's opening quote, an array element's first byte, or for an empty path the value itself.
pub fn offset(json: &str, path: &[Seg]) -> Option<usize> {
    let b = json.as_bytes();
    let mut i = 0;
    at(json, b, &mut i, path)
}

/// 1-based line and column (in characters) of a byte offset.
pub fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line_start = before.rfind('\n').map_or(0, |n| n + 1);
    (before.matches('\n').count() + 1, before[line_start..].chars().count() + 1)
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn at(text: &str, b: &[u8], i: &mut usize, path: &[Seg]) -> Option<usize> {
    ws(b, i);
    let Some((first, rest)) = path.split_first() else { return Some(*i) };
    match (b.get(*i)?, first) {
        (b'{', Seg::Key(want)) => {
            *i += 1;
            loop {
                ws(b, i);
                if b.get(*i)? == &b'}' {
                    return None;
                }
                let start = *i;
                skip(b, i)?;
                let key: String = serde_json::from_str(&text[start..*i]).ok()?;
                ws(b, i);
                (b.get(*i)? == &b':').then_some(())?;
                *i += 1;
                if &key == want {
                    return if rest.is_empty() { Some(start) } else { at(text, b, i, rest) };
                }
                ws(b, i);
                skip(b, i)?;
                ws(b, i);
                if b.get(*i) == Some(&b',') {
                    *i += 1;
                }
            }
        }
        (b'[', Seg::Index(want)) => {
            *i += 1;
            for n in 0.. {
                ws(b, i);
                if b.get(*i)? == &b']' {
                    return None;
                }
                if n == *want {
                    return at(text, b, i, rest);
                }
                skip(b, i)?;
                ws(b, i);
                if b.get(*i) == Some(&b',') {
                    *i += 1;
                }
            }
            None
        }
        _ => None,
    }
}

/// Moves past the value (or key) at `i`.
fn skip(b: &[u8], i: &mut usize) -> Option<()> {
    match b.get(*i)? {
        b'"' => {
            *i += 1;
            while *b.get(*i)? != b'"' {
                *i += if b[*i] == b'\\' { 2 } else { 1 };
            }
            *i += 1;
        }
        b'{' | b'[' => {
            let mut depth = 0usize;
            loop {
                match b.get(*i)? {
                    b'"' => {
                        skip(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            *i += 1;
                            return Some(());
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
        }
        _ => {
            while *i < b.len() && !matches!(b[*i], b',' | b'}' | b']') && !b[*i].is_ascii_whitespace() {
                *i += 1;
            }
        }
    }
    Some(())
}
