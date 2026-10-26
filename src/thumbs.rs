//! Article thumbnails: downloaded once, shrunk, and cached on disk as small JPEGs.
//! Downloading, decoding and resizing all happen off the UI thread; results
//! are returned as plain `image` buffers for the caller to convert.

use std::path::Path;

use image::imageops::FilterType;

use crate::{fetch, text};

/// Thumbnail size in physical pixels (2× the on-screen size for HiDPI).
const W: u32 = 264;
const H: u32 = 184;

/// Reader images are shrunk to this width (2× the widest reading column).
const PICTURE_MAX_W: u32 = 1440;
/// Tall infographics are capped so a single image can't exhaust memory.
const PICTURE_MAX_H: u32 = 4096;

pub enum Thumb {
    Ready(image::RgbImage),
    /// The image is unusable; remembered on disk so it isn't retried.
    Broken,
    /// Couldn't download it right now (offline); try again later.
    Unavailable,
}

/// Why an image couldn't become a thumbnail or a reader picture.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    #[error("couldn't decode the image")]
    Decode(#[from] image::ImageError),
    /// Smaller than is worth showing: icons, spacers, tracking pixels.
    #[error("the image is too small ({width}×{height})")]
    TooSmall { width: u32, height: u32 },
}

/// Loads a thumbnail from the disk cache, or downloads and caches it.
pub fn load(agent: &ureq::Agent, dir: &Path, url: &str) -> Thumb {
    let key = format!("{:016x}", text::fnv1a(url));
    let jpg = dir.join(format!("{key}.jpg"));
    let failed = dir.join(format!("{key}.none"));

    if let Ok(img) = image::open(&jpg) {
        return Thumb::Ready(img.to_rgb8());
    }
    if failed.exists() {
        return Thumb::Broken;
    }
    match fetch_and_shrink(agent, url) {
        Ok(rgb) => {
            let _ = rgb.save_with_format(&jpg, image::ImageFormat::Jpeg);
            Thumb::Ready(rgb)
        }
        // Offline: keep the placeholder and retry on a later reload.
        Err(Error::Fetch(fetch::Error::Unreachable { .. })) => Thumb::Unavailable,
        Err(_) => {
            let _ = std::fs::write(&failed, b"");
            Thumb::Broken
        }
    }
}

fn fetch_and_shrink(agent: &ureq::Agent, url: &str) -> Result<image::RgbImage, Error> {
    let bytes = fetch::download_image(agent, url)?;
    shrink(&bytes)
}

fn shrink(bytes: &[u8]) -> Result<image::RgbImage, Error> {
    let img = image::load_from_memory(bytes)?;
    if img.width() < 48 || img.height() < 48 {
        return Err(Error::TooSmall {
            width: img.width(),
            height: img.height(),
        });
    }
    Ok(img.resize_to_fill(W, H, FilterType::Triangle).to_rgb8())
}

/// Downloads an image for the reader view, shrunk to fit the reading column.
/// Not cached on disk: reader images are only needed while an article is open.
pub fn load_picture(agent: &ureq::Agent, url: &str) -> Result<image::RgbaImage, Error> {
    decode_picture(&fetch::download_image(agent, url)?)
}

fn decode_picture(bytes: &[u8]) -> Result<image::RgbaImage, Error> {
    let img = image::load_from_memory(bytes)?;
    // Icons, spacers and tracking pixels aren't worth a block of their own.
    if img.width() < 64 || img.height() < 32 {
        return Err(Error::TooSmall {
            width: img.width(),
            height: img.height(),
        });
    }
    let (w, h) = fit(img.width(), img.height(), PICTURE_MAX_W, PICTURE_MAX_H);
    let img = if (w, h) == (img.width(), img.height()) {
        img
    } else {
        img.resize_exact(w, h, FilterType::Triangle)
    };
    Ok(img.to_rgba8())
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
    fn unreachable_picture_is_a_fetch_error() {
        assert!(matches!(
            load_picture(&fetch::agent(), "http://127.0.0.1:9/a.png"),
            Err(Error::Fetch(fetch::Error::Unreachable { .. }))
        ));
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn tiny_images_are_too_small_and_garbage_does_not_decode() {
        assert!(matches!(
            shrink(&png(1, 1)),
            Err(Error::TooSmall {
                width: 1,
                height: 1
            })
        ));
        assert!(matches!(
            decode_picture(&png(1, 1)),
            Err(Error::TooSmall {
                width: 1,
                height: 1
            })
        ));
        assert!(matches!(shrink(b"not an image"), Err(Error::Decode(_))));
    }
}
