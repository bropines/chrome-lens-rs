//! Downscale and encode an image the way Chromium's lens overlay does.
//!
//! The previous version of this file resized anything over 1500 px on a side
//! and always re-encoded PNG. Both were wrong in ways that cost something real:
//! the resize shrank images Chromium keeps at full resolution, losing OCR
//! detail for nothing, and PNG of a screenshot is several times the size of the
//! equivalent JPEG, so every request paid for it on the upload and got nothing
//! back.

use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader, RgbImage, RgbaImage};
use std::io::Cursor;
use std::path::Path;

use crate::constants::{
    DEFAULT_IMAGE_JPEG_QUALITY, DEFAULT_IMAGE_MAX_AREA, DEFAULT_IMAGE_MAX_HEIGHT,
    DEFAULT_IMAGE_MAX_WIDTH,
};

/// What to upload for an image that really uses transparency.
///
/// Chromium's own answer is WebP at quality 40 with the alpha intact. Ours is
/// [`AlphaMode::Contrast`], and that is a measured decision rather than a
/// shortcut: the server composites a transparent upload onto black, so black
/// text on a transparent PNG reads back empty if the alpha is preserved, and
/// white text vanishes if it is flattened onto white. Picking the background by
/// the content's luminance reads both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AlphaMode {
    /// Flatten onto white or black, whichever the content stands out against.
    #[default]
    Contrast,
    /// Flatten onto white, the way a browser would have shown it.
    Flatten,
}

#[derive(Clone, Debug)]
pub struct ProcessedImage {
    /// JPEG bytes, ready for `ImagePayload.image_bytes`.
    pub bytes: Vec<u8>,
    /// Dimensions *after* any downscale - what `ImageMetadata` must carry.
    pub width: i32,
    pub height: i32,
}

/// Chromium's rule: shrink only when the image is both large and oversized.
///
/// Mirrors `lens::ShouldDownscaleSize`.
#[must_use]
pub fn should_downscale(width: u32, height: u32) -> bool {
    u64::from(width) * u64::from(height) > DEFAULT_IMAGE_MAX_AREA
        && (width > DEFAULT_IMAGE_MAX_WIDTH || height > DEFAULT_IMAGE_MAX_HEIGHT)
}

/// The size an oversized image is scaled to, preserving aspect ratio.
#[must_use]
pub fn preferred_size(width: u32, height: u32) -> (u32, u32) {
    let scale = f64::min(
        f64::from(DEFAULT_IMAGE_MAX_WIDTH) / f64::from(width),
        f64::from(DEFAULT_IMAGE_MAX_HEIGHT) / f64::from(height),
    );
    (
        (f64::from(width) * scale).trunc().max(1.0) as u32,
        (f64::from(height) * scale).trunc().max(1.0) as u32,
    )
}

/// True if any pixel is not fully opaque.
///
/// Mirrors Chromium's `SkBitmap::isOpaque()`: an RGBA screenshot whose alpha
/// channel is solid 255 is, for upload purposes, an opaque image, and must not
/// be put through the flattening path.
#[must_use]
pub fn has_meaningful_alpha(image: &DynamicImage) -> bool {
    if !image.color().has_alpha() {
        return false;
    }
    image.to_rgba8().pixels().any(|p| p.0[3] < 255)
}

/// ITU-R 601-2 luma, the transform PIL's `convert("L")` applies.
///
/// `image`'s own `to_luma8` uses different coefficients, and the threshold
/// below is calibrated against this one.
fn luma601(r: u8, g: u8, b: u8) -> f64 {
    (f64::from(r) * 299.0 + f64::from(g) * 587.0 + f64::from(b) * 114.0) / 1000.0
}

/// White behind dark content, black behind light content.
fn contrasting_background(rgba: &RgbaImage) -> [u8; 3] {
    let mut total = 0.0_f64;
    let mut counted = 0_u64;
    for pixel in rgba.pixels() {
        let [r, g, b, a] = pixel.0;
        // The same >128 mask PIL's version uses: mostly-transparent pixels say
        // nothing about what colour the content is.
        if a > 128 {
            total += luma601(r, g, b);
            counted += 1;
        }
    }
    if counted == 0 {
        return [255, 255, 255];
    }
    if total / (counted as f64) < 128.0 {
        [255, 255, 255]
    } else {
        [0, 0, 0]
    }
}

fn flatten(rgba: &RgbaImage, background: [u8; 3]) -> RgbImage {
    let mut out = RgbImage::from_pixel(rgba.width(), rgba.height(), image::Rgb(background));
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let [r, g, b, a] = pixel.0;
        let alpha = f64::from(a) / 255.0;
        let under = out.get_pixel(x, y).0;
        out.put_pixel(
            x,
            y,
            image::Rgb([
                (f64::from(r) * alpha + f64::from(under[0]) * (1.0 - alpha)).round() as u8,
                (f64::from(g) * alpha + f64::from(under[1]) * (1.0 - alpha)).round() as u8,
                (f64::from(b) * alpha + f64::from(under[2]) * (1.0 - alpha)).round() as u8,
            ]),
        );
    }
    out
}

