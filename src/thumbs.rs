//! Article thumbnails: downloaded once, shrunk, and cached on disk as small JPEGs.
//! Decoding happens off the UI thread; only ready-to-upload pixel buffers
//! are handed to Slint.

use std::path::Path;

use anyhow::{Result, bail};
use image::imageops::FilterType;
use slint::{Rgb8Pixel, SharedPixelBuffer};

use crate::{fetch, text};

/// Thumbnail size in physical pixels (2× the on-screen size for HiDPI).
const W: u32 = 264;
const H: u32 = 184;

pub type Pixels = SharedPixelBuffer<Rgb8Pixel>;

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

fn to_pixels(rgb: image::RgbImage) -> Pixels {
    SharedPixelBuffer::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height())
}
