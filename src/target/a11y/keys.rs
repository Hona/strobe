//! What a page call asks for, before an OS turns it into input: buttons, modifiers and keys by
//! Playwright's names. Each OS's `input` module maps them to its own codes.

use crate::model::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(s: Option<&str>) -> Result<Button> {
        Ok(match s.unwrap_or("left") {
            "left" => Button::Left,
            "right" => Button::Right,
            "middle" => Button::Middle,
            other => return Err(Error::usage(format!("button: left, right or middle, not {other}"))),
        })
    }
}

/// Modifier keys held for an input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

/// `ControlOrMeta` is Command on macOS and Control elsewhere, as in Playwright.
fn control_or_meta(m: &mut Mods) {
    if cfg!(target_os = "macos") {
        m.meta = true;
    } else {
        m.ctrl = true;
    }
}

impl Mods {
    pub fn any(self) -> bool {
        self.ctrl || self.shift || self.alt || self.meta
    }

    pub fn or(self, o: Mods) -> Mods {
        Mods { ctrl: self.ctrl || o.ctrl, shift: self.shift || o.shift, alt: self.alt || o.alt, meta: self.meta || o.meta }
    }

    /// `["ctrl", "shift", "alt", "meta"]`, as click options give them.
    pub fn parse_list(v: &serde_json::Value) -> Result<Mods> {
        let mut m = Mods::default();
        for s in v.as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            match s.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => m.ctrl = true,
                "controlormeta" => control_or_meta(&mut m),
                "shift" => m.shift = true,
                "alt" | "option" => m.alt = true,
                "meta" | "win" | "super" | "command" | "cmd" => m.meta = true,
                other => return Err(Error::usage(format!("modifier {other}: ctrl, shift, alt or meta"))),
            }
        }
        Ok(m)
    }

    /// The modifier keys themselves, in the order they go down.
    pub fn keys(self) -> Vec<Named> {
        let mut v = Vec::new();
        if self.ctrl {
            v.push(Named::Control);
        }
        if self.shift {
            v.push(Named::Shift);
        }
        if self.alt {
            v.push(Named::Alt);
        }
        if self.meta {
            v.push(Named::Meta);
        }
        v
    }
}

/// A key by its name (Playwright's), independent of what it types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Space,
    Shift,
    Control,
    Alt,
    Meta,
    ContextMenu,
    CapsLock,
    /// F1 to F24.
    F(u8),
    /// `KeyA`: the key of an ASCII capital letter.
    Letter(u8),
    /// `Digit1`: the key of an ASCII digit.
    Digit(u8),
}

impl Named {
    pub fn modifier(self) -> bool {
        matches!(self, Named::Shift | Named::Control | Named::Alt | Named::Meta)
    }
}

/// A key: a named one, or a character the app's layout types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Named(Named),
    Char(char),
}

/// `"Enter"`, `"a"`, `"Control+Shift+P"`, `"ControlOrMeta+a"`, Playwright's names.
pub fn parse_chord(s: &str) -> Result<(Mods, Key)> {
    let parts: Vec<&str> = if s == "+" { vec!["+"] } else { s.split('+').collect() };
    let (last, mods) = parts.split_last().ok_or_else(|| Error::usage("an empty key"))?;
    let mut m = Mods::default();
    for p in mods {
        match p.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => m.ctrl = true,
            "controlormeta" => control_or_meta(&mut m),
            "shift" => m.shift = true,
            "alt" | "option" => m.alt = true,
            "meta" | "win" | "command" | "cmd" => m.meta = true,
            other => return Err(Error::usage(format!("key {s:?}: {other} is not a modifier (Control, Shift, Alt, Meta, ControlOrMeta)"))),
        }
    }
    Ok((m, key_named(last).ok_or_else(|| Error::usage(format!("key {s:?}: unknown key {last:?}")))?))
}

pub fn key_named(name: &str) -> Option<Key> {
    let n = match name {
        "Enter" => Named::Enter,
        "Tab" => Named::Tab,
        "Escape" | "Esc" => Named::Escape,
        "Backspace" => Named::Backspace,
        "Delete" => Named::Delete,
        "Insert" => Named::Insert,
        "Home" => Named::Home,
        "End" => Named::End,
        "PageUp" => Named::PageUp,
        "PageDown" => Named::PageDown,
        "ArrowUp" => Named::ArrowUp,
        "ArrowDown" => Named::ArrowDown,
        "ArrowLeft" => Named::ArrowLeft,
        "ArrowRight" => Named::ArrowRight,
        "Space" | " " => Named::Space,
        "Shift" => Named::Shift,
        "Control" => Named::Control,
        "Alt" => Named::Alt,
        "Meta" => Named::Meta,
        "ContextMenu" => Named::ContextMenu,
        "CapsLock" => Named::CapsLock,
        _ => {
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=24).contains(n)) {
                Named::F(n)
            } else if let Some(c) = name.strip_prefix("Key").filter(|c| c.len() == 1 && c.as_bytes()[0].is_ascii_uppercase()) {
                Named::Letter(c.as_bytes()[0])
            } else if let Some(c) = name.strip_prefix("Digit").filter(|c| c.len() == 1 && c.as_bytes()[0].is_ascii_digit()) {
                Named::Digit(c.as_bytes()[0])
            } else {
                let mut chars = name.chars();
                let c = chars.next()?;
                return chars.next().is_none().then_some(Key::Char(c));
            }
        }
    };
    Some(Key::Named(n))
}

/// Wheel notches for a delta in pixels: one notch per 100 px, Chromium's scroll per notch. What a
/// native app scrolls per notch is its own.
pub fn notches(px: f64) -> f64 {
    px / 100.0
}
