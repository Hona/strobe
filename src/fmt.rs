//! Formatting shared by modules below `app` (which re-exports it in `app::fmt`).

/// Bytes in binary units: `512 B`, `12 KB`, `3.4 MB`, `1.25 GB`.
pub fn bytes(v: f64) -> String {
    let a = v.abs();
    if a >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", v / (1024.0 * 1024.0 * 1024.0))
    } else if a >= 1024.0 * 1024.0 {
        format!("{:.1} MB", v / (1024.0 * 1024.0))
    } else if a >= 1024.0 {
        format!("{:.0} KB", v / 1024.0)
    } else {
        format!("{v:.0} B")
    }
}
