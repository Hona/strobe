//! Number formatting with precision that follows the interval width (SPEC §9.3 "Digits").

pub fn value(v: f64, unit: &str) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let digits = if v.abs() >= 100.0 {
        0
    } else if v.abs() >= 10.0 {
        1
    } else {
        2
    };
    // A value that rounds to zero prints without a sign (not −0.00%).
    let v = if (v * 10f64.powi(digits as i32)).round() == 0.0 { 0.0 } else { v };
    match unit {
        "ms" => format!("{v:.digits$} ms"),
        "%" => format!("{v:.digits$}%"),
        "count" => {
            if (v - v.round()).abs() < 1e-9 {
                format!("{}", v.round() as i64)
            } else {
                format!("{v:.1}")
            }
        }
        "bytes" => bytes(v),
        "pp" => format!("{v:.digits$} pp"),
        "MB" => format!("{v:.0} MB"),
        "/s" => format!("{v:.digits$}/s"),
        // A number the target reported in a unit of its own (`metric.<name>`).
        "value" => format!("{v:.digits$}"),
        // Counts in the billions: 4.12 G instr.
        "instructions" => match v.abs() {
            a if a >= 1e9 => format!("{:.2} G instr", v / 1e9),
            a if a >= 1e7 => format!("{:.1} M instr", v / 1e6),
            a if a >= 1e6 => format!("{:.2} M instr", v / 1e6),
            a if a >= 1e3 => format!("{:.0} k instr", v / 1e3),
            _ => format!("{v:.0} instr"),
        },
        _ => format!("{v:.digits$} {unit}"),
    }
}

/// `counters 12 mJ CPU energy · 4.12 G instr · 3.0/s package idle wakeups` for what the OS counted
/// (SPEC §7.5), or nothing when it counted none of them.
pub fn counters(get: impl Fn(&str) -> f64) -> String {
    let parts = [("energy.cpu", "mJ", " CPU energy"), ("cpu.instructions", "instructions", ""), ("cpu.idle_wakes", "/s", " package idle wakeups")];
    let shown: Vec<String> = parts.iter().filter(|(m, _, _)| get(m).is_finite()).map(|(m, unit, what)| format!("{}{what}", value(get(m), unit))).collect();
    if shown.is_empty() { String::new() } else { format!("counters {}\n", shown.join(" · ")) }
}

pub use crate::fmt::bytes;

/// A relative change as a signed percentage, e.g. −7.7% or +12%.
pub fn pct(r: f64) -> String {
    if !r.is_finite() {
        return "—".into();
    }
    let p = r * 100.0;
    let body = if p.abs() < 10.0 { format!("{:.1}", p.abs()) } else { format!("{:.0}", p.abs()) };
    let sign = if p < -0.05 {
        "−"
    } else if p > 0.05 {
        "+"
    } else {
        ""
    };
    format!("{sign}{body}%")
}

pub fn signed(v: f64, unit: &str) -> String {
    let body = value(v.abs(), unit);
    // One that shows as zero has no sign (`0 MB`, not `−0 MB`).
    if body == value(0.0, unit) {
        body
    } else if v < 0.0 {
        format!("−{body}")
    } else if v > 0.0 {
        format!("+{body}")
    } else {
        body
    }
}

pub fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s} ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

/// Rows as columns as wide as their widest cell, `gap` spaces apart, each line starting with
/// `indent`, so a long value never pushes the columns after it out of line. `right[i]` right-aligns
/// column i (numbers); the last column is never padded. A row of one cell (a heading between rows)
/// is printed as it is and sizes nothing.
pub fn table(rows: &[Vec<String>], right: &[bool], indent: &str, gap: usize) -> String {
    let cols = rows.iter().filter(|r| r.len() > 1).map(Vec::len).max().unwrap_or(0);
    let mut width = vec![0usize; cols];
    for r in rows.iter().filter(|r| r.len() > 1) {
        for (i, c) in r.iter().enumerate() {
            width[i] = width[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in rows {
        if r.len() <= 1 {
            out += &format!("{}\n", r.first().map(String::as_str).unwrap_or(""));
            continue;
        }
        let mut line = indent.to_string();
        for (i, c) in r.iter().enumerate() {
            let fill = " ".repeat(width[i] - c.chars().count());
            let last = i + 1 == r.len();
            match (right.get(i).copied().unwrap_or(false), last) {
                (true, _) => line += &format!("{fill}{c}"),
                (false, true) => line += c,
                (false, false) => line += &format!("{c}{fill}"),
            }
            if !last {
                line += &" ".repeat(gap);
            }
        }
        out += line.trim_end();
        out.push('\n');
    }
    out
}

/// `a · b · c` without the fields that are empty (a native target has no browser, a tray app no window).
pub fn fields<S: AsRef<str>>(parts: impl IntoIterator<Item = S>) -> String {
    parts.into_iter().filter(|p| !p.as_ref().trim().is_empty()).map(|p| p.as_ref().trim().to_string()).collect::<Vec<_>>().join(" · ")
}

/// `1400×900`, or nothing without a window size.
pub fn size(w: &serde_json::Value) -> String {
    match (w["width"].as_f64(), w["height"].as_f64()) {
        (Some(x), Some(y)) if x > 0.0 && y > 0.0 => format!("{x:.0}×{y:.0}"),
        _ => String::new(),
    }
}

pub fn ms_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 120_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{:.1} min", ms as f64 / 60_000.0)
    }
}
