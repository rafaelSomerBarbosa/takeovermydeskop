//! Windows: GDI for screenshots, SendInput for input (the same path real devices use) and the Win32
//! window list. The process is per-monitor DPI aware (see `init`), so every coordinate here is a
//! physical pixel on the virtual desktop.

use crate::input::{Button, Combo, Key, Wheel};
use crate::screen::{Monitor, Rect};
use crate::window::Window;
use anyhow::{Result, anyhow, bail};
use image::RgbImage;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, HDC, HMONITOR,
    MONITORINFO, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, EnumWindows, GWL_EXSTYLE, GetCursorInfo, GetCursorPos,
    GetForegroundWindow, GetIconInfo, GetSystemMetrics, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, ICONINFO, IsIconic, IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_SWAPBUTTON, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, SetCursorPos, SetForegroundWindow, ShowWindow,
    WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

pub type KeyCode = VIRTUAL_KEY;
pub type WindowId = HWND;

/// What Claude can't reach on this platform, for its instructions.
pub const LIMITS: &str = "Windows ignores this input in UAC prompts, on the lock screen and in apps running as administrator; ask the user to handle those.";

/// Whether `launch_app` is offered. Windows always has a Start menu to click.
pub const HAS_LAUNCHER: bool = false;

const MONITORINFOF_PRIMARY: u32 = 1;
/// One click of the mouse wheel (WHEEL_DELTA).
const NOTCH: i32 = 120;

pub fn init() {
    // Work in physical pixels. Otherwise Windows scales screenshots and coordinates on high-DPI
    // monitors, and clicks land in the wrong place.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// All monitors, primary first and the rest from left to right.
pub fn monitors() -> Result<Vec<Monitor>> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
        let list = unsafe { &mut *(list.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let r = info.rcMonitor;
            list.push(Monitor {
                left: r.left,
                top: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                density: 1.0,
            });
        }
        true.into()
    }

    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut list as *mut Vec<Monitor> as isize));
    }
    list.sort_by_key(|m| (!m.primary, m.left, m.top));
    Ok(list)
}

/// Copies a rectangle of the screen. GDI leaves the mouse pointer out, so it is drawn back in:
/// seeing it tells Claude where the last action landed.
pub fn capture(left: i32, top: i32, width: i32, height: i32) -> Result<RgbImage> {
    if width <= 0 || height <= 0 {
        bail!("empty capture area ({width}x{height})");
    }
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            bail!("couldn't access the screen (GetDC failed)");
        }
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, bitmap.into());
        // CAPTUREBLT includes layered windows: menus, tooltips and many modern popups.
        let copied = BitBlt(memory, 0, 0, width, height, Some(screen), left, top, SRCCOPY | CAPTUREBLT);
        if copied.is_ok() {
            draw_pointer(memory, left, top);
        }
        // GetDIBits needs the bitmap deselected first.
        SelectObject(memory, previous);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative: rows top to bottom
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = GetDIBits(memory, bitmap, 0, height as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        copied.map_err(|e| anyhow!("couldn't copy the screen ({e}); it may be locked"))?;
        if rows != height {
            bail!("couldn't read the captured pixels (GetDIBits returned {rows} rows)");
        }
    }
    let rgb = bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
    RgbImage::from_raw(width as u32, height as u32, rgb).ok_or_else(|| anyhow!("captured buffer has the wrong size"))
}

fn draw_pointer(dc: HDC, left: i32, top: i32) {
    let mut cursor = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
    if unsafe { GetCursorInfo(&mut cursor) }.is_err() || cursor.flags.0 & CURSOR_SHOWING.0 == 0 {
        return;
    }
    let mut icon = ICONINFO::default();
    if unsafe { GetIconInfo(cursor.hCursor.into(), &mut icon) }.is_err() {
        return;
    }
    unsafe {
        // GetIconInfo hands over copies of the pointer's bitmaps; we only needed the hotspot.
        let _ = DeleteObject(icon.hbmMask.into());
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(icon.hbmColor.into());
        }
        let x = cursor.ptScreenPos.x - icon.xHotspot as i32 - left;
        let y = cursor.ptScreenPos.y - icon.yHotspot as i32 - top;
        let _ = DrawIconEx(dc, x, y, cursor.hCursor.into(), 0, 0, 0, None, DI_NORMAL);
    }
}

pub fn cursor() -> Result<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point)? };
    Ok((point.x, point.y))
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

