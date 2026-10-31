//! `strobe.json`: the project's file, over the user's own (SPEC §9.5).

use crate::host;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The project root for `dir`: the nearest ancestor with a strobe.json, else the git root, else `dir`.
pub fn project_root(dir: &Path) -> PathBuf {
    let found = dir.ancestors().find(|d| d.join("strobe.json").is_file());
    if let Some(d) = found {
        return d.to_path_buf();
    }
    host::output(Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"])).map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf())
}

/// The project's strobe.json over the user's (`<state>/strobe.json`). `apps` merge per app, so a
/// user can keep profiles for apps they measure from anywhere.
pub fn load(root: &Path) -> Value {
    let user = read(&host::paths::state_dir().join("strobe.json"));
    let project = read(&root.join("strobe.json"));
    let mut out = user.as_object().cloned().unwrap_or_default();
    let mut apps = out.get("apps").and_then(Value::as_object).cloned().unwrap_or_default();
    for (k, v) in project.as_object().cloned().unwrap_or_default() {
        if k == "apps" {
            apps.extend(v.as_object().cloned().unwrap_or_default());
            continue;
        }
        out.insert(k, v);
    }
    out.insert("apps".into(), Value::Object(apps));
    Value::Object(out)
}

fn read(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&strip_jsonc(&t)).ok()).unwrap_or(Value::Object(Map::new()))
}

/// Drops `//` comment lines, so strobe.json can explain itself.
fn strip_jsonc(t: &str) -> String {
    t.lines().map(|l| if l.trim_start().starts_with("//") { "" } else { l }).collect::<Vec<_>>().join("\n")
}
