//! The power plan for a timed run (SPEC §9.4): High performance by default, the user's own plan back
//! afterwards. The plan to restore is journaled in `<state>/power.json` before anything changes, so a
//! strobe that was killed mid-run puts it back the next time any strobe starts (`restore_stale`).

use serde_json::{Value, json};

/// Windows' own schemes and Windows 11 power modes (overlays on Balanced).
const BALANCED: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
const HIGH: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
const ULTIMATE: &str = "e9a42b02-d5df-448d-aa00-03f14749eb61";
const BEST_PERFORMANCE: &str = "ded574b5-45a0-4f42-8737-46345c09c238";

/// While alive, the run's power plan; dropping it restores the user's.
pub struct Power {
    /// The plan the run uses, as the env line shows it.
    pub used: String,
    /// Why the user's plan was kept, when it was (a context note).
    pub kept: Option<String>,
    saved: Option<Saved>,
}

#[derive(Clone, Debug, PartialEq)]
struct Saved {
    scheme: String,
    overlay: Option<String>,
}

fn journal() -> std::path::PathBuf {
    super::paths::state_dir().join("power.json")
}

impl Power {
    /// Switches to the fastest plan this device offers unless `keep` (`--power keep`) or on battery:
    /// a High performance scheme (the active vendor's, else Windows', else Ultimate performance), or
    /// on a device with only Balanced, Windows 11's Best performance power mode. Already on one: kept.
    pub fn for_run(keep: bool) -> Power {
        let plan = super::env_power_plan();
        let kept = |why: String| Power { used: plan.clone(), kept: Some(why), saved: None };
        if keep {
            return kept(format!("{plan} (kept: --power keep)"));
        }
        if super::on_battery() {
            return kept(format!("{plan} (kept: on battery, where a faster plan drains it and clocks follow the battery anyway)"));
        }
        #[cfg(windows)]
        {
            imp::switch(&plan)
        }
        #[cfg(not(windows))]
        {
            kept(format!("{plan} (kept: strobe switches power plans on Windows only)"))
        }
    }
}

impl Drop for Power {
    fn drop(&mut self) {
        if let Some(s) = self.saved.take() {
            #[cfg(windows)]
            imp::restore(&s);
            let _ = std::fs::remove_file(journal());
            let _ = s;
        }
    }
}

