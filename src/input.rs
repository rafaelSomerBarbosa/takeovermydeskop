//! Synthetic mouse and keyboard input, built on the primitives each platform has in `sys`.
//! Coordinates are desktop coordinates (see `screen`).

use crate::sys;
use anyhow::{Result, bail};
use std::thread::sleep;
use std::time::Duration;

pub use crate::sys::{cursor, move_to};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name.trim().to_ascii_lowercase().as_str() {
            "left" => Button::Left,
            "right" => Button::Right,
            "middle" => Button::Middle,
            other => bail!("unknown mouse button {other:?}; use left, right or middle"),
        })
    }
}

/// A direction to turn the mouse wheel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wheel {
    Up,
    Down,
    Left,
    Right,
}

impl Wheel {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name.trim().to_ascii_lowercase().as_str() {
            "up" => Wheel::Up,
            "down" => Wheel::Down,
            "left" => Wheel::Left,
            "right" => Wheel::Right,
            other => bail!("unknown scroll direction {other:?}; use up, down, left or right"),
        })
    }
}

/// A key that doesn't type a character. Each platform turns these into its own key codes;
/// characters go through `sys::keys_for_char`, since where they are depends on the layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Ctrl,
    RightCtrl,
    Shift,
    RightShift,
    Alt,
    AltGr,
    Super,
    RightSuper,
    Enter,
    Esc,
    Tab,
    Space,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    CapsLock,
    NumLock,
    ScrollLock,
    PrintScreen,
    Pause,
    Menu,
    /// F1 to F24.
    F(u8),
    /// The numeric keypad's 0 to 9.
    Numpad(u8),
    Multiply,
    Add,
    Subtract,
    Decimal,
    Divide,
    VolumeUp,
    VolumeDown,
    VolumeMute,
    PlayPause,
    NextTrack,
    PrevTrack,
    MediaStop,
    BrowserBack,
    BrowserForward,
    BrowserRefresh,
}

pub fn click(x: i32, y: i32, button: Button, count: u32) -> Result<()> {
    move_to(x, y)?;
    pause(40);
    for i in 0..count {
        sys::button(button, true)?;
        pause(30);
        sys::button(button, false)?;
        if i + 1 < count {
            pause(60);
        }
    }
    Ok(())
}

pub fn drag(from: (i32, i32), to: (i32, i32), button: Button) -> Result<()> {
    move_to(from.0, from.1)?;
    pause(60);
    sys::button(button, true)?;
    pause(120);
    // Move in steps: many apps only start a drag after seeing movement in between.
    let steps = 20;
    let moved = (1..=steps).try_for_each(|i| {
        let t = i as f64 / steps as f64;
        pause(15);
        move_to(
            from.0 + ((to.0 - from.0) as f64 * t).round() as i32,
            from.1 + ((to.1 - from.1) as f64 * t).round() as i32,
        )
    });
    pause(120);
    // Release even if a step failed, so the button is never left held down.
    let released = sys::button(button, false);
    moved.and(released)
}

pub fn scroll(x: i32, y: i32, direction: &str, notches: u32) -> Result<()> {
    let wheel = Wheel::parse(direction)?;
    move_to(x, y)?;
    pause(40);
    for _ in 0..notches {
        sys::wheel(wheel)?;
        pause(30);
    }
    Ok(())
}

/// Types text as Unicode keystrokes, so any character works whatever the keyboard layout.
/// `between` runs every few characters and can stop a long text half-way.
pub fn type_text(text: &str, mut between: impl FnMut() -> Result<()>) -> Result<()> {
    for (i, (at, c)) in text.char_indices().filter(|&(_, c)| c != '\r').enumerate() {
        if i > 0 && i % 32 == 0 {
            between()?;
        }
        match c {
            '\n' => tap(Key::Enter)?,
            '\t' => tap(Key::Tab)?,
            c => sys::type_char(c, &text[at..])?,
        }
        // One character at a time: sent in a burst, an app that is still waking up can turn the
        // first character into a later one.
        pause(4);
    }
    Ok(())
}

fn tap(key: Key) -> Result<()> {
    let codes = sys::key_codes(key)?;
    sys::send_keys(&codes, false)?;
    sys::send_keys(&codes, true)
}

/// A key combination like ctrl+shift+s, in press order (modifiers first).
pub struct Combo(Vec<sys::KeyCode>);

