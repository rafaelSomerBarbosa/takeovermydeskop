//! X11: XTEST for input, GetImage and XFixes for screenshots, RandR for monitors and the EWMH
//! properties window managers keep on the root window for the window list.

use super::{Backend, KeyCode, WindowId, keys};
use crate::input::{Button, Key, Wheel, pause};
use crate::screen::{Monitor, Rect};
use crate::window::Window;
use anyhow::{Context, Result, bail};
use image::RgbImage;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent, ConnectionExt as _, EventMask, ImageFormat,
    ImageOrder, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Time for apps to take in a keyboard mapping change on either side of a keystroke.
const MAPPING_SETTLE_MS: u64 = 20;
/// `_NET_WM_DESKTOP` of a window shown on every desktop.
const ALL_DESKTOPS: u32 = 0xffff_ffff;
/// Longest property we read, in 32-bit units.
const MAX_PROPERTY: u32 = 1 << 16;
/// The `source` of `_NET_ACTIVE_WINDOW`: we speak for the user, like a taskbar.
const SOURCE_PAGER: u32 = 2;
const ALT_R: u32 = 0xffea;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING,
        _NET_ACTIVE_WINDOW,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_CURRENT_DESKTOP,
        _NET_WM_DESKTOP,
        _NET_WM_NAME,
        _NET_WM_STATE,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_DOCK,
    }
}

pub struct X11 {
    connection: RustConnection,
    root: u32,
    atoms: Atoms,
}

/// The server's table of keysyms by keycode. Each keycode has `per_key` columns: plain, then
/// Shift, then more levels and groups we leave alone.
struct KeyTable {
    first: u8,
    per_key: usize,
    keysyms: Vec<u32>,
}

impl KeyTable {
    /// The key that types `keysym` plain or with Shift, and which of those it is.
    fn find(&self, keysym: u32) -> Option<(u8, usize)> {
        let keys = self.keysyms.chunks_exact(self.per_key).enumerate();
        keys.filter_map(|(i, syms)| syms.iter().take(2).position(|&sym| sym == keysym).map(|column| (self.first + i as u8, column)))
            .min_by_key(|&(_, column)| column)
    }

    /// What to press for a key found by `find`.
    fn chord(&self, (code, column): (u8, usize)) -> Result<Vec<KeyCode>> {
        if column == 0 {
            return Ok(vec![code as KeyCode]);
        }
        let (shift, _) = self.find(keys::codes(Key::Shift).1).context("this keyboard layout has no Shift key")?;
        Ok(vec![shift as KeyCode, code as KeyCode])
    }

    /// A keycode with nothing on it, to lend to a character the layout doesn't have.
    fn spare(&self) -> Option<u8> {
        let keys = self.keysyms.chunks_exact(self.per_key).enumerate().rev();
        keys.filter(|(_, syms)| syms.iter().all(|&sym| sym == 0)).map(|(i, _)| self.first + i as u8).next()
    }
}

