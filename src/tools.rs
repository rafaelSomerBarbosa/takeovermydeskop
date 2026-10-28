//! The desktop tools Claude calls: their definitions for tools/list and what each one does.
//! Coordinates going in and out are pixels of the screenshot Claude saw, which may be scaled down
//! from the monitor's real resolution; `View` converts between the two.

use crate::input::{self, Button, Combo};
use crate::screen::{self, Monitor};
use crate::window;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::RgbImage;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use windows::Win32::Foundation::RECT;

/// Longest image edge that reaches the model untouched. Requests with more than 20 images (any
/// real session) only take images up to 2000x2000, and Claude Code shrinks bigger ones, which
/// would throw coordinates off.
const MAX_EDGE: f64 = 2000.0;
/// Pixel budget per image: 1080p's worth balances click accuracy against image tokens.
/// TAKEOVER_MAX_PIXELS overrides it.
const DEFAULT_MAX_PIXELS: f64 = 1920.0 * 1080.0;
/// Most a zoom enlarges the screen's real pixels.
const MAX_ZOOM: f64 = 3.0;
/// Time for the UI to react before the screenshot that follows an action.
const SETTLE_MS: u64 = 500;

const STOPPED: &str = "STOPPED BY USER: the mouse pointer was pushed into a screen corner, which is the \
emergency stop. Don't retry and don't continue the task: stop now and tell the user what you were doing.";

/// Which part of a monitor screenshots show. Squeezed into one image, an ultrawide monitor gets
/// too small to read, so it can also be viewed a 16:9 slice at a time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Full,
    Left,
    Center,
    Right,
}

impl Part {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name.trim().to_ascii_lowercase().as_str() {
            "full" => Part::Full,
            "left" => Part::Left,
            "center" => Part::Center,
            "right" => Part::Right,
            other => bail!("unknown part {other:?}; use full, left, center or right"),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Part::Full => "full",
            Part::Left => "left",
            Part::Center => "center",
            Part::Right => "right",
        }
    }

    /// The rectangle of `monitor` this part covers.
    fn area(self, monitor: Monitor) -> Monitor {
        let width = monitor.width.min((monitor.height as f64 * 16.0 / 9.0).round() as i32);
        let left = match self {
            Part::Full => return monitor,
            Part::Left => monitor.left,
            Part::Center => monitor.left + (monitor.width - width) / 2,
            Part::Right => monitor.left + monitor.width - width,
        };
        Monitor { left, width, ..monitor }
    }

    /// The sharpest part that shows all of `rect` (with some slack for window shadows).
    fn showing(monitor: Monitor, rect: RECT) -> Self {
        if !is_ultrawide(monitor) {
            return Part::Full;
        }
        let slack = 16;
        [Part::Center, Part::Left, Part::Right]
            .into_iter()
            .find(|part| {
                let area = part.area(monitor);
                rect.left >= area.left - slack && rect.right <= area.left + area.width + slack
            })
            .unwrap_or(Part::Full)
    }
}

/// 32:9 and wider; 21:9 monitors still read fine in one image.
fn is_ultrawide(monitor: Monitor) -> bool {
    monitor.width as f64 >= monitor.height as f64 * 2.5
}

/// How the shown area of the screen maps onto the screenshot Claude sees of it.
#[derive(Clone, Copy)]
struct View {
    index: usize,
    part: Part,
    /// The rectangle of the screen shown: a whole monitor or part of one.
    area: Monitor,
    /// Screenshot pixels per screen pixel (at most 1).
    scale: f64,
    width: u32,
    height: u32,
}

impl View {
    fn new(index: usize, monitor: Monitor, part: Part, max_pixels: f64) -> Self {
        let area = part.area(monitor);
        let scale = budget_scale(area.width, area.height, max_pixels).min(1.0);
        let width = (area.width as f64 * scale).round() as u32;
        let height = (area.height as f64 * scale).round() as u32;
        Self { index, part, area, scale, width, height }
    }

