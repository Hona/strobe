//! This build's identity, stamped by `build.rs` from its sources, target and profile: the same in
//! `strobe` and `strobew`, so any command can ask whether the daemon or the deep-mode helper is this
//! build. `up` restarts a daemon and replaces a helper whose build is not this one (one version
//! number covers many builds). Kept in the executable behind a marker, so `file_build` reads it from
//! an installed copy without running it.

#[used]
static BUILD_MARK: &str = concat!("strobe-build-id:", env!("STROBE_BUILD_ID"));

const MARK_PREFIX: &str = "strobe-build-id:";

pub fn build_id() -> &'static str {
    &BUILD_MARK[MARK_PREFIX.len()..]
}

/// The build id stamped in an executable, when it has one (builds before the id had none).
pub fn file_build(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let needle = MARK_PREFIX.as_bytes();
    let mut at = 0;
    while let Some(i) = bytes[at..].windows(needle.len()).position(|w| w == needle) {
        let start = at + i + needle.len();
        if let Some(id) = bytes.get(start..start + 16).filter(|id| id.iter().all(u8::is_ascii_hexdigit)) {
            return Some(String::from_utf8_lossy(id).to_string());
        }
        at = start;
    }
    None
}
