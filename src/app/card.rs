//! `report experiment <exp> --format card|badge` (SPEC §11.2): an experiment's result as an image a
//! pull request can lead with. A card (1200×630, the Open Graph size) shows each decided win as a large
//! number in plain words, before and this PR, one dot per pair and the case; without a win it says
//! calmly what happened. A badge is one line, shields-style (`CPU time −44%`). Both are drawn once as
//! SVG with the embedded Inter, outlined so they look the same everywhere, and rasterized to PNG by
//! resvg, so the bytes are the same on every run and machine. `--update-pr` attaches them to the block
//! (`for_pr`); the block's text says the same in words, so nothing depends on the image.

use super::Output;
use crate::cli::Args;
use crate::model::{self, Better, Error, Result, VerdictKind};
use crate::stats;
use resvg::{tiny_skia, usvg};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

mod forest;

const W: f32 = 1200.0;
const H: f32 = 630.0;
const HEAD: f32 = 72.0;
const FOOT: f32 = 64.0;
const PAD: f32 = 48.0;
const REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

// ---------- colours ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Light,
    Dark,
    /// SVG only: a stylesheet picks light or dark from the reader's colour scheme.
    Auto,
}

impl Theme {
    fn parse(v: Option<&str>, default: Theme) -> Result<Theme> {
        match v {
            None => Ok(default),
            Some("light") => Ok(Theme::Light),
            Some("dark") => Ok(Theme::Dark),
            Some("auto") => Ok(Theme::Auto),
            Some(o) => Err(Error::usage(format!("--theme is light, dark or auto (SVG only), not {o}"))),
        }
    }
}

/// What a colour is for; the theme says which colour it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ink {
    Bg,
    Fg,
    Muted,
    Line,
    Band,
    Accent,
    Ok,
    Err,
    Plain,
    TagBg,
    TagFg,
    OkBg,
    ErrBg,
    PlainBg,
    OnFill,
}

const INKS: [Ink; 15] = [Ink::Bg, Ink::Fg, Ink::Muted, Ink::Line, Ink::Band, Ink::Accent, Ink::Ok, Ink::Err, Ink::Plain, Ink::TagBg, Ink::TagFg, Ink::OkBg, Ink::ErrBg, Ink::PlainBg, Ink::OnFill];

/// Neutral greys, one blue accent, green and red only for the direction of a decided change.
fn colour(ink: Ink, dark: bool) -> &'static str {
    match (ink, dark) {
        (Ink::Bg, false) => "#ffffff",
        (Ink::Bg, true) => "#111111",
        (Ink::Fg, false) => "#171717",
        (Ink::Fg, true) => "#ededed",
        (Ink::Muted, false) => "#666666",
        (Ink::Muted, true) => "#a1a1a1",
        (Ink::Line, false) => "#e2e2e2",
        (Ink::Line, true) => "#2e2e2e",
        (Ink::Band, false) => "#f2f2f2",
        (Ink::Band, true) => "#1c1c1c",
        (Ink::Accent, _) => "#009fff",
        (Ink::Ok, false) => "#1a7f37",
        (Ink::Ok, true) => "#3fb950",
        (Ink::Err, false) => "#cf222e",
        (Ink::Err, true) => "#f85149",
        (Ink::Plain, false) => "#8f8f8f",
        (Ink::Plain, true) => "#707070",
        (Ink::TagBg, false) => "#f2f2f2",
        (Ink::TagBg, true) => "#262626",
        (Ink::TagFg, false) => "#171717",
        (Ink::TagFg, true) => "#ededed",
        (Ink::OkBg, false) => "#1a7f37",
        (Ink::OkBg, true) => "#238636",
        (Ink::ErrBg, false) => "#cf222e",
        (Ink::ErrBg, true) => "#da3633",
        (Ink::PlainBg, false) => "#6e7781",
        (Ink::PlainBg, true) => "#545d68",
        (Ink::OnFill, _) => "#ffffff",
    }
}

fn index(ink: Ink) -> usize {
    INKS.iter().position(|i| *i == ink).unwrap_or(0)
}

/// The colour an element is drawn in: the theme's, or for `Auto` a placeholder the stylesheet replaces.
fn paint(ink: Ink, theme: Theme) -> String {
    match theme {
        Theme::Light => colour(ink, false).to_string(),
        Theme::Dark => colour(ink, true).to_string(),
        Theme::Auto => format!("#0a0b{:02x}", index(ink)),
    }
}

// ---------- what the card says ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Out {
    Better,
    Worse,
    Same,
    Unclear,
    Refused,
    AaFalse,
    NotRun,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tone {
    Ok,
    Err,
    Plain,
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn verdict(c: &Value) -> &str {
    c["verdict"].as_str().unwrap_or("")
}

fn metric(c: &Value) -> &str {
    c["metric"].as_str().unwrap_or("")
}

fn lower_better(m: &str) -> bool {
    model::metric(m).map(|d| d.better == Better::Lower).unwrap_or(true)
}

fn unit_of(m: &str) -> &'static str {
    model::metric(m).map(|d| d.unit).unwrap_or("")
}

/// One comparison as the report reads it (the same reading as `--format pr`).
fn outcome(c: &Value, aa: bool) -> Out {
    if verdict(c).is_empty() {
        return Out::Unclear;
    }
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    match stats::kind_of(c) {
        VerdictKind::Descriptive | VerdictKind::Saturated => Out::Other,
        VerdictKind::Untested | VerdictKind::Stopped | VerdictKind::Unclear | VerdictKind::Underpowered => Out::Unclear,
        VerdictKind::Refused => Out::Refused,
        VerdictKind::Same | VerdictKind::NoWorse => Out::Same,
        // `worse (>2%)`: beyond a no-worse margin.
        VerdictKind::Worse if c["noworse"].is_number() => Out::Worse,
        VerdictKind::Better | VerdictKind::Worse if !(hi < 0.0 || lo > 0.0) => Out::Unclear,
        VerdictKind::Better | VerdictKind::Worse if aa => Out::AaFalse,
        VerdictKind::Better | VerdictKind::Worse if (hi < 0.0) == lower_better(metric(c)) => Out::Better,
        VerdictKind::Better | VerdictKind::Worse => Out::Worse,
    }
}