/// Downscale if Chromium would, then encode JPEG at quality 40.
pub fn encode_for_api(image: &DynamicImage, alpha_mode: AlphaMode) -> Result<ProcessedImage> {
    let (width, height) = image.dimensions();
    let scaled = if should_downscale(width, height) {
        let (w, h) = preferred_size(width, height);
        image.resize(w, h, image::imageops::FilterType::Lanczos3)
    } else {
        image.clone()
    };

    let rgb = if has_meaningful_alpha(&scaled) {
        let rgba = scaled.to_rgba8();
        let background = match alpha_mode {
            AlphaMode::Contrast => contrasting_background(&rgba),
            AlphaMode::Flatten => [255, 255, 255],
        };
        flatten(&rgba, background)
    } else {
        scaled.to_rgb8()
    };

    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(
        &mut Cursor::new(&mut bytes),
        DEFAULT_IMAGE_JPEG_QUALITY,
    )
    .encode_image(&rgb)
    .context("encoding the upload as JPEG")?;

    Ok(ProcessedImage {
        bytes,
        width: rgb.width() as i32,
        height: rgb.height() as i32,
    })
}

pub fn process_image_from_path<P: AsRef<Path>>(
    path: P,
    alpha_mode: AlphaMode,
) -> Result<ProcessedImage> {
    let image = ImageReader::open(path.as_ref())
        .with_context(|| format!("opening {}", path.as_ref().display()))?
        .with_guessed_format()
        .context("guessing the image format")?
        .decode()
        .context("decoding the image")?;
    encode_for_api(&image, alpha_mode)
}

pub fn process_image_from_bytes(data: &[u8], alpha_mode: AlphaMode) -> Result<ProcessedImage> {
    let image = image::load_from_memory(data).context("decoding the image from memory")?;
    encode_for_api(&image, alpha_mode)
}

/// Decode without re-encoding, for callers that want to inspect first.
pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    image::load_from_memory(data).context("decoding the image from memory")
}

/// The format `image` guesses for these bytes, if it recognises them.
#[must_use]
pub fn guess_format(data: &[u8]) -> Option<ImageFormat> {
    image::guess_format(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screenshot_under_the_area_limit_is_not_touched() {
        // 1600x900 is 1.44 MP: under the area limit, so Chromium sends it whole
        // even though one side is at the dimension limit.
        assert!(!should_downscale(1600, 900));
    }

    #[test]
    fn a_large_image_within_both_dimensions_is_not_touched() {
        // 1500x1200 is 1.8 MP - over the area limit - but neither side exceeds
        // 1600, so the AND in the rule keeps it at full resolution.
        assert!(!should_downscale(1500, 1200));
    }

    #[test]
    fn a_big_photo_is_downscaled_on_its_longest_side() {
        assert!(should_downscale(4000, 3000));
        assert_eq!(preferred_size(4000, 3000), (1600, 1200));
    }

    #[test]
    fn a_very_wide_panorama_keeps_its_aspect_ratio() {
        assert!(should_downscale(6000, 500));
        let (w, h) = preferred_size(6000, 500);
        assert_eq!(w, 1600);
        assert_eq!(h, 133); // 500 * 1600/6000 = 133.33, truncated
    }

    #[test]
    fn a_solid_alpha_channel_is_not_meaningful_alpha() {
        let opaque =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255])));
        assert!(!has_meaningful_alpha(&opaque));
    }

    #[test]
    fn one_transparent_pixel_is_meaningful_alpha() {
        let mut rgba = RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
        rgba.put_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
        assert!(has_meaningful_alpha(&DynamicImage::ImageRgba8(rgba)));
    }

    #[test]
    fn dark_content_gets_a_white_background_and_light_content_a_black_one() {
        // Black glyphs on transparency: flattening onto black would erase them.
        let dark = RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        assert_eq!(contrasting_background(&dark), [255, 255, 255]);

        let light = RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 255]));
        assert_eq!(contrasting_background(&light), [0, 0, 0]);
    }

    #[test]
    fn a_fully_transparent_image_falls_back_to_white() {
        let nothing = RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 0]));
        assert_eq!(contrasting_background(&nothing), [255, 255, 255]);
    }

    #[test]
    fn the_upload_is_jpeg_and_reports_the_scaled_size() {
        let big = DynamicImage::ImageRgb8(RgbImage::from_pixel(
            2000,
            2000,
            image::Rgb([128, 128, 128]),
        ));
        let out = encode_for_api(&big, AlphaMode::Contrast).expect("encode");
        assert_eq!((out.width, out.height), (1600, 1600));
        assert_eq!(guess_format(&out.bytes), Some(ImageFormat::Jpeg));
    }
}
