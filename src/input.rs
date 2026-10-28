//! Synthetic mouse and keyboard input through SendInput, the same path real devices use.
//! Coordinates are physical screen pixels.

use anyhow::{Result, bail};
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_SWAPBUTTON, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos,
};

/// One click of the mouse wheel (WHEEL_DELTA).
const NOTCH: i32 = 120;

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

    fn event(self, down: bool) -> INPUT {
        // SendInput presses physical buttons, so undo a left-handed button swap.
        let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
        let physical = match (self, swapped) {
            (Button::Left, true) => Button::Right,
            (Button::Right, true) => Button::Left,
            (button, _) => button,
        };
        let flags = match (physical, down) {
            (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (Button::Left, false) => MOUSEEVENTF_LEFTUP,
            (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
            (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        mouse(flags, 0, 0, 0)
    }
}

pub fn cursor() -> (i32, i32) {
    let mut point = POINT::default();
    let _ = unsafe { GetCursorPos(&mut point) };
    (point.x, point.y)
}

pub fn move_to(x: i32, y: i32) -> Result<()> {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let normalize = |value: i32, origin: i32, size: i32| ((value - origin) as f64 * 65535.0 / (size - 1).max(1) as f64).round() as i32;
    let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    send(&[mouse(flags, normalize(x, vx, vw), normalize(y, vy, vh), 0)])?;
    // The normalized move can land a pixel off; pin the pointer exactly.
    unsafe { SetCursorPos(x, y)? };
    Ok(())
}

pub fn click(x: i32, y: i32, button: Button, count: u32) -> Result<()> {
    move_to(x, y)?;
    pause(40);
    for i in 0..count {
        send(&[button.event(true)])?;
        pause(30);
        send(&[button.event(false)])?;
        if i + 1 < count {
            pause(60);
        }
    }
    Ok(())
}

pub fn drag(from: (i32, i32), to: (i32, i32), button: Button) -> Result<()> {
    move_to(from.0, from.1)?;
    pause(60);
    send(&[button.event(true)])?;
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
    let released = send(&[button.event(false)]);
    moved.and(released)
}

pub fn scroll(x: i32, y: i32, direction: &str, notches: u32) -> Result<()> {
    let (flags, delta) = match direction.trim().to_ascii_lowercase().as_str() {
        "up" => (MOUSEEVENTF_WHEEL, NOTCH),
        "down" => (MOUSEEVENTF_WHEEL, -NOTCH),
        "left" => (MOUSEEVENTF_HWHEEL, -NOTCH),
        "right" => (MOUSEEVENTF_HWHEEL, NOTCH),
        other => bail!("unknown scroll direction {other:?}; use up, down, left or right"),
    };
    move_to(x, y)?;
    pause(40);
    for _ in 0..notches {
        send(&[mouse(flags, 0, 0, delta as u32)])?;
        pause(30);
    }
    Ok(())
}

/// Types text as Unicode keystrokes, so any character works whatever the keyboard layout.
/// `between` runs every few characters and can stop a long text half-way.
pub fn type_text(text: &str, mut between: impl FnMut() -> Result<()>) -> Result<()> {
    let mut units = [0u16; 2];
    for (i, c) in text.chars().filter(|&c| c != '\r').enumerate() {
        if i > 0 && i % 32 == 0 {
            between()?;
        }
        let events: Vec<INPUT> = match c {
            '\n' => vec![key(VK_RETURN, false), key(VK_RETURN, true)],
            '\t' => vec![key(VK_TAB, false), key(VK_TAB, true)],
            c => c.encode_utf16(&mut units).iter().flat_map(|&unit| [unicode(unit, false), unicode(unit, true)]).collect(),
        };
        send(&events)?;
        // One character at a time: sent in a burst, an app that is still waking up can turn the
        // first character into a later one.
        pause(4);
    }
    Ok(())
}

/// A key combination like ctrl+shift+s, in press order (modifiers first).
pub struct Combo(Vec<VIRTUAL_KEY>);

impl Combo {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        // "+" and "ctrl++" name the plus key itself.
        let tokens: Vec<&str> = if text == "+" {
            vec!["+"]
        } else if let Some(rest) = text.strip_suffix("++") {
            rest.split('+').chain(["+"]).collect()
        } else {
            text.split('+').collect()
        };
        let mut keys = Vec::new();
        for token in tokens {
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
        send(&self.0.iter().map(|&k| key(k, false)).collect::<Vec<_>>())
    }

    pub fn release(&self) -> Result<()> {
        send(&self.0.iter().rev().map(|&k| key(k, true)).collect::<Vec<_>>())
    }
}

/// The keys to press for one token of a combination: a key name, or a single character.
fn keys_for(token: &str) -> Result<Vec<VIRTUAL_KEY>> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return keys_for_char(c);
    }
    let name: String = token.chars().filter(|c| !matches!(c, '_' | '-' | ' ')).collect::<String>().to_ascii_lowercase();
    if let Some(c) = punctuation(&name) {
        return keys_for_char(c);
    }
    match named_key(&name) {
        Some(key) => Ok(vec![key]),
        None => bail!("unknown key {token:?}"),
    }
}

fn keys_for_char(c: char) -> Result<Vec<VIRTUAL_KEY>> {
    if c.is_ascii_alphanumeric() {
        return Ok(vec![VIRTUAL_KEY(c.to_ascii_uppercase() as u16)]);
    }
    // Other characters depend on the keyboard layout: ask Windows which key (and shift state) types it.
    let mut units = [0u16; 2];
    let found = match c.encode_utf16(&mut units) {
        [unit] => unsafe { VkKeyScanW(*unit) },
        _ => -1,
    };
    if found == -1 {
        bail!("no key on this keyboard layout types {c:?}; use type_text for text");
    }
    let shift_state = (found >> 8) & 0xff;
    let mut keys = Vec::new();
    if shift_state & 2 != 0 {
        keys.push(VK_CONTROL);
    }
    if shift_state & 4 != 0 {
        keys.push(VK_MENU);
    }
    if shift_state & 1 != 0 {
        keys.push(VK_SHIFT);
    }
    keys.push(VIRTUAL_KEY((found & 0xff) as u16));
    Ok(keys)
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
fn named_key(name: &str) -> Option<VIRTUAL_KEY> {
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=24).contains(n)) {
        return Some(VIRTUAL_KEY(VK_F1.0 + n - 1));
    }
    let digit = name.strip_prefix("numpad").or_else(|| name.strip_prefix("kp"));
    if let Some(n) = digit.and_then(|n| n.parse::<u16>().ok()).filter(|n| *n <= 9) {
        return Some(VIRTUAL_KEY(VK_NUMPAD0.0 + n));
    }
    Some(match name {
        "ctrl" | "control" | "ctl" | "lctrl" | "controll" => VK_CONTROL,
        "rctrl" | "controlr" => VK_RCONTROL,
        "shift" | "lshift" | "shiftl" => VK_SHIFT,
        "rshift" | "shiftr" => VK_RSHIFT,
        "alt" | "lalt" | "altl" | "option" => VK_MENU,
        "altgr" | "ralt" | "altr" => VK_RMENU,
        "win" | "windows" | "super" | "superl" | "meta" | "cmd" | "command" | "lwin" => VK_LWIN,
        "rwin" | "superr" => VK_RWIN,
        "enter" | "return" | "kpenter" => VK_RETURN,
        "esc" | "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "space" | "spacebar" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" | "prior" => VK_PRIOR,
        "pagedown" | "pgdn" | "next" => VK_NEXT,
        "up" | "uparrow" | "arrowup" => VK_UP,
        "down" | "downarrow" | "arrowdown" => VK_DOWN,
        "left" | "leftarrow" | "arrowleft" => VK_LEFT,
        "right" | "rightarrow" | "arrowright" => VK_RIGHT,
        "capslock" | "caps" => VK_CAPITAL,
        "numlock" => VK_NUMLOCK,
        "scrolllock" => VK_SCROLL,
        "printscreen" | "print" | "prtsc" | "prtscn" | "snapshot" => VK_SNAPSHOT,
        "pause" | "break" => VK_PAUSE,
        "apps" | "menu" | "contextmenu" | "application" => VK_APPS,
        "multiply" | "kpmultiply" => VK_MULTIPLY,
        "add" | "kpadd" => VK_ADD,
        "subtract" | "kpsubtract" => VK_SUBTRACT,
        "decimal" | "kpdecimal" => VK_DECIMAL,
        "divide" | "kpdivide" => VK_DIVIDE,
        "volumeup" | "audioraisevolume" => VK_VOLUME_UP,
        "volumedown" | "audiolowervolume" => VK_VOLUME_DOWN,
        "volumemute" | "mute" | "audiomute" => VK_VOLUME_MUTE,
        "playpause" | "mediaplaypause" | "audioplay" => VK_MEDIA_PLAY_PAUSE,
        "nexttrack" | "medianext" | "audionext" => VK_MEDIA_NEXT_TRACK,
        "prevtrack" | "previoustrack" | "mediaprev" | "audioprev" => VK_MEDIA_PREV_TRACK,
        "mediastop" | "audiostop" => VK_MEDIA_STOP,
        "browserback" => VK_BROWSER_BACK,
        "browserforward" => VK_BROWSER_FORWARD,
        "browserrefresh" => VK_BROWSER_REFRESH,
        _ => return None,
    })
}