fn primary_out(s: &Value, aa: bool) -> Out {
    if s["not_run"] == true {
        return Out::NotRun;
    }
    if s["kind"] == "leak" {
        return if stats::kind_of(&s["primary"]) == VerdictKind::Worse {
            Out::Worse
        } else if s["decided"] == true {
            Out::Same
        } else {
            Out::Unclear
        };
    }
    outcome(&s["primary"], aa)
}

/// A metric in a word or two for a large number to stand beside, and the words for its two
/// directions when the catalog's would read oddly there.
fn short(m: &str) -> (String, Option<(&'static str, &'static str)>) {
    let fewer = Some(("fewer", "more"));
    let known = match m {
        "cpu.work" => "CPU time",
        "cpu.wakes" => return ("wake-ups".into(), fewer),
        "ui.wakes" => return ("UI thread wake-ups".into(), fewer),
        "ui.cpu" => "UI thread CPU time",
        "cpu.app.total" | "cpu.process.total" => "total CPU",
        "cpu.renderer" => "renderer CPU",
        "cpu.gpu" => "GPU process CPU",
        "cpu.browser" => "browser CPU",
        "memory.app.private" => "memory",
        "memory.app.host" => "heap memory",
        "gpu.vram" => "GPU memory",
        "gpu.shared" => "shared GPU memory",
        "gpu.process.3d" => "GPU use",
        "main.busy" => "main-thread busy time",
        "main.work" => "main-thread work",
        "main.script" => "JavaScript time",
        "main.style" => "style time",
        "main.layout" => "layout time",
        "main.gc" => "garbage collection",
        "main.tasks.long" => return ("long tasks".into(), fewer),
        "layout.count" => return ("layouts".into(), fewer),
        "style.count" => return ("style recalculations".into(), fewer),
        "run.duration" => "run time",
        "launch.ready" => "time until ready",
        "io.read" => "reads",
        "io.write" => "writes",
        "energy.cpu" => "CPU energy",
        "cpu.instructions" => return ("instructions".into(), fewer),
        "cpu.idle_wakes" => return ("idle wake-ups".into(), fewer),
        "frames.dropped.pct" | "frames.dropped" => return ("dropped frames".into(), fewer),
        "frames.presented.count" => return ("frames shown".into(), Some(("more", "fewer"))),
        "frames.interval.p50" => "frame gaps (p50)",
        "frames.interval.p95" => "frame gaps (p95)",
        "frames.interval.p99" => "frame gaps (p99)",
        "frames.interval.max" => "longest frame gap",
        "frames.presented.p50" => "frame latency (p50)",
        "frames.presented.p95" => "frame latency (p95)",
        "frames.presented.p99" => "frame latency (p99)",
        "interaction.p50" => "input response (p50)",
        "interaction.p95" => "input response (p95)",
        "scroll.latency.p95" => "scroll response (p95)",
        "js.heap.used" => "JavaScript heap",
        "dom.nodes" => return ("DOM nodes".into(), fewer),
        _ => "",
    };
    if !known.is_empty() {
        return (known.into(), None);
    }
    if let Some(rest) = m.strip_prefix("launch.") {
        return (format!("time until {rest}"), None);
    }
    if let Some(rest) = m.strip_prefix("phase.") {
        return (format!("time in {rest}"), None);
    }
    if let Some(rest) = m.strip_prefix("span.") {
        return (format!("time in {rest} spans"), None);
    }
    if let Some(rest) = m.strip_prefix("count.") {
        return (format!("{} count", rest.replace(['-', '_'], " ")), Some(("lower", "higher")));
    }
    let about = model::metric(m).map(|d| d.about).unwrap_or(m);
    let first = about.split([',', '(', ':']).next().unwrap_or(about).trim().trim_end_matches(" per run");
    (if first.is_empty() { m.to_string() } else { first.to_string() }, None)
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
        "/s" => format!("{}/s", sig3(v)),
        "bytes" => super::fmt::bytes(v),
        "count" | "" => super::fmt::value(v, "count"),
        u => format!("{} {u}", sig3(v)),
    }
}

fn abs_unit(unit: &str) -> &str {
    if unit == "%" { "pp" } else { unit }
}

/// One headline number with what it stands for.
struct Tile {
    /// `90%` or `219 MB`.
    amount: String,
    /// `fewer wake-ups`.
    words: String,
    /// `−90%`, for the badge.
    delta: String,
    noun: String,
    before: String,
    after: String,
    tone: Tone,
    /// Each pair's change as the comparison measured it: a log ratio `ln(head / base)` for a ratio
    /// metric (so −80% and +400% sit as far from no change as they are), else a difference in the
    /// metric's unit.
    dots: Vec<f64>,
    /// The comparison's bound in the same scale, (low, high): changes inside it count as no change.
    band: (f64, f64),
    lower: bool,
    tally: String,
    series: String,
}

fn tile(s: &Value, c: &Value, series: String) -> Tile {
    let m = metric(c);
    let est = f(&c["estimate"]);
    let lower = lower_better(m);
    let (noun, words) = short(m);
    let (good, bad) = words.or_else(|| model::metric(m).map(|d| d.words)).unwrap_or(("less", "more"));
    let improved = (est < 0.0) == lower;
    let word = if improved { good } else { bad };
    let relative = c["relative"] == true;
    let pct = (est * 100.0).abs();
    let amount = match relative {
        true if pct < 10.0 => format!("{pct:.1}%"),
        true => format!("{pct:.0}%"),
        false => disp(est.abs(), abs_unit(unit_of(m))),
    };
    let sign = if est < 0.0 { "−" } else { "+" };
    let delta = format!("{sign}{amount}");
    let unit = unit_of(m);
    let mut dots = Vec::new();
    let (mut won, mut n) = (0, 0);
    for (b, h) in pairs(s) {
        let (x, y) = (f(&b["values"][m]), f(&h["values"][m]));
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        // A ratio comparison had every value positive (`stats::compare`): its pairs as log ratios.
        let d = if relative { if x > 0.0 && y > 0.0 { (y / x).ln() } else { continue } } else { y - x };
        n += 1;
        if (lower && y < x) || (!lower && y > x) {
            won += 1;
        }
        dots.push(d);
    }
    let o = outcome(c, false);
    let tone = match o {
        Out::Better => Tone::Ok,
        Out::Worse => Tone::Err,
        _ => Tone::Plain,
    };
    let tally = match (o, n) {
        (_, 0) => String::new(),
        (Out::Worse, _) => format!("worse in {} of {n} pairs", n - won),
        _ => format!("better in {won} of {n} pairs"),
    };
    let band = f(&c["bound"]).abs();
    let band = if band.is_finite() { band } else { 0.02 };
    // `same` means a change inside ±bound of base: in log ratios, ln(1 − b) to ln(1 + b).
    let band = if relative { ((1.0 - band).max(1e-9).ln(), (1.0 + band).ln()) } else { (-band, band) };
    Tile { amount, words: format!("{word} {noun}"), delta, noun, before: disp(f(&c["base"]), unit), after: disp(f(&c["head"]), unit), tone, dots, band, lower, tally, series }
}

