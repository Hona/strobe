//! The card of an implementation comparison (SPEC §11.2): the headline journey large on the left
//! (the case marked `hero`, else the biggest win the scoreboard counts), every journey on the right
//! as a forest plot on one log-ratio axis (its interval and change, regressions in red and never
//! left out, gaps as grey words), the scoreboard under it, and two lines on what was held the same.
//! One journey: the headline across the whole card.

use super::*;
use crate::app::family;
use crate::app::pr;

const HEAD_I: f32 = 56.0;
const FOOT_I: f32 = 80.0;
const LEFT: f32 = 40.0;
const SPLIT: f32 = 580.0;
const RIGHT: f32 = 608.0;
const RIGHT_END: f32 = 1160.0;

/// One journey on the forest plot.
struct Row {
    name: String,
    /// The primary in words when the journeys do not share one.
    metric: String,
    /// The interval and estimate as log ratios (or differences against the median base, for a
    /// difference comparison), or why there is none.
    ci: Option<(f64, f64, f64)>,
    note: String,
    value: String,
    tone: Tone,
    regression: bool,
    hero: bool,
    /// Higher is better for its metric: plotted negated, so better is left on every row.
    flipped: bool,
}

/// The headline: a journey's main metric, large.
struct Lead {
    journey: String,
    tile: Tile,
    /// `less CPU time than Original`.
    words: String,
    scope: String,
    /// `Original 12.8 ms`, `Rewrite 6.99 ms`.
    medians: (String, String),
    tally: String,
    same: String,
}

pub(super) struct Forest {
    id: String,
    subject: String,
    lead: Option<Lead>,
    calm: (String, String),
    calm_pairs: Option<Lead>,
    rows: Vec<Row>,
    hidden: usize,
    band: f64,
    journeys: usize,
    score: (String, String),
    foot: (String, String),
    pub(super) alt: String,
    pub(super) badges: Vec<(String, String, String, Tone)>,
    pub(super) scoreboard_badge: (String, String, Tone),
    pub(super) headline: (String, String, Tone),
}

/// A change in log-ratio terms, for the shared axis: `ln(1 + x)` of a relative change.
fn log_of(c: &Value, x: f64) -> f64 {
    if c["relative"] == true {
        (1.0 + x).max(1e-9).ln()
    } else {
        let b = f(&c["base"]).abs().max(1e-9);
        (1.0 + x / b).max(1e-9).ln()
    }
}

/// The card's side names: the full ones in the header, these in the body.
fn names(r: &Value) -> (String, String) {
    let (bn, hn) = pr::side_names(r);
    pr::compact_names(&bn, &hn)
}

fn lead_of(r: &Value, s: &Value, multi: bool) -> Lead {
    let (bn, hn) = names(r);
    let c = &s["primary"];
    let mut tile = tile(s, c, String::new());
    let cl = pr::claim(c);
    tile.amount = cl.amount.clone();
    let m = metric(c);
    let unit = unit_of(m);
    let noun = capitalize(&short(m).0);
    // The footer says what CPU summed: a CPU time says only what it is per.
    let scope = match c["care_note"].as_str() {
        Some(_) if m.starts_with("cpu.") && unit == "ms" && !pr::cpu_scope(r, (&bn, &hn)).is_empty() => format!("{noun} per {}", if s["kind"] == "launch" { "launch" } else { "run" }),
        Some(n) => format!("{noun}: {n}"),
        None => String::new(),
    };
    let range = if c["relative"] == true { pr::range_x(c).replace('\u{a0}', " ") } else { String::new() };
    let tally = fmt_join([tile.tally.clone(), range]);
    let checks: Vec<String> = s["equivalence"].as_array().into_iter().flatten().filter(|e| e["ok"] == true).filter_map(|e| e["selector"].as_str().map(str::to_string)).collect();
    let same = if checks.is_empty() { String::new() } else { format!("same {} on both after every run", checks.join(", ")) };
    Lead {
        journey: if multi { title(s, &[]) } else { String::new() },
        words: format!("{} than {bn}", cl.card()),
        scope,
        medians: (format!("{bn} {}", disp(f(&c["base"]), unit)), format!("{hn} {}", disp(f(&c["head"]), unit))),
        tally,
        same,
        tile,
    }
}

fn fmt_join<const N: usize>(parts: [String; N]) -> String {
    super::super::fmt::fields(parts)
}

