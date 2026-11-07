//! What strobe knows about a device before a run: what it is (model, SoC, Android, build, emulator or
//! not), how it is connected, its display, and the state a timed run depends on (battery, thermal
//! status, Battery Saver, screen and keyguard, stay awake). One shell round trip: `getprop` and the
//! system services' dumps, each section after a marker line.

use super::{Device, Transport, classify};
use crate::model::{Error, Result};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct Facts {
    pub serial: String,
    pub transport: Transport,
    pub manufacturer: String,
    pub model: String,
    /// `<manufacturer> <model>` of the SoC (Android 12+), else the board platform.
    pub soc: Option<String>,
    /// The hardware serial (`ro.serialno`): the same phone over USB and over Wi-Fi.
    pub hardware_serial: Option<String>,
    pub android: String,
    pub sdk: u32,
    pub fingerprint: String,
    /// `user`, `userdebug` or `eng`.
    pub build_type: String,
    pub debuggable: bool,
    /// Why strobe takes it for an emulator (`ro.boot.qemu=1`, …); None on a real device.
    pub emulator: Option<String>,
    pub abi: String,
    pub display: Option<Display>,
    pub battery: Option<Battery>,
    pub thermal: Option<Thermal>,
    /// Battery Saver (`settings global low_power`).
    pub power_save: Option<bool>,
    /// Whether the display is on and the device awake (`mWakefulness=Awake`).
    pub screen_on: Option<bool>,
    /// The default display's power state (`ON`, `OFF`, `DOZE`, `DOZE_SUSPEND`, …).
    pub display_state: Option<String>,
    /// Whether the keyguard (lock screen) shows.
    pub keyguard: Option<bool>,
    /// Whether unlocking needs a PIN, pattern or password (a person), not a swipe.
    pub secure_lock: Option<bool>,
    /// `stay_on_while_plugged_in`: 1 AC, 2 USB, 4 wireless, 8 dock (`svc power stayon`).
    pub stay_on: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub density: u32,
    /// The active mode's refresh rate.
    pub refresh_hz: Option<f64>,
    /// The highest refresh rate among its modes (a variable-refresh panel's top).
    pub max_refresh_hz: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Battery {
    pub level: u32,
    pub temp_c: Option<f64>,
    /// `charging`, `discharging`, `not_charging`, `full` or `unknown`.
    pub status: &'static str,
    /// What powers it: `ac`, `usb`, `wireless`, `dock`; None on battery.
    pub plugged: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Thermal {
    /// PowerManager's thermal status: 0 none, 1 light, 2 moderate, 3 severe, 4 critical, 5 emergency,
    /// 6 shutdown.
    pub status: u32,
    pub status_name: &'static str,
    /// The thermal headroom now (`cmd thermalservice headroom`): 1.0 is where severe throttling
    /// starts; None where the device gives none.
    pub headroom: Option<f64>,
    /// Whether the status is forced (`cmd thermalservice override-status`), not measured.
    pub overridden: bool,
}

const SECTIONS: &[(&str, &str)] = &[
    ("props", "getprop"),
    ("wm", "wm size; wm density"),
    ("display", "dumpsys display | grep -E 'DisplayDeviceInfo|Display State=' | head -n 20"),
    ("battery", "dumpsys battery"),
    ("thermal", "dumpsys thermalservice | grep -E '^ *(Thermal Status|IsStatusOverride)' | head -n 4"),
    ("headroom", "cmd thermalservice headroom 0"),
    ("power", "dumpsys power | grep -E 'mWakefulness=|Display Power: state=' | head -n 4"),
    // The window manager's policy part (a few KB, where the whole dump is tens): its keyguard state.
    ("keyguard", "dumpsys window policy | grep -E '^ *(showing|secure)=' | head -n 2"),
    ("low_power", "settings get global low_power"),
    ("stay_on", "settings get global stay_on_while_plugged_in"),
];

pub(super) fn read(d: &Device) -> Result<Facts> {
    let script: String = SECTIONS.iter().map(|(name, cmd)| format!("echo '@@strobe:{name}'; {cmd} 2>/dev/null;\n")).collect();
    let out = d.shell_for(&script, std::time::Duration::from_secs(20))?.stdout_text();
    let mut sec: BTreeMap<&str, String> = BTreeMap::new();
    let mut cur = "";
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("@@strobe:") {
            cur = SECTIONS.iter().find(|(n, _)| *n == name.trim()).map(|(n, _)| *n).unwrap_or("");
            continue;
        }
        let s = sec.entry(cur).or_default();
        s.push_str(line);
        s.push('\n');
    }
    let get = |k: &str| sec.get(k).map(String::as_str).unwrap_or("");
    let props = parse_props(get("props"));
    let p = |k: &str| props.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let ps = |k: &str| p(k).unwrap_or("").to_string();
    // Valid or nothing: a device whose properties did not come back is not reported as a clean one.
    let sdk: u32 = p("ro.build.version.sdk").and_then(|s| s.parse().ok()).unwrap_or(0);
    if props.is_empty() || sdk == 0 || p("ro.build.fingerprint").is_none() {
        return Err(Error::msg(format!("{}: could not read its system properties (getprop gave {} of them)", d.serial, props.len())).hint("check `adb shell getprop` on it"));
    }

    let emulator = emulator_evidence(&d.serial, &props);
    let soc = match (p("ro.soc.manufacturer"), p("ro.soc.model")) {
        (Some(m), Some(n)) => Some(format!("{m} {n}")),
        (None, Some(n)) => Some(n.to_string()),
        _ => p("ro.board.platform").or(p("ro.hardware")).map(str::to_string),
    };
    let transport = d.adb.devices().ok().and_then(|all| all.into_iter().find(|l| l.serial == d.serial)).map(|l| l.transport).unwrap_or_else(|| classify(&d.serial, false));
    let flag = |s: &str| match s.trim() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    };
    Ok(Facts {
        serial: d.serial.clone(),
        transport,
        manufacturer: ps("ro.product.manufacturer"),
        model: ps("ro.product.model"),
        soc,
        hardware_serial: p("ro.serialno").or(p("ro.boot.serialno")).map(str::to_string),
        android: ps("ro.build.version.release"),
        sdk,
        fingerprint: ps("ro.build.fingerprint"),
        build_type: ps("ro.build.type"),
        debuggable: p("ro.debuggable") == Some("1"),
        emulator,
        abi: ps("ro.product.cpu.abi"),
        display: parse_display(get("wm"), get("display")),
        battery: parse_battery(get("battery")),
        thermal: parse_thermal(get("thermal"), get("headroom")),
        power_save: flag(get("low_power")),
        screen_on: field_after(get("power"), "mWakefulness=").map(|w| w == "Awake"),
        display_state: get("display").lines().find_map(|l| l.trim().strip_prefix("Display State=")).map(|s| s.trim().to_string()),
        keyguard: line_value(get("keyguard"), "showing=").and_then(|v| flag(&v)),
        secure_lock: line_value(get("keyguard"), "secure=").and_then(|v| flag(&v)),
        stay_on: get("stay_on").trim().parse().ok(),
    })
}