/// Each pair's two blocks (base, head), in pair order.
fn pairs(s: &Value) -> Vec<(&Value, &Value)> {
    let mut by: BTreeMap<u64, (Option<&Value>, Option<&Value>)> = BTreeMap::new();
    for b in s["blocks"].as_array().into_iter().flatten() {
        let Some(p) = b["pair"].as_u64().filter(|p| *p < 1_000_000) else { continue };
        let e = by.entry(p).or_default();
        if b["variant"] == "base" {
            e.0 = Some(b);
        } else if b["variant"] == "head" {
            e.1 = Some(b);
        }
    }
    by.into_values().filter_map(|(b, h)| Some((b?, h?))).collect()
}

/// A series as people say it: its case in sentence case, the target when the series differ in it,
/// and its parameters.
fn title(s: &Value, all: &[&Value]) -> String {
    let label = s["label"].as_str().unwrap_or("");
    let case = s["case"].as_str().unwrap_or_else(|| label.split(" · ").next().unwrap_or(label));
    let mut t = capitalize(&case.replace(['-', '_'], " "));
    let mut extra: Vec<String> = Vec::new();
    if let Some(target) = s["target"].as_str()
        && all.iter().filter_map(|x| x["target"].as_str()).any(|x| x != target)
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

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|x| x.to_uppercase().chain(c).collect()).unwrap_or_default()
}

enum Body {
    Tiles(Vec<Tile>),
    /// No win and nothing worse: a heading, a line on why, and the pairs when there are any.
    Calm {
        heading: String,
        detail: String,
        tile: Option<Box<Tile>>,
    },
}

struct Card {
    id: String,
    subject: String,
    body: Body,
    footer: String,
    /// Results the tiles had no room for.
    more: String,
    alt: String,
    /// One badge per series: (series label, noun, value, tone).
    badges: Vec<(String, String, String, Tone)>,
    /// What the tiles call base and head: "before" and "this PR", or an implementation comparison's names.
    sides: (String, String),
}

/// The same results as `--format pr`'s hero line and caution (`pr::hero`): the wins in its order and
/// what got decidedly worse, so a card never shows a win alone when something regressed. At most
/// three tiles (two for regressions beside a win); without a win, up to three regressions; else a
/// calm card.
fn build(r: &Value) -> Card {
    let aa = r["plan"]["aa"] == true;
    let series: Vec<&Value> = r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let multi = series.len() > 1;
    let hero = super::pr::hero(r);
    let name = |s: &Value| if multi { title(s, &series) } else { String::new() };
    let mut bad: Vec<(&Value, Tile)> = hero.worse.iter().map(|(s, c)| (*s, tile(s, c, name(s)))).collect();
    for (s, b, h, k, n) in &hero.late {
        bad.push((
            *s,
            Tile {
                amount: format!("{:.1} pp", (h - b) * 100.0),
                words: "more late frames".into(),
                delta: format!("+{:.1} pp", (h - b) * 100.0),
                noun: "late frames".into(),
                before: format!("{:.1}%", b * 100.0),
                after: format!("{:.1}%", h * 100.0),
                tone: Tone::Err,
                dots: Vec::new(),
                band: (0.0, 0.0),
                lower: true,
                tally: format!("more in {k} of {n} pairs · not tested"),
                series: name(s),
            },
        ));
    }
    let n_bad = if hero.wins.is_empty() { bad.len().min(3) } else { bad.len().min(2) };
    let n_wins = hero.wins.len().min(3 - n_bad);
    let hidden = hero.wins.len() - n_wins + bad.len() - n_bad;
    let mut shown: Vec<&Value> = hero.wins.iter().take(n_wins).map(|w| w.0).collect();
    shown.extend(bad.iter().take(n_bad).map(|b| b.0));
    let mut tiles: Vec<Tile> = hero.wins.iter().take(n_wins).map(|(s, c)| tile(s, c, name(s))).collect();
    tiles.extend(bad.into_iter().take(n_bad).map(|b| b.1));
    // Tiles of one series: the header names it once.
    if shown.windows(2).all(|w| std::ptr::eq(w[0], w[1])) {
        tiles.iter_mut().for_each(|t| t.series.clear());
    }

    let id = r["id"].as_str().unwrap_or("").to_string();
    if tiles.is_empty() {
        shown = series.clone();
    }
    let mut titles: Vec<String> = Vec::new();
    for s in &shown {
        let t = title(s, &series);
        if !titles.contains(&t) {
            titles.push(t);
        }
    }
    let file = r["file_repo"].as_str().or(r["file"].as_str()).unwrap_or("").replace('\\', "/");
    let subject = match titles.len() {
        1 | 2 => titles.join(" · "),
        _ => file.rsplit('/').next().unwrap_or("").to_string(),
    };
    let ns: Vec<u64> = shown.iter().filter_map(|s| s["pairs"].as_u64()).collect();
    let pairs_text = match (ns.iter().min(), ns.iter().max()) {
        (Some(a), Some(b)) if a == b => format!("{a} pair{}", if *a == 1 { "" } else { "s" }),
        (Some(a), Some(b)) => format!("{a}–{b} pairs"),
        _ => String::new(),
    };
    let footer = super::fmt::fields(["measured with strobe".to_string(), pairs_text, short_cpu(r["env"]["cpu"].as_str().unwrap_or("")), short_os(r["env"]["os"].as_str().unwrap_or(""))]);
    let more = match hidden {
        0 => String::new(),
        k => format!("{k} more in the report"),
    };

    let badges = series.iter().map(|s| badge_of(s, aa)).collect();
    let (body, alt) = if tiles.is_empty() {
        let (detail, tile) = calm(&series, aa);
        let heading = hero.calm;
        let alt = if detail.is_empty() { heading.clone() } else { format!("{heading}. {detail}") };
        (Body::Calm { heading, detail, tile }, alt)
    } else {
        let parts: Vec<String> = tiles
            .iter()
            .map(|t| {
                let who = if t.series.is_empty() { String::new() } else { format!(" ({})", t.series) };
                let tally = if t.tally.is_empty() { String::new() } else { format!(", {}", t.tally) };
                format!("{} {}{who}{tally}", t.amount, t.words)
            })
            .collect();
        (Body::Tiles(tiles), parts.join("; "))
    };
    let alt = if more.is_empty() { format!("{alt}.") } else { format!("{alt}; {more}.") };
    let sides = match super::pr::side_names(r) {
        (b, h) if r["comparison"] == "implementations" => (b, h),
        _ => ("before".to_string(), "this PR".to_string()),
    };
    Card { id, subject, body, footer, more, alt, badges, sides }
}

