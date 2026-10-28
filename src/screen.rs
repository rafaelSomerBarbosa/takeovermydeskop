//! Monitors and screen capture through GDI. The process is per-monitor DPI aware (see `main`),
//! so every coordinate here is a physical pixel on the virtual desktop.

use anyhow::{Result, anyhow, bail};
use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, HDC, HMONITOR,
    MONITORINFO, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, ICONINFO,
};
use windows::core::BOOL;

const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

impl Monitor {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }

    pub fn is_corner(&self, x: i32, y: i32) -> bool {
        (x == self.left || x == self.left + self.width - 1) && (y == self.top || y == self.top + self.height - 1)
    }
}

/// All monitors, primary first and the rest from left to right.
pub fn monitors() -> Vec<Monitor> {
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
            });
        }
        true.into()
    }

    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut list as *mut Vec<Monitor> as isize));
    }
    list.sort_by_key(|m| (!m.primary, m.left, m.top));
    list
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

/// Resizes to exactly `width`x`height` (a no-op when it already is).
pub fn fit(image: RgbImage, width: u32, height: u32) -> RgbImage {
    if image.dimensions() == (width, height) {
        return image;
    }
    let filter = if width > image.width() { FilterType::CatmullRom } else { FilterType::Triangle };
    imageops::resize(&image, width, height, filter)
}

pub fn jpeg(image: &RgbImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 85).encode_image(image)?;
    Ok(bytes)
}