impl X11 {
    pub fn connect() -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("couldn't connect to the X server")?;
        let root = connection.setup().roots[screen].root;
        let atoms = Atoms::new(&connection)?.reply()?;
        connection.xtest_get_version(2, 2)?.reply().context("the X server doesn't have the XTEST extension")?;
        connection.xfixes_query_version(5, 0)?.reply().context("the X server doesn't have the XFixes extension")?;
        connection.randr_query_version(1, 5)?.reply().context("the X server doesn't have the RandR extension")?;
        Ok(Self { connection, root, atoms })
    }

    fn key_table(&self) -> Result<KeyTable> {
        let setup = self.connection.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let reply = self.connection.get_keyboard_mapping(first, last - first + 1)?.reply()?;
        Ok(KeyTable { first, per_key: reply.keysyms_per_keycode as usize, keysyms: reply.keysyms })
    }

    fn fake(&self, event: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection.xtest_fake_input(event, detail, 0, self.root, x, y, 0)?;
        Ok(())
    }

    /// Makes sure the server has carried out everything sent so far.
    fn sync(&self) -> Result<()> {
        self.connection.get_input_focus()?.reply()?;
        Ok(())
    }

    fn tap(&self, code: u8) -> Result<()> {
        self.fake(KEY_PRESS_EVENT, code, 0, 0)?;
        self.fake(KEY_RELEASE_EVENT, code, 0, 0)?;
        self.sync()
    }

    /// Lends a spare keycode to `keysym` for one keystroke.
    fn type_unmapped(&self, table: &KeyTable, keysym: u32) -> Result<()> {
        let spare = table.spare().context("the keyboard has no spare key to type this character with")?;
        self.connection.change_keyboard_mapping(1, spare, 2, &[keysym, keysym])?;
        self.sync()?;
        pause(MAPPING_SETTLE_MS);
        let typed = self.tap(spare);
        pause(MAPPING_SETTLE_MS);
        self.connection.change_keyboard_mapping(1, spare, 2, &[0, 0])?;
        self.sync()?;
        typed
    }

    fn numbers(&self, window: u32, property: u32) -> Vec<u32> {
        let reply = self.connection.get_property(false, window, property, AtomEnum::ANY, 0, MAX_PROPERTY);
        let reply = reply.ok().and_then(|cookie| cookie.reply().ok());
        reply.and_then(|reply| reply.value32().map(Iterator::collect)).unwrap_or_default()
    }

    fn bytes(&self, window: u32, property: u32) -> Vec<u8> {
        let reply = self.connection.get_property(false, window, property, AtomEnum::ANY, 0, MAX_PROPERTY);
        reply.ok().and_then(|cookie| cookie.reply().ok()).map(|reply| reply.value).unwrap_or_default()
    }

    fn title(&self, window: u32) -> String {
        let name = self.bytes(window, self.atoms._NET_WM_NAME);
        let name = if name.is_empty() { self.bytes(window, AtomEnum::WM_NAME.into()) } else { name };
        String::from_utf8_lossy(&name).into_owned()
    }

    fn describe(&self, window: u32, desktop: Option<u32>) -> Option<Window> {
        let atoms = &self.atoms;
        let kinds = self.numbers(window, atoms._NET_WM_WINDOW_TYPE);
        if kinds.contains(&atoms._NET_WM_WINDOW_TYPE_DOCK) || kinds.contains(&atoms._NET_WM_WINDOW_TYPE_DESKTOP) {
            return None;
        }
        let states = self.numbers(window, atoms._NET_WM_STATE);
        if states.contains(&atoms._NET_WM_STATE_SKIP_TASKBAR) {
            return None;
        }
        let title = self.title(window);
        if title.is_empty() {
            return None;
        }
        let geometry = self.connection.get_geometry(window).ok()?.reply().ok()?;
        let origin = self.connection.translate_coordinates(window, self.root, 0, 0).ok()?.reply().ok()?;
        let (left, top) = (origin.dst_x as i32, origin.dst_y as i32);
        let on = self.numbers(window, atoms._NET_WM_DESKTOP).first().copied();
        let hidden = if states.contains(&atoms._NET_WM_STATE_HIDDEN) {
            Some("minimized".to_string())
        } else {
            on.filter(|&on| on != ALL_DESKTOPS && Some(on) != desktop).map(|on| format!("on desktop {}", on + 1))
        };
        Some(Window {
            id: window as WindowId,
            title,
            app: app_name(&self.bytes(window, AtomEnum::WM_CLASS.into())),
            rect: Rect { left, top, right: left + geometry.width as i32, bottom: top + geometry.height as i32 },
            hidden,
        })
    }
}

/// `WM_CLASS` is two NUL-terminated strings: the instance, then the class proper.
fn app_name(wm_class: &[u8]) -> String {
    let mut parts = wm_class.split(|&b| b == 0).filter(|part| !part.is_empty());
    let (instance, class) = (parts.next(), parts.next());
    class.or(instance).map(|name| String::from_utf8_lossy(name).into_owned()).unwrap_or_default()
}

/// Blends a pixel of the pointer, whose colors come already multiplied by its alpha.
fn blend(under: u8, over: u8, alpha: u8) -> u8 {
    (over as u32 + under as u32 * (255 - alpha as u32) / 255).min(255) as u8
}

