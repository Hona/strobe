//! A port's progress as a card and a badge (`report history <file|ser_…> --format card|badge`, SPEC
//! §25.7), in the implementation card's visual system: on the left the scoreboard at the latest tracked
//! commit, large, with how it started; on the right every journey's points over the head's commits
//! (`ln(head / base)` around no change, the bound shaded, gaps as grey marks, confirmed changes and
//! comparator updates marked); two lines on how the points were measured.

use super::*;
use crate::app::history::port::{self, Board, Port, State};

const HEAD_I: f32 = 56.0;
const FOOT_I: f32 = 80.0;
const LEFT: f32 = 40.0;
const SPLIT: f32 = 580.0;
const RIGHT: f32 = 608.0;
const RIGHT_END: f32 = 1160.0;

/// Tracked points are evidence, not verdicts: green and red are only for changes a paired A/B
/// confirmed. The tone of a port: red when some journey's latest confirmed change is a regression,
/// green when none is and some journey's is an improvement, else grey.
fn confirmed_tone(p: &Port) -> Tone {
    let latest: Vec<&Value> = p.journeys.iter().filter_map(|j| j.changes.iter().rfind(|c| c["state"] == "confirmed")).collect();
    if latest.iter().any(|c| c["direction"] == "regression") {
        Tone::Err
    } else if latest.iter().any(|c| c["direction"] == "improvement") {
        Tone::Ok
    } else {
        Tone::Plain
    }
}

/// A confirmed change as the paired A/B measured it: `+20%`, or `+4.1 ms` for one compared by differences.
fn confirmed_change(c: &Value, unit: &str) -> String {
    let est = c["confirm"]["estimate"].as_f64().unwrap_or(f64::NAN);
    if c["confirm"]["relative"] == true { crate::app::fmt::pct(est) } else { crate::app::fmt::signed(est, unit) }
}

/// The board at the first commit every journey had a point at, for "where it started".
fn first_board(p: &Port) -> Option<(Board, &Value)> {
    let i = (0..p.commits.len()).find(|&i| p.journeys.iter().any(|j| j.cells[i].is_some()))?;
    let cut = Port {
        file: String::new(),
        base: String::new(),
        head: String::new(),
        device: String::new(),
        bound: p.bound,
        pairs: p.pairs,
        confidence: p.confidence,
        commits: p.commits[..=i].to_vec(),
        journeys: p
            .journeys
            .iter()
            .map(|j| port::Journey {
                series: String::new(),
                label: String::new(),
                metric: String::new(),
                unit: String::new(),
                words: j.words.clone(),
                higher: j.higher,
                cells: j.cells[..=i].to_vec(),
                changes: Vec::new(),
            })
            .collect(),
        epochs: Vec::new(),
        parity: Vec::new(),
        hidden: 0,
        fingerprint: Value::Null,
    };
    Some((Board::latest(&cut), &p.commits[i]))
}