/// `getprop` lines: `[key]: [value]`.
pub(super) fn parse_props(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once("]: [")?;
            Some((k.strip_prefix('[')?.to_string(), v.strip_suffix(']').unwrap_or(v).to_string()))
        })
        .collect()
}

/// An emulator says so in its properties (QEMU, its virtual boards); its serial alone may lie (a
/// device reached through `adb connect`).
fn emulator_evidence(serial: &str, props: &BTreeMap<String, String>) -> Option<String> {
    let p = |k: &str| props.get(k).map(|v| v.trim()).unwrap_or("");
    for k in ["ro.boot.qemu", "ro.kernel.qemu"] {
        if p(k) == "1" {
            return Some(format!("{k}=1"));
        }
    }
    for k in ["ro.hardware", "ro.boot.hardware", "ro.product.board", "ro.board.platform"] {
        let v = p(k);
        if ["ranchu", "goldfish", "cutf_cvm", "vsoc"].iter().any(|e| v.contains(e)) {
            return Some(format!("{k}={v}"));
        }
    }
    if p("ro.build.characteristics").split(',').any(|c| c == "emulator") {
        return Some(format!("ro.build.characteristics={}", p("ro.build.characteristics")));
    }
    if serial.starts_with("emulator-") {
        return Some(format!("serial {serial}"));
    }
    None
}