impl Backend for X11 {
    fn monitors(&mut self) -> Result<Vec<Monitor>> {
        let reply = self.connection.randr_get_monitors(self.root, true)?.reply()?;
        let mut monitors: Vec<Monitor> = reply
            .monitors
            .iter()
            .map(|m| Monitor {
                left: m.x as i32,
                top: m.y as i32,
                width: m.width as i32,
                height: m.height as i32,
                primary: m.primary,
                density: 1.0,
            })
            .collect();
        monitors.sort_by_key(|m| (!m.primary, m.left, m.top));
        Ok(monitors)
    }

    /// Copies a rectangle of the root window. X leaves the mouse pointer out, so it's drawn back in:
    /// seeing it tells Claude where the last action landed.
    fn capture(&mut self, left: i32, top: i32, width: i32, height: i32) -> Result<RgbImage> {
        let reply = self
            .connection
            .get_image(ImageFormat::Z_PIXMAP, self.root, left as i16, top as i16, width as u16, height as u16, !0)?
            .reply()
            .context("couldn't copy the screen")?;
        let setup = self.connection.setup();
        let format = setup.pixmap_formats.iter().find(|f| f.depth == reply.depth);
        if format.is_none_or(|f| f.bits_per_pixel != 32) || setup.image_byte_order != ImageOrder::LSB_FIRST {
            bail!("can't read this X server's pixel format (depth {})", reply.depth);
        }
        let rgb = reply.data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
        let mut image = RgbImage::from_raw(width as u32, height as u32, rgb).context("captured buffer has the wrong size")?;

        let pointer = self.connection.xfixes_get_cursor_image()?.reply()?;
        let (origin_x, origin_y) = (pointer.x as i32 - pointer.xhot as i32 - left, pointer.y as i32 - pointer.yhot as i32 - top);
        for (i, argb) in pointer.cursor_image.iter().enumerate() {
            let (x, y) = (origin_x + (i % pointer.width as usize) as i32, origin_y + (i / pointer.width as usize) as i32);
            if x < 0 || y < 0 || x >= width || y >= height {
                continue;
            }
            let [blue, green, red, alpha] = argb.to_le_bytes();
            let pixel = image.get_pixel_mut(x as u32, y as u32);
            pixel.0 = [blend(pixel.0[0], red, alpha), blend(pixel.0[1], green, alpha), blend(pixel.0[2], blue, alpha)];
        }
        Ok(image)
    }