pub(super) fn build(r: &Value) -> Forest {
    let (bn, hn) = pr::side_names(r);
    let (cb, ch) = names(r);
    let series: Vec<&Value> = r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let board = family::Board::of(r);
    let fam = &board.family;
    let hero = board.hero;
    let multi = series.len() > 1;
    let metrics: Vec<&str> = series.iter().filter(|s| !family::not_compared(s)).map(|s| metric(&s["primary"])).collect();
    let mixed = metrics.windows(2).any(|w| w[0] != w[1]);
    let mut rows: Vec<Row> = Vec::new();
    for (i, s) in series.iter().enumerate() {
        let c = &s["primary"];
        let name = title(s, &[]);
        let gap = family::not_compared(s);
        let o = primary_out(s, false);
        // Green and red only for a verdict confirmed across the family; one on its own stays grey.
        let change = if c["relative"] == true { pr::factor_or_pct(f(&c["estimate"])) } else { tile(s, c, String::new()).delta };
        let (tone, value) = if gap {
            (Tone::Plain, if s["kind"] == "refused" { "not run" } else { "gap" }.to_string())
        } else {
            match o {
                Out::Better | Out::Worse if board.alone(i) => (Tone::Plain, change),
                Out::Better => (Tone::Ok, change),
                Out::Worse => (Tone::Err, change),
                Out::Same => (Tone::Plain, "same".into()),
                Out::NotRun => (Tone::Plain, "not run".into()),
                Out::Refused => (Tone::Plain, "refused".into()),
                _ => (Tone::Plain, "unclear".into()),
            }
        };
        // Better always to the left: a metric where higher is better plots its log ratio negated.
        let way = if lower_better(metric(c)) { 1.0 } else { -1.0 };
        let ci = (!gap && !matches!(o, Out::NotRun | Out::Refused))
            .then(|| {
                let (a, e, b) = (way * log_of(c, f(&c["lo"])), way * log_of(c, f(&c["estimate"])), way * log_of(c, f(&c["hi"])));
                (a.min(b), e, a.max(b))
            })
            .filter(|(a, b, d)| a.is_finite() && b.is_finite() && d.is_finite());
        let note = if s["kind"] == "refused" {
            format!("not run: {}", s["primary"]["refusal"]["validator"].as_str().unwrap_or(""))
        } else if gap {
            let g = &s["gap"];
            match (g["missing"].as_str(), g["reason"].as_str().filter(|x| !x.is_empty())) {
                (Some(missing), _) => format!("not comparable: no {missing}"),
                (None, Some(why)) => format!("not comparable: {why}"),
                _ => "not comparable".into(),
            }
        } else if ci.is_none() {
            match o {
                Out::NotRun => "not run".into(),
                _ => "not measured".into(),
            }
        } else {
            String::new()
        };
        rows.push(Row {
            name,
            metric: if mixed && !gap { pr::ready_noun(s).unwrap_or_else(|| short(metric(c)).0) } else { String::new() },
            ci,
            note,
            value,
            tone,
            regression: o == Out::Worse && !gap,
            hero: hero == Some(i),
            flipped: way < 0.0,
        });
    }
    // Every journey while rows of 24 px fit; beyond that every regression and the headline stay, then
    // the file's order, and the card says how many are left out.
    let cap = ((H - HEAD_I - FOOT_I - 150.0) / 24.0).floor() as usize;
    let mut hidden = 0;
    if rows.len() > cap {
        let must = rows.iter().filter(|r| r.regression || r.hero).count();
        let mut room = (cap - 1).saturating_sub(must);
        let keep: Vec<bool> = rows
            .iter()
            .map(|r| {
                if r.regression || r.hero {
                    true
                } else if room > 0 {
                    room -= 1;
                    true
                } else {
                    false
                }
            })
            .collect();
        hidden = keep.iter().filter(|k| !**k).count();
        rows = rows.into_iter().zip(keep).filter(|(_, k)| *k).map(|(r, _)| r).collect();
    }
    // Without a confirmed win the biggest confirmed regression leads, in red.
    let lead = hero.or(board.worst).map(|i| lead_of(r, series[i], multi));
    let calm = if lead.is_some() {
        (String::new(), String::new())
    } else if series.iter().all(|s| family::not_compared(s)) {
        ("Not comparable".into(), series.first().map(|s| format!("{}: {}", title(s, &[]), family::not_compared_reason(r, s))).unwrap_or_default())
    } else if series.iter().filter(|s| !family::not_compared(s)).all(|s| primary_out(s, false) == Out::Same) {
        (format!("No measurable difference from {cb}"), format!("every compared journey within its bound in {}", pr_pairs(&series)))
    } else {
        // Only a count past the pairs already run: "~6 pairs" after 8 reads as a contradiction.
        let need = series.iter().filter_map(|s| s["primary"]["needed_pairs"].as_u64().filter(|n| *n <= 1000 && *n > s["pairs"].as_u64().unwrap_or(0))).max();
        ("Not decided yet".into(), need.map(|n| format!("needs ~{n} pairs")).unwrap_or_default())
    };
    // Under a calm heading, the first undecided journey's pairs (else the first compared one's).
    let calm_pairs = if lead.is_some() {
        None
    } else {
        let compared: Vec<&&Value> = series.iter().filter(|s| !family::not_compared(s) && s["not_run"] != true).collect();
        compared.iter().find(|s| primary_out(s, false) == Out::Unclear).or(compared.first()).map(|s| lead_of(r, s, multi)).filter(|l| !l.tile.dots.is_empty())
    };
    let (head, rest) = board.counts();
    let mut score_parts = vec![head];
    score_parts.extend(rest);
    score_parts.extend(family::not_compared_counts(r));
    let score2 = capitalize(&board.correction());
    let (f1, f2) = pr::fairness(r, &series, (&cb, &ch));
    let foot = (super::super::fmt::fields(f1), super::super::fmt::fields(f2));
    let file = r["file_repo"].as_str().or(r["file"].as_str()).unwrap_or("").replace('\\', "/");
    let subject = super::super::fmt::fields([format!("{hn} vs {bn}"), file]);
    let lead_alt = match (&lead, hero) {
        (Some(_), Some(i)) => format!("{hn} {} than {bn} in {}", pr::claim(&series[i]["primary"]).pr(false), lower_first(&title(series[i], &[]))),
        (Some(l), None) => format!("{hn}: {} {}", l.tile.amount, l.words),
        _ => calm_alt(&calm),
    };
    let alt = format!("{lead_alt}; {}.", lower_first(&score_parts.join(", ")));
    // The equivalence bound the rows share: the first compared journey's (±2% by default).
    let band = series.iter().find(|s| !family::not_compared(s) && s["primary"]["relative"] == true).and_then(|s| s["primary"]["bound"].as_f64()).filter(|b| b.is_finite() && *b >= 0.0).unwrap_or(0.02);
    let badges: Vec<(String, String, String, Tone)> = series.iter().map(|s| badge_of(s, false)).collect();
    let sb_value = match fam.all_no_worse {
        Some(true) => format!("no worse in all {}", fam.compared),
        _ => {
            let mut v = format!("better in {}/{}", fam.better, fam.compared);
            if fam.worse > 0 {
                v += &format!(" · worse in {}", fam.worse);
            }
            for part in family::not_compared_counts(r) {
                v += &format!(" · {part}");
            }
            v
        }
    };
    // Red for a confirmed regression, green only with no regression and nothing unclear (an
    // unconfirmed regression is unclear: grey).
    let sb_tone = if fam.worse > 0 || fam.all_no_worse == Some(false) {
        Tone::Err
    } else if fam.unclear == 0 && fam.compared > 0 && (fam.better > 0 || fam.all_no_worse == Some(true)) {
        Tone::Ok
    } else {
        Tone::Plain
    };
    let scoreboard_badge = (format!("vs {bn}"), sb_value, sb_tone);
    // The headline badge: the headline journey's change, else the scoreboard.
    let headline = hero.and_then(|i| badges.get(i)).map(|b| (b.1.clone(), b.2.clone(), b.3)).unwrap_or_else(|| scoreboard_badge.clone());
    Forest {
        headline,
        id: r["id"].as_str().unwrap_or("").to_string(),
        subject,
        lead,
        calm,
        calm_pairs,
        rows,
        hidden,
        band,
        journeys: series.len(),
        score: (score_parts.join(" · "), score2),
        foot,
        alt,
        badges,
        scoreboard_badge,
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|x| x.to_lowercase().chain(c).collect()).unwrap_or_default()
}