    /// The screen pixel under the center of screenshot pixel (x, y).
    fn to_screen(self, x: f64, y: f64) -> Result<(i32, i32)> {
        // Written so NaN fails too.
        if !(x >= 0.0 && x < self.width as f64 && y >= 0.0 && y < self.height as f64) {
            bail!("({x}, {y}) is outside the screenshot, which is {}x{}", self.width, self.height);
        }
        Ok((
            self.area.left + ((x.floor() + 0.5) / self.scale) as i32,
            self.area.top + ((y.floor() + 0.5) / self.scale) as i32,
        ))
    }

    fn to_image(self, x: i32, y: i32) -> Option<(u32, u32)> {
        self.area.contains(x, y).then(|| {
            (((x - self.area.left) as f64 * self.scale) as u32, ((y - self.area.top) as f64 * self.scale) as u32)
        })
    }

    fn describe(self) -> String {
        match self.part {
            Part::Full => format!("monitor {}", self.index),
            part => format!("the {} part of monitor {}", part.name(), self.index),
        }
    }
}

pub struct Desktop {
    max_pixels: f64,
    /// Index into `screen::monitors()` of the monitor the next screenshot shows, and which part.
    monitor: usize,
    part: Part,
    /// The view of the last screenshot Claude got: the coordinates it sends refer to that image.
    shown: Option<View>,
    /// Where our last action left the pointer, to tell the user's moves from ours.
    pointer: Option<(i32, i32)>,
}

