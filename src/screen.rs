//! Monitors and the images made of them. Capturing the screen is platform code: see `sys`.
//!
//! Places on the desktop are in desktop coordinates. Those are physical pixels on Windows and X11,
//! and the compositor's logical pixels on Wayland, where a scaled monitor has more real pixels than
//! coordinates. `Monitor::density` relates the two.

use anyhow::Result;
use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};

pub use crate::sys::{capture, monitors};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    /// The monitor to start on: the primary one, or the focused one where there's no such thing.
    pub primary: bool,
    /// Real pixels per desktop coordinate.
    pub density: f64,
}

impl Monitor {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }

    pub fn is_corner(&self, x: i32, y: i32) -> bool {
        (x == self.left || x == self.left + self.width - 1) && (y == self.top || y == self.top + self.height - 1)
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