fn calm_alt(calm: &(String, String)) -> String {
    if calm.1.is_empty() { calm.0.clone() } else { format!("{}: {}", calm.0, calm.1) }
}

fn pr_pairs(series: &[&Value]) -> String {
    let ns: Vec<u64> = series.iter().filter_map(|s| s["pairs"].as_u64()).filter(|n| *n > 0).collect();
    match (ns.iter().min(), ns.iter().max()) {
        (Some(a), Some(b)) if a == b => format!("{a} pairs"),
        (Some(a), Some(b)) => format!("{a}–{b} pairs"),
        _ => String::new(),
    }
}

fn chrome_i(svg: &mut Svg, card: &Forest) {
    svg.rect(0.0, 0.0, W, H, Ink::Bg);
    svg.frame(0.0, 0.0, W, H, Ink::Line);
    svg.line(0.0, HEAD_I - 0.5, W, HEAD_I - 0.5, Ink::Line);
    svg.line(0.0, H - FOOT_I + 0.5, W, H - FOOT_I + 0.5, Ink::Line);
    svg.rect(LEFT, HEAD_I / 2.0 - 7.0, 14.0, 14.0, Ink::Accent);
    svg.text(LEFT + 24.0, HEAD_I / 2.0 + 7.0, (20.0, true), Ink::Fg, "start", "strobe");
    let id_w = measure(&card.id, 18.0, false);
    svg.text(W - LEFT, HEAD_I / 2.0 + 6.0, (18.0, false), Ink::Muted, "end", &card.id);
    let left = LEFT + 24.0 + measure("strobe", 20.0, true) + 24.0;
    let room = W - LEFT - id_w - 28.0 - left;
    squeezed(svg, (left, HEAD_I / 2.0 + 7.0), (20.0, false), Ink::Fg, &card.subject, room, 0.8);
    let fw = W - 2.0 * LEFT;
    squeezed(svg, (LEFT, H - FOOT_I + 32.0), (17.0, false), Ink::Muted, &card.foot.0, fw, 0.75);
    squeezed(svg, (LEFT, H - FOOT_I + 58.0), (17.0, false), Ink::Muted, &card.foot.1, fw, 0.75);
}

