//! Hyprland: input and screenshots go through Wayland, and everything Wayland keeps from clients
//! comes from Hyprland's IPC socket: monitors, windows and where the pointer is.
//!
//! Places are in logical pixels of the compositor's layout, so `Monitor::density` is the monitor's
//! scale, and screenshots come back with more pixels than a scaled monitor has coordinates.

use super::wayland::Wayland;
use super::{Backend, KeyCode, WindowId, keys};
use crate::input::{Button, Key, Wheel};
use crate::screen::{Monitor, Rect};
use crate::window::Window;
use anyhow::{Context, Result, bail};
use image::{RgbImage, imageops};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

pub struct Hyprland {
    socket: PathBuf,
    wayland: Wayland,
}

#[derive(Deserialize)]
struct RawMonitor {
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: f64,
    transform: u32,
    focused: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(rename = "activeWorkspace")]
    active_workspace: Workspace,
    #[serde(rename = "specialWorkspace")]
    special_workspace: Workspace,
}

#[derive(Deserialize)]
struct Workspace {
    id: i64,
    name: String,
}

#[derive(Deserialize)]
struct Client {
    address: String,
    mapped: bool,
    at: [i32; 2],
    size: [i32; 2],
    workspace: Workspace,
    class: String,
    title: String,
    #[serde(rename = "focusHistoryID")]
    focus_history: i32,
}

/// The reply to `activewindow`, which is an empty object when nothing has focus.
#[derive(Deserialize)]
struct Active {
    address: Option<String>,
}

impl RawMonitor {
    fn monitor(&self) -> Monitor {
        // Odd transforms turn the monitor on its side.
        let (width, height) = if self.transform % 2 == 1 { (self.height, self.width) } else { (self.width, self.height) };
        Monitor {
            left: self.x,
            top: self.y,
            width: (width as f64 / self.scale).round() as i32,
            height: (height as f64 / self.scale).round() as i32,
            primary: self.focused,
            density: self.scale,
        }
    }
}

impl Client {
    fn window(self, shown: &[i64]) -> Result<Window> {
        let [left, top] = self.at;
        let [width, height] = self.size;
        Ok(Window {
            id: parse_address(&self.address)?,
            hidden: (!shown.contains(&self.workspace.id)).then(|| format!("on workspace {}", self.workspace.name)),
            title: self.title,
            app: self.class,
            rect: Rect { left, top, right: left + width, bottom: top + height },
        })
    }
}

fn parse_address(address: &str) -> Result<WindowId> {
    WindowId::from_str_radix(address.trim_start_matches("0x"), 16).with_context(|| format!("bad window address {address:?}"))
}

/// Screencopy hands over a frame the way the panel holds it, so `transform` (a `wl_output.transform`)
/// still has to be undone for a rotated or flipped monitor.
fn upright(image: RgbImage, transform: u32) -> RgbImage {
    match transform {
        1 => imageops::rotate90(&image),
        2 => imageops::rotate180(&image),
        3 => imageops::rotate270(&image),
        4 => imageops::flip_horizontal(&image),
        5 => imageops::rotate270(&imageops::flip_horizontal(&image)),
        6 => imageops::flip_vertical(&image),
        7 => imageops::rotate90(&imageops::flip_horizontal(&image)),
        _ => image,
    }
}

