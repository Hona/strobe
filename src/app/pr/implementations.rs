//! An implementation comparison (two targets, SPEC §9.10) as a pull request block: the headline
//! journey, every regression in one caution, the scoreboard across journeys, one line per journey
//! with what was checked the same on both, the fairness line, and Details named by implementation.

use super::*;
use crate::app::family;

/// `2.4`, `12`: a factor with the digits it needs.
fn factor(ratio: f64) -> String {
    if ratio >= 10.0 { format!("{ratio:.0}") } else { format!("{ratio:.1}") }
}

/// A relative change between implementations: a cut as a signed percentage (bounded, `−83%`), an
/// increase of 2× or more as a factor (`2.4×`, never `+140%`).
pub fn factor_or_pct(x: f64) -> String {
    if x.is_finite() && x >= 1.0 { format!("{}×", factor(1.0 + x)) } else { fmt::pct(x) }
}

/// A decided difference in words a sentence about two implementations reads: `uses 62% less CPU
/// time`, `opens 64% faster`, `responds 43% faster`, `takes 2.4× longer`.
pub struct Claim {
    pub verb: String,
    /// `62%`, `2.4×`, `219 MB`.
    pub amount: String,
    /// `less`, `faster`, `longer`.
    pub word: String,
    /// What the word is about after it: `CPU time`, `time`, or nothing for `opens … faster`.
    pub noun: String,
    /// The part a card prints under its large number when the verb carries the metric (`to open`).
    after: String,
}

impl Claim {
    /// `uses 62% less CPU time`; `bold` sets the change apart, as a proof line does.
    pub fn pr(&self, bold: bool) -> String {
        let what = fmt::fields([self.amount.as_str(), self.word.as_str(), self.noun.as_str()]).replace(" · ", " ");
        if bold { format!("{} **{}**", self.verb, nb(&what)) } else { format!("{} {what}", self.verb) }
    }

    /// The words under a card's large number: `less CPU time`, `faster to open`, `longer`.
    pub fn card(&self) -> String {
        fmt::fields([self.word.as_str(), self.noun.as_str(), self.after.as_str()]).replace(" · ", " ")
    }
}

pub fn claim(c: &Value) -> Claim {
    let m = c["metric"].as_str().unwrap_or("");
    let est = f(&c["estimate"]);
    let lower = better_of(m) == Better::Lower;
    let improved = (est < 0.0) == lower;
    let relative = c["relative"] == true;
    let times = relative && est >= 1.0;
    let pct = (est * 100.0).abs();
    let amount = match (relative, times) {
        (true, true) => format!("{}×", factor(1.0 + est)),
        (true, false) if pct < 10.0 => format!("{pct:.1}%"),
        (true, false) => format!("{pct:.0}%"),
        (false, _) => disp(est.abs(), abs_unit(unit_of(m))),
    };
    let (good, bad) = model::metric(m).map(|d| d.words).unwrap_or(("less", "more"));
    let dir = if improved { good } else { bad };
    let quick = if improved { "faster" } else { "slower" };
    let mk = |verb: String, word: &str, noun: String, after: &str| Claim { verb, amount: amount.clone(), word: word.to_string(), noun, after: after.to_string() };
    if m == "launch.ready" {
        return mk("opens".into(), quick, String::new(), "to open");
    }
    if let Some(rest) = m.strip_prefix("launch.") {
        return mk(format!("reaches {rest}"), quick, String::new(), &format!("to reach {rest}"));
    }
    if m.starts_with("interaction.") || m.starts_with("scroll.latency.") {
        return mk("responds".into(), quick, String::new(), "to respond");
    }
    if m == "run.duration" {
        return if times { mk("takes".into(), "longer", String::new(), "") } else { mk("takes".into(), if improved { "less" } else { "more" }, "time".into(), "") };
    }
    if let Some(rest) = m.strip_prefix("phase.") {
        return if times { mk("takes".into(), "longer", format!("in {rest}"), "") } else { mk("takes".into(), if improved { "less" } else { "more" }, format!("time in {rest}"), "") };
    }
    let thing = lc(&noun(m)).trim_end_matches(" per run").to_string();
    let verb = if matches!(good, "less") { "uses" } else { "has" };
    mk(verb.into(), dir, thing, "")
}