impl Combo {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let mut keys = Vec::new();
        for token in tokens(text) {
            let token = token.trim();
            if token.is_empty() {
                bail!("invalid key combination {text:?}");
            }
            for key in keys_for(token)? {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        Ok(Combo(keys))
    }

    pub fn press(&self) -> Result<()> {
        self.hold()?;
        pause(30);
        self.release()
    }

    pub fn hold(&self) -> Result<()> {
        sys::send_keys(&self.0, false)
    }

    pub fn release(&self) -> Result<()> {
        sys::send_keys(&self.0.iter().rev().copied().collect::<Vec<_>>(), true)
    }
}

/// Splits a combination at its plus signs. "+" and "ctrl++" name the plus key itself.
fn tokens(text: &str) -> Vec<&str> {
    if text == "+" {
        vec!["+"]
    } else if let Some(rest) = text.strip_suffix("++") {
        rest.split('+').chain(["+"]).collect()
    } else {
        text.split('+').collect()
    }
}

/// The keys to press for one token of a combination: a key name, or a single character.
fn keys_for(token: &str) -> Result<Vec<sys::KeyCode>> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return sys::keys_for_char(c);
    }
    let name: String = token.chars().filter(|c| !matches!(c, '_' | '-' | ' ')).collect::<String>().to_ascii_lowercase();
    if let Some(c) = punctuation(&name) {
        return sys::keys_for_char(c);
    }
    match named_key(&name) {
        Some(key) => sys::key_codes(key),
        None => bail!("unknown key {token:?}"),
    }
}

fn punctuation(name: &str) -> Option<char> {
    Some(match name {
        "plus" => '+',
        "minus" | "dash" | "hyphen" => '-',
        "equal" | "equals" => '=',
        "comma" => ',',
        "period" | "dot" => '.',
        "slash" => '/',
        "backslash" => '\\',
        "semicolon" => ';',
        "quote" | "apostrophe" => '\'',
        "grave" | "backtick" => '`',
        "bracketleft" => '[',
        "bracketright" => ']',
        _ => return None,
    })
}

/// Key names, including the xdotool ones models often use ("Return", "Page_Down", "super").
fn named_key(name: &str) -> Option<Key> {
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=24).contains(n)) {
        return Some(Key::F(n));
    }
    let digit = name.strip_prefix("numpad").or_else(|| name.strip_prefix("kp"));
    if let Some(n) = digit.and_then(|n| n.parse::<u8>().ok()).filter(|n| *n <= 9) {
        return Some(Key::Numpad(n));
    }
    Some(match name {
        "ctrl" | "control" | "ctl" | "lctrl" | "controll" => Key::Ctrl,
        "rctrl" | "controlr" => Key::RightCtrl,
        "shift" | "lshift" | "shiftl" => Key::Shift,
        "rshift" | "shiftr" => Key::RightShift,
        "alt" | "lalt" | "altl" | "option" => Key::Alt,
        "altgr" | "ralt" | "altr" => Key::AltGr,
        "win" | "windows" | "super" | "superl" | "meta" | "cmd" | "command" | "lwin" => Key::Super,
        "rwin" | "superr" => Key::RightSuper,
        "enter" | "return" | "kpenter" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "tab" => Key::Tab,
        "space" | "spacebar" => Key::Space,
        "backspace" | "back" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" | "prior" => Key::PageUp,
        "pagedown" | "pgdn" | "next" => Key::PageDown,
        "up" | "uparrow" | "arrowup" => Key::Up,
        "down" | "downarrow" | "arrowdown" => Key::Down,
        "left" | "leftarrow" | "arrowleft" => Key::Left,
        "right" | "rightarrow" | "arrowright" => Key::Right,
        "capslock" | "caps" => Key::CapsLock,
        "numlock" => Key::NumLock,
        "scrolllock" => Key::ScrollLock,
        "printscreen" | "print" | "prtsc" | "prtscn" | "snapshot" => Key::PrintScreen,
        "pause" | "break" => Key::Pause,
        "apps" | "menu" | "contextmenu" | "application" => Key::Menu,
        "multiply" | "kpmultiply" => Key::Multiply,
        "add" | "kpadd" => Key::Add,
        "subtract" | "kpsubtract" => Key::Subtract,
        "decimal" | "kpdecimal" => Key::Decimal,
        "divide" | "kpdivide" => Key::Divide,
        "volumeup" | "audioraisevolume" => Key::VolumeUp,
        "volumedown" | "audiolowervolume" => Key::VolumeDown,
        "volumemute" | "mute" | "audiomute" => Key::VolumeMute,
        "playpause" | "mediaplaypause" | "audioplay" => Key::PlayPause,
        "nexttrack" | "medianext" | "audionext" => Key::NextTrack,
        "prevtrack" | "previoustrack" | "mediaprev" | "audioprev" => Key::PrevTrack,
        "mediastop" | "audiostop" => Key::MediaStop,
        "browserback" => Key::BrowserBack,
        "browserforward" => Key::BrowserForward,
        "browserrefresh" => Key::BrowserRefresh,
        _ => return None,
    })
}

pub fn pause(ms: u64) {
    sleep(Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_combinations_and_keeps_the_plus_key() {
        assert_eq!(tokens("ctrl+shift+s"), ["ctrl", "shift", "s"]);
        assert_eq!(tokens("+"), ["+"]);
        assert_eq!(tokens("ctrl++"), ["ctrl", "+"]);
    }

    #[test]
    fn knows_function_and_keypad_keys_by_number() {
        assert_eq!(named_key("f24"), Some(Key::F(24)));
        assert_eq!(named_key("f25"), None);
        assert_eq!(named_key("numpad7"), Some(Key::Numpad(7)));
        assert_eq!(named_key("kp7"), Some(Key::Numpad(7)));
        assert_eq!(named_key("kp10"), None);
    }
}