/// The value of the first `key=value` line (indented as dumps indent).
fn line_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| l.trim().strip_prefix(key)).map(|v| v.trim().to_string())
}

/// The text after `key` up to the next space, comma or bracket.
fn field_after(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)? + key.len();
    let v: String = text[at..].chars().take_while(|c| !c.is_whitespace() && !",}]".contains(*c)).collect();
    (!v.is_empty()).then_some(v)
}

/// `wm size` (`Physical size: 1080x2400`, an `Override size:` wins: it is what apps lay out in) and
/// `wm density`; the refresh rates from `dumpsys display`'s built-in display info.
fn parse_display(wm: &str, dump: &str) -> Option<Display> {
    let line = |prefix: &str| wm.lines().find_map(|l| l.trim().strip_prefix(prefix).map(|v| v.trim().to_string()));
    let size = line("Override size:").or_else(|| line("Physical size:"))?;
    let (w, h) = size.split_once('x')?;
    let density = line("Override density:").or_else(|| line("Physical density:")).and_then(|d| d.parse().ok()).unwrap_or(0);
    // The default display: the internal panel that may be the default, not a cast or virtual one.
    let infos: Vec<&str> = dump.lines().filter(|l| l.contains("DisplayDeviceInfo")).collect();
    let info = ["FLAG_ALLOWED_TO_BE_DEFAULT_DISPLAY", "type INTERNAL"].iter().find_map(|k| infos.iter().find(|l| l.contains(k))).or(infos.first()).copied().unwrap_or("");
    let num = |s: &str| s.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<f64>().ok();
    let fps: Vec<f64> = info.match_indices("fps=").filter_map(|(at, _)| num(&info[at + 4..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<String>())).collect();
    let refresh = field_after(info, "renderFrameRate ").and_then(|v| num(&v)).or_else(|| {
        let mode = field_after(info, "modeId ")?;
        let at = info.find(&format!("id={mode},"))?;
        field_after(&info[at..], "fps=").and_then(|v| num(&v))
    });
    Some(Display {
        width: w.trim().parse().ok()?,
        height: h.trim().parse().ok()?,
        density,
        refresh_hz: refresh,
        max_refresh_hz: fps.iter().copied().fold(None, |m: Option<f64>, f| Some(m.map_or(f, |m| m.max(f)))).or(refresh),
    })
}

/// `dumpsys battery`: `level: 87`, `temperature: 312` (tenths of a degree), `status: 2`, and which
/// of `AC powered`, `USB powered`, `Wireless powered`, `Dock powered` is true.
fn parse_battery(text: &str) -> Option<Battery> {
    let val = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k).and_then(|r| r.strip_prefix(':')).map(|v| v.trim().to_string()));
    let level = val("level")?.parse().ok()?;
    let status = match val("status").and_then(|s| s.parse::<u32>().ok()) {
        Some(2) => "charging",
        Some(3) => "discharging",
        Some(4) => "not_charging",
        Some(5) => "full",
        _ => "unknown",
    };
    let plugged = [("AC powered", "ac"), ("USB powered", "usb"), ("Wireless powered", "wireless"), ("Dock powered", "dock")].into_iter().find(|(k, _)| val(k).as_deref() == Some("true")).map(|(_, v)| v);
    Some(Battery { level, temp_c: val("temperature").and_then(|t| t.parse::<f64>().ok()).map(|t| t / 10.0), status, plugged })
}

fn parse_thermal(dump: &str, headroom: &str) -> Option<Thermal> {
    let status: u32 = dump.lines().find_map(|l| l.trim().strip_prefix("Thermal Status:")).and_then(|v| v.trim().parse().ok())?;
    let status_name = match status {
        0 => "none",
        1 => "light",
        2 => "moderate",
        3 => "severe",
        4 => "critical",
        5 => "emergency",
        6 => "shutdown",
        _ => "unknown",
    };
    // `Headroom in 0 seconds: 0.42`; NaN where the device has no forecast.
    let headroom = headroom.trim().rsplit_once(':').and_then(|(_, h)| h.trim().parse::<f64>().ok()).filter(|h| h.is_finite());
    let overridden = dump.lines().find_map(|l| l.trim().strip_prefix("IsStatusOverride:")).is_some_and(|v| v.trim() == "true");
    Some(Thermal { status, status_name, headroom, overridden })
}
