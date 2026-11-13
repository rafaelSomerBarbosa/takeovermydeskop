//! Linux has no single desktop API, so each kind of desktop gets a backend behind `Backend`, picked
//! on first use. The pointer's position decides whether the emergency stop works, and a compositor
//! only tells that through its own IPC, so a Wayland desktop needs support written for it.

mod apps;
mod hyprland;
mod keys;
mod wayland;
mod x11;

use crate::input::{Button, Key, Wheel};
use crate::screen::Monitor;
use crate::window::{Launched, Window};
use anyhow::{Result, bail};
use image::RgbImage;
use std::cell::RefCell;
use std::env;

/// A key as the backend knows it: an evdev code on Wayland, a keycode on X11.
pub type KeyCode = u32;
/// A Hyprland window address, or an X11 window.
pub type WindowId = u64;

/// What Claude can't reach on this platform, for its instructions.
pub const LIMITS: &str = "A locked screen, and apps that grab the keyboard or the pointer for themselves (some password prompts do), don't take this input the way other apps do; ask the user to handle those.";

/// Whether `launch_app` is offered.
pub const HAS_LAUNCHER: bool = true;

/// `launch_app`'s description, and its `name` argument's.
pub const LAUNCH_APP: (&str, &str) = (
    "Open an installed app, the way its entry in an applications menu would. This is how to start apps here: there may be no menu to click, and guessing at shortcuts rarely works. Takes a name, not a command line. Waits for the app's window, and screenshots follow it to its monitor.",
    "The app's name (\"firefox\") or kind (\"web browser\", \"text editor\", \"terminal\").",
);

/// Starts an app from its desktop entry. The caller waits for its window.
pub fn launch(app: &str, _check_stop: &(dyn Fn() -> Result<()> + Sync)) -> Result<Launched> {
    apps::launch(app).map(Launched::Started)
}

trait Backend {
    /// Monitors in a fixed order, with the one to start on marked primary.
    fn monitors(&mut self) -> Result<Vec<Monitor>>;
    fn capture(&mut self, left: i32, top: i32, width: i32, height: i32) -> Result<RgbImage>;
    fn cursor(&mut self) -> Result<(i32, i32)>;
    fn move_to(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, button: Button, down: bool) -> Result<()>;
    fn wheel(&mut self, wheel: Wheel) -> Result<()>;
    /// What to press for `key`, which on some layouts takes a modifier too.
    fn key_codes(&mut self, key: Key) -> Result<Vec<KeyCode>>;
    fn keys_for_char(&mut self, c: char) -> Result<Vec<KeyCode>>;
    fn send_keys(&mut self, keys: &[KeyCode], up: bool) -> Result<()>;
    /// Types `c`, the first character of `rest`, the text still to type.
    fn type_char(&mut self, c: char, rest: &str) -> Result<()>;
    /// Windows from the frontmost back.
    fn windows(&mut self) -> Result<Vec<Window>>;
    fn foreground(&mut self) -> Result<Option<WindowId>>;
    fn activate(&mut self, window: &Window) -> Result<()>;
}

thread_local! {
    static BACKEND: RefCell<Option<Box<dyn Backend>>> = const { RefCell::new(None) };
}

fn is_set(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn connect() -> Result<Box<dyn Backend>> {
    // X11 has to come last: a Wayland desktop also runs an X server for old apps (XWayland), but
    // that one shows only those apps and can't touch anything else.
    if is_set("WAYLAND_DISPLAY") {
        if is_set("HYPRLAND_INSTANCE_SIGNATURE") {
            return Ok(Box::new(hyprland::Hyprland::connect()?));
        }
        let desktop = env::var("XDG_CURRENT_DESKTOP").ok().filter(|name| !name.is_empty());
        bail!(
            "{} isn't supported on Wayland yet: takeovermydeskop works on Hyprland and in X11 sessions",
            desktop.as_deref().unwrap_or("this desktop")
        );
    }
    if is_set("DISPLAY") {
        return Ok(Box::new(x11::X11::connect()?));
    }
    bail!("no desktop found: neither WAYLAND_DISPLAY nor DISPLAY is set; run this from a terminal in your desktop session")
}

fn with<T>(action: impl FnOnce(&mut dyn Backend) -> Result<T>) -> Result<T> {
    BACKEND.with_borrow_mut(|backend| {
        let backend = match backend {
            Some(backend) => backend,
            empty => empty.insert(connect()?),
        };
        action(backend.as_mut())
    })
}

pub fn init() {}

pub fn monitors() -> Result<Vec<Monitor>> {
    with(|backend| backend.monitors())
}

pub fn capture(left: i32, top: i32, width: i32, height: i32) -> Result<RgbImage> {
    if width <= 0 || height <= 0 {
        bail!("empty capture area ({width}x{height})");
    }
    with(|backend| backend.capture(left, top, width, height))
}

pub fn cursor() -> Result<(i32, i32)> {
    with(|backend| backend.cursor())
}

pub fn move_to(x: i32, y: i32) -> Result<()> {
    with(|backend| backend.move_to(x, y))
}

pub fn button(button: Button, down: bool) -> Result<()> {
    with(|backend| backend.button(button, down))
}

pub fn wheel(wheel: Wheel) -> Result<()> {
    with(|backend| backend.wheel(wheel))
}

pub fn key_codes(key: Key) -> Result<Vec<KeyCode>> {
    with(|backend| backend.key_codes(key))
}

pub fn keys_for_char(c: char) -> Result<Vec<KeyCode>> {
    with(|backend| backend.keys_for_char(c))
}

pub fn send_keys(keys: &[KeyCode], up: bool) -> Result<()> {
    with(|backend| backend.send_keys(keys, up))
}

pub fn type_char(c: char, rest: &str) -> Result<()> {
    with(|backend| backend.type_char(c, rest))
}

pub fn list() -> Result<Vec<Window>> {
    with(|backend| backend.windows())
}

pub fn foreground() -> Result<Option<WindowId>> {
    with(|backend| backend.foreground())
}

/// "Title" (app) of the window with keyboard focus.
pub fn foreground_label() -> String {
    let Ok(Some(id)) = foreground() else { return "none".into() };
    let found = list().ok().and_then(|windows| windows.into_iter().find(|window| window.id == id));
    found.map_or_else(|| "unknown".into(), |window| window.label())
}

/// Brings `window` to the front, wherever it is.
pub fn activate(window: &Window) -> Result<()> {
    with(|backend| backend.activate(window))
}
