//! Values that mirror Chromium's lens overlay, so payloads match what the
//! server is tuned for.
//!
//! The image numbers come from `components/lens/lens_features.cc` and
//! `components/lens/lens_bitmap_processing.cc`, by way of chrome-lens-py's
//! `constants.py`, which is kept in step with them.

pub const LENS_CRUPLOAD_ENDPOINT: &str = "https://lensfrontend-pa.googleapis.com/v1/crupload";

pub const DEFAULT_API_KEY: &str = "AIzaSyDr2UxVnv_U85AbhhY8XSHSIavUW0DC-sY";

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Downscale only when the image is *both* larger than this area and over one
/// of the dimension limits below - `lens::ShouldDownscaleSize`.
///
/// An unconditional shrink to some thumbnail size costs OCR detail on images
/// Chromium keeps at full resolution: a 1600x900 screenshot, for instance, is
/// under the area limit and is sent untouched.
pub const DEFAULT_IMAGE_MAX_AREA: u64 = 1_500_000;
pub const DEFAULT_IMAGE_MAX_WIDTH: u32 = 1600;
pub const DEFAULT_IMAGE_MAX_HEIGHT: u32 = 1600;

/// `kLensOverlayImageCompressionQuality`.
pub const DEFAULT_IMAGE_JPEG_QUALITY: u8 = 40;

/// Empty, not "en": Chromium sends no OCR language unless one was chosen, and
/// the field has implicit presence, so an empty string is simply not on the
/// wire. Sending "en" is a different request from sending nothing.
pub const DEFAULT_OCR_LANG: &str = "";

pub const DEFAULT_CLIENT_REGION: &str = "US";
pub const DEFAULT_CLIENT_TIME_ZONE: &str = "America/New_York";