/// Under the calm heading (`pr::hero`'s, which it follows in order): the line on why, and the series
/// whose pairs the card shows.
fn calm(series: &[&Value], aa: bool) -> (String, Option<Box<Tile>>) {
    let outs: Vec<(Out, &Value)> = series.iter().map(|s| (primary_out(s, aa), *s)).collect();
    let find = |want: Out| outs.iter().find(|(o, _)| *o == want).map(|(_, s)| *s);
    let noun = |s: &Value| capitalize(&short(metric(&s["primary"])).0);
    let with_tile = |s: &Value| {
        let t = tile(s, &s["primary"], String::new());
        if t.dots.is_empty() { None } else { Some(Box::new(t)) }
    };
    if let Some(s) = series.iter().find(|s| s["kind"] == "leak") {
        return (capitalize(verdict(&s["primary"]).split(';').next().unwrap_or("")), None);
    }
    if let Some(s) = find(Out::Worse) {
        let c = &s["primary"];
        return (format!("{}: {} per pair, past the margin allowed", noun(s), super::fmt::pct(f(&c["estimate"]))), with_tile(s));
    }
    if let Some(s) = find(Out::AaFalse) {
        let d = super::fmt::pct(f(&s["primary"]["estimate"]));
        return (format!("{} differed by {d} between two identical builds: this setup is not steady enough yet", noun(s)), with_tile(s));
    }
    if let Some(s) = find(Out::Unclear) {
        let n = s["primary"]["n"].as_u64().or(s["pairs"].as_u64()).unwrap_or(0);
        return (format!("{}: no clear change in {n} pairs", noun(s)), with_tile(s));
    }
    if let Some(s) = find(Out::Refused) {
        let v = verdict(&s["primary"]);
        // Results from before comparisons kept their refusal: from the verdict's words.
        let why = s["primary"]["refusal"]["summary"].as_str().or_else(|| v.split_once("): ").map(|x| x.1)).or_else(|| v.split_once(": ").map(|x| x.1)).unwrap_or("");
        let plain = match why.split_once(" frames < ") {
            Some((have, need)) => format!("too few frames ({have} per launch, {need} needed)"),
            None if why.is_empty() => "strobe refused the measurement".into(),
            None => why.to_string(),
        };
        return (format!("{}: {plain}", noun(s)), None);
    }
    if let Some(s) = find(Out::Same) {
        let c = &s["primary"];
        let unit = unit_of(metric(c));
        let b = f(&c["bound"]);
        let bound = if c["relative"] == true { format!("±{:.0}%", b * 100.0) } else { format!("±{}", disp(b, abs_unit(unit))) };
        return (format!("{}: {} → {}, within {bound}", noun(s), disp(f(&c["base"]), unit), disp(f(&c["head"]), unit)), with_tile(s));
    }
    if find(Out::NotRun).is_some() {
        return ("The bench stopped before this series ran".into(), None);
    }
    (String::new(), None)
}

/// A series' main metric as a badge: its noun, then the change, `same`, `unclear` or why not.
fn badge_of(s: &Value, aa: bool) -> (String, String, String, Tone) {
    let label = s["label"].as_str().unwrap_or("").to_string();
    let c = &s["primary"];
    let noun = short(metric(c)).0;
    let o = primary_out(s, aa);
    let (value, tone) = match o {
        _ if s["kind"] == "leak" => (
            if o == Out::Worse {
                "leaks"
            } else if o == Out::Same {
                "no leak"
            } else {
                "unclear"
            }
            .to_string(),
            if o == Out::Worse { Tone::Err } else { Tone::Plain },
        ),
        Out::Better | Out::Worse => (tile(s, c, String::new()).delta, if o == Out::Better { Tone::Ok } else { Tone::Err }),
        Out::Same => ("same".into(), Tone::Plain),
        Out::Refused => ("not measured".into(), Tone::Plain),
        Out::AaFalse => ("differed".into(), Tone::Plain),
        Out::NotRun => ("not run".into(), Tone::Plain),
        _ => ("unclear".into(), Tone::Plain),
    };
    (label, noun, value, tone)
}

/// `AMD Ryzen 9 9950X3D 16-Core Processor` → `Ryzen 9 9950X3D`.
fn short_cpu(cpu: &str) -> String {
    let c = cpu.replace("(R)", "").replace("(TM)", "").replace(" CPU", "");
    let c = c.split(" @ ").next().unwrap_or(&c).to_string();
    c.split_whitespace().filter(|w| !w.ends_with("-Core") && *w != "Processor" && *w != "AMD" && *w != "Intel").collect::<Vec<_>>().join(" ")
}

