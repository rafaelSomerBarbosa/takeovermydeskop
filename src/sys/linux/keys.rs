//! Key tables shared by the Linux backends: evdev key codes for Wayland, X keysyms for X11, and the
//! keymap the Wayland virtual keyboard runs on.

use crate::input::Key;

pub const SHIFT: u32 = 42;
const SHIFT_MASK: u32 = 1;
const CTRL_MASK: u32 = 1 << 2;
const ALT_MASK: u32 = 1 << 3;
const SUPER_MASK: u32 = 1 << 6;

/// The US layout by evdev code: each row's keys are consecutive, and the second string is what
/// those keys type with Shift.
const US_ROWS: [(u32, &str, &str); 4] = [
    (2, "1234567890-=", "!@#$%^&*()_+"),
    (16, "qwertyuiop[]", "QWERTYUIOP{}"),
    (30, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
    (43, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
];
const SPACE: u32 = 57;

/// Spare keys of the virtual keyboard, which can be given any character. Their keycodes stop below
/// 256: X11 apps running under XWayland can't see higher ones.
pub const FIRST_SLOT: u32 = 200;
pub const SLOTS: usize = 39;
/// XKB numbers keys 8 above evdev.
const XKB_OFFSET: u32 = 8;

/// The evdev code and the X keysym of a key.
pub fn codes(key: Key) -> (u32, u32) {
    match key {
        Key::Ctrl => (29, 0xffe3),
        Key::RightCtrl => (97, 0xffe4),
        Key::Shift => (SHIFT, 0xffe1),
        Key::RightShift => (54, 0xffe2),
        Key::Alt => (56, 0xffe9),
        Key::AltGr => (100, 0xfe03),
        Key::Super => (125, 0xffeb),
        Key::RightSuper => (126, 0xffec),
        Key::Enter => (28, 0xff0d),
        Key::Esc => (1, 0xff1b),
        Key::Tab => (15, 0xff09),
        Key::Space => (SPACE, 0x20),
        Key::Backspace => (14, 0xff08),
        Key::Delete => (111, 0xffff),
        Key::Insert => (110, 0xff63),
        Key::Home => (102, 0xff50),
        Key::End => (107, 0xff57),
        Key::PageUp => (104, 0xff55),
        Key::PageDown => (109, 0xff56),
        Key::Up => (103, 0xff52),
        Key::Down => (108, 0xff54),
        Key::Left => (105, 0xff51),
        Key::Right => (106, 0xff53),
        Key::CapsLock => (58, 0xffe5),
        Key::NumLock => (69, 0xff7f),
        Key::ScrollLock => (70, 0xff14),
        Key::PrintScreen => (99, 0xff61),
        Key::Pause => (119, 0xff13),
        Key::Menu => (127, 0xff67),
        Key::F(n) => {
            let code = match n as u32 {
                n @ 1..=10 => 58 + n,
                n @ 11..=12 => 76 + n,
                n => 170 + n,
            };
            (code, 0xffbd + n as u32)
        }
        Key::Numpad(n) => ([82, 79, 80, 81, 75, 76, 77, 71, 72, 73][n as usize], 0xffb0 + n as u32),
        Key::Multiply => (55, 0xffaa),
        Key::Add => (78, 0xffab),
        Key::Subtract => (74, 0xffad),
        Key::Decimal => (83, 0xffae),
        Key::Divide => (98, 0xffaf),
        Key::VolumeUp => (115, 0x1008ff13),
        Key::VolumeDown => (114, 0x1008ff11),
        Key::VolumeMute => (113, 0x1008ff12),
        Key::PlayPause => (164, 0x1008ff14),
        Key::NextTrack => (163, 0x1008ff17),
        Key::PrevTrack => (165, 0x1008ff16),
        Key::MediaStop => (166, 0x1008ff15),
        Key::BrowserBack => (158, 0x1008ff26),
        Key::BrowserForward => (159, 0x1008ff27),
        Key::BrowserRefresh => (173, 0x1008ff29),
    }
}

/// The evdev code of the key that types `c` on the US layout, and whether it takes Shift.
pub fn us_key(c: char) -> Option<(u32, bool)> {
    if c == ' ' {
        return Some((SPACE, false));
    }
    US_ROWS.iter().find_map(|&(first, plain, shifted)| {
        let at = |row: &str| row.chars().position(|k| k == c).map(|i| first + i as u32);
        at(plain).map(|code| (code, false)).or_else(|| at(shifted).map(|code| (code, true)))
    })
}

/// The modifier bit a key sets while held. XKB fixes these for the real modifiers.
pub fn modifier_mask(code: u32) -> u32 {
    match code {
        42 | 54 => SHIFT_MASK,
        29 | 97 => CTRL_MASK,
        56 | 100 => ALT_MASK,
        125 | 126 => SUPER_MASK,
        _ => 0,
    }
}

/// The X keysym of a character: Latin-1 stands for itself, the rest of Unicode has its own range.
pub fn keysym(c: char) -> u32 {
    match c as u32 {
        code @ (0x20..=0x7e | 0xa0..=0xff) => code,
        code => 0x0100_0000 + code,
    }
}

/// A US keyboard with `slots` on its spare keys. Real keys keep their real codes, so shortcuts reach
/// apps as they would from a physical keyboard. The keypad always types digits, as on Windows, so
/// `numpad7` doesn't depend on Num Lock.
pub fn keymap(slots: &[char]) -> String {
    let mut symbols = String::new();
    for (i, c) in slots.iter().enumerate() {
        let keycode = FIRST_SLOT + XKB_OFFSET + i as u32;
        symbols += &format!("        key <I{keycode}> {{ type=\"ONE_LEVEL\", [ U{:04X} ] }};\n", *c as u32);
    }
    for (name, sym) in [
        ("KP0", "KP_0"),
        ("KP1", "KP_1"),
        ("KP2", "KP_2"),
        ("KP3", "KP_3"),
        ("KP4", "KP_4"),
        ("KP5", "KP_5"),
        ("KP6", "KP_6"),
        ("KP7", "KP_7"),
        ("KP8", "KP_8"),
        ("KP9", "KP_9"),
        ("KPDL", "KP_Decimal"),
    ] {
        symbols += &format!("        key <{name}> {{ type=\"ONE_LEVEL\", [ {sym} ] }};\n");
    }
    format!(
        "xkb_keymap {{
    xkb_keycodes {{ include \"evdev+aliases(qwerty)\" }};
    xkb_types {{ include \"complete\" }};
    xkb_compat {{ include \"complete\" }};
    xkb_symbols {{
        include \"pc+us+inet(evdev)\"
{symbols}    }};
}};
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_printable_ascii_character_on_the_us_layout() {
        for c in ' '..='~' {
            assert!(us_key(c).is_some(), "{c:?}");
        }
        assert_eq!(us_key('ç'), None);
    }

    #[test]
    fn places_characters_on_their_real_keys() {
        assert_eq!(us_key('1'), Some((2, false)));
        assert_eq!(us_key('='), Some((13, false)));
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('A'), Some((30, true)));
        assert_eq!(us_key('`'), Some((41, false)));
        assert_eq!(us_key('\\'), Some((43, false)));
        assert_eq!(us_key('?'), Some((53, true)));
    }

    #[test]
    fn numbers_function_keys_across_their_three_ranges() {
        assert_eq!(codes(Key::F(1)), (59, 0xffbe));
        assert_eq!(codes(Key::F(10)), (68, 0xffc7));
        assert_eq!(codes(Key::F(11)), (87, 0xffc8));
        assert_eq!(codes(Key::F(12)), (88, 0xffc9));
        assert_eq!(codes(Key::F(13)), (183, 0xffca));
        assert_eq!(codes(Key::F(24)), (194, 0xffd5));
    }

    #[test]
    fn keysyms_follow_the_x11_convention() {
        assert_eq!(keysym('a'), 0x61);
        assert_eq!(keysym('é'), 0xe9);
        assert_eq!(keysym('€'), 0x0100_20ac);
        assert_eq!(keysym('😀'), 0x0101_f600);
    }

    #[test]
    fn keymap_puts_each_character_on_a_slot() {
        let keymap = keymap(&['ç', '😀']);
        assert!(keymap.contains("key <I208> { type=\"ONE_LEVEL\", [ U00E7 ] };"));
        assert!(keymap.contains("key <I209> { type=\"ONE_LEVEL\", [ U1F600 ] };"));
    }

    #[test]
    fn slots_stay_within_what_xwayland_can_see() {
        assert!(FIRST_SLOT + XKB_OFFSET + SLOTS as u32 <= 256);
    }
}
