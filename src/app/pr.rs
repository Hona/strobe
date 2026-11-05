//! `report experiment <exp> --format pr` (SPEC §11.2): an experiment as one delimited block of
//! GitHub-flavoured Markdown for a pull request body: one hero line, a caution for what got worse, one
//! line per series in plain words, at most one caveat, then a Details drawer (metrics, pairs,
//! profiles, method) and a Rerun drawer. The block sits between two HTML comments that carry a hash
//! of what strobe wrote, so a tool can
//! find it, replace it (`--update-pr`) and see when someone edited it by hand.

use super::fmt;
use crate::model::{self, Better, Error, Result};
use crate::store;
use serde_json::Value;
use std::collections::BTreeMap;

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

fn begin_marker(id: &str, hash: &str) -> String {
    format!("<!-- strobe:report {id} v1 begin sha256:{hash} -->")
}

fn end_marker(id: &str) -> String {
    format!("<!-- strobe:report {id} end -->")
}

/// The first 16 hex digits of the SHA-256 of the content between the markers, trimmed, with LF line ends.
fn digest(content: &str) -> String {
    use sha2::Digest;
    let norm = content.replace("\r\n", "\n");
    sha2::Sha256::digest(norm.trim().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The report between its two quiet visible boundary lines.
fn framed(r: &Value, level: u8) -> String {
    let id = r["id"].as_str().unwrap_or("");
    let top = format!("<sub>strobe · {id} · generated</sub>");
    let bottom = format!("<sub>strobe · {id} · end of generated report</sub>");
    let body = if r["kind"] == "bisect" { bisect(r) } else { Doc::new(r, level).render() };
    format!("{top}\n\n{}\n\n{bottom}", body.trim())
}

/// `strobe 0.6.0 (a1b2c3d4e5)`: Cargo.toml's version and, for a build from a checkout, its commit, so
/// a build from main after a release does not pass for the release.
pub fn version() -> String {
    let commit = env!("STROBE_COMMIT");
    if commit.is_empty() { format!("strobe {}", env!("CARGO_PKG_VERSION")) } else { format!("strobe {}+{commit}", env!("CARGO_PKG_VERSION")) }
}

/// Replaces this experiment's block in pull request `n`'s body, or appends it, through `gh`. The
/// author's text outside the markers is untouched. A block edited by hand (its content no longer
/// matches the marker's hash) is refused unless `force`.
pub fn update_pr(n: &str, repo: Option<&str>, id: &str, block: &str, force: bool) -> Result<String> {
    let gh = |extra: &[&str]| {
        let mut c = std::process::Command::new("gh");
        c.args(["pr"]).args(extra);
        if let Some(r) = repo {
            c.args(["-R", r]);
        }
        c
    };
    let out = gh(&["view", n, "--json", "body,url"]).output().map_err(|e| Error::msg(format!("--update-pr needs the GitHub CLI (gh) on PATH: {e}")))?;
    if !out.status.success() {
        return Err(Error::msg(format!("gh pr view {n} failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let pr: Value = serde_json::from_slice(&out.stdout).map_err(|_| Error::msg("gh pr view: unexpected output"))?;
    let url = pr["url"].as_str().unwrap_or("").to_string();
    let body = splice(pr["body"].as_str().unwrap_or(""), id, block, force)?;
    if utf16(&body) > 65_536 {
        return Err(Error::usage(format!("the body would be {} characters, over GitHub's 65,536: shorten the author's text or the report", utf16(&body))));
    }
    let tmp = std::env::temp_dir().join(format!("strobe-pr-{id}-{}.md", std::process::id()));
    std::fs::write(&tmp, &body)?;
    let path = tmp.display().to_string();
    let edit = gh(&["edit", n, "--body-file", &path]).output();
    let _ = std::fs::remove_file(&tmp);
    let edit = edit.map_err(|e| Error::msg(format!("gh pr edit: {e}")))?;
    if !edit.status.success() {
        return Err(Error::msg(format!("gh pr edit {n} failed: {}", String::from_utf8_lossy(&edit.stderr).trim())));
    }
    Ok(url)
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

// ---------- outcome of one comparison ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Out {
    Desc,
    Gated,
    Refused,
    Same,
    Unclear,
    Stopped,
    Saturated,
    Better,
    Worse,
    NoWorse,
    NoWorseSlower,
    AaFalse,
    NotRun,
}

fn verdict(c: &Value) -> &str {
    c["verdict"].as_str().unwrap_or("")
}

fn better_of(metric: &str) -> Better {
    model::metric(metric).map(|m| m.better).unwrap_or(Better::Lower)
}

fn unit_of(metric: &str) -> &'static str {
    model::metric(metric).map(|m| m.unit).unwrap_or("")
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn outcome(c: &Value, aa: bool) -> Out {
    let v = verdict(c);
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    match v {
        "·" => Out::Desc,
        "—" | "" => Out::Gated,
        _ if v.starts_with("refused") => Out::Refused,
        _ if v.starts_with("same") => Out::Same,
        _ if v.starts_with("saturated") => Out::Saturated,
        _ if v.starts_with("stopped") => Out::Stopped,
        _ if v.starts_with("no worse") && v.contains(", but ") => Out::NoWorseSlower,
        _ if v.starts_with("no worse") => Out::NoWorse,
        _ if v.starts_with("worse") => Out::Worse,
        _ if v.starts_with("unclear") || v.starts_with("underpowered") => Out::Unclear,
        _ if !(hi < 0.0 || lo > 0.0) => Out::Unclear,
        _ if aa => Out::AaFalse,
        _ => {
            let improved = (hi < 0.0) == (better_of(c["metric"].as_str().unwrap_or("")) == Better::Lower);
            if improved { Out::Better } else { Out::Worse }
        }
    }
}

/// Whether a change is decided in a direction, so it is set in bold.
fn marked(c: &Value, o: Out) -> bool {
    match o {
        Out::Better | Out::Worse | Out::AaFalse | Out::NoWorseSlower => true,
        // `no worse (≤2%, faster)`: within the margin, and the interval also clears 0 the good way.
        Out::NoWorse => {
            let words = model::metric(c["metric"].as_str().unwrap_or("")).map(|m| m.words.0).unwrap_or("");
            !words.is_empty() && verdict(c).contains(&format!(", {words}"))
        }
        _ => false,
    }
}

fn abs_unit(unit: &str) -> &str {
    if unit == "%" { "pp" } else { unit }
}

/// The change as a table shows it: relative changes as percentages, others in the metric's unit.
fn delta(c: &Value, x: f64) -> String {
    let unit = unit_of(c["metric"].as_str().unwrap_or(""));
    if c["relative"] == true { fmt::pct(x) } else { signed_disp(x, abs_unit(unit)) }
}

/// `−54% to −32%`, or `exact` for a counter that held one value per side.
fn range(c: &Value) -> String {
    if c["exact"] == true {
        return "exact".into();
    }
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    if !lo.is_finite() || !hi.is_finite() {
        return "—".into();
    }
    let (a, b) = (delta(c, lo), delta(c, hi));
    if a == b { a.replace(' ', "\u{a0}") } else { format!("{a} to {b}").replace(' ', "\u{a0}") }
}

fn value(c: &Value, side: &str) -> String {
    disp(f(&c[side]), unit_of(c["metric"].as_str().unwrap_or("")))
}

/// `7.05 → 3.90 ms`: both medians, the unit once when they share it.
fn before_after(c: &Value) -> String {
    let (b, h) = (value(c, "base"), value(c, "head"));
    match (b.rsplit_once(' '), h.rsplit_once(' ')) {
        (Some((bn, bu)), Some((_, hu))) if bu == hu && bn.parse::<f64>().is_ok() => nb(&format!("{bn} → {h}")),
        _ => nb(&format!("{b} → {h}")),
    }
}

/// `7.10 → 3.89 ms`: before → this PR, the unit once when they share it; it may break after the arrow.
fn arrow(b: &str, h: &str) -> String {
    match (b.rsplit_once(' '), h.rsplit_once(' ')) {
        (Some((bn, bu)), Some((_, hu))) if bu == hu && bn.parse::<f64>().is_ok() => format!("{bn}\u{a0}→ {}", nb(h)),
        _ => format!("{}\u{a0}→ {}", nb(b), nb(h)),
    }
}

fn html_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Text for a table cell: no raw pipes, no stray HTML.
fn cell(s: &str) -> String {
    html_esc(s).replace('|', "\\|")
}

fn code(s: &str) -> String {
    format!("`{}`", s.replace('`', "'").replace('|', "\\|"))
}

/// `refused (TailTooShort): 51 frames < 200` → (`TailTooShort`, `51 frames < 200`).
fn refusal(v: &str) -> (String, String) {
    let rest = v.trim_start_matches("refused").trim();
    let (id, why) = rest.split_once("): ").map(|(a, b)| (a.trim_start_matches('('), b)).unwrap_or((rest.trim_matches(['(', ')']), ""));
    (id.to_string(), why.to_string())
}

// ---------- the pairs of a series ----------

/// A pair's two blocks by pair number: (position in the series' blocks, block) per side.
type Sides<'a> = (Option<(usize, &'a Value)>, Option<(usize, &'a Value)>);

struct Pair<'a> {
    n: usize,
    ab: bool,
    base: &'a Value,
    head: &'a Value,
}

fn pairs(s: &Value) -> Vec<Pair<'_>> {
    let mut by: BTreeMap<u64, Sides> = BTreeMap::new();
    for (i, b) in s["blocks"].as_array().into_iter().flatten().enumerate() {
        let Some(p) = b["pair"].as_u64().filter(|p| *p < 1_000_000) else { continue };
        let e = by.entry(p).or_default();
        if b["variant"] == "base" {
            e.0 = Some((i, b));
        } else if b["variant"] == "head" {
            e.1 = Some((i, b));
        }
    }
    by.into_iter().filter_map(|(p, (b, h))| Some(Pair { n: p as usize + 1, ab: b?.0 < h?.0, base: b?.1, head: h?.1 })).collect()
}

fn bv(b: &Value, m: &str) -> f64 {
    f(&b["values"][m])
}

/// Pairs in which head did better on `metric`, of those with both values.
fn wins(ps: &[Pair], metric: &str) -> (usize, usize) {
    let lower = better_of(metric) == Better::Lower;
    let both: Vec<(f64, f64)> = ps.iter().map(|p| (bv(p.base, metric), bv(p.head, metric))).filter(|(a, b)| a.is_finite() && b.is_finite()).collect();
    (both.iter().filter(|(a, b)| if lower { b < a } else { b > a }).count(), both.len())
}

fn conf(c: &Value) -> String {
    let x = f(&c["confidence"]);
    if !x.is_finite() || x <= 0.0 {
        return String::new();
    }
    let p = x * 100.0;
    let t = if (p - p.round()).abs() < 0.05 { format!("{:.0}", p) } else { format!("{p:.1}") };
    if x < 0.945 { format!("{t}% range") } else { format!("{t}%") }
}

// ---------- the document ----------

struct Doc<'a> {
    r: &'a Value,
    level: u8,
    aa: bool,
    id: &'a str,
    series: Vec<&'a Value>,
}

fn drawer(summary: &str, body: &str) -> String {
    format!("<details>\n<summary>{summary}</summary>\n\n{}\n\n</details>\n\n", body.trim())
}

impl<'a> Doc<'a> {
    fn new(r: &'a Value, level: u8) -> Doc<'a> {
        Doc { r, level, aa: r["plan"]["aa"] == true, id: r["id"].as_str().unwrap_or(""), series: r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default() }
    }

    /// What a reader sees: the hero line, one caution with every regression, one line per series (and
    /// a second only for untested drift or a warning about that series), at most one caveat for the
    /// report, then two drawers: Details (metrics, pairs, profiles, method under plain headings) and
    /// Rerun.
    fn render(&self) -> String {
        let mut out = String::new();
        // The hero: one `#` line, bigger than the `##` sections an author writes above the block, with
        // the decided wins of the main metrics; without one, what happened, calmly, or the regression.
        let (wins, second) = self.win_phrases();
        let mut regs = self.regression_lines();
        let hero = if !wins.is_empty() {
            wins.join(" · ")
        } else if let Some(i) = regs.iter().position(|r| r.0) {
            format!("Regression: {}", regs.remove(i).2)
        } else {
            self.calm()
        };
        // A card image (`--format card`, attached by `--update-pr`) leads when there is one; the
        // heading stays, so the result reads without images.
        if let Some(url) = self.r["card"]["url"].as_str() {
            out += &format!("![{}]({url})\n\n", hero.replace('\u{a0}', " ").replace(['[', ']'], ""));
        }
        out += &format!("# {hero}\n\n");
        // What got decidedly worse: as hard to miss, but never in a win's typography.
        if !regs.is_empty() {
            out += "> [!CAUTION]\n";
            out += &regs.iter().map(|r| format!("> **Regression:** {}", r.1)).collect::<Vec<_>>().join("\n");
            out += "\n\n";
        }
        // The proof: every series once, by name.
        for s in &self.series {
            let mut line = self.sentence(s);
            // A series' badge image (`--format card`) stands in for its bold change.
            if let Some(url) = self.r["card"]["badges"][s["label"].as_str().unwrap_or("")].as_str()
                && let (Some(a), Some(b)) = (line.find("**"), line.rfind("**"))
                && b > a + 2
            {
                let change = line[a + 2..b].to_string();
                line = format!("{}![{change}]({url}){}", &line[..a], &line[b + 2..]);
            }
            // The hero's second win, from this series: its proof too.
            if let Some((x, c)) = second
                && std::ptr::eq(x, *s)
            {
                let m = c["metric"].as_str().unwrap_or("");
                line = format!("{}; {} **{}** (medians {}), likely {}.", line.trim_end_matches('.'), lc(&self.noun_in(s, m)), nb(&delta(c, f(&c["estimate"]))), before_after(c), range(c));
            }
            if unsteady(s) {
                line = format!("{}; **unsteady case**: its work grew each run.", line.trim_end_matches('.'));
            }
            let mut lines = vec![format!("**{}:** {line}", cell(&self.title(s)))];
            if let Some(b) = self.drift(s) {
                lines.push(format!("**But** {b}."));
            }
            if let Some(c) = caveat(s) {
                lines.push(c);
            }
            out += &lines.join("\n");
            out += "\n\n";
        }
        if let Some(n) = self.note() {
            out += &format!("{n}\n\n");
        }
        let mut d = String::new();
        d += &self.metrics();
        if self.level < 6 {
            d += &self.pairs_section();
        }
        d += &self.pacing_section();
        if self.level < 5 {
            d += &self.attribution_section();
        }
        d += &self.method();
        d += "\n";
        if self.level < 5 {
            d += &self.also_measured();
        }
        out += &drawer("<b>Details</b> · metrics, pairs, profiles, method", &d);
        out += &self.rerun_drawer();
        out
    }

    fn primary_out(&self, s: &Value) -> Out {
        if s["not_run"] == true {
            return Out::NotRun;
        }
        if s["kind"] == "leak" {
            let v = verdict(&s["primary"]);
            return if v.starts_with("leaks") {
                Out::Worse
            } else if s["decided"] == true {
                Out::Same
            } else {
                Out::Unclear
            };
        }
        outcome(&s["primary"], self.aa)
    }

    fn file(&self) -> String {
        self.r["file_repo"].as_str().map(str::to_string).unwrap_or_else(|| repo_relative(self.r["file"].as_str().unwrap_or("")))
    }

    fn multi(&self) -> bool {
        self.series.len() > 1
    }

    /// A series as people say it: its case in sentence case, the target only when the series do not
    /// all share one, and its parameters.
    fn title(&self, s: &Value) -> String {
        let label = s["label"].as_str().unwrap_or("");
        let case = s["case"].as_str().unwrap_or_else(|| short_label(label));
        let mut t = capitalize(&case.replace(['-', '_'], " "));
        let mut extra: Vec<String> = Vec::new();
        if let Some(target) = s["target"].as_str()
            && self.series.iter().filter_map(|x| x["target"].as_str()).any(|x| x != target)
        {
            extra.push(target.replace(['-', '_'], " "));
        }
        for (k, v) in s["params"].as_object().into_iter().flatten() {
            extra.push(format!("{k} {}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())));
        }
        if !extra.is_empty() {
            t += &format!(" ({})", extra.join(", "));
        }
        t
    }

    /// A metric's name in a series' line: "per run" says how long a run is.
    fn noun_in(&self, s: &Value, m: &str) -> String {
        let n = noun(m);
        let dur = all_rows(s).into_iter().find(|c| c["metric"] == "run.duration").map(|c| f(&c["base"])).filter(|d| d.is_finite() && *d > 0.0);
        match dur {
            Some(d) if s["kind"] == "runs" && m != "run.duration" && n.ends_with("per run") => n.replace("per run", &format!("per {} run", nb(&short_duration(d)))),
            _ => n,
        }
    }

    /// One series in one line: the metric, the paired change (bold when decided), the medians labelled
    /// as such (the paired change is the median of the pairs' own changes, not the medians' ratio),
    /// and how sure.
    fn sentence(&self, s: &Value) -> String {
        let c = &s["primary"];
        let m = c["metric"].as_str().unwrap_or("");
        let noun = self.noun_in(s, m);
        let o = self.primary_out(s);
        let medians = format!("medians {}", before_after(c));
        let (w, n) = wins(&pairs(s), m);
        let bound = bound_text(c);
        let change = nb(&delta(c, f(&c["estimate"])));
        let margin = c["noworse"].as_f64().map(|x| if c["relative"] == true { format!("{:.0}%", x * 100.0) } else { disp(x, abs_unit(unit_of(m))) }).unwrap_or_else(|| bound.clone());
        match o {
            Out::NotRun => format!("not run ({}).", cell(s["halt"]["message"].as_str().unwrap_or("the bench stopped first")).trim_end_matches('.')),
            _ if s["kind"] == "leak" => format!("{}.", cell(&capitalize(verdict(c))).trim_end_matches('.')),
            Out::Better | Out::Worse if !verdict(c).starts_with("worse") => {
                let (k, word) = if o == Out::Better { (w, "better") } else { (n - w, "worse") };
                let small = if verdict(c).contains("(small)") { format!(", perhaps by under {}", bound.trim_start_matches('±')) } else { String::new() };
                format!("{noun} **{change}** per pair ({medians}), {word} in {k} of {n} pairs, likely {}{small}.", range(c))
            }
            Out::AaFalse => format!("identical builds differed by **{change}** in {}: don't trust this setup yet.", lc(&noun)),
            Out::Same if self.aa => format!("identical builds matched on {} within {bound}.", lc(&noun)),
            Out::Same => format!("{noun} unchanged, within {bound} ({medians})."),
            Out::NoWorse if marked(c, o) => format!("{noun} **{change}** per pair ({medians}), within the {margin} allowed."),
            Out::NoWorse => format!("{noun} no worse, within the {margin} allowed ({medians})."),
            Out::NoWorseSlower => format!("{noun} **{change}** per pair ({medians}), worse but within the {margin} allowed."),
            Out::Worse => format!("{noun} **{change}** per pair ({medians}), more than the {margin} allowed."),
            Out::Refused => {
                let (id, why) = refusal(verdict(c));
                format!("{noun} not measured: {}.", plain_refusal(&id, &why))
            }
            Out::Saturated => format!("{noun} at the ceiling in both builds ({medians}): choose another main metric."),
            Out::Stopped => format!("{noun}: no answer, stopped after {n} pair{}.", if n == 1 { "" } else { "s" }),
            _ => match c["needed_pairs"].as_u64().filter(|x| *x <= 1000 && *x > n as u64) {
                Some(need) => format!("{noun}: no clear change in {n} pairs ({medians}); ~{need} pairs would settle it."),
                None => format!("{noun}: no clear change in {n} pairs ({medians})."),
            },
        }
    }

    /// Untested metrics whose whole likely range lies past the bound on the worse side: not decided,
    /// so a plain line, never the caution. An unsteady case drifts by itself, so it says none.
    fn drift(&self, s: &Value) -> Option<String> {
        if unsteady(s) {
            return None;
        }
        let mut untested: Vec<(f64, String)> = Vec::new();
        for c in all_rows(s).into_iter().skip(1) {
            if !matches!(outcome(c, self.aa), Out::Desc | Out::Gated) {
                continue;
            }
            let m = c["metric"].as_str().unwrap_or("");
            // Frame gaps are said once, as late frames.
            if m.starts_with("frames.interval") {
                continue;
            }
            let (lo, hi, b) = (f(&c["lo"]), f(&c["hi"]), f(&c["bound"]).abs());
            let lower = better_of(m) == Better::Lower;
            let bound = if b.is_finite() && b > 0.0 { b } else { 0.02 };
            if lo.is_finite() && hi.is_finite() && ((lower && lo > bound) || (!lower && hi < -bound)) {
                untested.push((size(c), format!("{} {}", lc(&noun(m)), nb(&delta(c, f(&c["estimate"]))))));
            }
        }
        untested.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        if untested.is_empty() {
            return None;
        }
        let more = untested.len().saturating_sub(2);
        let list: Vec<String> = untested.into_iter().take(2).map(|x| x.1).collect();
        let list = if more > 0 { format!("{} and {more} more", list.join(", ")) } else { join_and(&list) };
        Some(format!("{list} (not tested)"))
    }

    /// A decided change in the metric's own words for the direction it moved: the amount (`44%`,
    /// `219 MB`, a number and its unit kept together), the word, the thing.
    fn hero_parts(&self, c: &Value) -> (String, &'static str, String) {
        let m = c["metric"].as_str().unwrap_or("");
        let est = f(&c["estimate"]);
        let (good, bad) = model::metric(m).map(|d| d.words).unwrap_or(("less", "more"));
        let word = if (est < 0.0) == (better_of(m) == Better::Lower) { good } else { bad };
        let pct = (est * 100.0).abs();
        let amount = match c["relative"] == true {
            true if pct < 10.0 => format!("{pct:.1}%"),
            true => format!("{pct:.0}%"),
            false => nb(&disp(est.abs(), abs_unit(unit_of(m)))),
        };
        (amount, word, lc(&noun(m)).trim_end_matches(" per run").to_string())
    }

    /// "44% less CPU time", "Time until ready: 15% faster".
    fn hero_phrase(&self, c: &Value) -> String {
        let (amount, word, thing) = self.hero_parts(c);
        phrase(&amount, word, &thing)
    }

    /// The hero's wins: each series' main metric that decidedly improved, in the series' order, the
    /// same metric in several series said once (`19–23% less CPU time`), at most three. A single win
    /// from one series makes room for one tested metric of another kind that also won there.
    fn win_phrases(&self) -> (Vec<String>, Option<(&'a Value, &'a Value)>) {
        let celebrates = |s: &Value| !self.aa && s["kind"] != "leak" && s["not_run"] != true && self.primary_out(s) == Out::Better && !verdict(&s["primary"]).starts_with("worse");
        let mut groups: Vec<(String, Vec<&'a Value>)> = Vec::new();
        for s in self.series.iter().copied().filter(|s| celebrates(s)) {
            let c = &s["primary"];
            let key = format!("{}|{}", c["metric"].as_str().unwrap_or(""), self.hero_parts(c).1);
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => g.1.push(c),
                None => groups.push((key, vec![c])),
            }
        }
        let mut out: Vec<String> = groups
            .iter()
            .take(3)
            .map(|(_, cs)| {
                let parts: Vec<(String, &'static str, String)> = cs.iter().map(|c| self.hero_parts(c)).collect();
                let mut sized: Vec<(f64, &String)> = cs.iter().zip(&parts).map(|(c, p)| (size(c), &p.0)).collect();
                sized.sort_by(|a, b| a.0.total_cmp(&b.0));
                let (lo, hi) = (sized[0].1, sized[sized.len() - 1].1);
                let amount = if lo == hi { lo.clone() } else { span(lo, hi) };
                phrase(&amount, parts[0].1, &parts[0].2)
            })
            .collect();
        let mut second = None;
        if let [(_, cs)] = groups.as_slice()
            && cs.len() == 1
            && let Some(s) = self.series.iter().copied().find(|s| std::ptr::eq(&s["primary"], cs[0]))
        {
            let main = family(cs[0]["metric"].as_str().unwrap_or(""));
            let extra = tested(s)
                .into_iter()
                .skip(1)
                .filter(|c| outcome(c, self.aa) == Out::Better && !verdict(c).starts_with("worse") && family(c["metric"].as_str().unwrap_or("")) != main)
                .max_by(|a, b| size(a).total_cmp(&size(b)).then(b["metric"].as_str().cmp(&a["metric"].as_str())));
            if let Some(c) = extra {
                out.push(self.hero_phrase(c));
                second = Some((s, c));
            }
        }
        (out, second)
    }

    /// Everything decidedly worse, one line each: (a main metric, the line, the hero phrase). A main
    /// metric that got worse, a tested metric that got worse, and late frames up by a point or more.
    fn regression_lines(&self) -> Vec<(bool, String, String)> {
        let mut out = Vec::new();
        // `30% more thread wake-ups in *Warm* (7 of 8 pairs)`: the pairs this PR did worse in.
        let line = |s: &Value, ph: &str, k: usize, n: usize| {
            let who = if self.multi() { format!(" in *{}*", html_esc(&self.title(s))) } else { String::new() };
            format!("{}{who} ({k} of {n} pairs)", lc(ph))
        };
        for s in self.series.iter().copied() {
            if self.aa || s["kind"] == "leak" || s["not_run"] == true {
                continue;
            }
            let worse = |c: &Value| {
                let (w, n) = wins(&pairs(s), c["metric"].as_str().unwrap_or(""));
                (n - w, n)
            };
            let p = &s["primary"];
            if self.primary_out(s) == Out::Worse {
                let (ph, (k, n)) = (self.hero_phrase(p), worse(p));
                out.push((true, line(s, &ph, k, n), ph));
            }
            for c in tested(s).into_iter().skip(1) {
                if outcome(c, self.aa) == Out::Worse {
                    let (ph, (k, n)) = (self.hero_phrase(c), worse(c));
                    out.push((false, line(s, &ph, k, n), ph));
                }
            }
            if let Some((b, h, k, n)) = late_frames(s, self.r)
                && h - b >= 0.01
            {
                let ph = format!("late frames rose from {:.1}% to {:.1}%", b * 100.0, h * 100.0);
                out.push((false, line(s, &ph, k, n), ph));
            }
        }
        out
    }

    /// The heading when nothing is worth celebrating: what happened, calmly.
    fn calm(&self) -> String {
        let outs: Vec<(Out, &Value)> = self.series.iter().map(|s| (self.primary_out(s), *s)).collect();
        if let Some((_, s)) = outs.iter().find(|(o, s)| *o == Out::Worse && !verdict(&s["primary"]).starts_with("worse")) {
            return self.hero_phrase(&s["primary"]);
        }
        if outs.iter().any(|(o, _)| *o == Out::Worse) {
            return "Worse than the margin allowed".into();
        }
        if outs.iter().any(|(o, _)| *o == Out::AaFalse) {
            return "Identical builds differed".into();
        }
        if outs.iter().any(|(o, _)| matches!(o, Out::Unclear | Out::Gated)) {
            let need = outs.iter().filter_map(|(_, s)| s["primary"]["needed_pairs"].as_u64().filter(|n| *n <= 1000)).max();
            return need.map(|n| format!("Not decided yet: needs ~{n} pairs")).unwrap_or_else(|| "Not decided yet".into());
        }
        if outs.iter().any(|(o, _)| *o == Out::Refused) {
            return "Not measured".into();
        }
        if outs.iter().any(|(o, _)| matches!(o, Out::Same | Out::NoWorse | Out::NoWorseSlower)) {
            return if self.aa { "Identical builds matched".into() } else { "No measurable change".into() };
        }
        "No answer yet".into()
    }

    /// The one caveat line for the whole report.
    fn note(&self) -> Option<String> {
        if let Some(st) = self.r["stopped"].as_object() {
            let not_run = st.get("not_run").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            let rest = if not_run > 0 { format!("{not_run} series did not run, and the rest cover only the pairs that finished") } else { "the results cover only the pairs that finished".into() };
            return Some(format!("**Stopped early:** {rest}."));
        }
        let (flagged, blocks) = self.busy_blocks();
        let line = match self.regime().as_str() {
            "session locked, display off" => "Measured with the screen locked and the display off: compare only with runs measured the same way.".to_string(),
            "session locked" => "Measured with the screen locked: compare only with other locked runs.".to_string(),
            _ if blocks > 0 && flagged * 2 >= blocks => format!("Other apps were busy in {flagged} of {blocks} timed blocks: expect wider ranges."),
            _ if self.r["ledger"]["total"].as_u64().is_some_and(|t| t > 1) => "Other experiments compared these same builds: see Details.".to_string(),
            _ => return None,
        };
        Some(format!("*{line}*"))
    }

    /// Timed blocks in which other processes used 25% of a core or more on the measured CPUs, of all.
    fn busy_blocks(&self) -> (usize, usize) {
        let mut flagged = 0;
        let mut blocks = 0;
        for s in &self.series {
            for p in pairs(s) {
                for b in [p.base, p.head] {
                    if bv(b, "cpu.foreign.max").is_finite() {
                        blocks += 1;
                        flagged += (bv(b, "cpu.foreign.max") >= 25.0) as usize;
                    }
                }
            }
        }
        (flagged, blocks)
    }

    fn drew_frames(&self) -> bool {
        self.series.iter().flat_map(|s| s["blocks"].as_array().into_iter().flatten()).any(|b| bv(b, "frames.n") > 0.0)
    }

    /// `session locked, display off`: the state every block ran in, which the numbers compare with.
    fn regime(&self) -> String {
        let ran: Vec<&&Value> = self.series.iter().filter(|s| s["session"].is_string()).collect();
        if ran.is_empty() {
            return String::new();
        }
        let locked = ran.iter().all(|s| s["session"] == "locked");
        let mixed = ran.iter().any(|s| s["session"] == "mixed") || (!locked && ran.iter().any(|s| s["session"] == "locked"));
        let dark = ran.iter().all(|s| has_warning(s, "DisplayOff"));
        match (mixed, locked, dark) {
            (true, _, _) => "session lock state mixed".into(),
            (_, true, true) => "session locked, display off".into(),
            (_, true, false) => "session locked".into(),
            (_, false, true) => "display off".into(),
            _ => String::new(),
        }
    }

    /// Before series `s`'s first row (`i` 0) of a table across series, a row with its title, so no
    /// column is spent on it; GitHub fills the row's other cells.
    fn series_cell(&self, s: &Value, i: usize) -> String {
        if self.multi() && i == 0 { format!("| **{}** |\n", cell(&self.title(s))) } else { String::new() }
    }

    // ---------- Details ----------

    /// Every tested metric of every series in one table.
    fn metrics(&self) -> String {
        let mut t = String::from("#### Metrics\n\n| Metric | Before | This PR | Change | Likely range | Verdict |\n|:---|---:|---:|---:|---:|:---|\n");
        let (mut conf_main, mut conf_rest) = (String::new(), String::new());
        for s in &self.series {
            if s["not_run"] == true || s["kind"] == "leak" {
                t += &format!("{}| {} | | | | | {} |\n", self.series_cell(s, 0), if s["kind"] == "leak" { "Memory growth per run" } else { "Not run" }, cell(verdict(&s["primary"])));
                continue;
            }
            for (i, c) in tested(s).into_iter().enumerate() {
                let m = c["metric"].as_str().unwrap_or("");
                let o = if i == 0 { self.primary_out(s) } else { outcome(c, self.aa) };
                let name = if i == 0 { format!("{} <sub>main</sub>", cell(&noun(m))) } else { cell(&noun(m)) };
                let (chg, rng) = change_cells(c, o);
                t += &format!("{}| {name} | {} | {} | {chg} | {rng} | {} |\n", self.series_cell(s, i), nb(&value(c, "base")), nb(&value(c, "head")), verdict_text(c, o, s, self.aa));
                if i == 0 && conf_main.is_empty() {
                    conf_main = conf(c);
                } else if i == 1 && conf_rest.is_empty() {
                    conf_rest = conf(c);
                }
            }
        }
        let foot = if conf_main.is_empty() {
            "Change: the median of the pairs' own changes.".to_string()
        } else if !conf_rest.is_empty() && conf_rest != conf_main {
            format!("Likely range: {conf_main} confidence for the main metric, {conf_rest} for the others. Change: the median of the pairs' own changes.")
        } else {
            format!("Likely range: {conf_main} confidence. Change: the median of the pairs' own changes.")
        };
        t + &format!("\n<sub>{foot}</sub>\n\n")
    }

    /// The metrics measured but not tested, last: what moved in a table, what held still in a line.
    fn also_measured(&self) -> String {
        let mut t = String::from("| Metric | Before | This PR | Change | Likely range |\n|:---|---:|---:|---:|---:|\n");
        let (mut any, mut zeros, mut held) = (false, Vec::<String>::new(), Vec::<(String, String)>::new());
        for s in &self.series {
            if s["not_run"] == true || s["kind"] == "leak" {
                continue;
            }
            let (rows, z) = context(s);
            zeros.extend(z);
            let mut i = 0;
            for c in rows {
                let o = outcome(c, self.aa);
                let m = c["metric"].as_str().unwrap_or("");
                // Unchanged: the same value on both sides and a range that never left zero.
                let zero = delta(c, 0.0);
                if o != Out::Refused && value(c, "base") == value(c, "head") && delta(c, f(&c["lo"])) == zero && delta(c, f(&c["hi"])) == zero {
                    held.push((lc(&noun(m)).trim_end_matches(" per run").to_string(), nb(&value(c, "base"))));
                    continue;
                }
                any = true;
                let (chg, rng) = change_cells(c, o);
                let rng = if o == Out::Refused {
                    let (id, why) = refusal(verdict(c));
                    format!("skipped <sub>{}</sub>", plain_refusal(&id, &why))
                } else {
                    rng
                };
                t += &format!("{}| {} | {} | {} | {chg} | {rng} |\n", self.series_cell(s, i), cell(&noun(m)), nb(&value(c, "base")), nb(&value(c, "head")));
                i += 1;
            }
        }
        // `run time (8.01 s, 20.0 s)`: a name once, with each series' value.
        let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, v) in held {
            let e = by.entry(name).or_default();
            if !e.contains(&v) {
                e.push(v);
            }
        }
        zeros.sort();
        zeros.dedup();
        let mut foot = Vec::new();
        if !by.is_empty() {
            foot.push(format!("Unchanged: {}.", by.iter().map(|(n, v)| format!("{n} ({})", v.join(", "))).collect::<Vec<_>>().join(", ")));
        }
        if !zeros.is_empty() {
            let io = zeros.iter().any(|m| m == "io.read") && zeros.iter().any(|m| m == "io.write");
            let mut names: Vec<String> = zeros.iter().filter(|m| !(io && (*m == "io.read" || *m == "io.write"))).map(|m| lc(&noun(m)).trim_end_matches(" per run").to_string()).collect();
            if io {
                names.insert(0, "file and pipe reads and writes".into());
            }
            foot.push(format!("Zero in both builds: {}.", join_and(&names)));
        }
        if !any && foot.is_empty() {
            return String::new();
        }
        let mut out = String::from("#### Also measured, not tested\n\n");
        if any {
            out += &t;
            out += "\n";
        }
        if !foot.is_empty() {
            out += &format!("<sub>{}</sub>\n\n", foot.join(" "));
        }
        out
    }

    /// Every pair of every series: a real change shows in most of them. Each cell is before → this PR,
    /// so the table stays narrow enough for a phone.
    fn pairs_section(&self) -> String {
        let all: Vec<(&Value, Vec<Pair>)> = self.series.iter().map(|s| (*s, pairs(s))).filter(|(_, p)| !p.is_empty()).collect();
        if all.is_empty() {
            return String::new();
        }
        let foreign = all.iter().any(|(_, ps)| ps.iter().any(|p| bv(p.base, "cpu.foreign.max").is_finite()));
        let frames = all.iter().any(|(_, ps)| ps.iter().any(|p| bv(p.base, "frames.n") > 0.0 && bv(p.base, "frames.interval.max").is_finite()));
        let metric = if self.multi() { "Main metric".to_string() } else { noun(all[0].0["primary"]["metric"].as_str().unwrap_or("")) };
        let mut t = format!("#### Pairs\n\n| Pair | {}, before → this PR | Change |", cell(&metric));
        let mut sep = String::from("|:---|---:|---:|");
        if frames {
            t += " Longest frame gap |";
            sep += "---:|";
        }
        if foreign {
            t += " Other apps' CPU |";
            sep += "---:|";
        }
        t += &format!("\n{sep}\n");
        let mut left = 0;
        let mut busy: Vec<&Value> = Vec::new();
        for (s, ps) in &all {
            let m = s["primary"]["metric"].as_str().unwrap_or("");
            let unit = unit_of(m);
            let flagged = |p: &Pair| bv(p.base, "cpu.foreign.max") >= 25.0 || bv(p.head, "cpu.foreign.max") >= 25.0;
            let directional = matches!(self.primary_out(s), Out::Better | Out::Worse | Out::AaFalse);
            if self.multi() {
                // The series names its rows, so no column is spent on it.
                t += &self.series_cell(s, 0);
            }
            for (i, p) in ps.iter().enumerate() {
                // Shedding: a series without a direction keeps its flagged pairs; a long one its ends and its flagged pairs.
                let keep = !(self.level >= 3 && !directional && !flagged(p)) && !(self.level >= 4 && ps.len() > 16 && !(i < 4 || i + 2 >= ps.len() || flagged(p)));
                if !keep {
                    left += 1;
                    continue;
                }
                if flagged(p) {
                    busy.extend([p.base, p.head]);
                }
                let (a, b) = (bv(p.base, m), bv(p.head, m));
                let d = if a > 0.0 && unit != "count" { fmt::pct(b / a - 1.0) } else { signed_disp(b - a, abs_unit(unit)) };
                t += &format!("| {} <sub>{}</sub> | {} | {} |", p.n, if p.ab { "AB" } else { "BA" }, arrow(&disp(a, unit), &disp(b, unit)), nb(&d));
                if frames {
                    t += &format!(" {} |", arrow(&disp(bv(p.base, "frames.interval.max"), "ms"), &disp(bv(p.head, "frames.interval.max"), "ms")));
                }
                if foreign {
                    let pct = |x: f64| {
                        if !x.is_finite() {
                            "—".into()
                        } else if x >= 25.0 {
                            format!("**{x:.0}%**")
                        } else {
                            format!("{x:.0}%")
                        }
                    };
                    t += &format!(" {}\u{a0}→ {} |", pct(bv(p.base, "cpu.foreign.max")), pct(bv(p.head, "cpu.foreign.max")));
                }
                t += "\n";
            }
        }
        let mut foot = vec!["Each pair launched both builds fresh, in alternating order (AB or BA). Change: the main metric.".to_string()];
        if foreign {
            let who = top_procs(&busy, 3);
            foot.push(format!(
                "Other apps' CPU: other processes on the measured CPUs, % of one core, bold from 25%{}.",
                if who.is_empty() { String::new() } else { format!("; busiest {}", who.join(", ")) }
            ));
        }
        if left > 0 {
            foot.push(format!("{left} quiet pairs left out for length."));
        }
        t + &format!("\n<sub>{}</sub>\n\n", foot.join(" "))
    }

    /// Frame pacing, for series that drew frames.
    fn pacing_section(&self) -> String {
        let mut t = String::new();
        for s in &self.series {
            let ps = pairs(s);
            let pool = |head: bool| -> Vec<f64> {
                let mut v: Vec<f64> = ps.iter().flat_map(|p| (if head { p.head } else { p.base })["intervals"].as_array().into_iter().flatten().filter_map(Value::as_f64)).collect();
                v.sort_by(f64::total_cmp);
                v
            };
            let (b, h) = (pool(false), pool(true));
            if ps.is_empty() || b.len() < 30 * ps.len() || h.len() < 30 * ps.len() {
                continue;
            }
            let vs = 1000.0 / refresh(&ps, self.r);
            let share = |v: &[f64], lo: f64, hi: f64| v.iter().filter(|x| **x >= lo * vs && **x < hi * vs).count() as f64 / v.len() as f64;
            // Percentiles as frames.interval.* defines them (each launch's own, the median across launches).
            let rows = all_rows(s);
            // Only pacing that changed: on-time frames by half a point, or the p99 gap by a tenth.
            let p99 = |side: &str, v: &[f64]| {
                rows.iter().find(|c| c["metric"] == "frames.interval.p99").map(|c| f(&c[side])).filter(|x| x.is_finite()).unwrap_or(v[((v.len() - 1) as f64 * 0.99).round() as usize])
            };
            let (pb, ph) = (p99("base", &b), p99("head", &h));
            if (share(&b, 0.0, 1.5) - share(&h, 0.0, 1.5)).abs() < 0.005 && (ph / pb - 1.0).abs() < 0.10 {
                continue;
            }
            let gap = |name: &str, side: &str| {
                let pooled = |v: &[f64]| {
                    if name.ends_with("max") {
                        v[v.len() - 1]
                    } else {
                        v[((v.len() - 1) as f64
                            * if name.ends_with("p99") {
                                0.99
                            } else if name.ends_with("p95") {
                                0.95
                            } else {
                                0.5
                            })
                        .round() as usize]
                    }
                };
                nb(&disp(rows.iter().find(|c| c["metric"] == name).map(|c| f(&c[side])).unwrap_or_else(|| pooled(if side == "base" { &b } else { &h })), "ms"))
            };
            let title = if self.multi() { format!(": {}", self.title(s)) } else { String::new() };
            t += &format!("#### Frame pacing{title}\n\n| {:.0} Hz display | Before | This PR |\n|:---|---:|---:|\n", 1000.0 / vs);
            t += &format!("| On time | {} | {} |\n", pct2(share(&b, 0.0, 1.5)), pct2(share(&h, 0.0, 1.5)));
            t += &format!("| One frame late | {} | {} |\n", pct2(share(&b, 1.5, 2.5)), pct2(share(&h, 1.5, 2.5)));
            t += &format!("| Two or more late | {} | {} |\n", pct2(share(&b, 2.5, f64::INFINITY)), pct2(share(&h, 2.5, f64::INFINITY)));
            for (label, name) in
                [("Frame gap, p50", "frames.interval.p50"), ("Frame gap, p95", "frames.interval.p95"), ("Frame gap, p99", "frames.interval.p99"), ("Longest frame gap", "frames.interval.max")]
            {
                t += &format!("| {label} | {} | {} |\n", gap(name, "base"), gap(name, "head"));
            }
            t += &format!(
                "\n<sub>Shares of all {} and {} gaps between frames; a frame is late from 1.5 frame times. Gap percentiles: each launch's own, median across launches.</sub>\n\n",
                thousands(b.len()),
                thousands(h.len())
            );
        }
        t
    }

    /// Where it came from: the profiled launches, as tables across series.
    fn attribution_section(&self) -> String {
        let prof: Vec<&&Value> = self.series.iter().filter(|s| s["attribution"]["base"].is_string() && s["attribution"]["head"].is_string()).collect();
        if prof.is_empty() {
            return String::new();
        }
        let top = if self.level >= 1 { 3 } else { 4 };
        let mut t = String::from("#### Where it came from\n\n");
        // Native: threads and memory.
        let mut th = String::from("| Thread | Started in | Wake-ups/s, before → this PR | CPU per run, before → this PR |\n|:---|:---|---:|---:|\n");
        let mut mem = String::from("| Memory | Before | This PR | Change |\n|:---|---:|---:|---:|\n");
        let (mut any_th, mut any_mem) = (false, false);
        // Web: main thread, then the functions that moved.
        let mut main = String::from("| Main thread, per run | Before | This PR |\n|:---|---:|---:|\n");
        let mut fns = String::from("| Function | File | What, per run | Before | This PR | Change |\n|:---|:---|:---|---:|---:|---:|\n");
        let (mut any_main, mut any_fns) = (false, false);
        let mut heaps = Vec::new();
        for s in &prof {
            for (i, ((name, module), x, y)) in thread_rows(s).into_iter().filter(|(_, x, y)| (y.1 - x.1).abs() >= 0.05).take(top).enumerate() {
                any_th = true;
                let count = x.3.max(y.3);
                let name = if count > 1 { format!("{} ×{count}", code(&name)) } else { code(&name) };
                th += &format!(
                    "{}| {name} | {} | {} | {} |\n",
                    self.series_cell(s, i),
                    if module.is_empty() { "—".to_string() } else { code(&module) },
                    arrow(&sig3(x.1), &sig3(y.1)),
                    arrow(&disp(x.2, "ms"), &disp(y.2, "ms"))
                );
            }
            let layers: Vec<&Value> = s["memory_diff"]["rows"].as_array().into_iter().flatten().filter(|r| f(&r["delta_bytes"]).abs() >= 64.0 * 1024.0).take(top).collect();
            for (i, r) in layers.into_iter().enumerate() {
                any_mem = true;
                let mb = |v: f64| nb(&format!("{} MB", sig3(v / (1024.0 * 1024.0))));
                let d = f(&r["delta_bytes"]);
                mem += &format!(
                    "{}| {} | {} | {} | {}{} |\n",
                    self.series_cell(s, i),
                    layer_name(r["layer"].as_str().unwrap_or("")),
                    mb(f(&r["base_bytes"])),
                    mb(f(&r["head_bytes"])),
                    if d < 0.0 { "−" } else { "+" },
                    mb(d.abs())
                );
            }
            let rows = all_rows(s);
            let mut i = 0;
            for (n, m) in [("All work", "main.work"), ("JavaScript", "main.script"), ("Style", "main.style"), ("Layout", "main.layout"), ("Garbage collection", "main.gc")] {
                if let Some(c) = rows.iter().find(|c| c["metric"] == m) {
                    any_main = true;
                    main += &format!("{}| {n} | {} | {} |\n", self.series_cell(s, i), nb(&value(c, "base")), nb(&value(c, "head")));
                    i += 1;
                }
            }
            let (gb, gh) = (&s["hotspots"]["base"]["gc"]["values"], &s["hotspots"]["head"]["gc"]["values"]);
            if gb.is_object() && gh.is_object() {
                any_main = true;
                main += &format!("{}| GC pauses, profiled | {} | {} |\n", self.series_cell(s, i), nb(&disp(f(&gb["main_ms_per_run"]), "ms")), nb(&disp(f(&gh["main_ms_per_run"]), "ms")));
                main += &format!(
                    "{}| Memory freed by GC, profiled | {} | {} |\n",
                    self.series_cell(s, i + 1),
                    nb(&fmt::bytes(f(&gb["freed_bytes_per_run"]))),
                    nb(&fmt::bytes(f(&gh["freed_bytes_per_run"])))
                );
            }
            for (i, m) in self.movers_sorted(s).into_iter().take(top + 1).enumerate() {
                any_fns = true;
                let ms = m["unit"] == "ms";
                let show = |v: f64| nb(&if ms { disp(v, "ms") } else { fmt::bytes(v) });
                let (x, y) = (f(&m["base_per_run"]), f(&m["head_per_run"]));
                let func = m["function"].as_str().unwrap_or("");
                let (name, file) = func.rsplit_once(' ').unwrap_or((func, ""));
                fns += &format!(
                    "{}| {} | {} | {} | {} | {} | {}{} |\n",
                    self.series_cell(s, i),
                    code(name),
                    if file.is_empty() { "—".into() } else { code(file) },
                    if ms { "CPU time" } else { "Allocated" },
                    show(x),
                    show(y),
                    if y < x { "−" } else { "+" },
                    show((y - x).abs())
                );
            }
            let d = &s["heap_diff"];
            if d["base"].is_object() && d["head"].is_object() {
                heaps.push(format!(
                    "{}live heap after GC {} → {}",
                    if self.multi() { format!("{}: ", self.title(s)) } else { String::new() },
                    nb(&fmt::bytes(f(&d["base"]["self_size"]))),
                    nb(&fmt::bytes(f(&d["head"]["self_size"])))
                ));
            }
        }
        for (any, table) in [(any_th, th), (any_mem, mem), (any_main, main), (any_fns, fns)] {
            if any {
                t += &table;
                t += "\n";
            }
        }
        if !(any_th || any_mem || any_main || any_fns) {
            return String::new();
        }
        let recs: Vec<String> = prof.iter().map(|s| format!("{} → {}", code(s["attribution"]["base"].as_str().unwrap_or("")), code(s["attribution"]["head"].as_str().unwrap_or("")))).collect();
        let mut foot = format!("From one profiled launch per build ({}), apart from the timed pairs; profiling slows a launch, so read these as directions.", recs.join(", "));
        if !heaps.is_empty() {
            foot += &format!(" {}.", capitalize(&heaps.join("; ")));
        }
        t + &format!("<sub>{foot}</sub>\n\n")
    }

    fn insight_series(&self, s: &Value) -> Option<&'a Value> {
        self.r["insights"]["series"].as_array()?.iter().find(|x| x["series"] == s["label"])
    }

    /// The functions whose CPU time or allocations moved most, biggest first (1 ms against 1 MB).
    fn movers_sorted(&self, s: &Value) -> Vec<&'a Value> {
        let Some(i) = self.insight_series(s) else { return Vec::new() };
        // The browser's own native frames (`requestAnimationFrame`) gather sampling time the trace
        // counts elsewhere, so they can exceed all the main thread's work: only page code explains.
        let native = |x: &Value| x["unit"] == "ms" && x["function"].as_str().unwrap_or("").ends_with(" (native)");
        // When the main metric moved decidedly, the functions that moved the same way explain it.
        let p = &s["primary"];
        let way = match self.primary_out(s) {
            Out::Better | Out::Worse if better_of(p["metric"].as_str().unwrap_or("")) == Better::Lower => f(&p["estimate"]).signum(),
            _ => 0.0,
        };
        let along = |x: &Value| way == 0.0 || (f(&x["head_per_run"]) - f(&x["base_per_run"])).signum() == way;
        let mut rows: Vec<&Value> = i["movers"].as_array().into_iter().flatten().filter(|x| !native(x) && along(x)).collect();
        let d = |x: &Value| {
            let raw = (f(&x["head_per_run"]) - f(&x["base_per_run"])).abs();
            if x["unit"] == "ms" { raw } else { raw / (1024.0 * 1024.0) }
        };
        rows.sort_by(|a, b| d(b).total_cmp(&d(a)).then_with(|| a["function"].as_str().cmp(&b["function"].as_str())));
        rows
    }

    /// How it was measured: the builds, the machine, the method and every note, a line each.
    fn method(&self) -> String {
        let r = self.r;
        let (b, h) = variants(r);
        let subj = |v: &Value| v["subject"].as_str().map(|s| format!(", *{}*", cell(&no_refs(s)))).unwrap_or_default();
        let mut t = String::from("#### How it was measured\n\n");
        if !b.is_null() {
            t += &format!("- **Before:** {}{}\n", code(&tag(&b)), subj(&b));
        }
        t += &format!("- **This PR:** {}{}\n", code(&tag(&h)), subj(&h));
        let mut hw = vec![r["env"]["os"].as_str().unwrap_or("").to_string(), short_cpu(r["env"]["cpu"].as_str().unwrap_or(""))];
        hw.push(r["gpu"]["devices"][0].as_str().unwrap_or("").to_string());
        hw.push(r["env"]["power_plan"].as_str().map(|p| format!("{p} power plan")).unwrap_or_default());
        hw.push(r["browser"].as_str().unwrap_or("").replace('/', " "));
        if self.drew_frames() {
            hw.push(r["refresh_hz"].as_f64().map(|x| format!("{x:.0} Hz display")).unwrap_or_default());
        }
        if let Some(p) = r["plan"]["pin"].as_str().filter(|p| !p.is_empty()) {
            let n = r["plan"]["reserve"]["cpus"].as_u64().map(|m| format!(", other processes kept off its {} CPUs", m.count_ones())).unwrap_or_default();
            hw.push(format!("pinned to {}{n}", p.split(" · ").next().unwrap_or(p)));
        }
        t += &format!("- **Machine:** {}\n", cell(&fmt::fields(hw).replace(" · ", ", ")));
        // What each build showed on screen (`bench::shown_line`), so a reader sees what was measured.
        for s in &self.series {
            if let Some(l) = super::bench::shown_line(s) {
                t += &format!("- **On screen{}:** {}\n", if self.multi() { format!(", {}", cell(&self.title(s))) } else { String::new() }, cell(&l));
            }
        }
        let elapsed: f64 = self.series.iter().filter_map(|s| s["elapsed_ms"].as_f64()).sum();
        t += &if self.series.iter().all(|s| s["kind"] == "leak") {
            format!("- **Method:** growth per run as a Theil–Sen slope over post-GC readings, {} in all\n", nb(&fmt::ms_duration(elapsed as u64)))
        } else {
            let looks: Vec<String> = r["plan"]["looks"].as_array().into_iter().flatten().map(|x| x.to_string()).collect();
            format!(
                "- **Method:** fresh launches in alternating pairs; Wilcoxon signed-rank test and Hodges–Lehmann interval; main metric checked at {} pairs, others Holm-corrected; {} in all\n",
                if looks.is_empty() { "the planned".to_string() } else { looks.join(", ") },
                nb(&fmt::ms_duration(elapsed as u64))
            )
        };
        // Every note strobe made, once, with the series it came from.
        let mut seen: Vec<(String, String, Vec<String>)> = Vec::new();
        for (series, w) in r["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| (String::new(), w))
            .chain(self.series.iter().flat_map(|s| s["warnings"].as_array().into_iter().flatten().map(move |w| (self.title(s), w))))
        {
            let id = w["id"].as_str().unwrap_or("");
            if id == "ReservedCpus" || id == "SessionLocked" && w["level"] != "warn" || id == "DisplayOff" {
                continue;
            }
            let (id, text) = if id == "Background" && !series.is_empty() {
                let (k, n) = self.busy_blocks();
                ("OtherLoad", format!("other processes used 25% of a core or more in {k} of {n} timed blocks (kept; see Pairs)"))
            } else {
                (id, plain_note(&first_clause(w["text"].as_str().unwrap_or(""), 150)))
            };
            match seen.iter_mut().find(|(i, t, _)| i == id && *t == text) {
                Some(e) => e.2.push(series),
                None => seen.push((id.to_string(), text, vec![series])),
            }
        }
        for (id, text, series) in seen {
            let every = series.iter().any(String::is_empty) || series.len() == self.series.len();
            let whose = if every || !self.multi() { String::new() } else { format!(" ({})", cell(&series.join(", "))) };
            t += &format!("- **{}{whose}:** {}\n", warning_name(&id), cell(text.trim_end_matches('.')));
        }
        if let Some(l) = store::ledger::line(r) {
            t += &format!("- **Other experiments on these builds:** {}\n", cell(&l));
        }
        let now = version();
        t += &match r["env"]["strobe"].as_str() {
            Some(v) if now != format!("strobe {v}") => format!("- **Recorded with** strobe {v}, **formatted with** {now}\n"),
            _ => format!("- **Recorded and formatted with** {now}\n"),
        };
        t
    }

    /// Rerun: the command, then where to look closer.
    fn rerun_drawer(&self) -> String {
        let cmd = self.rerun();
        let (first, rest) = cmd.split_once('\n').unwrap_or((&cmd, ""));
        // One flag per line, so no line scrolls sideways.
        let first = first.replace(" --", " \\\n  --");
        let mut t = format!("```sh\n{first}{}{rest}\n```\n\n", if rest.is_empty() { "" } else { "\n" });
        t += &format!("- **Full report:** <code>strobe report experiment {} --open</code>\n", self.id);
        let profiled = self.series.iter().any(|s| s["attribution"]["head"].is_string());
        if profiled && super::open::pr_line(self.r).is_some() {
            t += &format!("- **Timeline and flame graphs:** <code>strobe open {0}/head</code>, <code>strobe open {0}/base</code>\n", self.id);
        }
        for s in &self.series {
            let kept: Vec<String> = s["timing_recordings"]
                .as_array()
                .into_iter()
                .flatten()
                .take(2)
                .filter_map(|x| Some(format!("{} ({}, pair {})", code(x["recording"].as_str()?), if x["variant"] == "base" { "before" } else { "this PR" }, x["pair"].as_u64()? + 1)))
                .collect();
            if !kept.is_empty() {
                t += &format!("- **Kept launches{}:** {}\n", if self.multi() { format!(", {}", cell(&self.title(s))) } else { String::new() }, kept.join(", "));
            }
        }
        drawer("<b>Rerun</b> · command and recordings", &t)
    }

    /// The `strobe bench` line that reruns the experiment from the repository's root: the bench file by
    /// its path there, variants by commit (a folder by the commit it was at), and every flag that chose
    /// what ran and how. Then, for an underpowered series, the flags that would settle it.
    fn rerun(&self) -> String {
        let r = self.r;
        let (base, head) = variants(r);
        let quote = |s: &str| if s.is_empty() || s.contains([' ', '"', '\'', '*', '&', '|', '<', '>', ';']) { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() };
        let spec = |v: &Value, fallback: &str| -> String {
            let s = v["spec"].as_str().unwrap_or(fallback);
            if s == "." {
                return ".".into();
            }
            match (v["kind"].as_str(), v["commit"].as_str()) {
                (Some("dir"), Some(c)) => return c.to_string(),
                (Some(_), _) => return s.to_string(),
                _ => {}
            }
            // Before variants named their kind: a path is a folder, named by the commit in its label.
            if local_path(s) { label_commit(v).unwrap_or_else(|| fallback.to_string()) } else { s.to_string() }
        };
        let flags = r["args"]["flags"].as_object();
        let mut line = format!("strobe bench {}", quote(&self.file()));
        if flags.is_some_and(|f| f.contains_key("aa")) || base.is_null() {
            line += &format!("{} --head {}", if base.is_null() { "" } else { " --aa" }, quote(&spec(&head, ".")));
        } else {
            line += &format!(" --base {} --head {}", quote(&spec(&base, "<base>")), quote(&spec(&head, ".")));
        }
        const SKIP: &[&str] = &["base", "head", "aa", "id", "json", "quiet", "verbose", "resume", "stop", "keep", "open", "progress", "update-pr", "repo", "force", "format", "out"];
        let mut rest: Vec<(&String, &Value)> = flags.into_iter().flatten().filter(|(k, _)| !SKIP.contains(&k.as_str())).collect();
        rest.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in rest {
            match v.as_str() {
                Some("true") => line += &format!(" --{k}"),
                Some(x) if local_path(x) => line += &format!(" --{k} <{k}>"),
                Some(x) => line += &format!(" --{k} {}", quote(x)),
                None => {}
            }
        }
        for s in &self.series {
            let need = s["primary"]["needed_pairs"].as_u64().filter(|n| *n <= 400);
            if let (Out::Unclear, Some(n)) = (self.primary_out(s), need) {
                line += &format!("\n# to settle {} (~{n} pairs), add: --filter {} --pairs {n}", self.title(s), quote(s["case"].as_str().unwrap_or("")));
            }
        }
        line
    }
}

// ---------- helpers over the result ----------

/// A thread (or same-named threads) on one side: owner, wakes/s, CPU ms/run, how many.
type Waker = (String, f64, f64, usize);

/// Threads by (name, module) on both sides, the one whose wake-ups moved most first.
fn thread_rows(s: &Value) -> Vec<((String, String), Waker, Waker)> {
    let side = |k: &str| -> BTreeMap<(String, String), Waker> {
        let mut m = BTreeMap::new();
        for t in s["hotspots"][k]["threads"]["threads"].as_array().into_iter().flatten() {
            let key = (t["name"].as_str().unwrap_or("(unnamed)").to_string(), t["module"].as_str().unwrap_or("").to_string());
            let e = m.entry(key).or_insert((t["origin"].as_str().unwrap_or("").to_string(), 0.0, 0.0, 0));
            e.1 += t["wakes_per_s"].as_f64().unwrap_or(0.0);
            e.2 += t["cpu_ms_per_run"].as_f64().unwrap_or(0.0);
            e.3 += 1;
        }
        m
    };
    let (b, h) = (side("base"), side("head"));
    let mut keys: Vec<&(String, String)> = b.keys().chain(h.keys()).collect();
    keys.sort();
    keys.dedup();
    let zero = (String::new(), 0.0, 0.0, 0);
    let mut rows: Vec<((String, String), Waker, Waker)> = keys.into_iter().map(|k| (k.clone(), b.get(k).cloned().unwrap_or(zero.clone()), h.get(k).cloned().unwrap_or(zero.clone()))).collect();
    rows.sort_by(|x, y| (y.2.1 - y.1.1).abs().total_cmp(&(x.2.1 - x.1.1).abs()).then(x.0.cmp(&y.0)));
    rows
}

fn layer_name(layer: &str) -> &str {
    match layer {
        "private" => "Private",
        "host" => "Heap and stacks",
        "gpu_mapped" => "GPU memory mapped in",
        "image" => "Loaded code",
        "working_set" => "Working set",
        "private_working_set" => "Private working set",
        "shared" => "Shared",
        "files" => "Mapped files",
        "unmapped" => "Committed, not mapped",
        other => other,
    }
}

/// A warning that changes how to read one series, said right under its line.
fn caveat(s: &Value) -> Option<String> {
    let warn = |id: &str| s["warnings"].as_array().into_iter().flatten().any(|w| w["id"] == id && w["level"] == "warn");
    if warn("SessionLocked") {
        Some("**Mixed lock state:** some pairs ran locked and some unlocked, so nothing was decided.".into())
    } else if warn("ScreenChanged") {
        Some("**Different screens:** the builds did not all show the same screen, so part of the difference may be the screen.".into())
    } else if has_warning(s, "BackgroundPriority") {
        Some("**Background priority:** the app was throttled in some blocks.".into())
    } else {
        None
    }
}

/// A change's size for ranking: relative changes as they are, absolute ones against their base.
fn size(c: &Value) -> f64 {
    if c["relative"] == true { f(&c["estimate"]).abs() } else { (f(&c["estimate"]) / f(&c["base"]).abs().max(1e-9)).abs() }
}

/// "44% less CPU time", or "Time until ready: 15% faster" for words that need their subject first.
fn phrase(amount: &str, word: &str, thing: &str) -> String {
    if matches!(word, "faster" | "slower" | "smoother" | "choppier") { format!("{}: {amount} {word}", capitalize(thing)) } else { format!("{amount} {word} {thing}") }
}

/// `19–23%`, `120–219 MB`: two amounts of one unit as a span.
fn span(lo: &str, hi: &str) -> String {
    let split = |s: &str| -> (String, String) {
        let i = s.find(|ch: char| !(ch.is_ascii_digit() || ch == '.')).unwrap_or(s.len());
        (s[..i].to_string(), s[i..].to_string())
    };
    let ((ln, lu), (hn, hu)) = (split(lo), split(hi));
    if lu == hu { format!("{ln}–{hn}{hu}") } else { format!("{lo}–{hi}") }
}

/// What a metric measures, so a hero says each kind of thing once.
fn family(m: &str) -> &str {
    match m {
        "cpu.wakes" => "wakes",
        _ if m.starts_with("memory.") || m == "gpu.vram" || m == "gpu.shared" || m.starts_with("js.heap") => "memory",
        _ => m.split('.').next().unwrap_or(m),
    }
}

/// A case whose work grew from run to run: its numbers may not mean what they seem.
fn unsteady(s: &Value) -> bool {
    has_warning(s, "NotSteady") || has_warning(s, "StateDrift")
}

/// The display's refresh rate the series ran at.
fn refresh(ps: &[Pair], r: &Value) -> f64 {
    let mut v: Vec<f64> = ps.iter().flat_map(|p| [bv(p.base, "display.hz"), bv(p.head, "display.hz")]).filter(|x| x.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().or(r["refresh_hz"].as_f64()).unwrap_or(60.0)
}

/// Late frames (a gap of 1.5 frame times or more) as shares of all gaps, before and this PR, and in
/// how many pairs this PR had a larger share of them, of those with frames on both sides.
fn late_frames(s: &Value, r: &Value) -> Option<(f64, f64, usize, usize)> {
    let ps = pairs(s);
    let vs = 1000.0 / refresh(&ps, r);
    let gaps = |b: &Value| b["intervals"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
    let (mut lb, mut tb, mut lh, mut th, mut worse, mut n) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for p in &ps {
        let (gb, gh) = (gaps(p.base), gaps(p.head));
        if gb.len() < 30 || gh.len() < 30 {
            continue;
        }
        let late = |g: &[f64]| g.iter().filter(|x| **x >= 1.5 * vs).count();
        let (xb, xh) = (late(&gb), late(&gh));
        lb += xb;
        tb += gb.len();
        lh += xh;
        th += gh.len();
        n += 1;
        if xh as f64 / gh.len() as f64 > xb as f64 / gb.len() as f64 {
            worse += 1;
        }
    }
    (n > 0).then(|| (lb as f64 / tb as f64, lh as f64 / th as f64, worse, n))
}

/// The change and likely range cells of a metric row; a decided change is bold.
fn change_cells(c: &Value, o: Out) -> (String, String) {
    if o == Out::Refused {
        return ("—".into(), "—".into());
    }
    let d = nb(&delta(c, f(&c["estimate"])));
    (if marked(c, o) { format!("**{d}**") } else { d }, range(c))
}

/// A metric in everyday words, one scheme for each family (`Frame gap, p95`): from this list, else the
/// catalog's description without its parentheses and up to its first comma.
fn noun(metric: &str) -> String {
    let known = match metric {
        "cpu.work" => "CPU time per run",
        "cpu.wakes" => "Thread wake-ups",
        "cpu.app.total" | "cpu.process.total" => "Total CPU use",
        "cpu.renderer" => "Renderer CPU use",
        "cpu.gpu" => "GPU process CPU use",
        "cpu.browser" => "Browser process CPU use",
        "memory.app.private" => "App memory",
        "memory.app.host" => "App heap memory",
        "gpu.vram" => "GPU memory",
        "gpu.shared" => "Shared GPU memory",
        "gpu.process.3d" => "GPU use",
        "main.busy" => "Main-thread busy time",
        "main.work" => "Main-thread work per run",
        "main.script" => "JavaScript time per run",
        "main.style" => "Style time per run",
        "main.layout" => "Layout time per run",
        "main.gc" => "Garbage collection per run",
        "main.tasks.long" => "Long tasks per run",
        "layout.count" => "Layouts per run",
        "style.count" => "Style recalculations per run",
        "run.duration" => "Run time",
        "launch.ready" => "Time until ready",
        "io.read" => "File and pipe reads per run",
        "io.write" => "File and pipe writes per run",
        "frames.dropped.pct" => "Dropped frames",
        "frames.dropped" => "Dropped frames per run",
        "frames.presented.count" => "Frames shown per run",
        "frames.interval.p50" => "Frame gap, p50",
        "frames.interval.p95" => "Frame gap, p95",
        "frames.interval.p99" => "Frame gap, p99",
        "frames.interval.max" => "Longest frame gap",
        "frames.presented.p50" => "Frame latency, p50",
        "frames.presented.p90" => "Frame latency, p90",
        "frames.presented.p95" => "Frame latency, p95",
        "frames.presented.p99" => "Frame latency, p99",
        "frames.presented.max" => "Longest frame latency",
        "interaction.p95" => "Input response, p95",
        "interaction.p50" => "Input response, p50",
        "scroll.latency.p95" => "Scroll response, p95",
        "js.heap.used" => "JavaScript heap",
        "dom.nodes" => "DOM nodes",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    if let Some(rest) = metric.strip_prefix("launch.") {
        return format!("Time until {rest}");
    }
    if let Some(rest) = metric.strip_prefix("phase.") {
        return format!("Time in {rest} per run");
    }
    if let Some(rest) = metric.strip_prefix("count.") {
        return capitalize(&format!("{rest} count per run"));
    }
    let about = model::metric(metric).map(|m| m.about).unwrap_or(metric);
    let plain = regex_lite::Regex::new(r"\s*\([^)]*\)").expect("pattern").replace_all(about, "").to_string();
    let first = plain.split(',').next().unwrap_or(&plain).trim().to_string();
    if first.is_empty() { metric.to_string() } else { capitalize(&first) }
}

/// Descriptive rows not already tested, and the names of those that read 0 on both sides.
fn context(s: &Value) -> (Vec<&Value>, Vec<String>) {
    let tested: Vec<&str> = tested(s).iter().filter_map(|c| c["metric"].as_str()).collect();
    let mut seen: Vec<&str> = Vec::new();
    let (mut rows, mut zeros) = (Vec::new(), Vec::new());
    for c in s["descriptive"].as_array().into_iter().flatten() {
        let m = c["metric"].as_str().unwrap_or("");
        if m.is_empty() || tested.contains(&m) || seen.contains(&m) {
            continue;
        }
        seen.push(m);
        if f(&c["base"]) == 0.0 && f(&c["head"]) == 0.0 {
            zeros.push(m.to_string());
        } else {
            rows.push(c);
        }
    }
    (rows, zeros)
}

fn all_rows(s: &Value) -> Vec<&Value> {
    let mut v = tested(s);
    let have: Vec<String> = v.iter().filter_map(|c| c["metric"].as_str().map(str::to_string)).collect();
    v.extend(s["descriptive"].as_array().into_iter().flatten().filter(|c| !have.iter().any(|h| c["metric"] == h.as_str())));
    v
}

/// A verdict in a table, in one of a few plain words.
fn verdict_text(c: &Value, o: Out, s: &Value, aa: bool) -> String {
    let v = verdict(c);
    let sub = |x: &str| if x.is_empty() { String::new() } else { format!(" <sub>{}</sub>", cell(x)) };
    match o {
        Out::Better | Out::Worse if !v.starts_with("worse") => format!("**{}**", cell(v.trim_end_matches(" (small)"))),
        Out::AaFalse => "**false positive**".into(),
        Out::Same if aa => "same <sub>A/A passed</sub>".into(),
        Out::Same => "same".into(),
        Out::NoWorse | Out::NoWorseSlower => format!("**no worse**{}", sub(v.trim_start_matches("no worse").trim().trim_matches(['(', ')']).split(',').next().unwrap_or(""))),
        Out::Worse | Out::Better => format!("**worse**{}", sub(v.trim_start_matches("worse").trim().trim_matches(['(', ')']))),
        Out::Refused => {
            let (id, why) = refusal(v);
            format!("skipped{}", sub(&plain_refusal(&id, &why)))
        }
        Out::Saturated => "at the ceiling".into(),
        Out::Stopped => "stopped".into(),
        Out::Gated if s["decided"] == true && (f(&c["lo"]) > 0.0 || f(&c["hi"]) < 0.0) => {
            let need = s["holm_pairs"].as_u64().filter(|n| *n > s["pairs"].as_u64().unwrap_or(0));
            format!("inconclusive{}", sub(&need.map(|n| format!("needs {n} pairs")).unwrap_or_default()))
        }
        Out::Unclear | Out::Gated => {
            let need = c["needed_pairs"].as_u64().filter(|n| *n <= 1000 && (o == Out::Unclear || s["decided"] == true)).map(|n| format!("~{n} pairs")).unwrap_or_default();
            format!("inconclusive{}", sub(&need))
        }
        Out::Desc => String::new(),
        Out::NotRun => "not run".into(),
    }
}

/// Why a metric was refused, in a few words.
fn plain_refusal(id: &str, why: &str) -> String {
    match id {
        "TailTooShort" => match why.split_once(" < ") {
            Some((have, need)) => format!("too few frames, {} of {need}", have.trim_end_matches(" frames")),
            None => "too few frames".into(),
        },
        "SessionLocked" => "lock state mixed".into(),
        "DisplayOff" => "display off".into(),
        _ if why.is_empty() => id.to_string(),
        _ => why.to_string(),
    }
}

/// A note strobe wrote, in the report's words: this PR and before for head and base, and metric ids by
/// their names.
fn plain_note(text: &str) -> String {
    let t = text.replacen("head: ", "this PR: ", 1).replacen("base: ", "before: ", 1);
    let ids = regex_lite::Regex::new(r"\b[a-z]+(\.[a-z0-9]+)+\b").expect("pattern");
    ids.replace_all(&t, |c: &regex_lite::Captures| if model::metric(&c[0]).is_some() { lc(&noun(&c[0])) } else { c[0].to_string() }).to_string()
}

/// Spaces that may not break: a number stays with its unit.
fn nb(s: &str) -> String {
    s.replace(' ', "\u{a0}")
}

/// Three significant figures, no exponent: 229, 50.4, 7.05, 0.0881.
fn sig3(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    if a == 0.0 {
        return "0".into();
    }
    let d = if a >= 100.0 { 0 } else { (2 - a.log10().floor() as i32).clamp(0, 4) as usize };
    let s = format!("{v:.d$}");
    if s.trim_start_matches('-').chars().all(|c| c == '0' || c == '.') { "0".into() } else { s }
}

/// A value with three significant figures and its unit; a second or longer in seconds.
fn disp(v: f64, unit: &str) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    match unit {
        "ms" if v.abs() >= 1000.0 => format!("{} s", sig3(v / 1000.0)),
        "ms" => format!("{} ms", sig3(v)),
        "%" => format!("{}%", sig3(v)),
        "pp" => format!("{} pp", sig3(v)),
        "MB" => format!("{} MB", sig3(v)),
        "/s" => format!("{}/s", sig3(v)),
        "bytes" => fmt::bytes(v),
        "count" | "" => fmt::value(v, "count"),
        u => format!("{} {u}", sig3(v)),
    }
}

fn signed_disp(v: f64, unit: &str) -> String {
    let body = disp(v.abs(), unit);
    if v < 0.0 {
        format!("−{body}")
    } else if v > 0.0 {
        format!("+{body}")
    } else {
        body
    }
}

/// A share as a percentage with two decimals: 10.28%.
fn pct2(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

/// `8 s`, `20 s`, `103 ms`: a run's length, rounded for a sentence.
fn short_duration(ms: f64) -> String {
    if ms >= 1000.0 {
        let s = ms / 1000.0;
        if s >= 10.0 || (s - s.round()).abs() < 0.05 { format!("{:.0} s", s) } else { format!("{s:.1} s") }
    } else {
        format!("{ms:.0} ms")
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `AMD Ryzen 9 9950X3D 16-Core Processor` → `Ryzen 9 9950X3D`.
fn short_cpu(cpu: &str) -> String {
    let c = cpu.replace("(R)", "").replace("(TM)", "").replace(" CPU", "");
    let c = c.split(" @ ").next().unwrap_or(&c).to_string();
    c.split_whitespace().filter(|w| !w.ends_with("-Core") && *w != "Processor" && *w != "AMD" && *w != "Intel").collect::<Vec<_>>().join(" ")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|x| x.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `CPU time` → `cPU time` would be wrong: lower the first letter only when the second is lower too.
fn lc(s: &str) -> String {
    let mut c = s.chars();
    match (c.next(), s.chars().nth(1)) {
        (Some(a), Some(b)) if b.is_lowercase() => a.to_lowercase().chain(c).collect(),
        _ => s.to_string(),
    }
}

/// The equivalence bound in the comparison's scale: `±2%`, or `±26.5` in the metric's unit.
fn bound_text(c: &Value) -> String {
    let b = f(&c["bound"]);
    if !b.is_finite() || b <= 0.0 {
        return "±2%".into();
    }
    if c["relative"] == true { format!("±{:.0}%", b * 100.0) } else { format!("±{}", fmt::value(b, abs_unit(unit_of(c["metric"].as_str().unwrap_or(""))))) }
}

/// A commit subject without pull request references: `#551` would link to this PR's repository,
/// which is often not the one the commit came from.
fn no_refs(subject: &str) -> String {
    let re = regex_lite::Regex::new(r"\s*\(#\d+\)").expect("pattern");
    let s = re.replace_all(subject, "");
    let re = regex_lite::Regex::new(r"#(\d)").expect("pattern");
    re.replace_all(&s, "#\u{2060}$1").to_string()
}

/// A warning's name for people, beside its id.
fn warning_name(id: &str) -> &str {
    match id {
        "PowerPlan" => "Power plan",
        "Background" => "Busy machine",
        "OtherLoad" => "Other load",
        "NotSteady" => "Unsteady warmup",
        "StateDrift" => "State drift",
        "HighVariance" => "High variance",
        "Drift" => "Drift",
        "MultimodalDistribution" => "Multimodal values",
        "Unresolved" => "Unmapped scripts",
        "Attribution" => "Attribution failed",
        "PixelsDiffer" => "Pixels differ",
        "CrossAdapterScanout" => "Cross-adapter scanout",
        "FewPairs" => "Few pairs",
        "UnpacedSpin" => "Unpaced frames",
        "BackgroundPriority" => "Background priority",
        "StaleCalibration" => "Stale calibration",
        "VariantChanged" => "Variant changed",
        "ScreenChanged" => "Different screens",
        other => other,
    }
}

fn has_warning(s: &Value, id: &str) -> bool {
    s["warnings"].as_array().into_iter().flatten().any(|w| w["id"] == id)
}

/// The primary first, then the secondaries, each metric once.
fn tested(s: &Value) -> Vec<&Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut rows = Vec::new();
    for c in std::iter::once(&s["primary"]).chain(s["secondaries"].as_array().into_iter().flatten()) {
        let m = c["metric"].as_str().unwrap_or("");
        if m.is_empty() || seen.contains(&m) {
            continue;
        }
        seen.push(m);
        rows.push(c);
    }
    rows
}

fn variants(r: &Value) -> (Value, Value) {
    let find = |role: &str| r["variants"].as_array().into_iter().flatten().find(|v| v["role"] == role).cloned().unwrap_or(Value::Null);
    (find("base"), find("head"))
}

fn local_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with('\\') || s.starts_with("~") || s.as_bytes().get(1) == Some(&b':') || s.starts_with("./") || s.starts_with("../")
}

/// The commit in a variant's label (`<name>@<sha>…`).
fn label_commit(v: &Value) -> Option<String> {
    let label = v["label"].as_str()?;
    let (_, rest) = label.rsplit_once('@')?;
    let sha: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    (sha.len() >= 7).then_some(sha)
}

/// How a report names a variant in public: `1982f5313f`, `origin/perf-x@7bf4e2d566`, `… (dirty)`;
/// never a local folder's name.
fn tag(v: &Value) -> String {
    let label = v["label"].as_str().unwrap_or("");
    let spec = v["spec"].as_str().unwrap_or("");
    let dirty = v["dirty"] == true || label.ends_with("(dirty)");
    let sha = v["commit"].as_str().map(|c| c.chars().take(10).collect::<String>()).or_else(|| label_commit(v));
    let Some(sha) = sha else { return if local_path(spec) || spec == "." { "working tree".into() } else { spec.to_string() } };
    let is_ref = !spec.is_empty() && spec != "." && !local_path(spec) && !spec.chars().all(|c| c.is_ascii_hexdigit()) && v["kind"] != "dir";
    let mut t = if is_ref { format!("{spec}@{sha}") } else { sha };
    if dirty {
        t += " (dirty)";
    }
    t
}

/// The busiest other processes across blocks, `` `bun.exe` 1230% ``.
fn top_procs(blocks: &[&Value], n: usize) -> Vec<String> {
    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for b in blocks {
        for e in b["background"].as_array().into_iter().flatten() {
            let (Some(name), Some(p)) = (e[0].as_str(), e[1].as_f64()) else { continue };
            let x = best.entry(name.to_string()).or_insert(0.0);
            *x = x.max(p);
        }
    }
    let mut v: Vec<(String, f64)> = best.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(n).map(|(name, p)| format!("{} {p:.0}%", code(&name))).collect()
}

/// A series label without the target (`window-closed · tray` → `window-closed`).
fn short_label(label: &str) -> &str {
    label.rsplit_once(" · ").map(|(a, _)| a).unwrap_or(label)
}

/// A warning's text up to its first `: ` detail list, at most `max` characters.
fn first_clause(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let cut: String = t.chars().take(max).collect();
    let at = cut.rfind([';', ',', ' ']).unwrap_or(cut.len());
    format!("{}…", cut[..at].trim_end_matches([',', ';', ' ']))
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A bisect experiment: the culprit (or what is left) and each step's comparison.
fn bisect(r: &Value) -> String {
    let id = r["id"].as_str().unwrap_or("");
    let file = repo_relative(r["file"].as_str().unwrap_or(""));
    let span = r["range"].as_str().unwrap_or("").split("..").map(|c| c.chars().take(10).collect::<String>()).collect::<Vec<_>>().join("..");
    let m = r["metric"].as_str().unwrap_or("");
    let verdict_line = r["verdict"].as_str().unwrap_or("");
    let head = match verdict_line.strip_prefix("found: ") {
        Some(rest) => format!("**Bisect:** {} changed at {}.", noun(m), cell(rest)),
        None => format!("**Bisect:** {}.", cell(verdict_line)),
    };
    let mut t = format!("{head}\nSearched {} in {}, one full comparison per step.\n\n", code(&span), code(&file));
    t += "<details>\n<summary><b>Which commit?</b> Each step of the search</summary>\n\n| Step | Commit, against the last good one | Pairs | Change | Likely range | Verdict |\n|---:|:---|---:|---:|---:|:---|\n";
    for s in r["steps"].as_array().into_iter().flatten() {
        let o = outcome(s, false);
        t += &format!(
            "| {} | {} {} | {} | {} | {} | {} |\n",
            s["step"],
            code(s["short"].as_str().unwrap_or("")),
            cell(&no_refs(s["subject"].as_str().unwrap_or(""))),
            s["pairs"],
            if marked(s, o) { format!("**{}**", nb(&delta(s, f(&s["estimate"])))) } else { nb(&delta(s, f(&s["estimate"]))) },
            range(s),
            cell(verdict(s))
        );
    }
    t + &format!("\nEach step is an experiment of its own (`strobe report experiment <step> --format pr`); the whole search: `strobe report experiment {id} --open`.\n\n</details>\n")
}
