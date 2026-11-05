//! `strobe helper` (hidden, SPEC §5.11, §13.4): install and remove the deep-mode helper (elevated
//! steps `up` and `leave` run through one UAC prompt), and `serve`, which its scheduled task runs.

use super::Output;
use crate::cli::Args;
use crate::model::{Error, Result};
#[cfg(windows)]
use serde_json::json;

pub fn run(args: &Args) -> Result<Output> {
    #[cfg(windows)]
    {
        use crate::probe::helper;
        let sid = args.get("sid").map(str::to_string).or_else(helper::user_sid).ok_or_else(|| Error::usage("helper needs --sid"))?;
        let result = match args.pos.first().map(String::as_str) {
            Some("install") => helper::install(&sid),
            Some("uninstall") => helper::uninstall(&sid),
            Some("serve") => {
                let path = crate::host::paths::sub("log").join("helper.log");
                if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
                    let _ = std::fs::rename(&path, path.with_extension("log.old"));
                }
                let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
                helper::serve(&sid, &mut log).map(|_| String::new())
            }
            _ => return Err(Error::usage("helper install|uninstall|serve [--sid <SID>] [--log <file>]")),
        };
        // An elevated step runs in its own console: its result goes to the file the caller named.
        if let Some(log) = args.get("log") {
            let line = match &result {
                Ok(t) => json!({ "ok": t }),
                Err(e) => json!({ "err": e.message }),
            };
            let _ = std::fs::write(log, line.to_string());
        }
        let text = result?;
        Ok(Output::new(text.clone(), json!({ "ok": text })))
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(Error::usage("the deep-mode helper is Windows only for now"))
    }
}

/// `up`'s step: present events for this user, and the helper for native stacks. Prompts once when
/// anything is missing (the helper is absent, from another build, or the user is not in
/// Performance Log Users). Returns (ok, detail).
#[cfg(windows)]
pub fn setup(skip: bool) -> (&'static str, String) {
    use crate::probe::helper;
    let a = crate::probe::present::access();
    let current = a.helper && helper::helper_current();
    let grouped = a.token != "none" || a.pending_sign_out;
    if current && grouped {
        return ("✓", describe(&a));
    }
    if skip {
        return ("!", format!("skipped: {}", describe(&a)));
    }
    let Some(sid) = helper::user_sid() else { return ("✗", "read this user's SID".into()) };
    let log = crate::host::paths::run().join(format!("helper-install-{}.json", std::process::id()));
    crate::app::progress("deep mode: approve the UAC prompt (present events and native stacks)");
    let code = helper::elevated(&format!("helper install --sid {sid} --log \"{}\"", log.display()));
    let reply: serde_json::Value = std::fs::read_to_string(&log).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    match code {
        Ok(0) => {
            let a = crate::probe::present::access();
            ("✓", format!("{} · {}", reply["ok"].as_str().unwrap_or("installed"), describe(&a)))
        }
        Ok(_) => ("✗", format!("helper install failed: {}", reply["err"].as_str().unwrap_or("see the elevated window"))),
        Err(e) => ("!", format!("{}: present events and native stacks stay off (strobe up asks again)", e.message)),
    }
}

/// What this user can do today, for `up` and `status`.
pub fn describe(a: &crate::probe::present::Access) -> String {
    let presents = match (a.token, a.pending_sign_out, a.helper) {
        ("admin", _, _) => "present events in-process (elevated)",
        ("group", _, _) => "present events in-process (Performance Log Users)",
        (_, true, true) => "present events through the helper until you sign out and in once (Performance Log Users)",
        (_, true, false) => "present events after you sign out and in once (Performance Log Users)",
        (_, false, true) => "present events through the helper",
        _ => "no present events (strobe up)",
    };
    let stacks = match (a.helper, cfg!(target_arch = "x86_64")) {
        (true, _) => "through the helper",
        (false, true) => "without the helper: strobe suspends threads itself (user frames only; strobe up for the kernel's sampler)",
        (false, false) => "off (strobe up)",
    };
    #[cfg(windows)]
    if a.helper && !crate::probe::helper::helper_current() {
        return format!("{presents} · native stacks {stacks} · the installed helper is another build: strobe up replaces it");
    }
    format!("{presents} · native stacks {stacks}")
}