/// `Windows 11 26300 (26H2)` → `Windows 11`; `macOS 15.1` stays.
fn short_os(os: &str) -> String {
    os.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

// ---------- drawing ----------

fn fonts() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(REGULAR.to_vec());
        db.load_font_data(SEMIBOLD.to_vec());
        db.set_sans_serif_family("Inter");
        Arc::new(db)
    })
    .clone()
}

fn options() -> usvg::Options<'static> {
    let mut o = usvg::Options { font_family: "Inter".into(), ..Default::default() };
    o.fontdb = fonts();
    o
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The drawn width of a line of text, from the same shaping the image uses.
fn measure(text: &str, size: f32, bold: bool) -> f32 {
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="400"><text x="0" y="300" font-family="Inter" font-size="{size}" font-weight="{}">{}</text></svg>"#,
        if bold { 600 } else { 400 },
        esc(text)
    );
    usvg::Tree::from_str(&svg, &options()).map(|t| t.root().abs_bounding_box().width()).unwrap_or(text.chars().count() as f32 * size * 0.55)
}

/// The largest size up to `size` (and down to 60% of it) at which `text` fits `width`.
fn fit(text: &str, width: f32, size: f32, bold: bool) -> f32 {
    let w = measure(text, size, bold);
    if w <= width { size } else { (size * width / w).max(size * 0.6).floor() }
}

/// `text` in at most two lines of `width`, at the largest size that fits.
fn wrap(text: &str, width: f32, size: f32, bold: bool) -> (Vec<String>, f32) {
    // A little smaller on one line reads better than two lines.
    let one = fit(text, width, size, bold);
    if one >= size * 0.85 {
        return (vec![text.to_string()], one);
    }
    // The most even break: its longer line is the shortest of any break.
    let words: Vec<&str> = text.split(' ').collect();
    let mut best: Option<(Vec<String>, f32)> = None;
    for cut in 1..words.len() {
        let (a, b) = (words[..cut].join(" "), words[cut..].join(" "));
        let longer = measure(&a, size, bold).max(measure(&b, size, bold));
        if best.as_ref().is_none_or(|x| longer < x.1) {
            best = Some((vec![a, b], longer));
        }
    }
    match best {
        Some((lines, longer)) => (lines, if longer <= width { size } else { (size * width / longer).max(size * 0.6).floor() }),
        None => (vec![text.to_string()], one),
    }
}