impl Desktop {
    pub fn new() -> Self {
        let max_pixels = std::env::var("TAKEOVER_MAX_PIXELS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|pixels| (300_000.0..=3_750_000.0).contains(pixels))
            .unwrap_or(DEFAULT_MAX_PIXELS);
        Self { max_pixels, monitor: 0, part: Part::Full, shown: None, pointer: None }
    }

    /// Runs one tool and returns the MCP tools/call result.
    pub fn call(&mut self, name: &str, args: Value) -> Value {
        let args = if args.is_null() { json!({}) } else { args };
        match self.run(name, args) {
            Ok(content) => json!({ "content": content }),
            Err(error) => json!({ "content": [text(format!("{error:#}"))], "isError": true }),
        }
    }

    /// What Claude would see right now, for `takeovermydeskop screenshot`.
    pub fn save_screenshot(&self, path: &str) -> Result<()> {
        let (image, _) = self.capture()?;
        image.save(path).with_context(|| format!("couldn't save {path}"))
    }

    fn run(&mut self, name: &str, args: Value) -> Result<Vec<Value>> {
        match name {
            "screenshot" => {
                let args: ScreenshotArgs = parse(args)?;
                if let Some(index) = args.monitor {
                    let count = screen::monitors().len();
                    if index >= count {
                        bail!("there is no monitor {index}; monitors are numbered 0 to {}", count - 1);
                    }
                    if index != self.monitor {
                        self.monitor = index;
                        self.part = Part::Full;
                    }
                }
                if let Some(part) = args.part {
                    self.part = Part::parse(&part)?;
                }
                self.screenshot("")
            }
            "zoom" => self.zoom(parse(args)?),
            "list_windows" => Ok(vec![text(self.windows_report())]),
            action => {
                let want_screenshot = args.get("screenshot").and_then(Value::as_bool).unwrap_or(true);
                self.check_stop()?;
                let done = self.act(action, args);
                // Remember where we left the pointer even when the action failed half-way.
                self.pointer = Some(input::cursor());
                let note = done?;
                if want_screenshot {
                    input::pause(SETTLE_MS);
                    self.screenshot(&format!("{note}\n"))
                } else {
                    Ok(vec![text(note)])
                }
            }
        }
    }

    fn act(&mut self, action: &str, args: Value) -> Result<String> {
        match action {
            "click" => {
                let args: ClickArgs = parse(args)?;
                let (x, y) = self.shown_view()?.to_screen(args.x, args.y)?;
                let button = Button::parse(args.button.as_deref().unwrap_or("left"))?;
                let clicks = args.clicks.unwrap_or(1).clamp(1, 3);
                with_held(args.modifiers.as_deref(), || input::click(x, y, button, clicks))?;
                let what = ["Clicked", "Double-clicked", "Triple-clicked"][clicks as usize - 1];
                Ok(format!("{what} at ({}, {}).", args.x, args.y))
            }
            "move_mouse" => {
                let args: PointArgs = parse(args)?;
                let (x, y) = self.shown_view()?.to_screen(args.x, args.y)?;
                input::move_to(x, y)?;
                Ok(format!("Moved the mouse to ({}, {}).", args.x, args.y))
            }
            "drag" => {
                let args: DragArgs = parse(args)?;
                let view = self.shown_view()?;
                let from = view.to_screen(args.from_x, args.from_y)?;
                let to = view.to_screen(args.to_x, args.to_y)?;
                input::drag(from, to, Button::parse(args.button.as_deref().unwrap_or("left"))?)?;
                Ok(format!("Dragged from ({}, {}) to ({}, {}).", args.from_x, args.from_y, args.to_x, args.to_y))
            }
            "scroll" => {
                let args: ScrollArgs = parse(args)?;
                let (x, y) = self.shown_view()?.to_screen(args.x, args.y)?;
                let notches = args.amount.unwrap_or(3).clamp(1, 50);
                input::scroll(x, y, &args.direction, notches)?;
                Ok(format!("Scrolled {} {notches} notches at ({}, {}).", args.direction, args.x, args.y))
            }
            "type_text" => {
                let args: TypeTextArgs = parse(args)?;
                input::type_text(&args.text, || self.check_stop())?;
                Ok(format!("Typed {} characters.", args.text.chars().count()))
            }
            "press_keys" => {
                let args: PressKeysArgs = parse(args)?;
                let combo = Combo::parse(&args.keys)?;
                let times = args.repeat.unwrap_or(1).clamp(1, 50);
                for i in 0..times {
                    if i > 0 {
                        input::pause(50);
                        self.check_stop()?;
                    }
                    combo.press()?;
                }
                let repeated = if times > 1 { format!(" {times} times") } else { String::new() };
                Ok(format!("Pressed {}{repeated}.", args.keys))
            }
            "wait" => {
                let args: WaitArgs = parse(args)?;
                let seconds = if args.seconds.is_finite() { args.seconds.clamp(0.0, 60.0) } else { 1.0 };
                input::pause((seconds * 1000.0) as u64);
                Ok(format!("Waited {seconds} s."))
            }
            "focus_window" => {
                let args: FocusWindowArgs = parse(args)?;
                let window = window::focus(&args.title)?;
                let mut note = format!("Focused {}.", window.label());
                // Follow the window so the next screenshot shows it, as sharp as possible.
                let monitors = screen::monitors();
                if let Some(index) = monitor_of(&monitors, window.rect) {
                    let part = Part::showing(monitors[index], window.rect);
                    if (index, part) != (self.monitor, self.part) {
                        (self.monitor, self.part) = (index, part);
                        note += &format!(" Screenshots now show {}.", self.view()?.describe());
                    }
                }
                Ok(note)
            }
            other => bail!("unknown tool {other:?}"),
        }
    }

    /// The emergency stop: the user pushed the pointer into a screen corner (and we didn't).
    fn check_stop(&self) -> Result<()> {
        let (x, y) = input::cursor();
        if self.pointer == Some((x, y)) {
            return Ok(());
        }
        if screen::monitors().iter().any(|m| m.is_corner(x, y)) {
            bail!(STOPPED);
        }
        Ok(())
    }

    /// The view the next screenshot will have.
    fn view(&self) -> Result<View> {
        let monitors = screen::monitors();
        let index = if self.monitor < monitors.len() { self.monitor } else { 0 };
        let monitor = *monitors.get(index).context("no monitors found")?;
        Ok(View::new(index, monitor, self.part, self.max_pixels))
    }

    /// The view coordinates from Claude refer to.
    fn shown_view(&self) -> Result<View> {
        match self.shown {
            Some(view) => Ok(view),
            None => self.view(),
        }
    }

    fn capture(&self) -> Result<(RgbImage, View)> {
        let view = self.view()?;
        let a = view.area;
        let image = screen::capture(a.left, a.top, a.width, a.height)?;
        Ok((screen::fit(image, view.width, view.height), view))
    }

    fn screenshot(&mut self, note: &str) -> Result<Vec<Value>> {
        let (image, view) = self.capture()?;
        let block = jpeg_block(&image)?;
        self.shown = Some(view);
        let (x, y) = input::cursor();
        let pointer = match view.to_image(x, y) {
            Some((x, y)) => format!("at ({x}, {y})"),
            None => "outside this screenshot".into(),
        };
        let scaled = if view.scale < 1.0 {
            format!(", scaled to {:.0}% (zoom in to read small text)", view.scale * 100.0)
        } else {
            String::new()
        };
        let mut summary = format!(
            "{note}Screenshot of {}, {}x{}{scaled}. Mouse pointer {pointer}. Active window: {}.",
            view.describe(),
            view.width,
            view.height,
            window::foreground_label()
        );
        let monitors = screen::monitors();
        if monitors.len() > 1 {
            let list: Vec<String> = monitors
                .iter()
                .enumerate()
                .map(|(i, m)| format!("{i} = {}x{}{}", m.width, m.height, if m.primary { " primary" } else { "" }))
                .collect();
            summary += &format!(" Monitors: {} (pass monitor to screenshot to switch).", list.join(", "));
        }
        if let Some(&monitor) = monitors.get(view.index)
            && is_ultrawide(monitor)
        {
            let sharper = View::new(view.index, monitor, Part::Center, self.max_pixels).scale;
            summary += &format!(
                " Monitor {} is ultrawide: pass part \"left\", \"center\" or \"right\" to screenshot to see a 16:9 slice of it at {:.0}%, or \"full\" for all of it.",
                view.index,
                sharper * 100.0
            );
        }
        Ok(vec![text(summary), block])
    }

    fn zoom(&self, args: ZoomArgs) -> Result<Vec<Value>> {
        let view = self.shown_view()?;
        if !(args.width >= 1.0 && args.height >= 1.0) {
            bail!("width and height must be at least 1");
        }
        // Rejects a rectangle that starts outside the screenshot.
        view.to_screen(args.x, args.y)?;
        let a = view.area;
        let left = a.left + (args.x / view.scale) as i32;
        let top = a.top + (args.y / view.scale) as i32;
        let right = (a.left + ((args.x + args.width) / view.scale).ceil() as i32).min(a.left + a.width);
        let bottom = (a.top + ((args.y + args.height) / view.scale).ceil() as i32).min(a.top + a.height);
        let (width, height) = ((right - left).max(1), (bottom - top).max(1));
        let factor = budget_scale(width, height, self.max_pixels).min(MAX_ZOOM);
        let size = |n: i32| ((n as f64 * factor).round() as u32).max(1);
        let image = screen::fit(screen::capture(left, top, width, height)?, size(width), size(height));
        let summary = format!(
            "Zoomed view of ({}, {}) to ({}, {}), {:.1}x the screenshot's size. Only for looking: take click coordinates from screenshots, not from this image.",
            args.x,
            args.y,
            args.x + args.width,
            args.y + args.height,
            factor / view.scale
        );
        Ok(vec![text(summary), jpeg_block(&image)?])
    }

    fn windows_report(&self) -> String {
        let monitors = screen::monitors();
        let foreground = window::foreground();
        let lines: Vec<String> = window::list()
            .iter()
            .map(|w| {
                let place = match (w.minimized, monitor_of(&monitors, w.rect)) {
                    (true, _) => "minimized".to_string(),
                    (false, Some(index)) => format!("monitor {index}"),
                    (false, None) => "off-screen".to_string(),
                };
                let active = if w.handle == foreground { ", active" } else { "" };
                format!("- {}: {place}{active}", w.label())
            })
            .collect();
        if lines.is_empty() { "No open windows.".into() } else { format!("Open windows, frontmost first:\n{}", lines.join("\n")) }
    }
}

/// Runs `action` with `keys` (e.g. "ctrl+shift") held down.
fn with_held(keys: Option<&str>, action: impl FnOnce() -> Result<()>) -> Result<()> {
    let Some(keys) = keys.filter(|k| !k.trim().is_empty()) else {
        return action();
    };
    let combo = Combo::parse(keys)?;
    combo.hold()?;
    let result = action();
    let released = combo.release();
    result.and(released)
}

/// The scale that fits a `width`x`height` area of the screen into one image.
fn budget_scale(width: i32, height: i32, max_pixels: f64) -> f64 {
    let pixels = width as f64 * height as f64;
    (MAX_EDGE / width.max(height) as f64).min((max_pixels / pixels).sqrt())
}

/// The monitor holding the center of `rect`.
fn monitor_of(monitors: &[Monitor], rect: RECT) -> Option<usize> {
    let (x, y) = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
    monitors.iter().position(|m| m.contains(x, y))
}

fn text(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

fn jpeg_block(image: &RgbImage) -> Result<Value> {
    Ok(json!({ "type": "image", "data": BASE64.encode(screen::jpeg(image)?), "mimeType": "image/jpeg" }))
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|e| anyhow!("invalid arguments: {e}"))
}

#[derive(Deserialize)]
struct ScreenshotArgs {
    monitor: Option<usize>,
    part: Option<String>,
}

#[derive(Deserialize)]
struct ZoomArgs {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Deserialize)]
struct ClickArgs {
    x: f64,
    y: f64,
    button: Option<String>,
    clicks: Option<u32>,
    modifiers: Option<String>,
}

