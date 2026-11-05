//! `report experiment <exp> --format pr` (SPEC §11.2): an experiment as one delimited block of
//! GitHub-flavoured Markdown for a pull request body: one hero line, a caution for what got worse, one
//! line per series in plain words, at most one caveat, then a Details drawer (metrics, pairs,
//! profiles, method) and a Rerun drawer. The block sits between two HTML comments that carry a hash
//! of what strobe wrote, so a tool can
//! find it, replace it (`--update-pr`) and see when someone edited it by hand.

use super::fmt;

mod extract;
mod github;
mod implementations;
mod markdown;

use extract::*;
use github::*;
pub use github::{Updated, hero_level, relevel, splice, update_pr};
pub use implementations::{claim, factor_or_pct, fairness, range_x};
use markdown::*;
use crate::model::{self, Better, Error, Result, VerdictKind};
use crate::stats;
use crate::store;
use serde_json::Value;
use std::collections::BTreeMap;

/// What a report calls base and head: an implementation comparison's sides by name (SPEC §9.10),
/// else "Before" and "This PR".
pub(crate) fn side_names(r: &Value) -> (String, String) {
    let name = |i: usize, other: &str| r["variants"][i]["name"].as_str().filter(|_| r["comparison"] == "implementations").unwrap_or(other).to_string();
    (name(0, "Before"), name(1, "This PR"))
}

/// GitHub refuses a body over 65,536 characters (UTF-16 units); the block leaves most of it to the author.
pub const CAP: usize = 24_000;
/// How much a long report sheds, in order, until it fits `CAP` (`body` documents each step).
const SHED_MAX: u8 = 7;

/// The whole block: markers, the generated-by line, the report, the end line.
pub fn block(r: &Value) -> String {
    let id = r["id"].as_str().unwrap_or("");
    let mut content = String::new();
    for level in 0..=SHED_MAX {
        content = framed(r, level);
        if utf16(&content) <= CAP {
            break;
        }
    }
    if utf16(&content) > CAP {
        // Nothing left to shed but the series themselves: cut at a line, and say so.
        let mut cut = String::new();
        for line in content.lines() {
            if utf16(&cut) + utf16(line) > CAP - 400 {
                break;
            }
            cut += line;
            cut.push('\n');
        }
        content = format!("{cut}\n</details>\n\n<sub>The rest of this report did not fit a pull request: <code>strobe report experiment {id} --open</code>.</sub>");
    }
    let content = content.trim().to_string();
    format!("{}\n\n{content}\n\n{}\n", begin_marker(id, &digest(&content)), end_marker(id))
}

/// The report between its two quiet visible boundary lines.
fn framed(r: &Value, level: u8) -> String {
    let id = r["id"].as_str().unwrap_or("");
    // The bench file: a body with several blocks says which file each covers.
    let file = r["file_repo"].as_str().map(str::to_string).unwrap_or_else(|| repo_relative(r["file"].as_str().unwrap_or("")));
    let file = if file.is_empty() { String::new() } else { format!(" · {}", cell(&file)) };
    let top = format!("<sub>strobe · {id}{file} · generated</sub>");
    let bottom = format!("<sub>strobe · {id} · end of generated report</sub>");
    let body = if r["kind"] == "bisect" { bisect(r) } else { Doc::new(r, level).render() };
    // No machine's paths in a pull request, whatever a note or a refusal quoted.
    format!("{top}\n\n{}\n\n{bottom}", scrub_paths(body.trim()))
}

/// `strobe 0.6.0 (a1b2c3d4e5)`: Cargo.toml's version and, for a build from a checkout, its commit, so
/// a build from main after a release does not pass for the release.
pub fn version() -> String {
    let commit = env!("STROBE_COMMIT");
    if commit.is_empty() { format!("strobe {}", env!("CARGO_PKG_VERSION")) } else { format!("strobe {}+{commit}", env!("CARGO_PKG_VERSION")) }
}

/// A bench file's path from its repository's root, with forward slashes; the file name when it is in
/// no repository (or no longer on this disk).
pub fn repo_relative(file: &str) -> String {
    let clean = file.trim_start_matches(r"\\?\").replace('\\', "/");
    let path = std::path::Path::new(&clean);
    for dir in path.ancestors().skip(1) {
        if dir.join(".git").exists()
            && let Ok(rel) = path.strip_prefix(dir)
        {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    clean.rsplit('/').next().unwrap_or(&clean).to_string()
}

/// What the hero line and the caution say, for the card (`card.rs`) to draw the same: each a
/// (series, comparison).
pub struct Hero<'a> {
    /// The hero's wins: each celebrated series' main metric (a metric that won in several series is
    /// one phrase in the line and one row per series here), then the one extra tested metric.
    pub wins: Vec<(&'a Value, &'a Value)>,
    /// Everything decidedly worse: main and tested metrics.
    pub worse: Vec<(&'a Value, &'a Value)>,
    /// Late frames up by a point or more: (series, base share, head share, pairs worse, pairs).
    pub late: Vec<(&'a Value, f64, f64, usize, usize)>,
    /// The heading without a win or a regression.
    pub calm: String,
}

pub fn hero(r: &Value) -> Hero<'_> {
    let d = Doc::new(r, 0);
    let second = d.win_phrases().1;
    let mut keys: Vec<String> = Vec::new();
    let mut wins = Vec::new();
    for s in d.series.iter().copied() {
        let c = &s["primary"];
        if d.aa || s["kind"] == "leak" || s["not_run"] == true || d.primary_out(s) != Out::Better || beyond_margin(c) {
            continue;
        }
        // The same groups as `win_phrases`, at most three.
        let key = format!("{}|{}", c["metric"].as_str().unwrap_or(""), d.hero_parts(c).1);
        if !keys.contains(&key) {
            if keys.len() == 3 {
                continue;
            }
            keys.push(key);
        }
        wins.push((s, c));
    }
    wins.extend(second);
    let (mut worse, mut late) = (Vec::new(), Vec::new());
    for s in d.series.iter().copied() {
        if d.aa || s["kind"] == "leak" || s["not_run"] == true {
            continue;
        }
        if d.primary_out(s) == Out::Worse {
            worse.push((s, &s["primary"]));
        }
        worse.extend(tested(s).into_iter().skip(1).filter(|c| outcome(c, d.aa) == Out::Worse).map(|c| (s, c)));
        if let Some((b, h, k, n)) = late_frames(s, r)
            && h - b >= 0.01
        {
            late.push((s, b, h, k, n));
        }
    }
    Hero { wins, worse, late, calm: d.calm() }
}