fn svg(p: &Port, theme: Theme) -> String {
    let mut svg = Svg::new(W, H, theme);
    let board = Board::latest(p);
    // Chrome: the header names the port, the device on the right.
    svg.rect(0.0, 0.0, W, H, Ink::Bg);
    svg.frame(0.0, 0.0, W, H, Ink::Line);
    svg.line(0.0, HEAD_I - 0.5, W, HEAD_I - 0.5, Ink::Line);
    svg.line(0.0, H - FOOT_I + 0.5, W, H - FOOT_I + 0.5, Ink::Line);
    svg.rect(LEFT, HEAD_I / 2.0 - 7.0, 14.0, 14.0, Ink::Accent);
    svg.text(LEFT + 24.0, HEAD_I / 2.0 + 7.0, (20.0, true), Ink::Fg, "start", "strobe");
    let right = format!("history · {}", p.device);
    let rw = measure(&right, 18.0, false);
    svg.text(W - LEFT, HEAD_I / 2.0 + 6.0, (18.0, false), Ink::Muted, "end", &right);
    let left = LEFT + 24.0 + measure("strobe", 20.0, true) + 24.0;
    let subject = crate::app::fmt::fields([format!("{} vs {}", p.head, p.base), p.file.clone()]);
    svg.text(left, HEAD_I / 2.0 + 7.0, (20.0, false), Ink::Fg, "start", &clip(&subject, W - LEFT - rw - 28.0 - left, 20.0, false));
    // Footer: how a point is measured, and the comparator's story.
    let f1 = crate::app::fmt::fields([
        p.device.clone(),
        short_os(p.fingerprint["machine"]["os"].as_str().unwrap_or("")),
        short_cpu(p.fingerprint["machine"]["cpu"].as_str().unwrap_or("")),
        format!("tracked: {} pairs per commit, {} intervals", p.pairs, port::confidence_text(p.confidence)),
        format!("red and green: confirmed by an A/B of {} alone", p.head),
    ]);
    let f2 = match p.epochs.last() {
        Some(e) => format!("{} at {}", crate::app::track::comparator_updated(e), e["short"].as_str().unwrap_or("").chars().take(7).collect::<String>()),
        None => format!("{} the same at every point · a word only when the whole interval lies past ±{:.0}%", p.base, p.bound * 100.0),
    };
    let fw = W - 2.0 * LEFT;
    svg.text(LEFT, H - FOOT_I + 32.0, (17.0, false), Ink::Muted, "start", &clip(&f1, fw, 17.0, false));
    svg.text(LEFT, H - FOOT_I + 58.0, (17.0, false), Ink::Muted, "start", &clip(&f2, fw, 17.0, false));
    left_pane(&mut svg, p, &board, LEFT, HEAD_I, SPLIT - 28.0 - LEFT);
    svg.line(SPLIT + 0.5, HEAD_I + 28.0, SPLIT + 0.5, H - FOOT_I - 28.0, Ink::Line);
    rows(&mut svg, p, &board, HEAD_I, H - FOOT_I);
    svg.end()
}

/// The scoreboard at the latest commit, large; where it started; what changed on the way.
fn left_pane(svg: &mut Svg, p: &Port, b: &Board, x: f32, top: f32, w: f32) {
    let mut y = top + 34.0;
    let at =
        b.at.as_ref()
            .map(|c| format!("tracked, {} · at {} {}", port::tracked(p), c["short"].as_str().unwrap_or("").chars().take(7).collect::<String>(), c["subject"].as_str().unwrap_or("")))
            .unwrap_or_default();
    svg.text(x, y, (18.0, false), Ink::Muted, "start", &clip(&at, w, 18.0, false));
    let (big, words) =
        if b.compared == 0 { ("—".to_string(), format!("no journey compared with {} yet", p.base)) } else { (format!("{} of {}", b.better, b.compared), format!("journeys better than {}", p.base)) };
    let size = 120.0;
    y += size * 0.86 + 6.0;
    let bs = fit(&big, w, size, true);
    // The tracked count stays neutral: no point decides anything.
    svg.text(x - 4.0, y, (bs, true), Ink::Fg, "start", &big);
    let (lines, ws) = wrap(&words, w, 30.0, false);
    y += bs * 0.12;
    for l in &lines {
        y += ws * 1.25;
        svg.text(x, y, (ws, false), Ink::Fg, "start", l);
    }
    let rest = b.rest();
    if !rest.is_empty() {
        y += 34.0;
        svg.text(x, y, (20.0, false), Ink::Fg, "start", &clip(&rest.join(" · "), w, 20.0, false));
    }
    y += 40.0;
    let mut notes: Vec<String> = Vec::new();
    if let Some((first, c)) = first_board(p).filter(|(_, c)| b.at.as_ref().is_some_and(|a| a["commit"] != c["commit"])) {
        let gaps = if first.gaps > 0 { format!(" · {} gap{}", first.gaps, if first.gaps == 1 { "" } else { "s" }) } else { String::new() };
        notes.push(format!(
            "from {} ({}): better in {} of {}{gaps}",
            c["short"].as_str().unwrap_or("").chars().take(7).collect::<String>(),
            c["date"].as_str().unwrap_or(""),
            first.better,
            first.compared
        ));
    }
    notes.push(format!("{} commit{} tracked", p.commits.len(), if p.commits.len() == 1 { "" } else { "s" }));
    let confirmed: Vec<String> = p
        .journeys
        .iter()
        .flat_map(|j| j.changes.iter().filter(|c| c["state"] == "confirmed").map(move |c| format!("{} {} in {}", c["direction"].as_str().unwrap_or(""), confirmed_change(c, &j.unit), lower(&j.label))))
        .collect();
    if !confirmed.is_empty() {
        notes.push(format!("confirmed: {}", confirmed.join(", ")));
    }
    // A journey compared since a commit, while it still is (its latest point is not a gap).
    let still = |label: &Value| p.journeys.iter().any(|j| *label == j.label.as_str() && j.cells.iter().rev().flatten().next().is_some_and(|c| c.state != State::Gap));
    for m in p.parity.iter().filter(|m| m["to"] == "compared" && still(&m["label"])) {
        notes.push(format!("{} compared since {}", m["label"].as_str().unwrap_or(""), m["short"].as_str().unwrap_or("").chars().take(7).collect::<String>()));
    }
    for n in notes.iter().take(5) {
        let (lines, s) = wrap(n, w, 17.0, false);
        for l in &lines {
            svg.text(x, y, (s, false), Ink::Muted, "start", &clip(l, w, s, false));
            y += s * 1.35;
        }
        y += 4.0;
    }
}