/// `text` cut to `width` with an ellipsis.
fn clip(text: &str, width: f32, size: f32, bold: bool) -> String {
    if measure(text, size, bold) <= width {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while chars.len() > 1 {
        chars.pop();
        let t = format!("{}…", chars.iter().collect::<String>().trim_end());
        if measure(&t, size, bold) <= width {
            return t;
        }
    }
    "…".into()
}

struct Svg {
    out: String,
    theme: Theme,
}

impl Svg {
    fn new(w: f32, h: f32, theme: Theme) -> Svg {
        Svg { out: format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"#), theme }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, ink: Ink) {
        self.out += &format!(r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{}"/>"#, paint(ink, self.theme));
    }

    /// A 1px rule, on the pixel grid.
    fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, ink: Ink) {
        self.out += &format!(r#"<path d="M{x1} {y1}L{x2} {y2}" fill="none" stroke="{}" stroke-width="1"/>"#, paint(ink, self.theme));
    }

    fn frame(&mut self, x: f32, y: f32, w: f32, h: f32, ink: Ink) {
        self.out += &format!(r#"<rect x="{}" y="{}" width="{}" height="{}" fill="none" stroke="{}" stroke-width="1"/>"#, x + 0.5, y + 0.5, w - 1.0, h - 1.0, paint(ink, self.theme));
    }

    fn dot(&mut self, x: f32, y: f32, r: f32, ink: Ink) {
        self.out += &format!(r#"<circle cx="{x}" cy="{y}" r="{r}" fill="{}"/>"#, paint(ink, self.theme));
    }

    /// The font is (size, semibold).
    fn text(&mut self, x: f32, y: f32, (size, bold): (f32, bool), ink: Ink, anchor: &str, text: &str) {
        self.out += &format!(
            r#"<text x="{x}" y="{y}" font-family="Inter" font-size="{size}" font-weight="{}" text-anchor="{anchor}" fill="{}">{}</text>"#,
            if bold { 600 } else { 400 },
            paint(ink, self.theme),
            esc(text)
        );
    }

    fn end(mut self) -> String {
        self.out += "</svg>";
        self.out
    }
}

fn tone_ink(t: Tone) -> Ink {
    match t {
        Tone::Ok => Ink::Ok,
        Tone::Err => Ink::Err,
        Tone::Plain => Ink::Fg,
    }
}

/// One dot per pair on a line through no change, the bound shaded around it. For a decided change
/// (`colour`) green where this PR did better than the bound, red where worse, grey inside it; else
/// all grey, since no single pair decides anything.
fn strip(svg: &mut Svg, x: f32, y: f32, w: f32, t: &Tile, r: f32, colour: bool) {
    if t.dots.is_empty() {
        return;
    }
    let h = r * 6.0;
    let cy = y + h / 2.0;
    let (b_lo, b_hi) = t.band;
    let lo = t.dots.iter().cloned().fold(b_lo * 1.5, f64::min).min(0.0);
    let hi = t.dots.iter().cloned().fold(b_hi * 1.5, f64::max).max(0.0);
    let pad = (hi - lo) * 0.06;
    // A band of 0 and pairs that all read no change leave no width: give it some, not NaN.
    let pad = if hi - lo < 1e-9 { 0.01 } else { pad };
    let (lo, hi) = (lo - pad, hi + pad);
    let px = |v: f64| x + r + ((v - lo) / (hi - lo)) as f32 * (w - 2.0 * r);
    svg.rect(px(b_lo), y, (px(b_hi) - px(b_lo)).max(1.0), h, Ink::Band);
    svg.line(x, cy.round() + 0.5, x + w, cy.round() + 0.5, Ink::Line);
    let zero = px(0.0).round() + 0.5;
    svg.line(zero, y, zero, y + h, Ink::Muted);
    let label = 14.0_f32.max(r * 2.0);
    let anchor = if zero > x + w - 60.0 {
        "end"
    } else if zero < x + 60.0 {
        "start"
    } else {
        "middle"
    };
    svg.text(zero, y + h + label + 2.0, (label, false), Ink::Muted, anchor, "no change");
    let mut sorted = t.dots.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mut placed: Vec<(f32, i32)> = Vec::new();
    for v in sorted {
        let dx = px(v);
        let row = [0, -1, 1, -2, 2].into_iter().find(|row| !placed.iter().any(|(px2, r2)| r2 == row && (px2 - dx).abs() < 2.0 * r + 1.0)).unwrap_or(0);
        placed.push((dx, row));
        let better = if t.lower { v < b_lo } else { v > b_hi };
        let worse = if t.lower { v > b_hi } else { v < b_lo };
        let ink = if colour && better {
            Ink::Ok
        } else if colour && worse {
            Ink::Err
        } else {
            Ink::Plain
        };
        svg.dot(dx, cy + row as f32 * (2.0 * r * 0.8), r, ink);
    }
}

fn chrome(svg: &mut Svg, card: &Card) {
    svg.rect(0.0, 0.0, W, H, Ink::Bg);
    svg.frame(0.0, 0.0, W, H, Ink::Line);
    svg.line(0.0, HEAD - 0.5, W, HEAD - 0.5, Ink::Line);
    svg.line(0.0, H - FOOT + 0.5, W, H - FOOT + 0.5, Ink::Line);
    svg.rect(PAD, HEAD / 2.0 - 7.0, 14.0, 14.0, Ink::Accent);
    svg.text(PAD + 26.0, HEAD / 2.0 + 8.0, (22.0, true), Ink::Fg, "start", "strobe");
    let id_w = measure(&card.id, 20.0, false);
    svg.text(W - PAD, HEAD / 2.0 + 7.0, (20.0, false), Ink::Muted, "end", &card.id);
    let left = PAD + 26.0 + measure("strobe", 22.0, true) + 28.0;
    let room = W - PAD - id_w - 32.0 - left;
    svg.text(left, HEAD / 2.0 + 8.0, (22.0, false), Ink::Fg, "start", &clip(&card.subject, room, 22.0, false));
    let more_w = if card.more.is_empty() { 0.0 } else { measure(&card.more, 20.0, false) + 32.0 };
    svg.text(PAD, H - FOOT / 2.0 + 7.0, (20.0, false), Ink::Muted, "start", &clip(&card.footer, W - 2.0 * PAD - more_w, 20.0, false));
    svg.text(W - PAD, H - FOOT / 2.0 + 7.0, (20.0, false), Ink::Fg, "end", &card.more);
}

/// The card's source SVG (live text, set in Inter).
fn card_svg(card: &Card, theme: Theme) -> String {
    let mut svg = Svg::new(W, H, theme);
    chrome(&mut svg, card);
    let top = HEAD;
    let bottom = H - FOOT;
    match &card.body {
        Body::Tiles(tiles) if tiles.len() == 1 => {
            let t = &tiles[0];
            let width = W - 2.0 * PAD;
            let big = fit(&t.amount, width, 180.0, true);
            svg.text(PAD - 5.0, top + 196.0, (big, true), tone_ink(t.tone), "start", &t.amount);
            let ws = fit(&t.words, width, 52.0, false);
            svg.text(PAD, top + 292.0, (ws, false), Ink::Fg, "start", &t.words);
            before_after(&mut svg, PAD, top + 346.0, 26.0, t, width, &card.sides);
            strip(&mut svg, PAD, top + 378.0, width * 0.62, t, 8.0, t.tone != Tone::Plain);
            let note = if t.tally.is_empty() { String::new() } else { format!("each dot is one pair · {}", t.tally) };
            svg.text(W - PAD, top + 408.0, (20.0, false), Ink::Muted, "end", &note);
        }
        Body::Tiles(tiles) => {
            let n = tiles.len() as f32;
            let col = (W - 2.0 * PAD) / n;
            let gap = 40.0;
            for (i, t) in tiles.iter().enumerate() {
                let x = PAD + i as f32 * col + if i > 0 { gap / 2.0 } else { 0.0 };
                let w = col - if i > 0 && i + 1 < tiles.len() { gap } else { gap / 2.0 };
                if i > 0 {
                    let sx = (PAD + i as f32 * col).round() + 0.5;
                    svg.line(sx, top + 36.0, sx, bottom - 36.0, Ink::Line);
                }
                let mut y = top + 64.0;
                if !t.series.is_empty() {
                    svg.text(x, y, (20.0, false), Ink::Muted, "start", &clip(&t.series, w, 20.0, false));
                }
                let big_size = if tiles.len() == 2 { 140.0 } else { 108.0 };
                y += big_size * 0.95;
                let big = fit(&t.amount, w, big_size, true);
                svg.text(x - 3.0, y, (big, true), tone_ink(t.tone), "start", &t.amount);
                let (lines, ws) = wrap(&t.words, w, if tiles.len() == 2 { 40.0 } else { 32.0 }, false);
                // Clear of the number's descenders (`pp`), then line by line.
                y += big * 0.26 - ws * 0.3;
                for line in &lines {
                    y += ws * 1.25;
                    svg.text(x, y, (ws, false), Ink::Fg, "start", line);
                }
                let y = bottom - 150.0;
                before_after(&mut svg, x, y, if tiles.len() == 2 { 24.0 } else { 20.0 }, t, w, &card.sides);
                strip(&mut svg, x, y + 26.0, w, t, 6.0, t.tone != Tone::Plain);
                svg.text(x, bottom - 40.0, (18.0, false), Ink::Muted, "start", &t.tally);
            }
        }
        Body::Calm { heading, detail, tile } => {
            let width = W - 2.0 * PAD;
            let hs = fit(heading, width, 88.0, true);
            let ink = if heading.starts_with("Worse") { Ink::Err } else { Ink::Fg };
            // Without pairs to show, the words sit in the middle.
            let lift = if tile.is_some() { 0.0 } else { 40.0 };
            svg.text(PAD - 3.0, top + 170.0 + lift, (hs, true), ink, "start", heading);
            if !detail.is_empty() {
                let (lines, ds) = wrap(detail, width, 32.0, false);
                let mut y = top + 232.0 + lift;
                for line in &lines {
                    svg.text(PAD, y, (ds, false), Ink::Muted, "start", line);
                    y += ds * 1.3;
                }
            }
            if let Some(t) = tile {
                strip(&mut svg, PAD, top + 330.0, width * 0.62, t, 8.0, false);
                let note = if t.tally.is_empty() { String::new() } else { format!("each dot is one pair · {}", t.tally) };
                svg.text(W - PAD, top + 364.0, (20.0, false), Ink::Muted, "end", &note);
            }
        }
    }
    svg.end()
}

/// `before 71.4/s → this PR 7.06/s` (or the sides' names), each part placed after the last (SVG drops
/// edge spaces).
fn before_after(svg: &mut Svg, x: f32, y: f32, size: f32, t: &Tile, width: f32, sides: &(String, String)) {
    let parts = [(sides.0.as_str(), Ink::Muted, false), (t.before.as_str(), Ink::Fg, false), ("→", Ink::Muted, false), (sides.1.as_str(), Ink::Muted, false), (t.after.as_str(), Ink::Fg, true)];
    let gaps = [0.3, 0.6, 0.6, 0.3, 0.0];
    // The advance, not the ink: the glyphs' box between two visible stand-ins.
    let advance = |text: &str, size: f32, bold: bool| measure(&format!("|{text}|"), size, bold) - measure("||", size, bold);
    let total: f32 = parts.iter().zip(gaps).map(|((text, _, bold), g)| advance(text, size, *bold) + g * size).sum();
    let size = if total > width { (size * width / total).floor() } else { size };
    let mut at = x;
    for ((text, ink, bold), g) in parts.into_iter().zip(gaps) {
        svg.text(at, y, (size, bold), ink, "start", text);
        at += advance(text, size, bold) + g * size;
    }
}

/// A badge's source SVG: the noun on grey (none: the value alone, as in a sentence that names the
/// metric already), the value on its tone, square, 20px high.
fn badge_svg(noun: &str, value: &str, tone: Tone, theme: Theme) -> String {
    let size = 11.0;
    let lw = if noun.is_empty() { 0.0 } else { (measure(noun, size, false) + 14.0).round() };
    let vw = (measure(value, size, true) + 14.0).round();
    let (w, h) = (lw + vw, 20.0);
    let mut svg = Svg::new(w, h, theme);
    let fill = match tone {
        Tone::Ok => Ink::OkBg,
        Tone::Err => Ink::ErrBg,
        Tone::Plain => Ink::PlainBg,
    };
    svg.rect(lw, 0.0, vw, h, fill);
    if lw > 0.0 {
        svg.rect(0.0, 0.0, lw, h, Ink::TagBg);
        svg.frame(0.0, 0.0, lw, h, Ink::Line);
        svg.text(lw / 2.0, 14.0, (size, false), Ink::TagFg, "middle", noun);
    }
    svg.text(lw + vw / 2.0, 14.0, (size, true), Ink::OnFill, "middle", value);
    svg.end()
}

/// Lays the source out with the embedded font, then writes it with every glyph as a path (the same in
/// any viewer, no font needed) and, for `Auto`, a stylesheet that swaps light for dark.
fn outline(source: &str, theme: Theme, label: &str) -> Result<(String, usvg::Tree)> {
    let tree = usvg::Tree::from_str(source, &options()).map_err(|e| Error::msg(format!("card: {e}")))?;
    let opt = usvg::WriteOptions { coordinates_precision: 2, transforms_precision: 4, indent: usvg::Indent::None, ..Default::default() };
    let mut out = tree.to_string(&opt);
    let mut head = format!("<title>{}</title>", esc(label));
    if theme == Theme::Auto {
        let (mut light, mut dark) = (String::new(), String::new());
        for ink in INKS {
            let i = index(ink);
            let sentinel = format!("#0a0b{i:02x}");
            for (attr, class, prop) in [("fill", 'f', "fill"), ("stroke", 's', "stroke")] {
                let from = format!("{attr}=\"{sentinel}\"");
                if out.contains(&from) {
                    out = out.replace(&from, &format!("class=\"{class}{i}\""));
                    light += &format!(".{class}{i}{{{prop}:{}}}", colour(ink, false));
                    dark += &format!(".{class}{i}{{{prop}:{}}}", colour(ink, true));
                }
            }
        }
        head += &format!("<style>{light}@media (prefers-color-scheme:dark){{{dark}}}</style>");
    }
    let svg_open = out.find('>').map(|i| i + 1).unwrap_or(0);
    out.insert_str(svg_open, &head);
    out = out.replacen("<svg ", &format!("<svg role=\"img\" aria-label=\"{}\" ", esc(label)), 1);
    Ok((out, tree))
}

/// The tree as PNG pixels, `scale` times its size.
fn rasterize(tree: &usvg::Tree, scale: f32) -> Result<Vec<u8>> {
    let size = tree.size();
    let (w, h) = ((size.width() * scale).round() as u32, (size.height() * scale).round() as u32);
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or_else(|| Error::msg("card: empty image"))?;
    resvg::render(tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let rgba = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    });
    crate::png::write(&crate::png::Image { width: w as usize, height: h as usize, rgba: rgba.collect() })
}

/// An image in the format its file name asks for: `.svg` (outlined; `Auto` follows the reader's colour
/// scheme) or `.png` (rasterized at `scale`; `Auto` draws light).
fn image(source: impl Fn(Theme) -> String, path: &Path, theme: Theme, label: &str, scale: f32) -> Result<Vec<u8>> {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("svg") => Ok(outline(&source(theme), theme, label)?.0.into_bytes()),
        Some("png") => {
            let theme = if theme == Theme::Auto { Theme::Light } else { theme };
            rasterize(&outline(&source(theme), theme, label)?.1, scale)
        }
        _ => Err(Error::usage(format!("--out {}: a card or badge is .png or .svg", path.display()))),
    }
}

fn shields_url(noun: &str, value: &str, tone: Tone) -> String {
    let part = |s: &str| {
        let s = s.replace('-', "--").replace('_', "__").replace(' ', "_");
        let mut out = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                out.push(b as char);
            } else {
                out += &format!("%{b:02X}");
            }
        }
        out
    };
    let colour = match tone {
        Tone::Ok => "1a7f37",
        Tone::Err => "cf222e",
        Tone::Plain => "6e7781",
    };
    format!("https://img.shields.io/badge/{}-{}-{colour}?style=flat-square", part(noun), part(value))
}

/// The headline badge: the card's first tile, else the first series' main metric.
fn headline(card: &Card) -> (String, String, Tone) {
    match &card.body {
        Body::Tiles(t) => (t[0].noun.clone(), t[0].delta.clone(), t[0].tone),
        _ => card.badges.first().map(|b| (b.1.clone(), b.2.clone(), b.3)).unwrap_or(("strobe".into(), "no result".into(), Tone::Plain)),
    }
}

/// What a card draws, for either kind of experiment: the image's source, its words, each series'
/// badge, the headline badge.
struct Drawn {
    draw: Box<dyn Fn(Theme) -> String>,
    id: String,
    alt: String,
    badges: Vec<(String, String, String, Tone)>,
    headline: (String, String, Tone),
    /// An implementation comparison's scoreboard badge.
    scoreboard: Option<(String, String, Tone)>,
}

fn drawn(r: &Value) -> Drawn {
    if crate::app::family::implementations(r) {
        let card = Arc::new(forest::build(r));
        let c = card.clone();
        return Drawn {
            draw: Box::new(move |t| forest::svg(&c, t)),
            id: r["id"].as_str().unwrap_or("").to_string(),
            alt: card.alt.clone(),
            badges: card.badges.clone(),
            headline: card.headline.clone(),
            scoreboard: Some(card.scoreboard_badge.clone()),
        };
    }
    let card = Arc::new(build(r));
    let c = card.clone();
    Drawn { draw: Box::new(move |t| card_svg(&c, t)), id: card.id.clone(), alt: card.alt.clone(), badges: card.badges.clone(), headline: headline(&card), scoreboard: None }
}

/// `report experiment <exp> --format card|badge [--out <file.png|.svg>] [--theme light|dark|auto] [--badge-url] [--scoreboard]`.
pub fn report(args: &Args, dir: &Path, r: &Value, format: &str) -> Result<Output> {
    let card = drawn(r);
    let default = if format == "card" { "card.png" } else { "badge.svg" };
    let out = args.get("out").map(PathBuf::from).unwrap_or_else(|| dir.join(default));
    let svg_out = out.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let theme = Theme::parse(args.get("theme"), if svg_out { Theme::Auto } else { Theme::Light })?;
    if args.has("scoreboard") && (format != "badge" || card.scoreboard.is_none()) {
        return Err(Error::usage("--scoreboard draws an implementation comparison's scoreboard as a badge: --format badge of an experiment whose sides are two targets"));
    }
    if format == "badge" {
        let (noun, value, tone) = if args.has("scoreboard") { card.scoreboard.clone().unwrap_or(card.headline.clone()) } else { card.headline.clone() };
        let url = shields_url(&noun, &value, tone);
        let series: Vec<Value> = card.badges.iter().map(|b| json!({ "series": b.0, "label": b.1, "value": b.2, "url": shields_url(&b.1, &b.2, b.3) })).collect();
        // The zero-file form: shields.io draws it, and GitHub fetches it through its image proxy.
        if args.has("badge-url") {
            return Ok(Output::new(format!("{url}\n"), json!({ "url": url, "label": noun, "value": value, "series": series })));
        }
        let bytes = image(|t| badge_svg(&noun, &value, tone, t), &out, theme, &format!("{noun} {value}"), 2.0)?;
        super::report::write_whole(&out, &bytes)?;
        let path = out.display().to_string();
        return Ok(Output::new(format!("{path} · {noun} {value}\n"), json!({ "path": path, "bytes": bytes.len(), "label": noun, "value": value, "url": url, "series": series })));
    }
    let bytes = image(&card.draw, &out, theme, &card.alt, 1.0)?;
    super::report::write_whole(&out, &bytes)?;
    let path = out.display().to_string();
    Ok(Output::new(format!("{path} · {}\n{}\n", super::fmt::bytes(bytes.len() as f64), card.alt), json!({ "path": path, "bytes": bytes.len(), "alt": card.alt })))
}

/// The card and each series' badge as SVG files that follow the reader's colour scheme, in `dir`'s
/// `pr-card` folder, for `--update-pr` to attach: `card` for the report (`url` and `alt`, `badges`
/// by series label, each a `./name` that `gh --attach` replaces with the uploaded image's address,
/// and a `digest` of the images) and the files.
pub fn for_pr(r: &Value, dir: &Path) -> Result<(Value, Vec<PathBuf>)> {
    let card = drawn(r);
    let folder = dir.join("pr-card");
    std::fs::create_dir_all(&folder)?;
    let id = card.id.clone();
    let mut files = Vec::new();
    let name = format!("strobe-card-{id}.svg");
    let path = folder.join(&name);
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let bytes = image(&card.draw, &path, Theme::Auto, &card.alt, 1.0)?;
    hash.update(&bytes);
    super::report::write_whole(&path, &bytes)?;
    files.push(path);
    let mut badges = serde_json::Map::new();
    for (i, (label, noun, value, tone)) in card.badges.iter().enumerate() {
        let name = format!("strobe-badge-{id}-{}.svg", i + 1);
        let path = folder.join(&name);
        // The series' line names the metric: the badge is the change alone.
        let bytes = image(|t| badge_svg("", value, *tone, t), &path, Theme::Auto, &format!("{noun} {value}"), 1.0)?;
        hash.update(&bytes);
        super::report::write_whole(&path, &bytes)?;
        files.push(path);
        badges.insert(label.clone(), json!(format!("./{name}")));
    }
    let digest: String = hash.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
    Ok((json!({ "url": format!("./{name}"), "alt": card.alt, "badges": badges, "digest": digest }), files))
}