/// `−66% to −58%`, `2.2× to 2.6×`: a likely range as `factor_or_pct` words it.
fn range_x(c: &Value) -> String {
    if c["relative"] != true {
        return range(c);
    }
    let (lo, hi) = (f(&c["lo"]), f(&c["hi"]));
    if !lo.is_finite() || !hi.is_finite() {
        return "—".into();
    }
    nb(&format!("{} to {}", factor_or_pct(lo), factor_or_pct(hi)))
}

fn change_x(c: &Value) -> String {
    if c["relative"] == true { factor_or_pct(f(&c["estimate"])) } else { delta(c, f(&c["estimate"])) }
}

/// The checks `expect.same` read on both implementations, as one sentence: ` Same `@topRow` after
/// every run.`, or what differed.
fn same_line(s: &Value) -> String {
    let checks: Vec<&Value> = s["equivalence"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    if checks.is_empty() {
        return String::new();
    }
    let differed: Vec<String> = checks.iter().filter(|e| e["ok"] != true).map(|e| code(e["selector"].as_str().unwrap_or(""))).collect();
    if !differed.is_empty() {
        return format!(" **Not the same {}** on both.", join_and(&differed));
    }
    let sel: Vec<String> = checks.iter().map(|e| code(e["selector"].as_str().unwrap_or(""))).collect();
    format!(" Same {} after every run.", join_and(&sel))
}

/// A gap series' reason: `no `@find` in Rewrite`.
fn gap_text(d: &Doc, s: &Value) -> String {
    let text = cell(&family::gap_reason(d.r, s));
    // The missing step as code: `no `@find` in …`.
    match s["gap"]["missing"].as_str() {
        Some(missing) => text.replacen(&cell(missing), &code(missing), 1),
        None => text,
    }
}

/// How many pairs the compared journeys ran: `11 alternating pairs`, `11–12 …`.
fn pairs_span(series: &[&Value]) -> String {
    let ns: Vec<u64> = series.iter().filter(|s| !family::gap(s)).filter_map(|s| s["pairs"].as_u64()).filter(|n| *n > 0).collect();
    match (ns.iter().min(), ns.iter().max()) {
        (Some(a), Some(b)) if a == b => format!("{a} alternating pair{}", if *a == 1 { "" } else { "s" }),
        (Some(a), Some(b)) => format!("{a}–{b} alternating pairs"),
        _ => String::new(),
    }
}

/// What the CPU of each side summed: `CPU sums every app process (Original 5, Rewrite 1;
/// server excluded)`.
pub fn cpu_scope(r: &Value) -> String {
    let (bn, hn) = side_names(r);
    let (b, h) = variants(r);
    let count = |v: &Value| v["process_set"]["median_count"].as_f64();
    let server = if r["fixture"].is_object() { "; server excluded" } else { "" };
    match (count(&b), count(&h)) {
        (Some(x), Some(y)) => format!("CPU sums every app process ({bn} {x:.0}, {hn} {y:.0}{server})"),
        _ => String::new(),
    }
}

/// The fairness line's parts: machine, window, data, input, what CPU summed, pairs.
pub fn fairness(r: &Value, series: &[&Value]) -> (Vec<String>, Vec<String>) {
    let w = &r["fairness"]["window"];
    let window = match (w["client_dip"][0].as_f64(), w["client_dip"][1].as_f64()) {
        (Some(x), Some(y)) => {
            let scale = w["scale"].as_f64().map(|s| format!(" @{}×", if (s - s.round()).abs() < 1e-6 { format!("{s:.0}") } else { format!("{s:.2}") })).unwrap_or_default();
            let hz = w["hz"].as_f64().map(|h| format!(", {h:.0} Hz")).unwrap_or_default();
            format!("window {x:.0}×{y:.0}{scale}{hz}")
        }
        _ => String::new(),
    };
    let os = r["env"]["os"].as_str().unwrap_or("").split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let first = vec![
        short_cpu(r["env"]["cpu"].as_str().unwrap_or("")),
        os,
        window,
        if r["fixture"].is_object() { "same server and data".into() } else { String::new() },
        if r["fairness"]["input"] == "os" { "OS input to both".into() } else { String::new() },
    ];
    (first, vec![cpu_scope(r), pairs_span(series)])
}

impl<'a> Doc<'a> {
    /// The block of an implementation comparison.
    pub(super) fn render_implementations(&self) -> String {
        let r = self.r;
        let (bn, hn) = side_names(r);
        let fam = family::of(r);
        let hero = family::hero(r, &fam);
        let mut out = String::new();
        if let Some(url) = r["card"]["url"].as_str() {
            let alt = r["card"]["alt"].as_str().unwrap_or("");
            let digest = r["card"]["digest"].as_str().map(|d| format!(" <!-- strobe:card {d} -->")).unwrap_or_default();
            out += &format!("![{}]({url}){digest}\n\n", alt.replace(['[', ']'], ""));
        }
        // The headline: the journey the file marks `hero`, else the biggest win the scoreboard counts.
        let regs = self.implementation_regressions();
        let heading = match hero {
            Some(i) => {
                let s = self.series[i];
                format!("{} {} than {} · {}", cell(&hn), claim(&s["primary"]).pr(false), cell(&bn), lc(&cell(&self.title(s))))
            }
            None => match regs.first() {
                Some((s, c)) if std::ptr::eq(*c, &s["primary"]) => format!("Regression: {} {} than {} · {}", cell(&hn), claim(c).pr(false), cell(&bn), lc(&cell(&self.title(s)))),
                _ => self.calm_implementations(&bn),
            },
        };
        out += &format!("# {heading}\n\n");
        if !regs.is_empty() {
            out += "> [!CAUTION]\n";
            let lines: Vec<String> = regs
                .iter()
                .map(|(s, c)| {
                    let (w, n) = wins(&pairs(s), c["metric"].as_str().unwrap_or(""));
                    let what = if std::ptr::eq(*c, &s["primary"]) { String::new() } else { format!(" ({})", lc(&noun(c["metric"].as_str().unwrap_or("")))) };
                    format!("> **Regression:** {}{what}: {} {} than {} (medians {}, worse in {} of {n} pairs).", cell(&self.title(s)), cell(&hn), claim(c).pr(false), cell(&bn), before_after(c), n - w)
                })
                .collect();
            out += &lines.join("\n");
            out += "\n\n";
        }
        out += &format!("{}\n\n", self.scoreboard_line(&fam, &bn));
        for (i, s) in self.series.iter().enumerate() {
            out += &format!("- **{}:** {}\n", cell(&self.title(s)), self.journey_line(s, fam.holm.get(i).copied().unwrap_or(false), fam.compared));
        }
        out += "\n";
        let (a, b) = fairness(r, &self.series);
        out += &format!("{}.\n\n", cell(&fmt::fields(a.into_iter().chain(b))));
        if let Some(n) = self.note() {
            out += &format!("{n}\n\n");
        }
        let mut d = String::new();
        d += &self.implementation_metrics();
        if self.level < 4 {
            d += &self.per_side_section();
        }
        if self.level < 6 {
            d += &self.equivalence_section();
        }
        if self.level < 5 {
            d += &self.spends_section();
        }
        d += &self.implementation_method();
        out += &drawer("<b>Details</b> · metrics, per implementation, equivalence, method", &d);
        out += &self.rerun_drawer_with(self.rerun_implementations());
        out
    }

    /// Everything decidedly worse in head than base: each journey's main metric, then its tested
    /// metrics, in the file's order.
    fn implementation_regressions(&self) -> Vec<(&'a Value, &'a Value)> {
        let mut out = Vec::new();
        for s in self.series.iter().copied() {
            if s["not_run"] == true || family::gap(s) {
                continue;
            }
            if self.primary_out(s) == Out::Worse {
                out.push((s, &s["primary"]));
            }
            out.extend(tested(s).into_iter().skip(1).filter(|c| outcome(c, false) == Out::Worse).map(|c| (s, c)));
        }
        out
    }

    /// The heading without a counted win or a regression of a main metric.
    fn calm_implementations(&self, base: &str) -> String {
        let outs: Vec<Out> = self.series.iter().filter(|s| !family::gap(s)).map(|s| self.primary_out(s)).collect();
        if outs.is_empty() {
            return "Not compared: every journey is a parity gap".into();
        }
        if outs.iter().all(|o| matches!(o, Out::Same | Out::NoWorse | Out::NoWorseSlower)) {
            return format!("No measurable difference from {}", cell(base));
        }
        if outs.contains(&Out::Worse) {
            return format!("Worse than the margin allowed against {}", cell(base));
        }
        if outs.iter().all(|o| *o == Out::Refused) {
            return "Not measured".into();
        }
        let need = self.series.iter().filter_map(|s| s["primary"]["needed_pairs"].as_u64().filter(|n| *n <= 1000)).max();
        need.map(|n| format!("Not decided yet: needs ~{n} pairs")).unwrap_or_else(|| "Not decided yet".into())
    }

    /// `**Better in 4 of 5 compared journeys** · worse in 1 · not comparable: 1 (…). Holm-adjusted
    /// across 5 journeys.`
    fn scoreboard_line(&self, fam: &stats::Family, base: &str) -> String {
        let mut parts = Vec::new();
        let journeys = |n: usize| if n == 1 { "journey" } else { "journeys" };
        match fam.all_no_worse {
            Some(true) => {
                let margin = self.series.iter().find(|s| !family::gap(s)).map(|s| margin_text(&s["primary"])).unwrap_or_default();
                parts.push(format!("**No worse than {} ({margin}) in all {} compared {}**", cell(base), fam.compared, journeys(fam.compared)))
            }
            Some(false) => parts.push(format!("**Not no worse in all {} compared {}**", fam.compared, journeys(fam.compared))),
            None => parts.push(format!("**Better in {} of {} compared {}**", fam.better, fam.compared, journeys(fam.compared))),
        }
        if fam.all_no_worse.is_some() && fam.better > 0 {
            parts.push(format!("better in {}", fam.better));
        }
        if fam.worse > 0 {
            parts.push(format!("worse in {}", fam.worse));
        }
        let same_word = if fam.all_no_worse.is_some() { "no worse" } else { "the same" };
        if fam.same > 0 && fam.all_no_worse != Some(true) {
            parts.push(format!("{same_word} in {}", fam.same));
        }
        if fam.unclear > 0 {
            parts.push(format!("unclear in {}", fam.unclear));
        }
        if fam.gaps > 0 {
            let list: Vec<String> = self.series.iter().filter(|s| family::gap(s)).map(|s| format!("{}: {}", cell(&self.title(s)), gap_text(self, s))).collect();
            parts.push(format!("not comparable: {} ({})", fam.gaps, list.join("; ")));
        }
        let mut line = format!("{}.", parts.join(" · "));
        if fam.all_no_worse.is_some() && fam.compared > 1 {
            line += " Each journey at its own 2.5%: all of them no worse needs no correction (intersection–union).";
        } else if fam.compared > 1 {
            line += &format!(" Holm-adjusted across {} journeys.", fam.compared);
            if fam.better_alone > 0 {
                line += &format!(" {} better on {} own did not clear it.", fam.better_alone, if fam.better_alone == 1 { "its" } else { "their" });
            }
        }
        line
    }

    /// One journey's proof line.
    fn journey_line(&self, s: &Value, counted: bool, compared: usize) -> String {
        if family::gap(s) {
            return format!("not compared: {}.", gap_text(self, s));
        }
        let c = &s["primary"];
        let m = c["metric"].as_str().unwrap_or("");
        let o = self.primary_out(s);
        let (w, n) = wins(&pairs(s), m);
        let medians = format!("medians {}", before_after(c));
        let thing = self.noun_in(s, m);
        let same = same_line(s);
        let line = match o {
            Out::Better | Out::Worse if !beyond_margin(c) => {
                let cl = claim(c);
                let metric = if matches!(cl.verb.as_str(), "uses" | "has") { String::new() } else { format!("{}, ", lc(&thing)) };
                let (k, word) = if o == Out::Better { (w, "better") } else { (n - w, "worse") };
                let alone = if o == Out::Better && !counted && compared > 1 { format!("; better on its own, not after Holm across {compared} journeys") } else { String::new() };
                format!("{} ({metric}{medians}), {word} in {k} of {n} pairs, likely {}{alone}.", cl.pr(true), range_x(c))
            }
            Out::Same => format!("{thing} the same within {} ({medians}).", bound_text(c)),
            Out::NoWorse | Out::NoWorseSlower => format!("{thing} no worse, within the {} allowed ({medians}).", margin_text(c)),
            Out::Worse => format!("{thing} **{}** ({medians}), more than the {} allowed.", nb(&change_x(c)), margin_text(c)),
            Out::Unclear | Out::Gated => match c["needed_pairs"].as_u64().filter(|x| *x <= 1000 && *x > n as u64) {
                Some(need) => format!("{thing}: no clear difference in {n} pairs ({medians}); ~{need} pairs would settle it."),
                None => format!("{thing}: no clear difference in {n} pairs ({medians})."),
            },
            _ => self.sentence(s),
        };
        format!("{line}{same}")
    }

    /// Every tested metric, columns named by implementation.
    fn implementation_metrics(&self) -> String {
        let (bn, hn) = side_names(self.r);
        let mut t = format!("#### Metrics\n\n| Journey / metric | {} | {} | Change | Likely range | Verdict |\n|:---|---:|---:|---:|---:|:---|\n", cell(&bn), cell(&hn));
        let fam = family::of(self.r);
        let mut care: Vec<(String, String)> = Vec::new();
        for (si, s) in self.series.iter().enumerate() {
            if family::gap(s) || s["not_run"] == true {
                let why = if family::gap(s) { format!("not compared <sub>{}</sub>", gap_text(self, s)) } else { "not run".into() };
                t += &format!("{}| {} | | | | | {why} |\n", self.series_cell(s, 0), cell(&noun(s["primary"]["metric"].as_str().unwrap_or(""))));
                continue;
            }
            for (i, c) in tested(s).into_iter().enumerate() {
                let m = c["metric"].as_str().unwrap_or("");
                let o = if i == 0 { self.primary_out(s) } else { outcome(c, false) };
                let name = if i == 0 { format!("{} <sub>main</sub>", cell(&noun(m))) } else { cell(&noun(m)) };
                let (chg, rng) = if o == Out::Refused {
                    ("—".to_string(), "—".to_string())
                } else {
                    let d = nb(&change_x(c));
                    (if marked(c, o) { format!("**{d}**") } else { d }, range_x(c))
                };
                let mut verdict = verdict_text(c, o, s, false);
                if i == 0 && o == Out::Better && !fam.holm.get(si).copied().unwrap_or(false) && fam.compared > 1 {
                    verdict += " <sub>not after Holm</sub>";
                }
                t += &format!("{}| {name} | {} | {} | {chg} | {rng} | {verdict} |\n", self.series_cell(s, i), nb(&value(c, "base")), nb(&value(c, "head")));
                if let Some(note) = c["care_note"].as_str()
                    && !care.iter().any(|(n, x)| n == m && x == note)
                {
                    care.push((m.to_string(), note.to_string()));
                }
            }
        }
        let mut foot = String::from(
            "Likely range: 98.4% confidence for each main metric (five planned looks, 95% overall), 95% for the others. Change: the median of the pairs' own changes; an increase of 2× or more as a factor.",
        );
        for (m, note) in care {
            foot += &format!(" {}: {}.", noun(&m), cell(note.trim_end_matches('.')));
        }
        t + &format!("\n<sub>{foot}</sub>\n\n")
    }

    /// Numbers each implementation has and the other may not, never compared.
    fn per_side_section(&self) -> String {
        let (bn, hn) = side_names(self.r);
        let mut t = format!("#### Per implementation, not compared\n\n| Journey / metric | {} | {} |\n|:---|---:|---:|\n", cell(&bn), cell(&hn));
        let mut any = false;
        for s in &self.series {
            let (b, h) = (&s["per_side"]["base"], &s["per_side"]["head"]);
            let mut keys: Vec<&String> = b.as_object().into_iter().flatten().chain(h.as_object().into_iter().flatten()).map(|(k, _)| k).collect();
            keys.sort();
            keys.dedup();
            for (i, k) in keys.into_iter().enumerate() {
                any = true;
                let show = |v: &Value| {
                    let unit = v["unit"].as_str().unwrap_or_else(|| unit_of(k));
                    v["median"].as_f64().or(v["value"].as_f64()).map(|x| nb(&disp(x, unit))).unwrap_or_else(|| "—".into())
                };
                t += &format!("{}| {} | {} | {} |\n", self.series_cell(s, i), cell(&noun(k)), show(&b[k.as_str()]), show(&h[k.as_str()]));
            }
        }
        if !any {
            return String::new();
        }
        t + "\n<sub>Engine-internal numbers (a browser's main thread, a window's thread, heaps): each implementation's own, measured its own way, so never compared.</sub>\n\n"
    }

    /// What `expect.same` read on both implementations.
    fn equivalence_section(&self) -> String {
        let (bn, hn) = side_names(self.r);
        let mut t = String::from("#### Equivalence checked\n\n| Journey | Check | Read on both | Pairs |\n|:---|:---|:---|---:|\n");
        let mut any = false;
        for s in &self.series {
            for e in s["equivalence"].as_array().into_iter().flatten() {
                any = true;
                let q = |v: &Value| match v.as_str() {
                    Some("") => "(empty)".to_string(),
                    Some(x) => format!("\"{}\"", cell(x)),
                    None => "—".into(),
                };
                let read = if e["ok"] == true { q(&e["base"]) } else { format!("**differed:** {} {} vs {} {}", cell(&bn), q(&e["base"]), cell(&hn), q(&e["head"])) };
                t += &format!("| {} | {} | {read} | {} |\n", cell(&self.title(s)), code(e["selector"].as_str().unwrap_or("")), e["pairs"].as_u64().map(|n| n.to_string()).unwrap_or_default());
            }
        }
        if !any {
            return String::new();
        }
        t + "\n<sub>Read on each side after setup and after every run, outside the measured window, trimmed and with whitespace collapsed.</sub>\n\n"
    }

    /// Where each implementation spends its time, from its profiled launches: its busiest threads,
    /// never a diff (the two share no code).
    fn spends_section(&self) -> String {
        let (bn, hn) = side_names(self.r);
        let mut t = format!("#### Where each spends its time, in profiled launches\n\n| | {} | {} |\n|:---|:---|:---|\n", cell(&bn), cell(&hn));
        let mut any = false;
        let side = |s: &Value, k: &str| -> Option<(f64, String)> {
            let mut threads: Vec<(String, f64)> = s["hotspots"][k]["threads"]["threads"]
                .as_array()?
                .iter()
                .map(|x| (x["name"].as_str().unwrap_or("(unnamed)").to_string(), f(&x["cpu_ms_per_run"])))
                .filter(|x| x.1.is_finite() && x.1 > 0.0)
                .collect();
            let total: f64 = threads.iter().map(|x| x.1).sum();
            if total <= 0.0 {
                return None;
            }
            threads.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let top: Vec<String> = threads.iter().take(4).map(|(n, ms)| format!("{} {:.0}%", code(n), 100.0 * ms / total)).collect();
            Some((total, top.join(" · ")))
        };
        for s in &self.series {
            let (Some(b), Some(h)) = (side(s, "base"), side(s, "head")) else { continue };
            any = true;
            let who = if self.multi() { format!("{}: ", cell(&self.title(s))) } else { String::new() };
            t += &format!("| {who}CPU per run, profiled | {} | {} |\n", nb(&disp(b.0, "ms")), nb(&disp(h.0, "ms")));
            t += &format!("| {who}By thread | {} | {} |\n", b.1, h.1);
        }
        if !any {
            return String::new();
        }
        t + "\n<sub>From one profiled launch per implementation, apart from the timed pairs and slowed by the profiler: where each one's time goes, not a change.</sub>\n\n"
    }

    /// How it was measured: both implementations, the machine and window, the fixture, the method.
    fn implementation_method(&self) -> String {
        let r = self.r;
        let (bn, hn) = side_names(r);
        let (b, h) = variants(r);
        let mut t = String::from("#### How it was measured\n\n");
        for (name, v) in [(&bn, &b), (&hn, &h)] {
            let subject = v["subject"].as_str().map(|s| format!(", *{}*", cell(&no_refs(s)))).unwrap_or_default();
            let label = v["label"].as_str().filter(|l| v["commit"].is_null() && !local_path(l)).map(|l| format!(" ({})", cell(l))).unwrap_or_default();
            let procs = v["process_set"]["median_count"].as_f64().map(|n| format!("{n:.0} process{}", if n == 1.0 { "" } else { "es" })).unwrap_or_default();
            let what = fmt::fields([v["engine"].as_str().unwrap_or(""), v["gpu_backend"].as_str().unwrap_or(""), procs.as_str()]);
            t += &format!("- **{}:** {}{label}{subject}{}\n", cell(name), code(&tag(v)), if what.is_empty() { String::new() } else { format!("; {}", cell(&what)) });
        }
        t += &format!("- **Machine:** {}\n", cell(&self.machine()));
        let fair = &r["fairness"];
        let w = &fair["window"];
        if let (Some(x), Some(y)) = (w["client_dip"][0].as_f64(), w["client_dip"][1].as_f64()) {
            let extra = fmt::fields([
                w["scale"].as_f64().map(|s| format!("scale {s}")).unwrap_or_default(),
                w["hz"].as_f64().map(|h| format!("{h:.0} Hz")).unwrap_or_default(),
                w["display"].as_str().map(|d| format!("on {d}")).unwrap_or_default(),
                fair["adapter"].as_str().map(|a| format!("GPU {a}")).unwrap_or_default(),
                fair["gpu_cache"].as_str().map(|g| format!("GPU caches {g}")).unwrap_or_default(),
                fair["priority"].as_str().map(|p| format!("{p} priority")).unwrap_or_default(),
            ])
            .replace(" · ", ", ");
            t += &format!("- **Window, the same on both:** client area {x:.0}×{y:.0} device-independent pixels, {}\n", cell(&extra));
        }
        if fair["input"].is_string() {
            let obs = |k: &str| match fair["observer"][k].as_str() {
                Some("devtools") => "DevTools (its DOM)",
                Some("accessibility") => "the accessibility tree",
                Some(o) => o,
                None => "",
            };
            let reads = |k: &str| fair["observer"]["reads_per_run"][k].as_f64();
            let input = match fair["input"].as_str() {
                Some("os") => "real OS input to both".to_string(),
                Some("none") => "no input during the runs".to_string(),
                other => format!("{} input", cell(other.unwrap_or(""))),
            };
            let drive =
                if !obs("base").is_empty() && !obs("head").is_empty() { format!("; {} driven through {}, {} through {}", cell(&bn), obs("base"), cell(&hn), obs("head")) } else { String::new() };
            let read = match (reads("base"), reads("head")) {
                (Some(x), Some(y)) => format!("; strobe read {x:.0} and {y:.0} times per measured run"),
                _ => String::new(),
            };
            let frames = fair["frames"].as_str().map(|f| format!("; frames from {} on both", cell(f))).unwrap_or_default();
            t += &format!("- **Input:** {input}{drive}{read}{frames}\n");
        }
        let fx = &r["fixture"];
        if fx.is_object() {
            let seed = fx["seed"]
                .as_str()
                .map(|sd| format!(", seeded from {}{}", code(sd), fx["seed_sha256"].as_str().map(|h| format!(" (sha256 {})", &h[..h.len().min(12)])).unwrap_or_default()))
                .unwrap_or_default();
            let conn = |k: &str| fx["connected"][k].as_bool();
            let connected = match (conn("base"), conn("head")) {
                (Some(true), Some(true)) => "; both connected to it".to_string(),
                (Some(a), Some(b)) => format!("; connected: {} {}, {} {}", cell(&bn), if a { "yes" } else { "no" }, cell(&hn), if b { "yes" } else { "no" }),
                _ => String::new(),
            };
            let cpu = match (fx["cpu_ms_per_run"]["base"].as_f64(), fx["cpu_ms_per_run"]["head"].as_f64()) {
                (Some(x), Some(y)) => format!("; it used {} ({}) and {} ({}) of CPU per run", nb(&disp(x, "ms")), cell(&bn), nb(&disp(y, "ms")), cell(&hn)),
                _ => String::new(),
            };
            t += &format!("- **Fixture:** {} started per block outside both apps{seed}{connected}{cpu}\n", code(&scrub_paths(fx["command"].as_str().unwrap_or(""))));
        }
        let scope = cpu_scope(r);
        if !scope.is_empty() {
            t += &format!("- **What CPU counts:** {}\n", cell(&scope));
        }
        let looks: Vec<String> = r["plan"]["looks"].as_array().into_iter().flatten().map(|x| x.to_string()).collect();
        let elapsed: f64 = self.series.iter().filter_map(|s| s["elapsed_ms"].as_f64()).sum();
        t += &format!(
            "- **Method:** each journey in alternating pairs of fresh launches of both implementations; Wilcoxon signed-rank test and Hodges–Lehmann interval; main metric checked at {} pairs, others Holm-corrected; the scoreboard counts a win only when its repeated-interval p-value clears Holm across the compared journeys, and every regression; {} in all\n",
            if looks.is_empty() { "the planned".to_string() } else { looks.join(", ") },
            nb(&fmt::ms_duration(elapsed as u64))
        );
        t += "- **Not checked by strobe:** that both render the same features beyond the equivalence checks; that the fixture behaves like production\n";
        t += &self.notes();
        t + &self.recorded()
    }

    /// `strobe bench <file> --base original --head rewrite=a4ca326f10`: each side by its target, and by
    /// commit when it was built from one.
    fn rerun_implementations(&self) -> String {
        let r = self.r;
        let (b, h) = variants(r);
        let side = |v: &Value, role: &str| -> String {
            let target = v["target"].as_str().unwrap_or(role);
            match v["commit"].as_str() {
                Some(c) => format!("{target}={}", c.chars().take(10).collect::<String>()),
                None if v["kind"] == "exe" => format!("{target}={}", symbolic(v["spec"].as_str().unwrap_or(""))),
                None => target.to_string(),
            }
        };
        let mut line = format!("strobe bench {} --base {} --head {}", self.file(), side(&b, "base"), side(&h, "head"));
        const SKIP: &[&str] = &["base", "head", "aa", "id", "json", "quiet", "verbose", "resume", "stop", "keep", "open", "progress", "update-pr", "repo", "force", "format", "out"];
        let mut rest: Vec<(&String, &Value)> = r["args"]["flags"].as_object().into_iter().flatten().filter(|(k, _)| !SKIP.contains(&k.as_str())).collect();
        rest.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in rest {
            match v.as_str() {
                Some("true") => line += &format!(" --{k}"),
                Some(x) if local_path(x) => line += &format!(" --{k} <{k}>"),
                Some(x) => line += &format!(" --{k} {x}"),
                None => {}
            }
        }
        line
    }
}

/// A no-worse margin as it was given: `≤5%`.
fn margin_text(c: &Value) -> String {
    match c["noworse"].as_f64() {
        Some(x) if c["relative"] == true => format!("≤{:.0}%", x * 100.0),
        Some(x) => format!("≤{}", disp(x, abs_unit(unit_of(c["metric"].as_str().unwrap_or(""))))),
        None => bound_text(c),
    }
}