fn lower(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|x| x.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// Every journey's points over the commits: one row each, no change across the middle, the bound
/// shaded, better below it.
fn rows(svg: &mut Svg, p: &Port, b: &Board, top: f32, bottom: f32) {
    let (x0, x1) = (RIGHT, RIGHT_END);
    let name_w = 170.0;
    let value_w = 84.0;
    let (ax0, ax1) = (x0 + name_w + 12.0, x1 - value_w - 8.0);
    let head_y = top + 34.0;
    svg.text(x0, head_y, (20.0, true), Ink::Fg, "start", &format!("{} journey{} · {} commits", p.journeys.len(), if p.journeys.len() == 1 { "" } else { "s" }, p.commits.len()));
    svg.text(x1, head_y - 1.0, (15.0, false), Ink::Muted, "end", "below the line: better");
    let n = p.commits.len().max(1);
    let px = |i: usize| if n == 1 { (ax0 + ax1) / 2.0 } else { ax0 + 6.0 + i as f32 * (ax1 - ax0 - 12.0) / (n - 1) as f32 };
    let foot_h = 66.0;
    let rows_top = head_y + 18.0;
    let max_rows = (((bottom - foot_h - rows_top) / 34.0).floor() as usize).max(1);
    // Every regression stays; then the file's order.
    let mut shown: Vec<usize> = (0..p.journeys.len()).collect();
    let mut hidden = 0;
    if shown.len() > max_rows {
        let bad = |j: usize| p.journeys[j].cells.iter().rev().flatten().next().is_some_and(|c| c.state == State::Worse);
        let mut keep: Vec<usize> = shown.iter().copied().filter(|j| bad(*j)).take(max_rows - 1).collect();
        for j in 0..p.journeys.len() {
            if keep.len() >= max_rows - 1 {
                break;
            }
            if !keep.contains(&j) {
                keep.push(j);
            }
        }
        keep.sort_unstable();
        hidden = shown.len() - keep.len();
        shown = keep;
    }
    let rh = ((bottom - foot_h - 24.0 - rows_top) / shown.len().max(1) as f32).clamp(34.0, 120.0);
    let rows_bottom = rows_top + rh * shown.len() as f32;
    let b_ln = ((1.0 - p.bound).max(1e-3).ln(), (1.0 + p.bound).ln());
    // Comparator updates: a dashed line through every row between the commits it fell between.
    for (i, _) in p.epoch_at() {
        if i == 0 {
            continue;
        }
        let ex = ((px(i - 1) + px(i)) / 2.0).round() + 0.5;
        svg.out += &format!(r#"<path d="M{ex} {}L{ex} {}" fill="none" stroke="{}" stroke-width="1" stroke-dasharray="3 3"/>"#, rows_top, rows_bottom, paint(Ink::Muted, svg.theme));
        let anchor = if ex > (ax0 + ax1) / 2.0 { "end" } else { "start" };
        svg.text(if anchor == "end" { ex - 4.0 } else { ex + 4.0 }, rows_bottom + 32.0, (13.0, false), Ink::Muted, anchor, "comparator updated");
    }
    for (k, &ji) in shown.iter().enumerate() {
        let j = &p.journeys[ji];
        let y0 = rows_top + rh * k as f32;
        let cy = y0 + rh / 2.0;
        if k > 0 {
            svg.line(x0, y0.round() + 0.5, x1, y0.round() + 0.5, Ink::Line);
        }
        // The value at the right is the newest commit's, as the scoreboard counts it.
        let last = b.at.as_ref().and_then(|a| p.commits.iter().position(|c| c["commit"] == a["commit"])).and_then(|i| j.cells[i].as_ref());
        svg.text(x0, cy + 5.5, (16.0, false), Ink::Fg, "start", &clip(&j.label, name_w, 16.0, false));
        // The row's own scale: from its lowest point to its highest, no change and the bound always
        // inside it, so a row whose points all sit far from no change still shows how they move;
        // plotted so that better (lower, or higher for a metric where higher is better) is below.
        let way = if j.higher { -1.0 } else { 1.0 };
        let vals: Vec<f64> = j.cells.iter().flatten().filter(|c| c.est.is_finite()).map(|c| way * c.est).collect();
        let lo = vals.iter().copied().fold(b_ln.0.min(-b_ln.1), f64::min);
        let hi = vals.iter().copied().fold(b_ln.1, f64::max);
        let pad = ((hi - lo) * 0.08).max(0.005);
        let (lo, hi) = (lo - pad, hi + pad);
        let (ry0, ry1) = (y0 + 10.0, y0 + rh - 8.0);
        let py = |v: f64| ry1 - ((way * v - lo) / (hi - lo)) as f32 * (ry1 - ry0);
        let (ya, yb) = (py(b_ln.1), py(b_ln.0));
        svg.rect(ax0, ya.min(yb), ax1 - ax0, (ya - yb).abs().max(1.0), Ink::Band);
        let zero = py(0.0).round() + 0.5;
        svg.line(ax0, zero, ax1, zero, Ink::Muted);
        // A thin line through the compared points, then each point.
        let pts: Vec<(f32, f32)> = j.cells.iter().enumerate().filter_map(|(i, c)| c.as_ref().filter(|c| c.est.is_finite()).map(|c| (px(i), py(c.est)))).collect();
        if pts.len() > 1 {
            let d: String = pts.iter().enumerate().map(|(i, (x, y))| format!("{}{x:.1} {y:.1}", if i == 0 { "M" } else { "L" })).collect();
            svg.out += &format!(r#"<path d="{d}" fill="none" stroke="{}" stroke-width="1"/>"#, paint(Ink::Line, svg.theme));
        }
        for (i, c) in j.cells.iter().enumerate() {
            let Some(c) = c else { continue };
            match c.state {
                State::Gap => svg.rect(px(i) - 4.0, zero - 1.5, 8.0, 2.0, Ink::Plain),
                State::NoValue => svg.text(px(i), zero + 4.0, (12.0, false), Ink::Plain, "middle", "×"),
                _ => svg.dot(px(i), py(c.est), 3.5, Ink::Plain),
            }
        }
        // Confirmed changes at the commit after them.
        for ch in j.changes.iter().filter(|c| c["state"] == "confirmed") {
            if let Some(i) = p.commits.iter().position(|c| c["commit"] == ch["bad"]) {
                let ink = if ch["direction"] == "regression" { Ink::Err } else { Ink::Ok };
                let (tx, ty) = (px(i), y0 + 3.0);
                svg.out += &format!(r#"<path d="M{} {ty}l5 0l-2.5 5z" fill="{}"/>"#, tx - 2.5, paint(ink, svg.theme));
            }
        }
        let (value, ink) = match last {
            Some(c) => match c.state {
                State::Gap => ("gap".to_string(), Ink::Muted),
                State::Same => ("same".into(), Ink::Muted),
                State::NoValue => ("no value".into(), Ink::Muted),
                _ => (port::pct(c.est), Ink::Fg),
            },
            None => ("—".into(), Ink::Muted),
        };
        svg.text(x1, cy + 5.5, (16.0, true), ink, "end", &value);
    }
    // The commits under the rows: the first and the last.
    if let (Some(a), Some(z)) = (p.commits.first(), p.commits.last()) {
        let s = |c: &Value| c["short"].as_str().unwrap_or("").chars().take(7).collect::<String>();
        svg.text(ax0, rows_bottom + 16.0, (13.0, false), Ink::Muted, "start", &s(a));
        if p.commits.len() > 1 {
            svg.text(ax1, rows_bottom + 16.0, (13.0, false), Ink::Muted, "end", &s(z));
        }
    }
    let score = format!("{}{}", capitalize(&b.words()), if hidden > 0 { format!(" · {hidden} more in strobe history") } else { String::new() });
    let sy = bottom - 38.0;
    svg.text(x0, sy, (18.0, true), Ink::Fg, "start", &clip(&score, x1 - x0, 18.0, true));
    svg.text(
        x0,
        sy + 24.0,
        (15.0, false),
        Ink::Muted,
        "start",
        &clip(&format!("tracked points at the newest commit, not a verdict · bound ±{:.0}%", p.bound * 100.0), x1 - x0, 15.0, false),
    );
}

/// The badge: `vs <base> (tracked, 3 pairs) | better in 4/5 · worse in 1 · 1 gap`, from the points at
/// the newest commit; grey unless a confirmed change gives it a tone (`confirmed_tone`).
fn badge(p: &Port) -> (String, String, Tone) {
    let b = Board::latest(p);
    let mut v = format!("better in {}/{}", b.better, b.compared);
    if b.worse > 0 {
        v += &format!(" · worse in {}", b.worse);
    }
    if b.gaps > 0 {
        v += &format!(" · {} gap{}", b.gaps, if b.gaps == 1 { "" } else { "s" });
    }
    if b.missing > 0 {
        v += &format!(" · {} without a value", b.missing);
    }
    (format!("vs {} (tracked, {} pairs)", p.base, p.pairs), v, confirmed_tone(p))
}

/// `report history <file|ser_…> --format card|badge [--out <file.png|.svg>] [--theme …] [--badge-url] [--scoreboard]`.
pub(crate) fn report(args: &Args, p: &Port, default: &Path, format: &str) -> Result<Output> {
    let out = args.get("out").map(PathBuf::from).unwrap_or_else(|| default.to_path_buf());
    let svg_out = out.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let theme = Theme::parse(args.get("theme"), if svg_out { Theme::Auto } else { Theme::Light })?;
    let board = Board::latest(p);
    if format == "badge" {
        let (noun, value, tone) = badge(p);
        let url = shields_url(&noun, &value, tone);
        if args.has("badge-url") {
            return Ok(Output::new(format!("{url}\n"), json!({ "url": url, "label": noun, "value": value, "scoreboard": board.json() })));
        }
        let bytes = image(|t| badge_svg(&noun, &value, tone, t), &out, theme, &format!("{noun} {value}"), 2.0)?;
        crate::app::report::write_whole(&out, &bytes)?;
        let path = out.display().to_string();
        return Ok(Output::new(format!("{path} · {noun} {value}\n"), json!({ "path": path, "bytes": bytes.len(), "label": noun, "value": value, "url": url, "scoreboard": board.json() })));
    }
    let alt = format!("{} vs {}: {} at the latest tracked commit, over {} commits.", p.head, p.base, board.words(), p.commits.len());
    let bytes = image(|t| svg(p, t), &out, theme, &alt, 1.0)?;
    crate::app::report::write_whole(&out, &bytes)?;
    let path = out.display().to_string();
    Ok(Output::new(format!("{path} · {}\n{alt}\n", crate::app::fmt::bytes(bytes.len() as f64)), json!({ "path": path, "bytes": bytes.len(), "alt": alt, "scoreboard": board.json() })))
}
