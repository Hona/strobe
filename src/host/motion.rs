//! The OS's own "reduce motion" setting, read and never changed (SPEC rule 10): what a target strobe
//! launches without `motion` sees, recorded as `env.reduced_motion`.

/// Whether the person turned the OS's animations off (Windows: "Animation effects" off,
/// `SPI_GETCLIENTAREAANIMATION`; macOS: Accessibility's Reduce motion; Linux: the settings portal's
/// `reduced-motion`, else the desktop's `enable-animations`, else GTK's `settings.ini`). None when
/// unknown, never a guess.
pub fn reduced_motion() -> Option<bool> {
    imp::reduced_motion()
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};

    pub fn reduced_motion() -> Option<bool> {
        let mut on: i32 = 0;
        let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
        (ok != 0).then_some(on == 0)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use crate::host::macos::cfstr;
    use std::ffi::c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFPreferencesAppSynchronize(app: *const c_void) -> u8;
        fn CFPreferencesGetAppBooleanValue(key: *const c_void, app: *const c_void, valid: *mut u8) -> u8;
    }

    /// One boolean preference, freshly read; None when the domain has no valid value for it.
    fn boolean(app: &str, key: &str) -> Option<bool> {
        let (key, app) = (cfstr(key), cfstr(app));
        if key.0.is_null() || app.0.is_null() {
            return None;
        }
        let mut valid = 0u8;
        let on = unsafe {
            CFPreferencesAppSynchronize(app.0);
            CFPreferencesGetAppBooleanValue(key.0, app.0, &mut valid)
        };
        (valid != 0).then_some(on != 0)
    }

    /// Accessibility's Reduce motion, without AppKit in strobe's process: `ReduceMotionEnabled` in
    /// `com.apple.Accessibility` (readable without Full Disk Access), else `reduceMotion` in
    /// `com.apple.universalaccess` when that holds one. Neither readable: unknown, never "off".
    pub fn reduced_motion() -> Option<bool> {
        boolean("com.apple.Accessibility", "ReduceMotionEnabled").or_else(|| boolean("com.apple.universalaccess", "reduceMotion"))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use crate::host::dbus::{Arg, Conn};
    use std::time::Duration;

    pub fn reduced_motion() -> Option<bool> {
        portal().or_else(gtk_ini)
    }

    /// The settings portal, within half a second each: the desktop-neutral `org.freedesktop.appearance
    /// reduced-motion` (1: reduce), else `org.gnome.desktop.interface enable-animations` (false: reduce),
    /// the value GTK apps follow.
    fn portal() -> Option<bool> {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        if !set("DBUS_SESSION_BUS_ADDRESS") && !set("XDG_RUNTIME_DIR") {
            return None;
        }
        let mut c = Conn::session().ok()?;
        c.set_timeout(Duration::from_millis(500)).ok()?;
        let mut read = |ns: &str, key: &str| {
            let args = [Arg::Str(ns), Arg::Str(key)];
            c.call("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings", "ReadOne", &args).ok()?.first()?.as_i64()
        };
        read("org.freedesktop.appearance", "reduced-motion").map(|v| v == 1).or_else(|| read("org.gnome.desktop.interface", "enable-animations").map(|on| on == 0))
    }

    /// GTK's own file (`$XDG_CONFIG_HOME/gtk-3.0/settings.ini`), for a session with no portal.
    fn gtk_ini() -> Option<bool> {
        let config = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| crate::host::home().join(".config"));
        let text = std::fs::read_to_string(config.join("gtk-3.0").join("settings.ini")).ok()?;
        let value = text.lines().find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == "gtk-enable-animations").map(|(_, v)| v.trim().to_ascii_lowercase()))?;
        Some(matches!(value.as_str(), "0" | "false"))
    }
}