/// `text` from `x` at `size`, or smaller down to `least` of it to fit `width`; cut with an ellipsis
/// only past that (`cut`), so a number stays whole wherever there is any room for it.
fn squeezed(svg: &mut Svg, (x, y): (f32, f32), (size, bold): (f32, bool), ink: Ink, text: &str, width: f32, least: f32) {
    let s = fit(text, width, size, bold).max((size * least).floor());
    svg.text(x, y, (s, bold), ink, "start", &cut(text, width, s, bold));
}

/// `text` cut to `width` with an ellipsis; a title's parameters at its end (`(rows 10000)`) stay
/// whole and the words before them are cut, while there is room for a few of them.
fn cut(text: &str, width: f32, size: f32, bold: bool) -> String {
    if measure(text, size, bold) <= width {
        return text.to_string();
    }
    if let Some((head, tail)) = text.strip_suffix(')').and_then(|t| t.rsplit_once(" (")) {
        let tail = format!("({tail})");
        let room = width - measure(&tail, size, bold) - size * 0.3;
        if room >= measure("Abc…", size, bold) {
            return format!("{} {tail}", clip(head, room, size, bold));
        }
    }
    clip(text, width, size, bold)
}

/// `Original 12.8 ms → Rewrite 6.99 ms` from `y`, a little smaller to fit, else on two lines (the
/// arrow ending the first); never cut. The last line's baseline.
fn medians_svg(svg: &mut Svg, (before, after): &(String, String), x: f32, y: f32, w: f32) -> f32 {
    let one = format!("{before} → {after}");
    let s = fit(&one, w, 20.0, false);
    if s >= 17.0 {
        svg.text(x, y, (s, false), Ink::Fg, "start", &one);
        return y;
    }
    let first = format!("{before} →");
    let s = fit(&first, w, 20.0, false).min(fit(after, w, 20.0, false));
    svg.text(x, y, (s, false), Ink::Fg, "start", &first);
    svg.text(x, y + s * 1.3, (s, false), Ink::Fg, "start", after);
    y + s * 1.3
}