    fn cursor(&mut self) -> Result<(i32, i32)> {
        let reply = self.connection.query_pointer(self.root)?.reply()?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.fake(MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)?;
        self.sync()
    }

    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        let number = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        };
        self.fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, number, 0, 0)?;
        self.sync()
    }

    /// X has no wheel: it reports notches as clicks of buttons 4 to 7.
    fn wheel(&mut self, wheel: Wheel) -> Result<()> {
        let number = match wheel {
            Wheel::Up => 4,
            Wheel::Down => 5,
            Wheel::Left => 6,
            Wheel::Right => 7,
        };
        self.fake(BUTTON_PRESS_EVENT, number, 0, 0)?;
        self.fake(BUTTON_RELEASE_EVENT, number, 0, 0)?;
        self.sync()
    }

    fn key_codes(&mut self, key: Key) -> Result<Vec<KeyCode>> {
        // What the keypad types depends on Num Lock, and the digit keys always type digits.
        if let Key::Numpad(n) = key {
            return self.keys_for_char(char::from(b'0' + n));
        }
        let table = self.key_table()?;
        let found = table.find(keys::codes(key).1);
        // Layouts without a third level have a plain right Alt there.
        let found = found.or_else(|| (key == Key::AltGr).then(|| table.find(ALT_R)).flatten());
        match found {
            Some(found) => table.chord(found),
            None => bail!("this keyboard layout has no {key:?} key"),
        }
    }

    fn keys_for_char(&mut self, c: char) -> Result<Vec<KeyCode>> {
        // Letters and digits name a key whatever their case: ctrl+A is ctrl+a.
        let lookup = if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { c };
        let table = self.key_table()?;
        match table.find(keys::keysym(lookup)) {
            Some(found) => table.chord(found),
            None => bail!("no key on this keyboard layout types {c:?}; use type_text for text"),
        }
    }

    fn send_keys(&mut self, keys: &[KeyCode], up: bool) -> Result<()> {
        for &key in keys {
            self.fake(if up { KEY_RELEASE_EVENT } else { KEY_PRESS_EVENT }, key as u8, 0, 0)?;
        }
        self.sync()
    }

    fn type_char(&mut self, c: char, _rest: &str) -> Result<()> {
        if c.is_control() {
            bail!("can't type the control character {c:?}");
        }
        let keysym = keys::keysym(c);
        let table = self.key_table()?;
        let Some(found) = table.find(keysym) else {
            return self.type_unmapped(&table, keysym);
        };
        let chord = table.chord(found)?;
        self.send_keys(&chord, false)?;
        self.send_keys(&chord, true)
    }

    /// Frontmost first: the stacking list goes from the bottom up.
    fn windows(&mut self) -> Result<Vec<Window>> {
        let mut stack = self.numbers(self.root, self.atoms._NET_CLIENT_LIST_STACKING);
        if stack.is_empty() {
            stack = self.numbers(self.root, self.atoms._NET_CLIENT_LIST);
        }
        let desktop = self.numbers(self.root, self.atoms._NET_CURRENT_DESKTOP).first().copied();
        Ok(stack.into_iter().rev().filter_map(|window| self.describe(window, desktop)).collect())
    }

    fn foreground(&mut self) -> Result<Option<WindowId>> {
        let active = self.numbers(self.root, self.atoms._NET_ACTIVE_WINDOW).first().copied();
        Ok(active.filter(|&window| window != 0).map(|window| window as WindowId))
    }

    /// Asks the window manager to focus `window`, which also restores it and switches desktops.
    fn activate(&mut self, window: &Window) -> Result<()> {
        let message = ClientMessageEvent::new(32, window.id as u32, self.atoms._NET_ACTIVE_WINDOW, [SOURCE_PAGER, 0, 0, 0, 0]);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        self.connection.send_event(false, self.root, mask, message)?;
        self.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> KeyTable {
        // Keycodes 10 to 12: "a" with "A", a key with "A" and "a" swapped, and an empty key.
        KeyTable { first: 10, per_key: 3, keysyms: vec![0x61, 0x41, 0, 0x41, 0x61, 0, 0, 0, 0] }
    }

    #[test]
    fn prefers_the_key_that_needs_no_shift() {
        assert_eq!(table().find(0x61), Some((10, 0)));
        assert_eq!(table().find(0x41), Some((11, 0)));
    }

    #[test]
    fn a_shifted_key_comes_with_shift() {
        // Keycode 10 is Shift_L; keycode 11 has KP_7 on its shift level.
        let table = KeyTable { first: 10, per_key: 2, keysyms: vec![0xffe1, 0, 0xff95, 0xffb7] };
        assert_eq!(table.chord((11, 0)).unwrap(), [11]);
        assert_eq!(table.chord((11, 1)).unwrap(), [10, 11]);
    }

    #[test]
    fn ignores_keysyms_past_the_shift_column() {
        let table = KeyTable { first: 10, per_key: 3, keysyms: vec![0x61, 0x41, 0xe9] };
        assert_eq!(table.find(0xe9), None);
    }

    #[test]
    fn lends_out_only_empty_keys() {
        assert_eq!(table().spare(), Some(12));
        let full = KeyTable { first: 10, per_key: 2, keysyms: vec![0x61, 0x41] };
        assert_eq!(full.spare(), None);
    }

    #[test]
    fn the_app_is_the_second_part_of_wm_class() {
        assert_eq!(app_name(b"navigator\0firefox\0"), "firefox");
        assert_eq!(app_name(b"xterm\0"), "xterm");
        assert_eq!(app_name(b""), "");
    }

    #[test]
    fn blending_keeps_premultiplied_colors() {
        assert_eq!(blend(200, 0, 0), 200);
        assert_eq!(blend(200, 255, 255), 255);
        assert_eq!(blend(100, 50, 128), 99);
    }
}