/// The `cursorpos` reply, "1668, 642".
fn parse_cursor(reply: &str) -> Option<(i32, i32)> {
    let (x, y) = reply.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

impl Hyprland {
    pub fn connect() -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR isn't set")?;
        let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").context("HYPRLAND_INSTANCE_SIGNATURE isn't set")?;
        let socket = PathBuf::from(runtime).join("hypr").join(instance).join(".socket.sock");
        Ok(Self { socket, wayland: Wayland::connect()? })
    }

    /// One request to the IPC socket. Hyprland answers and hangs up.
    fn ask(&self, request: &str) -> Result<String> {
        let mut stream = UnixStream::connect(&self.socket).with_context(|| format!("couldn't reach Hyprland at {}", self.socket.display()))?;
        stream.write_all(request.as_bytes())?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        Ok(reply)
    }

    fn json<T: DeserializeOwned>(&self, request: &str) -> Result<T> {
        let reply = self.ask(request)?;
        serde_json::from_str(&reply).with_context(|| format!("unexpected reply from Hyprland to {request}: {reply}"))
    }

    fn raw_monitors(&self) -> Result<Vec<RawMonitor>> {
        let mut monitors: Vec<RawMonitor> = self.json("j/monitors")?;
        monitors.retain(|m| !m.disabled);
        // Left to right. A monitor's number must not change while Claude is using it, so the
        // focused one can't come first the way the primary one does on Windows.
        monitors.sort_by_key(|m| (m.x, m.y));
        Ok(monitors)
    }
}

impl Backend for Hyprland {
    fn monitors(&mut self) -> Result<Vec<Monitor>> {
        Ok(self.raw_monitors()?.iter().map(RawMonitor::monitor).collect())
    }

    fn capture(&mut self, left: i32, top: i32, width: i32, height: i32) -> Result<RgbImage> {
        let monitors = self.raw_monitors()?;
        let found = monitors.iter().find(|m| m.monitor().contains(left, top));
        let raw = found.with_context(|| format!("no monitor at ({left}, {top})"))?;
        let monitor = raw.monitor();
        let image = upright(self.wayland.capture(&raw.name)?, raw.transform);
        // Real pixels of the frame per coordinate, along each edge.
        let (x_density, y_density) = (image.width() as f64 / monitor.width as f64, image.height() as f64 / monitor.height as f64);
        let x = ((left - monitor.left) as f64 * x_density).round() as u32;
        let y = ((top - monitor.top) as f64 * y_density).round() as u32;
        let (cut_width, cut_height) = ((width as f64 * x_density).round() as u32, (height as f64 * y_density).round() as u32);
        Ok(imageops::crop_imm(&image, x, y, cut_width, cut_height).to_image())
    }

