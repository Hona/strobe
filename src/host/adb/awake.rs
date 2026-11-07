//! What strobe holds on a phone while it runs: the screen kept on while plugged in (`svc power
//! stayon`), put back as the user had it, and the phone's slot, so two strobes never run on one phone
//! (from this computer or another).
//!
//! The setting to restore is journaled (owner-only) in `<state>/adb/stayon-<phone>.json` before it
//! changes, with what strobe set: a strobe killed mid-run has it put back by the next one that sees
//! the phone (`restore_stale`), over USB or Wi-Fi, but only while the phone still holds what strobe
//! set (a person who changed it since keeps theirs).

use super::{Adb, Conn, Device, State, journal_dir, owner_alive, quote, read_packet};
use crate::host::{self, paths, spawn};
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SETTING: &str = "stay_on_while_plugged_in";
/// Kept on with AC, USB and wireless power. `svc power stayon true` sets these (7), and dock (8)
/// too on Android 14 and later (15).
const ALL_POWER: u32 = 1 | 2 | 4;

/// A phone in a file name: its hardware serial (the same over USB and Wi-Fi), else the adb serial,
/// with anything but letters, digits, `.`, `_`, `-` as `_`.
fn phone_key(d: &Device, hw: Option<&str>) -> String {
    hw.unwrap_or(&d.serial).chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

fn journal(key: &str) -> PathBuf {
    journal_dir().join(format!("stayon-{key}.json"))
}

/// The setting: None when it was never set (`null`), else its bits (0 to 15).
fn read_setting(d: &Device) -> Result<Option<u32>> {
    let text = d.sh(&format!("settings get global {SETTING}"))?;
    match text.trim() {
        "null" => Ok(None),
        t => match t.parse::<u32>() {
            Ok(v) if v <= 15 => Ok(Some(v)),
            _ => Err(Error::msg(format!("{}: {SETTING} reads {t:?}, not a setting strobe knows", d.serial))),
        },
    }
}

fn put_setting(d: &Device, v: Option<u32>) -> Result<()> {
    let cmd = match v {
        Some(v) => format!("settings put global {SETTING} {v}"),
        None => format!("settings delete global {SETTING}"),
    };
    d.sh(&cmd).map(|_| ())
}

fn setting_json(v: Option<u32>) -> Value {
    v.map_or(Value::Null, |v| json!(v))
}

/// Puts `original` back if the phone still holds `applied` (None: strobe was killed before it read
/// back what it set, so any setting that keeps it on is taken for strobe's). Whether the journal is
/// done with (put back, or changed by a person since).
fn put_back(d: &Device, original: Option<u32>, applied: Option<u32>) -> bool {
    let Ok(now) = read_setting(d) else { return false };
    let ours = match applied {
        Some(a) => now == Some(a),
        None => now.is_some_and(|n| n & ALL_POWER == ALL_POWER) && now != original,
    };
    !ours || put_setting(d, original).is_ok()
}

/// The journal's original setting (`null`: never set).
fn original_of(v: &Value) -> Option<u32> {
    v["original"].as_u64().map(|o| o as u32)
}

/// `restore_stale` for one stay-awake journal of a strobe that no longer runs: the attached phone
/// with its hardware serial (it may have come back over another transport), else nothing yet.
pub(super) fn restore_journal(adb: &Adb, path: &Path, v: &Value) {
    let (serial, hw) = (v["serial"].as_str().unwrap_or(""), v["hw_serial"].as_str());
    let attached = adb.devices().unwrap_or_default();
    let ready = attached.iter().filter(|l| l.state == State::Device);
    // The same serial first: no shell needed to know it.
    let found = ready.clone().find(|l| l.serial == serial && hw.is_none()).map(|l| adb.device(&l.serial)).or_else(|| {
        let hw = hw?;
        ready.map(|l| adb.device(&l.serial)).find(|d| d.hardware_serial().as_deref() == Some(hw))
    });
    let Some(d) = found else { return };
    if put_back(&d, original_of(v), v["applied"].as_u64().map(|a| a as u32)) {
        let _ = std::fs::remove_file(path);
    }
}

/// While alive, the phone's screen stays on while it is plugged in (to a charger, USB or a wireless
/// charger); on battery it sleeps as it would. Dropping it puts the user's setting back.
pub struct StayAwake {
    device: Device,
    path: PathBuf,
    /// The user's setting and what strobe set, when strobe changed it.
    saved: Option<(Option<u32>, u32)>,
}

impl StayAwake {
    /// Keeps `d` awake. A phone already kept on with any power is left as it is.
    pub fn hold(d: &Device) -> Result<StayAwake> {
        super::restore_stale();
        let hw = d.hardware_serial();
        let path = journal(&phone_key(d, hw.as_deref()));
        let left = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if let Some(owner) = left.as_ref().and_then(|v| v["owner"].as_str()).filter(|o| owner_alive(o)) {
            return Err(Error::msg(format!("another strobe ({owner}) keeps {} awake", d.serial)).hint("one strobe runs on a phone at a time"));
        }
        // A journal a killed strobe left that could not be put back yet: the user's setting is in it,
        // not on the phone (which holds what that strobe set).
        let adopted = left.as_ref().map(original_of);
        let now = read_setting(d)?;
        if adopted.is_none() && now.is_some_and(|n| n & ALL_POWER == ALL_POWER) {
            return Ok(StayAwake { device: d.clone(), path, saved: None });
        }
        let original = adopted.unwrap_or(now);
        let write = |applied: Option<u32>| {
            let body = json!({ "owner": spawn::owner_stamp(), "serial": d.serial, "hw_serial": hw, "original": setting_json(original), "applied": setting_json(applied) });
            paths::write_owner_only(&path, body.to_string().as_bytes()).map_err(|e| Error::msg(format!("could not journal {}'s stay-awake setting: {e}", d.serial)))
        };
        // Journal first: a strobe killed between the two still has it put back.
        write(None)?;
        let applied = d.sh("svc power stayon true").and_then(|_| read_setting(d)).and_then(|a| a.ok_or_else(|| Error::msg(format!("{}: `svc power stayon true` left {SETTING} unset", d.serial))));
        let applied = match applied {
            Ok(a) => a,
            Err(e) => {
                if adopted.is_none() {
                    let _ = std::fs::remove_file(&path);
                }
                return Err(e);
            }
        };
        write(Some(applied))?;
        Ok(StayAwake { device: d.clone(), path, saved: Some((original, applied)) })
    }

    /// Whether strobe changed the setting (and puts it back when dropped).
    pub fn changed(&self) -> bool {
        self.saved.is_some()
    }
}

impl Drop for StayAwake {
    fn drop(&mut self) {
        if let Some((original, applied)) = self.saved.take()
            && put_back(&self.device, original, Some(applied))
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The phone's slot: one strobe at a time drives a phone. Two locks: on this computer an exclusive
/// lock on `<run>/phones/<phone>.slot` (who holds it beside it in `<phone>.json`), and on the phone a
/// `flock` on `/data/local/tmp/.strobe-phone.lock` held by a shell whose stream stays open (who holds
/// it in `.strobe-phone.json`), so a strobe on another computer sees it too. The OS frees the first
/// when its holder dies, the phone's kernel the second when the stream closes (strobe ends, the cable
/// or the adb server goes).
pub struct PhoneSlot {
    file: Option<File>,
    info: PathBuf,
    /// The shell holding the phone's lock: dropping it closes the stream.
    _held: Conn,
}

fn slot_dir() -> PathBuf {
    let d = paths::run().join("phones");
    let _ = std::fs::create_dir_all(&d);
    d
}

const DEVICE_LOCK: &str = "/data/local/tmp/.strobe-phone.lock";
const DEVICE_INFO: &str = "/data/local/tmp/.strobe-phone.json";

/// Takes the phone's own lock: the open stream when it got it, else who holds it.
fn device_lock(d: &Device, holder: &Value) -> Result<std::result::Result<Conn, Value>> {
    // The group's redirection, not `exec 9>>`: the shell opens `exec`'s descriptors close-on-exec, so
    // `flock` would not get it. `cat` keeps the shell, and its descriptor, until the stream closes.
    let script = format!(
        "{{ if flock -n 9; then echo {} > {DEVICE_INFO}; echo ok; cat >/dev/null; else echo busy; cat {DEVICE_INFO}; fi; }} 9>>{DEVICE_LOCK}",
        quote(&holder.to_string())
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut c = d.open_shell(&script, Duration::from_secs(10))?;
    let mut out = Vec::new();
    loop {
        match read_packet(&mut c, deadline)? {
            (1, data) => out.extend_from_slice(&data),
            (3, _) => break,
            _ => {}
        }
        if out.starts_with(b"ok\n") {
            // Held: the stream stays open, with no deadline.
            let _ = c.s.set_read_timeout(None);
            return Ok(Ok(c));
        }
        if out.len() > 64 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&out);
    match text.strip_prefix("busy\n") {
        Some(info) => Ok(Err(serde_json::from_str(info.trim()).unwrap_or_else(|_| json!({ "serial": d.serial })))),
        None => Err(Error::msg(format!("{}: the phone's lock did not answer ({:?})", d.serial, text.trim())).hint("strobe needs toybox's flock on the phone (Android 7 and later)")),
    }
}

impl PhoneSlot {
    /// Waits for the slot for up to `wait`. A stop (`strobe bench --stop`, Ctrl-C) ends the wait.
    pub fn take(d: &Device, what: &str, wait: Duration) -> Result<PhoneSlot> {
        let key = phone_key(d, d.hardware_serial().as_deref());
        let dir = slot_dir();
        let file = File::options().create(true).truncate(false).write(true).open(dir.join(format!("{key}.slot")))?;
        let info = dir.join(format!("{key}.json"));
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_secs()).unwrap_or(0);
        let me = json!({ "host": host::device_name(), "owner": spawn::owner_stamp(), "serial": d.serial, "what": what, "since": since });
        let until = Instant::now() + wait;
        let mut asked_phone: Option<Instant> = None;
        let mut last = json!({ "serial": d.serial });
        loop {
            if crate::app::stop_requested() {
                return Err(crate::app::stop_error());
            }
            let holder = if file.try_lock().is_ok() {
                // The phone is asked at most every 2 s: a phone another computer runs on is under test.
                if asked_phone.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    let _ = file.unlock();
                    None
                } else {
                    asked_phone = Some(Instant::now());
                    match device_lock(d, &me) {
                        Ok(Ok(held)) => {
                            let _ = paths::write_atomic(&info, me.to_string().as_bytes());
                            return Ok(PhoneSlot { file: Some(file), info, _held: held });
                        }
                        Ok(Err(holder)) => {
                            let _ = file.unlock();
                            Some(holder)
                        }
                        Err(e) => {
                            let _ = file.unlock();
                            return Err(e);
                        }
                    }
                }
            } else {
                Some(std::fs::read_to_string(&info).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or_else(|| json!({ "serial": d.serial })))
            };
            if let Some(h) = holder {
                last = h;
            }
            if Instant::now() >= until {
                let h = &last;
                let field = |k: &str| h[k].as_str().unwrap_or("?").to_string();
                return Err(Error::refused(
                    "ConcurrentBench",
                    format!("{} is in use by another strobe ({} on {}: {})", d.serial, field("owner"), field("host"), field("what")),
                    "wait for it to finish; one strobe runs on a phone at a time",
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for PhoneSlot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.info);
        if let Some(f) = self.file.take() {
            let _ = f.unlock();
        }
    }
}