/// Keys that live on the extended part of the keyboard; without the flag, arrows and friends
/// arrive as their numpad twins in some apps.
fn is_extended(key: VIRTUAL_KEY) -> bool {
    const EXTENDED: [VIRTUAL_KEY; 18] = [
        VK_RCONTROL, VK_RMENU, VK_LWIN, VK_RWIN, VK_APPS, VK_INSERT, VK_DELETE, VK_HOME, VK_END, VK_PRIOR, VK_NEXT,
        VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN, VK_NUMLOCK, VK_SNAPSHOT, VK_DIVIDE,
    ];
    // VK_BROWSER_BACK..=VK_LAUNCH_APP2: browser, volume, media and launcher keys.
    EXTENDED.contains(&key) || (0xA6..=0xB7).contains(&key.0)
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    // Games and remote-desktop clients read scan codes rather than virtual keys.
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    keyboard(vk, scan, flags)
}

fn unicode(unit: u16, up: bool) -> INPUT {
    let flags = if up { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP } else { KEYEVENTF_UNICODE };
    keyboard(VIRTUAL_KEY(0), unit, flags)
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!("Windows rejected the input: the screen may be locked or a UAC prompt may be open");
    }
    Ok(())
}

pub fn pause(ms: u64) {
    sleep(Duration::from_millis(ms));
}