    fn cursor(&mut self) -> Result<(i32, i32)> {
        let reply = self.ask("cursorpos")?;
        parse_cursor(&reply).with_context(|| format!("unexpected pointer position from Hyprland: {reply}"))
    }

    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        // Absolute positions are relative to the box around all the monitors.
        let monitors = self.monitors()?;
        let left = monitors.iter().map(|m| m.left).min().context("no monitors found")?;
        let top = monitors.iter().map(|m| m.top).min().context("no monitors found")?;
        let right = monitors.iter().map(|m| m.left + m.width).max().context("no monitors found")?;
        let bottom = monitors.iter().map(|m| m.top + m.height).max().context("no monitors found")?;
        self.wayland.move_pointer((x - left) as u32, (y - top) as u32, (right - left) as u32, (bottom - top) as u32)
    }

    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        self.wayland.button(button, down)
    }

    fn wheel(&mut self, wheel: Wheel) -> Result<()> {
        self.wayland.wheel(wheel)
    }

    fn key_codes(&mut self, key: Key) -> Result<Vec<KeyCode>> {
        // A virtual keyboard has lock state of its own, so these would report success and do nothing.
        if matches!(key, Key::CapsLock | Key::NumLock) {
            bail!("{key:?} can't be toggled here; type_text types either case and the keypad always types digits");
        }
        Ok(vec![keys::codes(key).0])
    }

    fn keys_for_char(&mut self, c: char) -> Result<Vec<KeyCode>> {
        // Letters and digits name a key whatever their case: ctrl+A is ctrl+a.
        let found = if c.is_ascii_alphanumeric() { keys::us_key(c.to_ascii_lowercase()) } else { keys::us_key(c) };
        match found {
            Some((code, true)) => Ok(vec![keys::SHIFT, code]),
            Some((code, false)) => Ok(vec![code]),
            None => bail!("no key on this keyboard layout types {c:?}; use type_text for text"),
        }
    }

    fn send_keys(&mut self, keys: &[KeyCode], up: bool) -> Result<()> {
        self.wayland.send_keys(keys, up)
    }

    fn type_char(&mut self, c: char, rest: &str) -> Result<()> {
        self.wayland.type_char(c, rest)
    }

    fn windows(&mut self) -> Result<Vec<Window>> {
        let monitors = self.raw_monitors()?;
        let shown: Vec<i64> = monitors.iter().flat_map(|m| [m.active_workspace.id, m.special_workspace.id]).collect();
        let mut clients: Vec<Client> = self.json("j/clients")?;
        clients.retain(|c| c.mapped && !c.title.is_empty());
        clients.sort_by_key(|c| c.focus_history);
        clients.into_iter().map(|c| c.window(&shown)).collect()
    }

    fn foreground(&mut self) -> Result<Option<WindowId>> {
        let active: Active = self.json("j/activewindow")?;
        active.address.as_deref().map(parse_address).transpose()
    }

    /// Focuses `window`, switching to its workspace if it needs to. Hyprland's dispatchers changed
    /// with its Lua config: `dispatch` takes Lua there, and the old syntax elsewhere.
    fn activate(&mut self, window: &Window) -> Result<()> {
        let address = format!("0x{:x}", window.id);
        if self.ask(&format!("dispatch hl.dsp.focus({{ window = 'address:{address}' }})"))? == "ok" {
            return Ok(());
        }
        let reply = self.ask(&format!("dispatch focuswindow address:{address}"))?;
        if reply != "ok" {
            bail!("Hyprland refused to focus {}: {reply}", window.label());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(width: i32, height: i32, scale: f64, transform: u32) -> RawMonitor {
        let workspace = || Workspace { id: 1, name: "1".into() };
        RawMonitor {
            name: "X".into(),
            x: 0,
            y: 0,
            width,
            height,
            scale,
            transform,
            focused: true,
            disabled: false,
            active_workspace: workspace(),
            special_workspace: workspace(),
        }
    }

    #[test]
    fn logical_size_comes_from_the_scale() {
        let monitor = raw(1920, 1200, 1.5, 0).monitor();
        assert_eq!((monitor.width, monitor.height, monitor.density), (1280, 800, 1.5));
    }

    #[test]
    fn a_rotated_monitor_swaps_its_edges() {
        let monitor = raw(1920, 1080, 1.0, 1).monitor();
        assert_eq!((monitor.width, monitor.height), (1080, 1920));
    }

    #[test]
    fn a_normal_frame_stays_as_it_is_and_a_rotated_one_turns() {
        let frame = RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8, y as u8, 0]));
        assert_eq!(upright(frame.clone(), 0), frame);
        assert_eq!(upright(frame.clone(), 1).dimensions(), (2, 3));
        assert_eq!(upright(frame.clone(), 2).dimensions(), (3, 2));
        assert_eq!(upright(frame, 7).dimensions(), (2, 3));
    }

    #[test]
    fn reads_the_pointer_position() {
        assert_eq!(parse_cursor("1668, 642"), Some((1668, 642)));
        assert_eq!(parse_cursor("-20, 7\n"), Some((-20, 7)));
        assert_eq!(parse_cursor("error"), None);
    }

    #[test]
    fn windows_on_a_workspace_nobody_shows_are_hidden() {
        let json = r#"{"address":"0x10","mapped":true,"at":[5,6],"size":[10,20],
            "workspace":{"id":3,"name":"3"},"class":"kitty","title":"t","focusHistoryID":0}"#;
        let client = || serde_json::from_str::<Client>(json).unwrap();
        let shown = client().window(&[3]).unwrap();
        assert_eq!(shown.id, 0x10);
        assert_eq!(shown.rect, Rect { left: 5, top: 6, right: 15, bottom: 26 });
        assert_eq!(shown.hidden, None);
        assert_eq!(client().window(&[1]).unwrap().hidden.as_deref(), Some("on workspace 3"));
    }

    #[test]
    fn no_active_window_is_an_empty_object() {
        assert!(serde_json::from_str::<Active>("{}").unwrap().address.is_none());
    }
}
