//! Top-level windows: listing them and bringing one to the front.

use crate::input;
use crate::screen::Rect;
use crate::sys;
use anyhow::{Result, bail};

pub use crate::sys::{foreground, foreground_label, list};

#[derive(Clone)]
pub struct Window {
    pub id: sys::WindowId,
    pub title: String,
    /// What the desktop calls the app, e.g. "chrome.exe" or "firefox" (empty when it won't say,
    /// e.g. for admin apps on Windows).
    pub app: String,
    pub rect: Rect,
    /// Why the window isn't on screen, e.g. "minimized", when it isn't.
    pub hidden: Option<String>,
}

impl Window {
    pub fn label(&self) -> String {
        if self.app.is_empty() { format!("\"{}\"", self.title) } else { format!("\"{}\" ({})", self.title, self.app) }
    }
}

/// Brings the best match for `query` (title or app name, case-insensitive) to the front.
pub fn focus(query: &str) -> Result<Window> {
    let query = query.trim().to_lowercase();
    let windows = list()?;
    let app = |w: &&Window| w.app.to_lowercase().trim_end_matches(".exe") == query.trim_end_matches(".exe");
    let found = windows
        .iter()
        .find(|w| w.title.to_lowercase() == query)
        .or_else(|| windows.iter().find(|w| w.title.to_lowercase().contains(&query)))
        .or_else(|| windows.iter().find(app));
    let Some(window) = found else {
        let launcher = if sys::HAS_LAUNCHER { ", or launch_app to start the app" } else { "" };
        bail!("no open window matches {query:?}; call list_windows to see what's open{launcher}");
    };
    activate(window)
}

/// What `sys::launch` did.
pub enum Launched {
    /// Started the app called this. Its window is still to come. (Linux)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Started(String),
    /// Saw to the app's window itself: a note on what happened, and the window when it's now in
    /// front. (Windows)
    #[cfg_attr(not(windows), allow(dead_code))]
    Shown(String, Option<Window>),
}

/// Brings `window` to the front, restoring it if it's minimized, and returns it as it is then.
pub fn activate(window: &Window) -> Result<Window> {
    sys::activate(window)?;
    input::pause(150);
    if foreground()? != Some(window.id) {
        bail!("the desktop refused to bring {} to the front; try clicking it", window.label());
    }
    // Re-read: restoring a minimized window changes its position.
    Ok(list()?.into_iter().find(|w| w.id == window.id).unwrap_or_else(|| window.clone()))
}
