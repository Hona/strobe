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

/// A mouse button as `page.down` and `page.up` name it: `Mouse` (the left one), `MouseRight`,
/// `MouseMiddle`.
pub fn mouse_button(name: &str) -> Option<Button> {
    match name {
        "Mouse" | "MouseLeft" => Some(Button::Left),
        "MouseRight" => Some(Button::Right),
        "MouseMiddle" => Some(Button::Middle),
        _ => None,
    }
}

/// Points evenly spread over a drag (`page.drag`'s path: offsets from where it starts) at `t` (0 to 1),
/// straight between neighbours.
pub fn path_at(path: &[(f64, f64)], t: f64) -> (f64, f64) {
    match path.len() {
        0 => (0.0, 0.0),
        1 => path[0],
        n => {
            let f = t.clamp(0.0, 1.0) * (n - 1) as f64;
            let i = (f.floor() as usize).min(n - 2);
            let u = f - i as f64;
            (path[i].0 + (path[i + 1].0 - path[i].0) * u, path[i].1 + (path[i + 1].1 - path[i].1) * u)
        }
    }
}

/// `page.drag`'s path from the runner: `[[dx, dy], …]` (a function's samples, or keyframes), starting
/// where the drag presses: offset (0, 0), put first when the path does not start there. Keyframes
/// such as `[[300, 0], [300, 300], [0, 0]]` name where to go, so without it the press landed 300 px
/// off the element and the drag moved nothing.
pub fn parse_path(v: &serde_json::Value) -> Result<Vec<(f64, f64)>> {
    let pts: Option<Vec<(f64, f64)>> = v.as_array().map(|a| a.iter().map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?))).collect()).unwrap_or(None);
    match pts {
        Some(mut p) if !p.is_empty() => {
            if p[0] != (0.0, 0.0) {
                p.insert(0, (0.0, 0.0));
            }
            Ok(p)
        }
        _ => Err(Error::usage("page.drag(selector, path): path is [[dx, dy], …] (offsets in CSS or device-independent pixels from where the drag starts) or a function t => [dx, dy]")),
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

/// Adds the modifier `name` names (any case): `Control`/`Ctrl`, `Shift`, `Alt`/`Option`,
/// `Meta`/`Win`/`Super`/`Command`/`Cmd`, and `ControlOrMeta`/`Mod`, which is Command on macOS and
/// Control elsewhere, as in Playwright. False for any other name.
fn add_modifier(m: &mut Mods, name: &str) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "control" | "ctrl" => m.ctrl = true,
        "shift" => m.shift = true,
        "alt" | "option" => m.alt = true,
        "meta" | "win" | "super" | "command" | "cmd" => m.meta = true,
        "controlormeta" | "mod" if cfg!(target_os = "macos") => m.meta = true,
        "controlormeta" | "mod" => m.ctrl = true,
        _ => return false,
    }
    true
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
            if !add_modifier(&mut m, s) {
                return Err(Error::usage(format!("modifier {s}: ctrl, shift, alt, meta or ControlOrMeta")));
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

/// `"Enter"`, `"a"`, `"F9"`, `"Control+Shift+P"`, `"ControlOrMeta+a"`, Playwright's names. `"+"`
/// alone, or as a chord's last key (`"Control++"`), is the plus key.
pub fn parse_chord(s: &str) -> Result<(Mods, Key)> {
    let (mods, last) = match s.strip_suffix("++") {
        Some(rest) => (rest, "+"),
        None if s == "+" => ("", "+"),
        None => s.rsplit_once('+').unwrap_or(("", s)),
    };
    let mut m = Mods::default();
    for p in mods.split('+').filter(|p| !mods.is_empty() || !p.is_empty()) {
        if !add_modifier(&mut m, p) {
            return Err(Error::usage(format!("key {s:?}: {p:?} is not a modifier (Control, Shift, Alt, Meta, ControlOrMeta)")));
        }
    }
    Ok((m, key_named(last).ok_or_else(|| Error::usage(format!("key {s:?}: unknown key {last:?} (a key name such as Enter, F9 or PageUp, or one character)")))?))
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

/// A key as a web page reads it, the one table every target's keys come from: `KeyboardEvent.key`
/// and `.code`, the Windows virtual key (also the event's `keyCode`; Windows posts these), and the
/// text it types without modifiers.
pub struct Dom {
    pub key: String,
    pub code: String,
    pub vk: u16,
    pub text: Option<String>,
}

pub fn dom(n: Named) -> Dom {
    let (key, code, vk, text): (&str, &str, u16, Option<&str>) = match n {
        Named::Enter => ("Enter", "Enter", 13, Some("\r")),
        Named::Tab => ("Tab", "Tab", 9, None),
        Named::Escape => ("Escape", "Escape", 27, None),
        Named::Backspace => ("Backspace", "Backspace", 8, None),
        Named::Delete => ("Delete", "Delete", 46, None),
        Named::Insert => ("Insert", "Insert", 45, None),
        Named::Home => ("Home", "Home", 36, None),
        Named::End => ("End", "End", 35, None),
        Named::PageUp => ("PageUp", "PageUp", 33, None),
        Named::PageDown => ("PageDown", "PageDown", 34, None),
        Named::ArrowUp => ("ArrowUp", "ArrowUp", 38, None),
        Named::ArrowDown => ("ArrowDown", "ArrowDown", 40, None),
        Named::ArrowLeft => ("ArrowLeft", "ArrowLeft", 37, None),
        Named::ArrowRight => ("ArrowRight", "ArrowRight", 39, None),
        Named::Space => (" ", "Space", 32, Some(" ")),
        Named::Shift => ("Shift", "ShiftLeft", 16, None),
        Named::Control => ("Control", "ControlLeft", 17, None),
        Named::Alt => ("Alt", "AltLeft", 18, None),
        Named::Meta => ("Meta", "MetaLeft", 91, None),
        Named::ContextMenu => ("ContextMenu", "ContextMenu", 93, None),
        Named::CapsLock => ("CapsLock", "CapsLock", 20, None),
        Named::F(k) => {
            let f = format!("F{k}");
            return Dom { key: f.clone(), code: f, vk: 0x6F + k as u16, text: None };
        }
        Named::Letter(c) | Named::Digit(c) => return dom_char(c.to_ascii_lowercase() as char),
    };
    Dom { key: key.into(), code: code.into(), vk, text: text.map(str::to_string) }
}

/// A character as a page reads it: its US key's code and virtual key where it has one (`a` and `A`
/// are `KeyA`), and itself as key and text.
pub fn dom_char(c: char) -> Dom {
    const PUNCT: &[(char, &str, u16)] = &[
        (';', "Semicolon", 186),
        ('=', "Equal", 187),
        (',', "Comma", 188),
        ('-', "Minus", 189),
        ('.', "Period", 190),
        ('/', "Slash", 191),
        ('`', "Backquote", 192),
        ('[', "BracketLeft", 219),
        ('\\', "Backslash", 220),
        (']', "BracketRight", 221),
        ('\'', "Quote", 222),
    ];
    let base = us_key(c).map_or(c, |k| k.0);
    let (code, vk) = match base {
        ' ' => ("Space".to_string(), 32),
        b if b.is_ascii_alphabetic() => (format!("Key{}", b.to_ascii_uppercase()), b.to_ascii_uppercase() as u16),
        b if b.is_ascii_digit() => (format!("Digit{b}"), b as u16),
        b => PUNCT.iter().find(|p| p.0 == b).map_or((String::new(), 0), |p| (p.1.to_string(), p.2)),
    };
    Dom { key: c.to_string(), code, vk, text: Some(c.to_string()) }
}

/// The key a US keyboard types a printable ASCII character with (that key's unshifted character),
/// and whether it takes Shift.
pub fn us_key(c: char) -> Option<(char, bool)> {
    const SHIFTED: &[(char, char)] = &[
        ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'), ('&', '7'), ('*', '8'), ('(', '9'), (')', '0'), ('_', '-'), ('+', '='),
        ('{', '['), ('}', ']'), ('|', '\\'), (':', ';'), ('"', '\''), ('<', ','), ('>', '.'), ('?', '/'), ('~', '`'),
    ];
    match c {
        c if c.is_ascii_uppercase() => Some((c.to_ascii_lowercase(), true)),
        c if c.is_ascii_graphic() || c == ' ' => Some(SHIFTED.iter().find(|s| s.0 == c).map_or((c, false), |s| (s.1, true))),
        _ => None,
    }
}

/// Wheel notches for a delta in pixels: one notch per 100 px, Chromium's scroll per notch. What a
/// native app scrolls per notch is its own.
pub fn notches(px: f64) -> f64 {
    px / 100.0
}
