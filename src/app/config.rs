//! `strobe config [<dir>]`: checks the project's strobe.json, over the user's own, without a bench
//! (SPEC §5, §9.5): the strict read `config::load` does, then every app profile resolved for this OS
//! the way a launch resolves it (its executable and guards, whose variables must be set), and what
//! the build and serve sections run here.

use super::Output;
use crate::cli::Args;
use crate::model::{Error, Result};
use crate::target::isolate;
use serde_json::json;
use std::path::PathBuf;

pub fn run(args: &Args) -> Result<Output> {
    let dir = match args.pos.as_slice() {
        [] => std::env::current_dir()?,
        [d] => PathBuf::from(d),
        [_, extra, ..] => return Err(Error::usage(format!("config takes one folder at most, not also {extra}"))),
    };
    let root = crate::config::project_root(&dir);
    let project = root.join("strobe.json");
    let user = crate::host::paths::state_path().join("strobe.json");
    let config = crate::config::load(&root)?;
    let mut text = String::new();
    let found = |p: &PathBuf| if p.is_file() { p.display().to_string() } else { format!("{} (none)", p.display()) };
    text += &format!("project  {}\nuser     {}\n", found(&project), found(&user));
    let mut apps = Vec::new();
    let mut failed = 0;
    for id in config["apps"].as_object().map(|a| a.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        match isolate::app(&id, &config) {
            Ok(a) => {
                // A relative executable is the project's own build, from its root.
                let path = if a.exe.is_relative() { root.join(&a.exe) } else { a.exe.clone() };
                let exe = if path.is_file() { a.exe.display().to_string() } else { format!("{} (not there yet: built, or installed, before a launch)", a.exe.display()) };
                let extra = [a.windowless.then_some("window: false".to_string()), a.gpu_cache_mode.as_ref().map(|g| format!("gpuCache {g}"))].into_iter().flatten().collect::<Vec<_>>();
                text += &format!("app      {id} · {} · {exe}{}\n", a.kind, extra.iter().map(|e| format!(" · {e}")).collect::<String>());
                apps.push(json!({ "id": id, "kind": a.kind, "exe": a.exe, "exe_found": path.is_file(), "window": !a.windowless, "gpu_cache": a.gpu_cache_mode }));
            }
            Err(e) => {
                failed += 1;
                text += &format!("app      {id} · {}{}\n", e.message, e.hint.as_ref().map(|h| format!("\n         fix: {h}")).unwrap_or_default());
                apps.push(json!({ "id": id, "error": e.message }));
            }
        }
    }
    let build = isolate::build_for_os(&config["build"]);
    // A variant's builds and server run in its checkout: their `cwd` and `output` are from its root, not
    // from the folder of strobe.json (SPEC §9.5).
    let checkout = crate::host::git::toplevel(&root).unwrap_or_else(|| root.clone());
    let at = |cwd: &serde_json::Value| checkout.join(cwd.as_str().unwrap_or(".")).display().to_string();
    if let Some(cmd) = build["command"].as_str() {
        // An output given per OS that names none for this OS leaves a build here with nothing to launch.
        if build["output"].is_object() && isolate::per_os(&build["output"]).is_none() {
            failed += 1;
            text += &format!("build    {cmd} · output names no path for {}: a variant built here has nothing to launch\n", isolate::os());
        } else {
            text += &format!("build    {cmd} · in {}{}\n", at(&build["cwd"]), isolate::per_os(&build["output"]).map(|o| format!(" → {}", checkout.join(o).display())).unwrap_or_default());
        }
    }
    if let Some(cmd) = config["serve"]["command"].as_str() {
        text += &format!("serve    {cmd} · in {}\n", at(&config["serve"]["cwd"]));
    }
    let out = json!({ "project": project, "user": user, "root": root, "checkout": checkout, "apps": apps, "build": build, "serve": config["serve"], "ok": failed == 0 });
    if failed > 0 {
        return Err(Error::usage(format!("{text}{failed} problem{} for this machine", if failed == 1 { "" } else { "s" })));
    }
    text += "ok       strobe.json reads as strobe reads it\n";
    Ok(Output::new(text, out))
}
