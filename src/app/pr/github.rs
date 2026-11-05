//! `--update-pr` (SPEC §11.2): a block's markers and hash, its place in a pull request body, and the edit through `gh`.

use super::*;

pub(super) fn begin_marker(id: &str, hash: &str) -> String {
    format!("<!-- strobe:report {id} v1 begin sha256:{hash} -->")
}

pub(super) fn end_marker(id: &str) -> String {
    format!("<!-- strobe:report {id} end -->")
}

/// The first 16 hex digits of the SHA-256 of the content between the markers, trimmed, with LF line
/// ends and without image addresses: gh replaces `./card.svg` with the uploaded image's.
pub(super) fn digest(content: &str) -> String {
    use sha2::Digest;
    let norm = content.replace("\r\n", "\n");
    let norm = regex_lite::Regex::new(r"(!\[[^\]]*\])\([^)\s]*\)").expect("pattern").replace_all(&norm, "$1()");
    sha2::Sha256::digest(norm.trim().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub(super) fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// What `update_pr` did.
pub struct Updated {
    pub url: String,
    /// Images gh attached (0: the block went in as text).
    pub images: usize,
    /// The same report was there already, so the body was left alone.
    pub unchanged: bool,
    /// GitHub showed the new block when strobe read the body back (it can serve the old body for a
    /// moment after an edit, and a second update read then would drop this block).
    pub seen: bool,
}

/// How long GitHub may serve the body from before an edit, at most, before strobe reads it again.
const LAG: std::time::Duration = std::time::Duration::from_secs(3);

/// Replaces this experiment's block in pull request `n`'s body, or appends it, through `gh`. The
/// author's text outside the markers is untouched. A block edited by hand (its content no longer
/// matches the marker's hash) is refused unless `force`. `attach` are the images the block names as
/// `./<file name>`: `gh pr edit --attach` uploads them and puts each one's address in its place. When
/// gh cannot (a gh without `--attach`, a failed upload), `plain`, the block without images, goes in.
pub fn update_pr(n: &str, repo: Option<&str>, id: &str, block: &str, force: bool, attach: &[std::path::PathBuf], plain: Option<&str>) -> Result<Updated> {
    let gh = |extra: &[&str]| {
        let mut c = std::process::Command::new("gh");
        c.args(["pr"]).args(extra);
        if let Some(r) = repo {
            c.args(["-R", r]);
        }
        c
    };
    // The body and address as GitHub serves them now.
    let read = || -> Result<(String, String)> {
        let out = gh(&["view", n, "--json", "body,url"]).output().map_err(|e| Error::msg(format!("--update-pr needs the GitHub CLI (gh) on PATH: {e}")))?;
        if !out.status.success() {
            return Err(Error::msg(format!("gh pr view {n} failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        let pr: Value = serde_json::from_slice(&out.stdout).map_err(|_| Error::msg("gh pr view: unexpected output"))?;
        Ok((pr["url"].as_str().unwrap_or("").to_string(), pr["body"].as_str().unwrap_or("").replace("\r\n", "\n")))
    };
    let (url, current) = read()?;
    // The hero's heading nests under the author's sections, and every block of the body agrees.
    let level = hero_level(&splice(&current, id, block, true)?, id);
    let (block, plain) = (relevel(block, level), plain.map(|p| relevel(p, level)));
    let (block, plain) = (block.as_str(), plain.as_deref());
    let original = current.clone();
    let current = relevel_others(&current, id, level);
    // Image addresses aside, the same report: keep the body and the images already uploaded.
    let hash = |text: &str, id: &str| {
        let re = regex_lite::Regex::new(&format!(r"<!-- strobe:report {} v1 begin sha256:([0-9a-f]+) -->", regex_lite::escape(id))).ok()?;
        re.captures(text).and_then(|m| m.get(1)).map(|g| g.as_str().to_string())
    };
    if !force
        && current == original
        && let (Some(a), Some(b)) = (hash(&current, id), hash(block, id))
        && a == b
    {
        // Unless someone edited the block by hand: splice refuses that.
        splice(&current, id, block, false)?;
        return Ok(Updated { url, images: 0, unchanged: true, seen: true });
    }
    // After an edit: whether GitHub serves the block, read back at once and once more after `LAG`.
    let seen = |text: &str| -> bool {
        let want = hash(text, id);
        for wait in [None, Some(LAG)] {
            if let Some(w) = wait {
                std::thread::sleep(w);
            }
            if read().is_ok_and(|(_, body)| hash(&body, id) == want) {
                return true;
            }
        }
        crate::app::progress(format!("strobe: GitHub still served PR {n}'s old body {} s after the edit; it may lag: check with gh pr view {n} before another --update-pr (a rerun puts the block in again)", LAG.as_secs()));
        false
    };
    let edit = |text: &str, files: &[std::path::PathBuf]| -> Result<bool> {
        let body = splice(&current, id, text, force)?;
        if utf16(&body) > 65_536 {
            return Err(Error::usage(format!("the body would be {} characters, over GitHub's 65,536: shorten the author's text or the report", utf16(&body))));
        }
        // gh finds `./name` beside the body file.
        let dir = files.first().and_then(|p| p.parent()).map(std::path::Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
        let tmp = dir.join(format!("strobe-pr-{id}-{}.md", std::process::id()));
        std::fs::write(&tmp, &body)?;
        let path = tmp.display().to_string();
        let mut args = vec!["edit".to_string(), n.to_string(), "--body-file".to_string(), path];
        for f in files {
            args.push("--attach".into());
            args.push(f.display().to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let done = gh(&refs).output();
        let _ = std::fs::remove_file(&tmp);
        let done = done.map_err(|e| Error::msg(format!("gh pr edit: {e}")))?;
        if !done.status.success() && files.is_empty() {
            return Err(Error::msg(format!("gh pr edit {n} failed: {}", String::from_utf8_lossy(&done.stderr).trim())));
        }
        if !done.status.success() {
            crate::app::progress(format!("strobe: gh could not attach the images ({}); the report goes in as text", String::from_utf8_lossy(&done.stderr).trim()));
        }
        Ok(done.status.success())
    };
    if !attach.is_empty() {
        let help = gh(&["edit", "--help"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("--attach")).unwrap_or(false);
        if !help {
            crate::app::progress("strobe: this gh cannot attach images (gh pr edit --attach); the report goes in as text");
        } else if edit(block, attach)? {
            return Ok(Updated { url, images: attach.len(), unchanged: false, seen: seen(block) });
        }
    }
    let text = if attach.is_empty() { block } else { plain.unwrap_or(block) };
    edit(text, &[])?;
    Ok(Updated { url, images: 0, unchanged: false, seen: seen(text) })
}

/// Every strobe block in `body`: (experiment, where it starts, where its end marker ends, whether
/// its content still matches its hash).
pub(super) fn blocks_in(body: &str) -> Vec<(String, usize, usize, bool)> {
    let begin = regex_lite::Regex::new(r"<!-- strobe:report (\S+) v1 begin sha256:([0-9a-f]+) -->").expect("pattern");
    let mut out = Vec::new();
    for m in begin.captures_iter(body) {
        let (Some(whole), Some(id), Some(hash)) = (m.get(0), m.get(1), m.get(2)) else { continue };
        let end = end_marker(id.as_str());
        let Some(at) = body[whole.end()..].find(&end).map(|i| whole.end() + i) else { continue };
        out.push((id.as_str().to_string(), whole.start(), at + end.len(), digest(&body[whole.end()..at]) == hash.as_str()));
    }
    out
}

/// The heading level a block's hero takes in `body` (which may already hold this block): below the
/// author's own headings (`##` sections make it `###`); `#` only alone in a body without headings,
/// and `##` beside other blocks, so several experiments read as peers, not as the PR's title.
pub fn hero_level(body: &str, id: &str) -> usize {
    let body = body.replace("\r\n", "\n");
    let blocks = blocks_in(&body);
    let mut author = String::new();
    let mut from = 0;
    for (_, start, end, _) in &blocks {
        author += &body[from..*start];
        from = *end;
    }
    author += &body[from..];
    let mut fence = false;
    let mut top: Option<usize> = None;
    for l in author.lines() {
        if l.trim_start().starts_with("```") {
            fence = !fence;
        }
        let hashes = l.chars().take_while(|c| *c == '#').count();
        if !fence && (1..=6).contains(&hashes) && l[hashes..].starts_with(' ') {
            top = Some(top.map_or(hashes, |t| t.min(hashes)));
        }
    }
    let others = blocks.iter().filter(|b| b.0 != id).count();
    match top {
        Some(t) => (t + 1).min(3),
        None if others > 0 => 2,
        None => 1,
    }
}

/// A whole block (markers included) with its hero at heading `level`, its hash redone.
pub fn relevel(block: &str, level: usize) -> String {
    let block = block.replace("\r\n", "\n");
    let Some((id, start, end, _)) = blocks_in(&block).into_iter().next() else { return block };
    let inner = &block[start..end];
    let (Some(open), Some(close)) = (inner.find(" -->\n"), inner.rfind("\n<!-- strobe:report ")) else { return block };
    let content = inner[open + 5..close].trim().to_string();
    let hero = regex_lite::Regex::new(r"(?m)^#{1,3} ").expect("pattern");
    let Some(m) = hero.find(&content) else { return block };
    // Only the hero: the first heading outside the drawers, before any `<details>`.
    if content[..m.start()].contains("<details") {
        return block;
    }
    let content = format!("{}{} {}", &content[..m.start()], "#".repeat(level), &content[m.end()..]);
    format!("{}{}\n\n{content}\n\n{}{}", &block[..start], begin_marker(&id, &digest(&content)), end_marker(&id), &block[end..])
}

/// `body` with every other block strobe wrote (and nobody edited) at hero `level`.
pub(super) fn relevel_others(body: &str, id: &str, level: usize) -> String {
    let body = body.replace("\r\n", "\n");
    let mut out = String::new();
    let mut from = 0;
    for (other, start, end, intact) in blocks_in(&body) {
        out += &body[from..start];
        out += &if other != id && intact { relevel(&body[start..end], level) } else { body[start..end].to_string() };
        from = end;
    }
    out + &body[from..]
}

/// `body` with experiment `id`'s block replaced by `block` (or `block` appended after a blank line).
pub fn splice(body: &str, id: &str, block: &str, force: bool) -> Result<String> {
    let body = body.replace("\r\n", "\n");
    let begin = regex_lite::Regex::new(&format!(r"<!-- strobe:report {} v1 begin sha256:([0-9a-f]+) -->", regex_lite::escape(id))).map_err(|e| Error::msg(e.to_string()))?;
    let end = end_marker(id);
    let Some(m) = begin.captures(&body) else {
        let sep = if body.trim().is_empty() { "" } else { "\n\n" };
        return Ok(format!("{}{sep}{}", body.trim_end(), block.trim_end()));
    };
    let whole = m.get(0).map(|g| (g.start(), g.end())).unwrap_or((0, 0));
    let hash = m.get(1).map(|g| g.as_str()).unwrap_or("");
    let Some(at) = body[whole.1..].find(&end).map(|i| whole.1 + i) else {
        return Err(Error::usage(format!("the body has the begin marker of {id}'s strobe report but not its end marker ({end}): fix the body by hand, then rerun")));
    };
    if digest(&body[whole.1..at]) != hash && !force {
        return Err(Error::usage(format!(
            "{id}'s strobe report in this body was edited by hand (its content no longer matches the hash in its marker): move your words outside the block, or pass --force to overwrite it"
        )));
    }
    Ok(format!("{}{}{}", &body[..whole.0], block.trim_end(), &body[at + end.len()..]))
}