/// The headline in a column `x`..`x + w`, from `top`; `wide` for a one-journey card.
fn lead_svg(svg: &mut Svg, l: &Lead, x: f32, top: f32, w: f32, wide: bool) {
    let mut y = top;
    if !l.journey.is_empty() {
        y += 34.0;
        squeezed(svg, (x, y), (20.0, false), Ink::Muted, &l.journey, w, 0.8);
    }
    let size = if wide { 168.0 } else { 120.0 };
    y += size * 0.86 + if l.journey.is_empty() { 16.0 } else { 6.0 };
    let big = fit(&l.tile.amount, w, size, true);
    svg.text(x - 4.0, y, (big, true), tone_ink(l.tile.tone), "start", &l.tile.amount);
    let (lines, ws) = wrap(&l.words, w, if wide { 44.0 } else { 30.0 }, false);
    y += big * 0.12;
    for line in &lines {
        y += ws * 1.25;
        svg.text(x, y, (ws, false), Ink::Fg, "start", line);
    }
    if !l.scope.is_empty() {
        let (lines, ss) = wrap(&l.scope, w, 17.0, false);
        y += if wide { 20.0 } else { 6.0 };
        for line in &lines {
            y += ss * 1.35;
            svg.text(x, y, (ss, false), Ink::Muted, "start", &clip(line, w, ss, false));
        }
    }
    y = medians_svg(svg, &l.medians, x, y + 36.0, w);
    y += 14.0;
    let sw = if wide { w * 0.62 } else { w };
    strip(svg, x, y, sw, &l.tile, 6.0, l.tile.tone != Tone::Plain);
    y += 36.0 + 40.0;
    let (tx, anchor, ty) = if wide { (x + w, "end", y - 46.0) } else { (x, "start", y) };
    svg.text(tx, ty, (17.0, false), Ink::Muted, anchor, &l.tally);
    if !l.same.is_empty() {
        svg.text(x, if wide { y } else { y + 26.0 }, (17.0, false), Ink::Muted, "start", &clip(&l.same, w, 17.0, false));
    }
}

/// Without a headline: what happened, and the first compared journey's pairs in grey (no single pair
/// decides anything).
fn calm_svg(svg: &mut Svg, card: &Forest, x: f32, top: f32, w: f32) {
    let calm = &card.calm;
    let (lines, hs) = wrap(&calm.0, w, 56.0, true);
    let mut y = top + 110.0;
    for line in &lines {
        svg.text(x - 2.0, y, (hs, true), Ink::Fg, "start", line);
        y += hs * 1.2;
    }
    if !calm.1.is_empty() {
        let (lines, ds) = wrap(&calm.1, w, 24.0, false);
        y += 8.0;
        for line in &lines {
            svg.text(x, y, (ds, false), Ink::Muted, "start", line);
            y += ds * 1.3;
        }
    }
    if let Some(l) = &card.calm_pairs {
        y += 30.0;
        if !l.journey.is_empty() {
            squeezed(svg, (x, y), (17.0, false), Ink::Muted, &l.journey, w, 0.8);
            y += 28.0;
        }
        y = medians_svg(svg, &l.medians, x, y, w) + 14.0;
        let sw = if card.journeys <= 1 { w * 0.62 } else { w };
        strip(svg, x, y, sw, &l.tile, 6.0, false);
        y += 36.0 + 40.0;
        svg.text(x, y, (17.0, false), Ink::Muted, "start", &format!("each dot is one pair · {}", l.tally));
    }
}