/// Puts back a plan a strobe that is no longer running switched away from (killed mid-run).
pub fn restore_stale() {
    let Some(v) = std::fs::read_to_string(journal()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
    let pid = v["pid"].as_u64().unwrap_or(0) as u32;
    let alive = pid != 0 && super::process_tree(pid).first().is_some_and(|p| p.name.to_lowercase().starts_with("strobe"));
    if alive {
        return;
    }
    let s = Saved { scheme: v["scheme"].as_str().unwrap_or("").to_string(), overlay: v["overlay"].as_str().map(str::to_string) };
    #[cfg(windows)]
    if !s.scheme.is_empty() {
        imp::restore(&s);
    }
    let _ = s;
    let _ = std::fs::remove_file(journal());
}

fn save(s: &Saved) -> bool {
    std::fs::write(journal(), json!({ "pid": std::process::id(), "scheme": s.scheme, "overlay": s.overlay }).to_string()).is_ok()
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::process::{Command, Stdio};
    use windows_sys::core::GUID;

    /// `powercfg /list`: (GUID, name) of every scheme.
    fn schemes() -> Vec<(String, String)> {
        let text = super::super::output(Command::new("powercfg").arg("/list")).unwrap_or_default();
        text.lines()
            .filter_map(|l| {
                let guid = l.split_whitespace().find(|w| w.len() == 36 && w.matches('-').count() == 4)?.to_lowercase();
                let name = l.split_once('(').and_then(|(_, r)| r.rsplit_once(')')).map(|(n, _)| n.trim().to_string()).unwrap_or_default();
                Some((guid, name))
            })
            .collect()
    }

    fn active() -> Option<String> {
        let text = super::super::output(Command::new("powercfg").arg("/getactivescheme"))?;
        text.split_whitespace().find(|w| w.len() == 36 && w.matches('-').count() == 4).map(str::to_lowercase)
    }

    fn set_scheme(guid: &str) -> bool {
        Command::new("powercfg").args(["/setactive", guid]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    fn text(g: &GUID) -> String {
        format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
            g.data1,
            g.data2,
            g.data3,
            g.data4[0],
            g.data4[1],
            g.data4[2..].iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    }

    fn parse(s: &str) -> Option<GUID> {
        let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (h.len() == 32).then(|| GUID::from_u128(u128::from_str_radix(&h, 16).unwrap_or(0)))
    }

    /// powrprof's power-mode calls (Windows 10 1709 and later), looked up at run time.
    fn proc(name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        let lib: Vec<u16> = "powrprof.dll\0".encode_utf16().collect();
        unsafe {
            let h = LoadLibraryW(lib.as_ptr());
            if h.is_null() {
                return None;
            }
            GetProcAddress(h, name.as_ptr())
        }
    }

    /// The power mode in effect (an overlay GUID; all zeros is Balanced's own "Balanced" mode).
    pub fn overlay() -> Option<String> {
        let f = proc(b"PowerGetEffectiveOverlayScheme\0").or_else(|| proc(b"PowerGetActualOverlayScheme\0"))?;
        let f: unsafe extern "system" fn(*mut GUID) -> u32 = unsafe { std::mem::transmute(f) };
        let mut g = GUID::from_u128(0);
        (unsafe { f(&mut g) } == 0).then(|| text(&g))
    }

    fn set_overlay(guid: &str) -> bool {
        let (Some(f), Some(g)) = (proc(b"PowerSetActiveOverlayScheme\0"), parse(guid)) else { return false };
        let f: unsafe extern "system" fn(GUID) -> u32 = unsafe { std::mem::transmute(f) };
        unsafe { f(g) == 0 }
    }

    /// The Windows 11 power mode's name, for the env line.
    pub fn mode_name(guid: &str) -> Option<&'static str> {
        match guid {
            BEST_PERFORMANCE => Some("Best performance"),
            "961cc777-2547-4f9d-8174-7d86181b8a7a" => Some("Best power efficiency"),
            "3af9b8d9-7c97-431d-ad78-34a8bfea439f" => Some("Battery saver"),
            _ => None,
        }
    }

    pub fn switch(plan: &str) -> Power {
        let kept = |why: String| Power { used: plan.to_string(), kept: Some(why), saved: None };
        let Some(current) = active() else { return kept(format!("{plan} (kept: powercfg could not read the active plan)")) };
        let list = schemes();
        let name_of = |g: &str| list.iter().find(|(x, _)| x == g).map(|(_, n)| n.clone()).unwrap_or_default();
        let fast = |g: &str| g == HIGH || g == ULTIMATE || name_of(g).to_lowercase().contains("high performance");
        if fast(&current) {
            return Power { used: plan.to_string(), kept: None, saved: None };
        }
        let saved = Saved { scheme: current.clone(), overlay: overlay() };
        // The active vendor's own (AMD Ryzen Balanced → AMD Ryzen High Performance), else Windows'.
        let vendor = name_of(&current).to_lowercase().replace("balanced", "high performance");
        let target = list
            .iter()
            .find(|(g, n)| *g != current && n.to_lowercase() == vendor)
            .or_else(|| list.iter().find(|(g, _)| g == HIGH))
            .or_else(|| list.iter().find(|(g, _)| g == ULTIMATE))
            .or_else(|| list.iter().find(|(_, n)| n.to_lowercase().contains("high performance")))
            .cloned();
        if !save(&saved) {
            return kept(format!("{plan} (kept: strobe could not journal it to put it back)"));
        }
        let done = match &target {
            Some((g, _)) => set_scheme(g),
            // Only Balanced here (modern standby devices): Windows 11's Best performance mode.
            None if current == BALANCED => set_overlay(BEST_PERFORMANCE),
            None => false,
        };
        if !done {
            let _ = std::fs::remove_file(journal());
            return kept(format!("{plan} (kept: this device offers no High performance plan or power mode strobe could switch to)"));
        }
        Power { used: super::super::env_power_plan(), kept: None, saved: Some(saved) }
    }

    pub fn restore(s: &Saved) {
        if active().as_deref() != Some(s.scheme.as_str()) {
            set_scheme(&s.scheme);
        }
        if let Some(o) = &s.overlay
            && overlay().as_deref() != Some(o.as_str())
        {
            set_overlay(o);
        }
    }
}

/// The power mode on top of the plan, for the env line ("Balanced · Best performance").
pub fn mode() -> Option<&'static str> {
    #[cfg(windows)]
    {
        imp::overlay().and_then(|g| imp::mode_name(&g))
    }
    #[cfg(not(windows))]
    {
        None
    }
}
