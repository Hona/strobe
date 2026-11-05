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
        _ => format!("{v:.digits$} {unit}"),
    }
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
    if v < 0.0 {
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
