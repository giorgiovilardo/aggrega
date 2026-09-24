//! Article thumbnails: downloaded once, shrunk, and cached on disk as small JPEGs.
//! Decoding happens off the UI thread; only ready-to-upload pixel buffers
//! are handed to Slint.

use std::path::Path;

use anyhow::{Result, bail};
use image::imageops::FilterType;
use slint::{Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};

use crate::{fetch, text};

/// Thumbnail size in physical pixels (2× the on-screen size for HiDPI).
const W: u32 = 264;
const H: u32 = 184;

pub type Pixels = SharedPixelBuffer<Rgb8Pixel>;
pub type Picture = SharedPixelBuffer<Rgba8Pixel>;

/// Reader images are shrunk to this width (2× the widest reading column).
const PICTURE_MAX_W: u32 = 1440;
/// Tall infographics are capped so a single image can't exhaust memory.
const PICTURE_MAX_H: u32 = 4096;

pub enum Thumb {
    Ready(Pixels),
    /// The image is unusable; remembered on disk so it isn't retried.
    Broken,
    /// Couldn't download it right now (offline); try again later.
    Unavailable,
}

/// Loads a thumbnail from the disk cache, or downloads and caches it.
pub fn load(agent: &ureq::Agent, dir: &Path, url: &str) -> Thumb {
    let key = format!("{:016x}", text::fnv1a(url));
    let jpg = dir.join(format!("{key}.jpg"));
    let failed = dir.join(format!("{key}.none"));

    if let Ok(img) = image::open(&jpg) {
        return Thumb::Ready(to_pixels(img.to_rgb8()));
    }
    if failed.exists() {
        return Thumb::Broken;
    }
    match fetch_and_shrink(agent, url) {
        Ok(rgb) => {
            let _ = rgb.save_with_format(&jpg, image::ImageFormat::Jpeg);
            Thumb::Ready(to_pixels(rgb))
        }
        // Offline: keep the placeholder and retry on a later reload.
        Err(e) if fetch::is_unreachable(&e) => Thumb::Unavailable,
        Err(_) => {
            let _ = std::fs::write(&failed, b"");
            Thumb::Broken
        }
    }
}

fn fetch_and_shrink(agent: &ureq::Agent, url: &str) -> Result<image::RgbImage> {
    let bytes = fetch::download_image(agent, url)?;
    let img = image::load_from_memory(&bytes)?;
    if img.width() < 48 || img.height() < 48 {
        bail!("image too small to be a thumbnail");
    }
    Ok(img.resize_to_fill(W, H, FilterType::Triangle).to_rgb8())
}

/// Downloads an image for the reader view, shrunk to fit the reading column.
/// Not cached on disk: reader images are only needed while an article is open.
pub fn load_picture(agent: &ureq::Agent, url: &str) -> Option<Picture> {
    let bytes = fetch::download_image(agent, url).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    // Icons, spacers and tracking pixels aren't worth a block of their own.
    if img.width() < 64 || img.height() < 32 {
        return None;
    }
    let (w, h) = fit(img.width(), img.height(), PICTURE_MAX_W, PICTURE_MAX_H);
    let img = if (w, h) == (img.width(), img.height()) {
        img
    } else {
        img.resize_exact(w, h, FilterType::Triangle)
    };
    let rgba = img.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}

/// Scales `w`×`h` down (never up) to fit within `max_w`×`max_h`, keeping the aspect ratio.
fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

fn to_pixels(rgb: image::RgbImage) -> Pixels {
    SharedPixelBuffer::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_pictures_without_upscaling() {
        assert_eq!(fit(800, 600, 1440, 4096), (800, 600));
        assert_eq!(fit(2880, 1620, 1440, 4096), (1440, 810));
        assert_eq!(fit(1000, 10_000, 1440, 4096), (410, 4096));
        assert_eq!(fit(100_000, 1, 1440, 4096), (1440, 1));
    }

    #[test]
    fn unreachable_picture_is_none() {
        assert!(load_picture(&fetch::agent(), "http://127.0.0.1:9/a.png").is_none());
    }
}