/// A small filled triangle pointing left or right, for the axis' direction words.
fn arrow(svg: &mut Svg, x: f32, y: f32, left: bool, ink: Ink) {
    let d = if left { format!("M{x} {y}l7 -4.5v9z") } else { format!("M{x} {y}l-7 -4.5v9z") };
    svg.out += &format!(r#"<path d="{d}" fill="{}"/>"#, paint(ink, svg.theme));
}

/// The forest plot: every kept journey on one log-ratio axis, the bound shaded, the scoreboard under it.
fn forest_svg(svg: &mut Svg, card: &Forest, top: f32, bottom: f32) {
    let (x0, x1) = (RIGHT, RIGHT_END);
    let head_y = top + 34.0;
    svg.text(x0, head_y, (20.0, true), Ink::Fg, "start", &format!("{} journey{}", card.journeys, if card.journeys == 1 { "" } else { "s" }));
    let rows = &card.rows;
    let score_h = if card.score.1.is_empty() { 34.0 } else { 58.0 };
    let more_h = if card.hidden > 0 { 26.0 } else { 0.0 };
    let tick_h = 26.0;
    let rows_top = head_y + 16.0;
    let avail = bottom - 14.0 - score_h - more_h - tick_h - rows_top;
    // Rows share the pane's height up to 112 px each; past 42 px everything in them grows with it
    // (`g`, 0 to 1), so a few journeys fill the pane rather than sit at its top.
    let rh = (avail / rows.len().max(1) as f32).clamp(24.0, 112.0);
    let g = ((rh - 42.0) / 70.0).clamp(0.0, 1.0);
    let name_w = 196.0 + 40.0 * g;
    let value_w = 76.0 + 24.0 * g;
    let (ax0, ax1) = (x0 + name_w + 12.0, x1 - value_w);
    // One axis for every row, wide enough for every interval (a regression is never cut off), with
    // room on both sides of no change.
    // `--equivalence 0` draws no band; 100% or more one that stops short of minus infinity.
    let band = ((1.0 - card.band).max(1e-3).ln(), (1.0 + card.band).ln());
    // At least ±5% wide, so a band of 0 and rows that all read no change still make an axis.
    let (mut lo, mut hi) = rows.iter().filter_map(|r| r.ci).fold(((band.0 * 3.0).min(-0.05), (band.1 * 3.0).max(0.05)), |(l, h), (a, _, b)| (l.min(a), h.max(b)));
    let span = hi - lo;
    lo = lo.min(-0.16 * span);
    hi = hi.max(0.16 * span);
    let pad = (hi - lo) * 0.05;
    let (lo, hi) = (lo - pad, hi + pad);
    let px = |v: f64| ax0 + ((v - lo) / (hi - lo)) as f32 * (ax1 - ax0);
    let zero = px(0.0).round() + 0.5;
    // Direction words over the axis, either side of no change, kept clear of the title and the edge.
    let (wb, ww) = (measure("better", 15.0, false), measure("worse", 15.0, false));
    let title_end = x0 + measure(&format!("{} journeys", card.journeys), 20.0, true) + 24.0;
    let mid = zero.max(title_end + wb + 34.0).min(x1 - ww - 34.0);
    svg.text(mid - 12.0, head_y - 1.0, (15.0, false), Ink::Muted, "end", "better");
    arrow(svg, mid - 12.0 - wb - 12.0, head_y - 5.5, true, Ink::Muted);
    svg.text(mid + 12.0, head_y - 1.0, (15.0, false), Ink::Muted, "start", "worse");
    arrow(svg, mid + 12.0 + ww + 12.0, head_y - 5.5, false, Ink::Muted);
    let rows_bottom = rows_top + rh * rows.len() as f32;
    // The bound, shaded across the rows.
    let (b0, b1) = (px(band.0), px(band.1));
    svg.rect(b0, rows_top, (b1 - b0).max(1.0), rows_bottom - rows_top, Ink::Band);
    svg.line(zero, rows_top, zero, rows_bottom, Ink::Muted);
    let name_size = if rh >= 34.0 { 17.0 + 7.0 * g } else { 15.0 };
    let (sub_size, note_size, tick_size) = (12.0 + 4.0 * g, 14.0 + 3.0 * g, 13.0 + 2.0 * g);
    let (dot, whisker, bar) = (5.5 + 2.5 * g, 6.0 + 3.0 * g, 2.0 + g);
    for (i, row) in rows.iter().enumerate() {
        let cy = rows_top + rh * (i as f32 + 0.5);
        if i > 0 {
            svg.line(x0, (rows_top + rh * i as f32).round() + 0.5, x1, (rows_top + rh * i as f32).round() + 0.5, Ink::Line);
        }
        let two = !row.metric.is_empty() && rh >= 36.0;
        let k = name_size / 17.0;
        let ny = if two { cy - 2.0 * k } else { cy + name_size * 0.35 };
        squeezed(svg, (x0, ny), (name_size, row.hero), Ink::Fg, &row.name, name_w, 0.8);
        if two {
            squeezed(svg, (x0, cy + 14.0 * k), (sub_size, false), Ink::Muted, &row.metric, name_w, 0.8);
        }
        let ink = tone_ink(row.tone);
        let ink = if ink == Ink::Fg { Ink::Plain } else { ink };
        match row.ci {
            Some((lo, est, hi)) => {
                let (a, b) = (px(lo), px(hi));
                svg.rect(a, cy - bar / 2.0, (b - a).max(1.0), bar, ink);
                svg.rect(a - 0.5, cy - whisker, 1.5, 2.0 * whisker, ink);
                svg.rect(b - 1.0, cy - whisker, 1.5, 2.0 * whisker, ink);
                svg.dot(px(est), cy, dot, ink);
            }
            None => {
                // On the background, so no change's line does not run through the words.
                let nw = measure(&row.note, note_size, false).min(ax1 - ax0) + 12.0;
                svg.rect(ax0 - 6.0, cy - note_size * 0.7, nw, note_size * 1.4, Ink::Bg);
                squeezed(svg, (ax0, cy + 5.0 * note_size / 14.0), (note_size, false), Ink::Muted, &row.note, ax1 - ax0, 0.8);
            }
        }
        let vink = match row.tone {
            Tone::Plain => Ink::Muted,
            t => tone_ink(t),
        };
        svg.text(x1, cy + name_size * 0.35, (name_size, true), vink, "end", &row.value);
    }
    // Ticks: no change, and up to two round changes either side that the axis reaches.
    let mut y = rows_bottom + 18.0 + 2.0 * g;
    svg.text(zero, y, (tick_size, false), Ink::Muted, "middle", "0");
    // Changes in the metric's own direction label the axis only when every row reads it that way
    // (lower is better); a row plotted negated would read them backwards.
    let marks: [(f64, &str); 12] =
        [(-0.9, "−90%"), (-0.75, "−75%"), (-0.5, "−50%"), (-0.25, "−25%"), (-0.1, "−10%"), (0.1, "+10%"), (0.25, "+25%"), (0.5, "+50%"), (1.0, "2×"), (2.0, "3×"), (4.0, "5×"), (9.0, "10×")];
    let labelled = !rows.iter().any(|r| r.flipped && r.ci.is_some());
    let inside: Vec<(f64, &str)> = marks.iter().copied().filter(|(m, _)| labelled && (1.0 + m).ln() > lo + pad && (1.0 + m).ln() < hi - pad).collect();
    // The outermost two each side of no change.
    let mut ticks: Vec<(f64, String)> = inside.iter().filter(|(m, _)| *m < 0.0).take(2).map(|(m, l)| (*m, l.to_string())).collect();
    ticks.extend(inside.iter().rev().filter(|(m, _)| *m > 0.0).take(2).map(|(m, l)| (*m, l.to_string())));
    let mut last_x: Vec<f32> = vec![zero];
    for (m, label) in ticks {
        let tx = px((1.0 + m).ln());
        if last_x.iter().any(|lx| (lx - tx).abs() < 40.0) {
            continue;
        }
        last_x.push(tx);
        svg.line(tx.round() + 0.5, rows_bottom, tx.round() + 0.5, rows_bottom + 5.0, Ink::Muted);
        svg.text(tx, y, (tick_size, false), Ink::Muted, "middle", &label);
    }
    if card.hidden > 0 {
        y += 24.0;
        svg.text(x0, y, (15.0, false), Ink::Muted, "start", &format!("and {} more in the report", card.hidden));
    }
    let sy = bottom - 14.0 - if card.score.1.is_empty() { 0.0 } else { 24.0 };
    squeezed(svg, (x0, sy), (18.0, true), Ink::Fg, &card.score.0, x1 - x0, 0.7);
    if !card.score.1.is_empty() {
        squeezed(svg, (x0, sy + 24.0), (15.0, false), Ink::Muted, &card.score.1, x1 - x0, 0.75);
    }
}

/// The card's source SVG.
pub(super) fn svg(card: &Forest, theme: Theme) -> String {
    let mut svg = Svg::new(W, H, theme);
    chrome_i(&mut svg, card);
    let (top, bottom) = (HEAD_I, H - FOOT_I);
    if card.journeys <= 1 {
        let w = W - 2.0 * LEFT;
        match &card.lead {
            Some(l) => lead_svg(&mut svg, l, LEFT, top + 8.0, w, true),
            None => calm_svg(&mut svg, card, LEFT, top, w),
        }
        return svg.end();
    }
    let lw = SPLIT - 28.0 - LEFT;
    match &card.lead {
        Some(l) => lead_svg(&mut svg, l, LEFT, top, lw, false),
        None => calm_svg(&mut svg, card, LEFT, top, lw),
    }
    svg.line(SPLIT + 0.5, top + 28.0, SPLIT + 0.5, bottom - 28.0, Ink::Line);
    forest_svg(&mut svg, card, top, bottom);
    svg.end()
}
