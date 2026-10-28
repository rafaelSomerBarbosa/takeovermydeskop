//! Top-level windows: listing them and bringing one to the front.

use crate::input::{self, Combo};
use anyhow::{Result, bail};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindow, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

#[derive(Clone)]
pub struct Window {
    pub handle: HWND,
    pub title: String,
    /// Executable name, e.g. "chrome.exe" (empty when Windows won't say, e.g. for admin apps).
    pub app: String,
    pub rect: RECT,
    pub minimized: bool,
}

impl Window {
    pub fn label(&self) -> String {
        if self.app.is_empty() { format!("\"{}\"", self.title) } else { format!("\"{}\" ({})", self.title, self.app) }
    }
}

/// Visible, titled windows from the frontmost back, roughly what Alt+Tab shows.
pub fn list() -> Vec<Window> {
    unsafe extern "system" fn collect(handle: HWND, list: LPARAM) -> BOOL {
        unsafe { (*(list.0 as *mut Vec<HWND>)).push(handle) };
        true.into()
    }

    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut handles as *mut Vec<HWND> as isize));
    }
    handles.into_iter().filter_map(describe).collect()
}

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// "Title" (app.exe) of the window with keyboard focus.
pub fn foreground_label() -> String {
    let handle = foreground();
    match describe(handle) {
        Some(window) => window.label(),
        None if handle.is_invalid() => "none".into(),
        None => format!("\"{}\"", title(handle)),
    }
}

/// Brings the best match for `query` (title or app name, case-insensitive) to the front.
pub fn focus(query: &str) -> Result<Window> {
    let query = query.trim().to_lowercase();
    let windows = list();
    let app = |w: &&Window| w.app.to_lowercase().trim_end_matches(".exe") == query.trim_end_matches(".exe");
    let found = windows
        .iter()
        .find(|w| w.title.to_lowercase() == query)
        .or_else(|| windows.iter().find(|w| w.title.to_lowercase().contains(&query)))
        .or_else(|| windows.iter().find(app));
    let Some(window) = found else {
        bail!("no open window matches {query:?}; call list_windows to see what's open");
    };
    unsafe {
        if window.minimized {
            let _ = ShowWindow(window.handle, SW_RESTORE);
        }
        if !SetForegroundWindow(window.handle).as_bool() || GetForegroundWindow() != window.handle {
            // Windows only lets the app in front hand focus away; a tap of Alt lifts that lock.
            Combo::parse("alt")?.press()?;
            let _ = SetForegroundWindow(window.handle);
        }
    }
    input::pause(150);
    if foreground() != window.handle {
        bail!("Windows refused to bring {} to the front; try clicking it in the taskbar", window.label());
    }
    // Re-read: restoring a minimized window changes its position.
    Ok(describe(window.handle).unwrap_or_else(|| window.clone()))
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
        Some(Window { handle, title, app: app_name(handle), rect, minimized: IsIconic(handle).as_bool() })
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