#[derive(Deserialize)]
struct PointArgs {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct DragArgs {
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    button: Option<String>,
}

#[derive(Deserialize)]
struct ScrollArgs {
    x: f64,
    y: f64,
    direction: String,
    amount: Option<u32>,
}

#[derive(Deserialize)]
struct TypeTextArgs {
    text: String,
}

#[derive(Deserialize)]
struct PressKeysArgs {
    keys: String,
    repeat: Option<u32>,
}

#[derive(Deserialize)]
struct WaitArgs {
    seconds: f64,
}

#[derive(Deserialize)]
struct FocusWindowArgs {
    title: String,
}

/// The tool list for tools/list.
pub fn definitions() -> Value {
    let coordinate = |what: &str| json!({ "type": "number", "description": format!("{what}, in screenshot pixels.") });
    let button = json!({ "type": "string", "enum": ["left", "right", "middle"], "description": "Default left." });
    let screenshot_after = json!({
        "type": "boolean",
        "description": "Return a screenshot after the action (default true). Pass false when chaining steps whose result you don't need to see."
    });
    let read_only = json!({ "readOnlyHint": true });
    json!([
        {
            "name": "screenshot",
            "description": "Capture the current monitor, mouse pointer included. Coordinates in every other tool are pixels of this image. With several monitors, pass monitor to switch.",
            "inputSchema": { "type": "object", "properties": {
                "monitor": { "type": "integer", "minimum": 0, "description": "Monitor to show from now on (0 = primary)." },
                "part": {
                    "type": "string",
                    "enum": ["full", "left", "center", "right"],
                    "description": "On an ultrawide monitor, show only a 16:9 slice of it at a higher resolution from now on. Default full."
                }
            } },
            "annotations": read_only
        },
        {
            "name": "zoom",
            "description": "Magnified view of a rectangle of the current monitor, to read small text or check details. Only for looking: keep taking coordinates from screenshots.",
            "inputSchema": { "type": "object", "properties": {
                "x": coordinate("Left edge"),
                "y": coordinate("Top edge"),
                "width": coordinate("Width"),
                "height": coordinate("Height")
            }, "required": ["x", "y", "width", "height"] },
            "annotations": read_only
        },
        {
            "name": "click",
            "description": "Move the mouse to (x, y) and click.",
            "inputSchema": { "type": "object", "properties": {
                "x": coordinate("Horizontal position"),
                "y": coordinate("Vertical position"),
                "button": button,
                "clicks": { "type": "integer", "minimum": 1, "maximum": 3, "description": "2 to double-click, 3 to triple-click. Default 1." },
                "modifiers": { "type": "string", "description": "Keys to hold while clicking, e.g. \"ctrl\" or \"ctrl+shift\"." },
                "screenshot": screenshot_after
            }, "required": ["x", "y"] }
        },
        {
            "name": "move_mouse",
            "description": "Move the mouse to (x, y) without clicking, e.g. to open a hover menu or show a tooltip.",
            "inputSchema": { "type": "object", "properties": {
                "x": coordinate("Horizontal position"),
                "y": coordinate("Vertical position"),
                "screenshot": screenshot_after
            }, "required": ["x", "y"] }
        },
        {
            "name": "drag",
            "description": "Press a mouse button at (from_x, from_y), move to (to_x, to_y) and release: moving files, selecting text, sliders, resizing windows.",
            "inputSchema": { "type": "object", "properties": {
                "from_x": coordinate("Start, horizontal"),
                "from_y": coordinate("Start, vertical"),
                "to_x": coordinate("End, horizontal"),
                "to_y": coordinate("End, vertical"),
                "button": button,
                "screenshot": screenshot_after
            }, "required": ["from_x", "from_y", "to_x", "to_y"] }
        },
        {
            "name": "scroll",
            "description": "Turn the mouse wheel with the pointer over (x, y).",
            "inputSchema": { "type": "object", "properties": {
                "x": coordinate("Horizontal position"),
                "y": coordinate("Vertical position"),
                "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "amount": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Wheel notches, about 3 lines each. Default 3." },
                "screenshot": screenshot_after
            }, "required": ["x", "y", "direction"] }
        },
        {
            "name": "type_text",
            "description": "Type text into whatever has keyboard focus (click the field first). Any characters work; \\n presses Enter and \\t presses Tab.",
            "inputSchema": { "type": "object", "properties": {
                "text": { "type": "string" },
                "screenshot": screenshot_after
            }, "required": ["text"] }
        },
        {
            "name": "press_keys",
            "description": "Press a key or key combination, e.g. \"enter\", \"esc\", \"ctrl+s\", \"alt+tab\", \"win+r\", \"ctrl+shift+t\", \"f5\", \"pagedown\".",
            "inputSchema": { "type": "object", "properties": {
                "keys": { "type": "string" },
                "repeat": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Press it this many times. Default 1." },
                "screenshot": screenshot_after
            }, "required": ["keys"] }
        },
        {
            "name": "wait",
            "description": "Wait for something to load or finish, then take a screenshot.",
            "inputSchema": { "type": "object", "properties": {
                "seconds": { "type": "number", "minimum": 0, "maximum": 60 },
                "screenshot": screenshot_after
            }, "required": ["seconds"] }
        },
        {
            "name": "list_windows",
            "description": "List the open windows, frontmost first, with their app and monitor.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only
        },
        {
            "name": "focus_window",
            "description": "Bring a window to the front by (part of) its title or its app name (e.g. \"notepad\"), restoring it if minimized. Screenshots follow it to its monitor.",
            "inputSchema": { "type": "object", "properties": {
                "title": { "type": "string" },
                "screenshot": screenshot_after
            }, "required": ["title"] }
        }
    ])
}
