//! The report as Markdown: the hero, one line per series, and the Details and Rerun drawers.

use super::*;

// ---------- the document ----------

pub(super) struct Doc<'a> {
    pub(super) r: &'a Value,
    pub(super) level: u8,
    pub(super) aa: bool,
    pub(super) id: &'a str,
    pub(super) series: Vec<&'a Value>,
}

pub(super) fn drawer(summary: &str, body: &str) -> String {
    format!("<details>\n<summary>{summary}</summary>\n\n{}\n\n</details>\n\n", body.trim())
}

impl<'a> Doc<'a> {
    pub(super) fn new(r: &'a Value, level: u8) -> Doc<'a> {
        Doc { r, level, aa: r["plan"]["aa"] == true, id: r["id"].as_str().unwrap_or(""), series: r["series"].as_array().map(|a| a.iter().collect()).unwrap_or_default() }
    }

    /// The columns' names for base and head: an implementation comparison's sides by name (SPEC
    /// §9.10), else "Before" and "This PR".
    pub(super) fn sides(&self) -> (String, String) {
        side_names(self.r)
    }

    /// What a reader sees: the hero line, one caution with every regression, one line per series (and
    /// a second only for untested drift or a warning about that series), at most one caveat for the
    /// report, then two drawers: Details (metrics, pairs, profiles, method under plain headings) and
    /// Rerun.
    pub(super) fn render(&self) -> String {
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
            let alt = self.r["card"]["alt"].as_str().map(str::to_string).unwrap_or_else(|| hero.replace('\u{a0}', " "));
            // The images' digest: the block's hash leaves image addresses out, so new pictures of the
            // same numbers (a newer strobe draws them) still count as a change.
            let digest = self.r["card"]["digest"].as_str().map(|d| format!(" <!-- strobe:card {d} -->")).unwrap_or_default();
            out += &format!("![{}]({url}){digest}\n\n", alt.replace(['[', ']'], ""));
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
            // The medians beside a paired change that went the other way: say why they disagree.
            if let (Some(m), false) = (uneven(&s["primary"]), matches!(self.primary_out(s), Out::Refused | Out::NotRun) || s["kind"] == "leak") {
                line = format!("{}; **uneven pairs**: the medians moved {} while the pairs' own changes centre on {} (see Details).", line.trim_end_matches('.'), nb(&delta(&s["primary"], m)), nb(&delta(&s["primary"], f(&s["primary"]["estimate"]))));
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

    pub(super) fn primary_out(&self, s: &Value) -> Out {
        if s["not_run"] == true {
            return Out::NotRun;
        }
        if s["kind"] == "leak" {
            return if kind(&s["primary"]) == VerdictKind::Worse {
                Out::Worse
            } else if s["decided"] == true {
                Out::Same
            } else {
                Out::Unclear
            };
        }
        outcome(&s["primary"], self.aa)
    }

    pub(super) fn file(&self) -> String {
        self.r["file_repo"].as_str().map(str::to_string).unwrap_or_else(|| repo_relative(self.r["file"].as_str().unwrap_or("")))
    }

    pub(super) fn multi(&self) -> bool {
        self.series.len() > 1
    }

    /// A series as people say it: its case in sentence case, the target only when the series do not
    /// all share one, and its parameters.
    pub(super) fn title(&self, s: &Value) -> String {
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
    pub(super) fn noun_in(&self, s: &Value, m: &str) -> String {
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
    pub(super) fn sentence(&self, s: &Value) -> String {
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
            Out::Better | Out::Worse if !beyond_margin(c) => {
                let (k, word) = if o == Out::Better { (w, "better") } else { (n - w, "worse") };
                let small = if small(c) { format!(", perhaps by under {}", bound.trim_start_matches('±')) } else { String::new() };
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
                let (id, why) = refusal(c);
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
    pub(super) fn drift(&self, s: &Value) -> Option<String> {
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
    pub(super) fn hero_parts(&self, c: &Value) -> (String, &'static str, String) {
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
    pub(super) fn hero_phrase(&self, c: &Value) -> String {
        let (amount, word, thing) = self.hero_parts(c);
        phrase(&amount, word, &thing)
    }

    /// The hero's wins: each series' main metric that decidedly improved, in the series' order, the
    /// same metric in several series said once (`19–23% less CPU time`), at most three. A single win
    /// from one series makes room for one tested metric of another kind that also won there.
    pub(super) fn win_phrases(&self) -> (Vec<String>, Option<(&'a Value, &'a Value)>) {
        let celebrates = |s: &Value| !self.aa && s["kind"] != "leak" && s["not_run"] != true && self.primary_out(s) == Out::Better && !beyond_margin(&s["primary"]);
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
                .filter(|c| outcome(c, self.aa) == Out::Better && !beyond_margin(c) && family(c["metric"].as_str().unwrap_or("")) != main)
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
    pub(super) fn regression_lines(&self) -> Vec<(bool, String, String)> {
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
    pub(super) fn calm(&self) -> String {
        let outs: Vec<(Out, &Value)> = self.series.iter().map(|s| (self.primary_out(s), *s)).collect();
        if let Some((_, s)) = outs.iter().find(|(o, s)| *o == Out::Worse && !beyond_margin(&s["primary"])) {
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
    pub(super) fn note(&self) -> Option<String> {
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
    pub(super) fn busy_blocks(&self) -> (usize, usize) {
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

    pub(super) fn drew_frames(&self) -> bool {
        self.series.iter().flat_map(|s| s["blocks"].as_array().into_iter().flatten()).any(|b| bv(b, "frames.n") > 0.0)
    }

    /// `session locked, display off`: the state every block ran in, which the numbers compare with.
    pub(super) fn regime(&self) -> String {
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
    pub(super) fn series_cell(&self, s: &Value, i: usize) -> String {
        if self.multi() && i == 0 { format!("| **{}** |\n", cell(&self.title(s))) } else { String::new() }
    }

    /// The footnote for rows whose medians and paired change point different ways: each one's spread
    /// of block values per build, and what that means.
    pub(super) fn uneven_note(&self, rows: &[(&Value, &Value)]) -> String {
        if rows.is_empty() {
            return String::new();
        }
        let spread = |s: &Value, m: &str, role: &str| {
            let mut v: Vec<f64> = s["blocks"].as_array().into_iter().flatten().filter(|b| b["variant"] == role).map(|b| bv(b, m)).filter(|x| x.is_finite()).collect();
            v.sort_by(f64::total_cmp);
            let unit = unit_of(m);
            match (v.first(), v.last()) {
                (Some(a), Some(b)) if disp(*a, unit) != disp(*b, unit) => span(&nb(&disp(*a, unit)), &nb(&disp(*b, unit))),
                (Some(a), _) => nb(&disp(*a, unit)),
                _ => "—".into(),
            }
        };
        let each: Vec<String> = rows
            .iter()
            .map(|(s, c)| {
                let m = c["metric"].as_str().unwrap_or("");
                let who = if self.multi() { format!(" in {}", self.title(s)) } else { String::new() };
                format!("{}{who} read {} before and {} with this PR", lc(&noun(m)), spread(s, m, "base"), spread(s, m, "head"))
            })
            .collect();
        format!(
            " Uneven pairs: {}; the medians' change (beside it) and the pairs' own point different ways, as when a few launches read far from the rest, so read neither as the change.",
            join_and(&each)
        )
    }

    // ---------- Details ----------

    /// Every tested metric of every series in one table.
    pub(super) fn metrics(&self) -> String {
        let (b, h) = self.sides();
        let mut t = format!("#### Metrics\n\n| Metric | {} | {} | Change | Likely range | Verdict |\n|:---|---:|---:|---:|---:|:---|\n", cell(&b), cell(&h));
        let (mut conf_main, mut conf_rest) = (String::new(), String::new());
        let mut odd: Vec<(&Value, &Value)> = Vec::new();
        for s in &self.series {
            if s["not_run"] == true || s["kind"] == "leak" || s["kind"] == "gap" {
                let what = match s["kind"].as_str() {
                    Some("leak") => "Memory growth per run",
                    Some("gap") => "Not compared",
                    _ => "Not run",
                };
                t += &format!("{}| {what} | | | | | {} |\n", self.series_cell(s, 0), cell(verdict(&s["primary"])));
                continue;
            }
            for (i, c) in tested(s).into_iter().enumerate() {
                let m = c["metric"].as_str().unwrap_or("");
                let o = if i == 0 { self.primary_out(s) } else { outcome(c, self.aa) };
                if o != Out::Refused && uneven(c).is_some() {
                    odd.push((s, c));
                }
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
        let foot = foot + &self.uneven_note(&odd);
        t + &format!("\n<sub>{foot}</sub>\n\n")
    }

    /// The metrics measured but not tested, last: what moved in a table, what held still in a line.
    pub(super) fn also_measured(&self) -> String {
        let (b, h) = self.sides();
        let mut t = format!("| Metric | {} | {} | Change | Likely range |\n|:---|---:|---:|---:|---:|\n", cell(&b), cell(&h));
        let (mut any, mut zeros, mut held) = (false, Vec::<String>::new(), Vec::<(String, String)>::new());
        let mut odd: Vec<(&Value, &Value)> = Vec::new();
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
                if o != Out::Refused && uneven(c).is_some() {
                    odd.push((s, c));
                }
                let (chg, rng) = change_cells(c, o);
                let rng = if o == Out::Refused {
                    let (id, why) = refusal(c);
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
        let note = self.uneven_note(&odd);
        if !note.is_empty() {
            foot.push(note.trim().to_string());
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
    pub(super) fn pairs_section(&self) -> String {
        let all: Vec<(&Value, Vec<Pair>)> = self.series.iter().map(|s| (*s, pairs(s))).filter(|(_, p)| !p.is_empty()).collect();
        if all.is_empty() {
            return String::new();
        }
        let foreign = all.iter().any(|(_, ps)| ps.iter().any(|p| bv(p.base, "cpu.foreign.max").is_finite()));
        let frames = all.iter().any(|(_, ps)| ps.iter().any(|p| bv(p.base, "frames.n") > 0.0 && bv(p.base, "frames.interval.max").is_finite()));
        let metric = if self.multi() { "Main metric".to_string() } else { noun(all[0].0["primary"]["metric"].as_str().unwrap_or("")) };
        let (b, h) = match self.r["comparison"] == "implementations" {
            true => self.sides(),
            false => ("before".to_string(), "this PR".to_string()),
        };
        let mut t = format!("#### Pairs\n\n| Pair | {}, {} → {} | Change |", cell(&metric), cell(&b), cell(&h));
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
        let mut summed: Vec<String> = Vec::new();
        for (s, ps) in &all {
            let m = s["primary"]["metric"].as_str().unwrap_or("");
            let unit = unit_of(m);
            let flagged = |p: &Pair| bv(p.base, "cpu.foreign.max") >= 25.0 || bv(p.head, "cpu.foreign.max") >= 25.0;
            let directional = matches!(self.primary_out(s), Out::Better | Out::Worse | Out::AaFalse);
            let ratio = |a: f64| a > 0.0 && unit != "count";
            let change = |a: f64, b: f64| if ratio(a) { fmt::pct(b / a - 1.0) } else { signed_disp(b - a, abs_unit(unit)) };
            if self.multi() {
                // The series names its rows, so no column is spent on it.
                t += &self.series_cell(s, 0);
            }
            // A long series shows its first and last pairs and up to five busy ones; one row sums up
            // the rest, so 140 pairs don't fill the drawer.
            let long = ps.len() > LONG_PAIRS;
            let mut busy_shown = 0;
            let mut rest: Vec<&Pair> = Vec::new();
            for (i, p) in ps.iter().enumerate() {
                if long && i + SHOWN_ENDS == ps.len() && !rest.is_empty() {
                    t += &summary_row(&rest, m, frames, foreign);
                    summed.push(self.summary_note(s, &rest, m));
                }
                if long {
                    let end = i < SHOWN_ENDS || i + SHOWN_ENDS >= ps.len();
                    if !end && !(flagged(p) && busy_shown < 5) {
                        rest.push(p);
                        continue;
                    }
                    busy_shown += (!end) as usize;
                }
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
                let d = change(a, b);
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
        foot.extend(summed);
        t + &format!("\n<sub>{}</sub>\n\n", foot.join(" "))
    }

    /// What the `…` row of a long series sums up, in a sentence: which pairs, how many this PR did
    /// better in, and where the middle 80% of their changes lay.
    pub(super) fn summary_note(&self, s: &Value, rest: &[&Pair], m: &str) -> String {
        let (first, last) = (rest[0].n, rest[rest.len() - 1].n);
        let which = if last - first + 1 == rest.len() { format!("pairs {first}–{last}") } else { format!("{} other pairs", rest.len()) };
        let owned: Vec<Pair> = rest.iter().map(|p| **p).collect();
        let (w, n) = wins(&owned, m);
        let (ch, ratio) = pair_changes(rest, m);
        let who = if self.multi() { format!(" of {}", self.title(s)) } else { String::new() };
        let spread = match (quantile(&ch, 0.1), quantile(&ch, 0.9)) {
            (Some(lo), Some(hi)) => format!("; the middle 80% of their changes ran from {} to {}", nb(&show_change(lo, ratio, m)), nb(&show_change(hi, ratio, m))),
            _ => String::new(),
        };
        format!("The … row{who} sums up {which}: this PR did better in {w} of {n}{spread}.")
    }

    /// Frame pacing, for series that drew frames.
    pub(super) fn pacing_section(&self) -> String {
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
    pub(super) fn attribution_section(&self) -> String {
        let prof: Vec<&&Value> = self.series.iter().filter(|s| s["attribution"]["base"].is_string() && s["attribution"]["head"].is_string()).collect();
        if prof.is_empty() {
            return String::new();
        }
        let top = if self.level >= 1 { 3 } else { 4 };
        // Profiled launches are untimed and slowed by the profiler: a row only explains a change the
        // timed pairs decided, the way it went, never stands as a change of its own.
        let prof: Vec<&&Value> = prof.into_iter().filter(|s| !self.decided_moves(s).is_empty()).collect();
        let mut used: Vec<&Value> = Vec::new();
        let mut t = String::from("#### Where it came from, in profiled launches\n\n");
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
            let moves = self.decided_moves(s);
            let way = |fam: fn(&str) -> bool| moves.iter().find(|(m, _)| fam(m)).map(|x| x.1);
            let cpu = way(|m| matches!(m, "cpu.work" | "cpu.app.total" | "cpu.process.total" | "ui.cpu"));
            let wakes = way(|m| matches!(m, "cpu.wakes" | "ui.wakes"));
            let memory = way(|m| family(m) == "memory");
            // A thread explains a CPU change by its CPU, else a wake-up change by its wake-ups, by 5% of its own or more.
            let along = |w: Option<f64>, x: f64, y: f64| w.is_some_and(|w| (y - x).abs() >= 0.05_f64.max(0.05 * x.abs().max(y.abs())) && (y - x).signum() == w);
            let mut rows: Vec<_> = thread_rows(s).into_iter().filter(|(_, x, y)| if cpu.is_some() { along(cpu, x.2, y.2) } else { along(wakes, x.1, y.1) }).collect();
            if cpu.is_some() {
                rows.sort_by(|a, b| (b.2.2 - b.1.2).abs().total_cmp(&(a.2.2 - a.1.2).abs()).then(a.0.cmp(&b.0)));
            }
            let before = (th.len(), mem.len(), main.len(), fns.len(), heaps.len());
            for (i, ((name, module), x, y)) in rows.into_iter().take(top).enumerate() {
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
            let layers: Vec<&Value> = s["memory_diff"]["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| f(&r["delta_bytes"]).abs() >= 64.0 * 1024.0 && memory.is_some_and(|w| f(&r["delta_bytes"]).signum() == w))
                .take(top)
                .collect();
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
            if d["base"].is_object() && d["head"].is_object() && memory.is_some() {
                heaps.push(format!(
                    "{}live heap after GC {} → {}",
                    if self.multi() { format!("{}: ", self.title(s)) } else { String::new() },
                    nb(&fmt::bytes(f(&d["base"]["self_size"]))),
                    nb(&fmt::bytes(f(&d["head"]["self_size"])))
                ));
            }
            if before != (th.len(), mem.len(), main.len(), fns.len(), heaps.len()) {
                used.push(**s);
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
        let recs: Vec<String> = used.iter().map(|s| format!("{} → {}", code(s["attribution"]["base"].as_str().unwrap_or("")), code(s["attribution"]["head"].as_str().unwrap_or("")))).collect();
        let mut foot = format!(
            "From one profiled launch per build ({}), apart from the timed pairs: only what moved the way a decided change did, to explain it. Profiling slows a launch, so read these as directions, not as changes.",
            recs.join(", ")
        );
        if !heaps.is_empty() {
            foot += &format!(" {}.", capitalize(&heaps.join("; ")));
        }
        t + &format!("<sub>{foot}</sub>\n\n")
    }

    /// The tested metrics (the main one first) the timed pairs decided better or worse in series
    /// `s`, each with the way its value went (+1 up, −1 down).
    pub(super) fn decided_moves(&self, s: &Value) -> Vec<(String, f64)> {
        tested(s)
            .into_iter()
            .enumerate()
            .filter(|(i, c)| matches!(if *i == 0 { self.primary_out(s) } else { outcome(c, self.aa) }, Out::Better | Out::Worse))
            .map(|(_, c)| (c["metric"].as_str().unwrap_or("").to_string(), f(&c["estimate"]).signum()))
            .collect()
    }

    pub(super) fn insight_series(&self, s: &Value) -> Option<&'a Value> {
        self.r["insights"]["series"].as_array()?.iter().find(|x| x["series"] == s["label"])
    }

    /// The functions whose CPU time or allocations moved most, biggest first (1 ms against 1 MB).
    pub(super) fn movers_sorted(&self, s: &Value) -> Vec<&'a Value> {
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
    pub(super) fn method(&self) -> String {
        let r = self.r;
        let (b, h) = variants(r);
        let subj = |v: &Value| v["subject"].as_str().map(|s| format!(", *{}*", cell(&no_refs(s)))).unwrap_or_default();
        let mut t = String::from("#### How it was measured\n\n");
        let (bn, hn) = self.sides();
        if !b.is_null() {
            t += &format!("- **{bn}:** {}{}\n", code(&tag(&b)), subj(&b));
        }
        t += &format!("- **{hn}:** {}{}\n", code(&tag(&h)), subj(&h));
        if let Some(f) = r["fixture"].as_object() {
            t += &format!(
                "- Fixture: {}, one per block outside both process sets{}.\n",
                code(f.get("command").and_then(Value::as_str).unwrap_or("")),
                f.get("seed_sha256")
                    .and_then(Value::as_str)
                    .map(|h| format!(", seeded from {} (sha256 {})", code(f.get("seed").and_then(Value::as_str).unwrap_or("")), &h[..12.min(h.len())]))
                    .unwrap_or_default()
            );
        }
        let mut hw = vec![r["env"]["os"].as_str().unwrap_or("").to_string(), short_cpu(r["env"]["cpu"].as_str().unwrap_or(""))];
        hw.push(r["gpu"]["devices"][0].as_str().unwrap_or("").to_string());
        hw.push(r["env"]["power_plan"].as_str().map(|p| format!("{p} power plan")).unwrap_or_default());
        hw.push(r["browser"].as_str().unwrap_or("").replace('/', " "));
        if self.drew_frames() {
            hw.push(r["refresh_hz"].as_f64().map(|x| format!("{x:.0} Hz display")).unwrap_or_default());
        }
        if let Some(p) = r["plan"]["pin"].as_str().filter(|p| !p.is_empty()) {
            let n = r["plan"]["reserve"]["cpus"].as_u64().map(|m| format!(", other processes kept off its {} CPUs", m.count_ones())).unwrap_or_default();
            let p = p.split(" · ").next().unwrap_or(p);
            hw.push(match p.strip_prefix("unpinned") {
                Some(why) => format!("not pinned{why}"),
                None => format!("pinned to {p}{n}"),
            });
        }
        t += &format!("- **Machine:** {}\n", cell(&fmt::fields(hw).replace(" · ", ", ")));
        // What each build showed on screen (`bench::shown_line`), so a reader sees what was measured.
        for s in &self.series {
            if let Some(l) = crate::app::bench::shown_line(s) {
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
            let raw = w["text"].as_str().unwrap_or("");
            let (id, text) = if id == "Background" && !series.is_empty() {
                let (k, n) = self.busy_blocks();
                ("OtherLoad", format!("other processes used 25% of a core or more in {k} of {n} timed blocks (kept; see Pairs)"))
            } else if let (true, Some((_, said))) = (id == "NoPresentSource", permission(raw)) {
                ("NoPresentSource", said.to_string())
            } else {
                (id, plain_note(&first_clause(&scrub_paths(raw), 150)))
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
    pub(super) fn rerun_drawer(&self) -> String {
        let cmd = self.rerun();
        let (first, rest) = cmd.split_once('\n').unwrap_or((&cmd, ""));
        // One flag per line, so no line scrolls sideways.
        let first = first.replace(" --", " \\\n  --");
        let mut t = format!("```sh\n{first}{}{rest}\n```\n\n", if rest.is_empty() { "" } else { "\n" });
        t += &format!("- **Full report:** <code>strobe report experiment {} --open</code>\n", self.id);
        let profiled = self.series.iter().any(|s| s["attribution"]["head"].is_string());
        if profiled && crate::app::open::pr_line(self.r).is_some() {
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
    pub(super) fn rerun(&self) -> String {
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
                (Some(_), _) => return symbolic(s),
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