pub fn button(button: Button, down: bool) -> Result<()> {
    // SendInput presses physical buttons, so undo a left-handed button swap.
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
    let physical = match (button, swapped) {
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
    send(&[mouse(flags, 0, 0, 0)])
}

pub fn wheel(wheel: Wheel) -> Result<()> {
    let (flags, delta) = match wheel {
        Wheel::Up => (MOUSEEVENTF_WHEEL, NOTCH),
        Wheel::Down => (MOUSEEVENTF_WHEEL, -NOTCH),
        Wheel::Left => (MOUSEEVENTF_HWHEEL, -NOTCH),
        Wheel::Right => (MOUSEEVENTF_HWHEEL, NOTCH),
    };
    send(&[mouse(flags, 0, 0, delta as u32)])
}

pub fn send_keys(keys: &[KeyCode], up: bool) -> Result<()> {
    send(&keys.iter().map(|&k| key(k, up)).collect::<Vec<_>>())
}

/// Types `c` as a Unicode keystroke. `_rest` is for platforms that need to plan ahead.
pub fn type_char(c: char, _rest: &str) -> Result<()> {
    let mut units = [0u16; 2];
    send(&c.encode_utf16(&mut units).iter().flat_map(|&unit| [unicode(unit, false), unicode(unit, true)]).collect::<Vec<_>>())
}

pub fn keys_for_char(c: char) -> Result<Vec<KeyCode>> {
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

pub fn key_codes(key: Key) -> Result<Vec<KeyCode>> {
    Ok(vec![match key {
        Key::Ctrl => VK_CONTROL,
        Key::RightCtrl => VK_RCONTROL,
        Key::Shift => VK_SHIFT,
        Key::RightShift => VK_RSHIFT,
        Key::Alt => VK_MENU,
        Key::AltGr => VK_RMENU,
        Key::Super => VK_LWIN,
        Key::RightSuper => VK_RWIN,
        Key::Enter => VK_RETURN,
        Key::Esc => VK_ESCAPE,
        Key::Tab => VK_TAB,
        Key::Space => VK_SPACE,
        Key::Backspace => VK_BACK,
        Key::Delete => VK_DELETE,
        Key::Insert => VK_INSERT,
        Key::Home => VK_HOME,
        Key::End => VK_END,
        Key::PageUp => VK_PRIOR,
        Key::PageDown => VK_NEXT,
        Key::Up => VK_UP,
        Key::Down => VK_DOWN,
        Key::Left => VK_LEFT,
        Key::Right => VK_RIGHT,
        Key::CapsLock => VK_CAPITAL,
        Key::NumLock => VK_NUMLOCK,
        Key::ScrollLock => VK_SCROLL,
        Key::PrintScreen => VK_SNAPSHOT,
        Key::Pause => VK_PAUSE,
        Key::Menu => VK_APPS,
        Key::F(n) => VIRTUAL_KEY(VK_F1.0 + n as u16 - 1),
        Key::Numpad(n) => VIRTUAL_KEY(VK_NUMPAD0.0 + n as u16),
        Key::Multiply => VK_MULTIPLY,
        Key::Add => VK_ADD,
        Key::Subtract => VK_SUBTRACT,
        Key::Decimal => VK_DECIMAL,
        Key::Divide => VK_DIVIDE,
        Key::VolumeUp => VK_VOLUME_UP,
        Key::VolumeDown => VK_VOLUME_DOWN,
        Key::VolumeMute => VK_VOLUME_MUTE,
        Key::PlayPause => VK_MEDIA_PLAY_PAUSE,
        Key::NextTrack => VK_MEDIA_NEXT_TRACK,
        Key::PrevTrack => VK_MEDIA_PREV_TRACK,
        Key::MediaStop => VK_MEDIA_STOP,
        Key::BrowserBack => VK_BROWSER_BACK,
        Key::BrowserForward => VK_BROWSER_FORWARD,
        Key::BrowserRefresh => VK_BROWSER_REFRESH,
    }])
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

pub fn launch(_app: &str) -> Result<String> {
    bail!("launch_app isn't available on Windows; use the Start menu")
}

/// Visible, titled windows from the frontmost back, roughly what Alt+Tab shows.
pub fn list() -> Result<Vec<Window>> {
    unsafe extern "system" fn collect(handle: HWND, list: LPARAM) -> BOOL {
        unsafe { (*(list.0 as *mut Vec<HWND>)).push(handle) };
        true.into()
    }

    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut handles as *mut Vec<HWND> as isize));
    }
    Ok(handles.into_iter().filter_map(describe).collect())
}

pub fn foreground() -> Result<Option<WindowId>> {
    let handle = unsafe { GetForegroundWindow() };
    Ok((!handle.is_invalid()).then_some(handle))
}

/// "Title" (app.exe) of the window with keyboard focus.
pub fn foreground_label() -> String {
    let handle = unsafe { GetForegroundWindow() };
    match describe(handle) {
        Some(window) => window.label(),
        None if handle.is_invalid() => "none".into(),
        None => format!("\"{}\"", title(handle)),
    }
}

/// Brings `window` to the front, restoring it if it's minimized.
pub fn activate(window: &Window) -> Result<()> {
    unsafe {
        if window.hidden.is_some() {
            let _ = ShowWindow(window.id, SW_RESTORE);
        }
        if !SetForegroundWindow(window.id).as_bool() || GetForegroundWindow() != window.id {
            // Windows only lets the app in front hand focus away; a tap of Alt lifts that lock.
            Combo::parse("alt")?.press()?;
            let _ = SetForegroundWindow(window.id);
        }
    }
    Ok(())
}

fn describe(handle: HWND) -> Option<Window> {
    unsafe {
        if !IsWindowVisible(handle).as_bool() || GetWindowLongW(handle, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        // Suspended UWP apps and windows on other virtual desktops are "cloaked": there but not shown.
        let mut cloaked = 0u32;
        let attribute = DwmGetWindowAttribute(handle, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        if attribute.is_ok() && cloaked != 0 {
            return None;
        }
        let title = title(handle);
        if title.is_empty() {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(handle, &mut rect).ok()?;
        Some(Window {
            id: handle,
            title,
            app: app_name(handle),
            rect: Rect { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom },
            hidden: IsIconic(handle).as_bool().then(|| "minimized".to_string()),
        })
    }
}

fn title(handle: HWND) -> String {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(handle, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn app_name(handle: HWND) -> String {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(handle, Some(&mut pid)) };
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    let found = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len) };
    unsafe {
        let _ = CloseHandle(process);
    }
    if found.is_err() {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    path.rsplit('\\').next().unwrap_or_default().to_string()
}
