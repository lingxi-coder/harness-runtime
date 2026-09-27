//! Shared image resize/downsample helpers used by multiple tool crates.
//!
//! This is the common claude-code-style image budget processor that used to
//! live only behind FileRead. Keeping it in `tool-api` lets `tool-file`,
//! `tool-shell`, and `tool-mcp` reuse the same implementation without adding
//! forbidden tool-to-tool dependency edges.

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView, ImageFormat};

/// claude-code `IMAGE_TARGET_RAW_SIZE` (apiLimits.ts): API_IMAGE_MAX_BASE64_SIZE*3/4 = 3.75 MB.
const IMAGE_TARGET_RAW_SIZE: usize = 3_932_160;
/// claude-code `IMAGE_MAX_WIDTH`/`IMAGE_MAX_HEIGHT`.
pub const IMAGE_MAX_DIM: u32 = 2000;
/// claude-code secondary-shrink dimension @ quality 20 (last resort).
const SECONDARY_SHRINK_DIMENSION: u32 = 1000;
/// claude-code JPEG quality ladder (imageResizer.ts).
const JPEG_QUALITY_LADDER: [u8; 4] = [80, 60, 40, 20];

/// A processed image ready for an `ImageSource::Base64`.
pub struct ProcessedImage {
    /// Base64-encoded processed image bytes for the Anthropic source.
    pub base64: String,
    /// `image/png` | `image/jpeg` | `image/gif` | `image/webp`.
    pub media_type: String,
    /// `(orig_w, orig_h, disp_w, disp_h)` — present only when the image was resized.
    pub resized: Option<(u32, u32, u32, u32)>,
}

fn format_to_media_type(fmt: Option<ImageFormat>) -> String {
    match fmt {
        Some(ImageFormat::Jpeg) => "image/jpeg",
        Some(ImageFormat::Gif) => "image/gif",
        Some(ImageFormat::WebP) => "image/webp",
        _ => "image/png",
    }
    .to_string()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode_image(img)
        .ok()?;
    Some(buf)
}

/// Decode, then resize/re-encode to fit the size + dimension budget.
///
/// # Errors
/// Empty input, or the bytes can't be decoded as a supported image.
pub fn process_image(bytes: Vec<u8>) -> Result<ProcessedImage, String> {
    process_image_with_raw_budget(bytes, IMAGE_TARGET_RAW_SIZE)
}

/// Decode and resize/re-encode an image so its standard-base64 representation
/// fits `max_base64_chars`, while retaining the normal 2000px dimension cap.
pub fn process_image_with_base64_budget(
    bytes: Vec<u8>,
    max_base64_chars: usize,
) -> Result<ProcessedImage, String> {
    let target_raw_size = (max_base64_chars / 4).saturating_mul(3);
    if target_raw_size == 0 {
        return Err("image base64 budget is too small".to_string());
    }
    process_image_with_raw_budget(bytes, target_raw_size)
}

fn process_image_with_raw_budget(
    bytes: Vec<u8>,
    target_raw_size: usize,
) -> Result<ProcessedImage, String> {
    if bytes.is_empty() {
        return Err("Image file is empty (0 bytes)".to_string());
    }
    let fmt = image::guess_format(&bytes).ok();
    let media_type = format_to_media_type(fmt);
    let img =
        image::load_from_memory(&bytes).map_err(|e| format!("failed to decode image: {e}"))?;
    let (w, h) = img.dimensions();

    if bytes.len() <= target_raw_size && w <= IMAGE_MAX_DIM && h <= IMAGE_MAX_DIM {
        return Ok(ProcessedImage {
            base64: b64(&bytes),
            media_type,
            resized: None,
        });
    }

    let working = if w > IMAGE_MAX_DIM || h > IMAGE_MAX_DIM {
        img.resize(
            IMAGE_MAX_DIM,
            IMAGE_MAX_DIM,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let (dw, dh) = working.dimensions();

    for &q in &JPEG_QUALITY_LADDER {
        if let Some(enc) = encode_jpeg(&working, q) {
            if enc.len() <= target_raw_size {
                return Ok(ProcessedImage {
                    base64: b64(&enc),
                    media_type: "image/jpeg".to_string(),
                    resized: (dw != w || dh != h).then_some((w, h, dw, dh)),
                });
            }
        }
    }

    let mut shrink_max_dimension = working
        .width()
        .max(working.height())
        .min(SECONDARY_SHRINK_DIMENSION)
        .max(1);
    loop {
        let small = working.resize(
            shrink_max_dimension,
            shrink_max_dimension,
            image::imageops::FilterType::Triangle,
        );
        let (sdw, sdh) = small.dimensions();
        let enc = encode_jpeg(&small, 20).ok_or_else(|| "jpeg encode failed".to_string())?;
        if enc.len() <= target_raw_size {
            return Ok(ProcessedImage {
                base64: b64(&enc),
                media_type: "image/jpeg".to_string(),
                resized: (sdw != w || sdh != h).then_some((w, h, sdw, sdh)),
            });
        }
        if shrink_max_dimension == 1 {
            return Err("image cannot fit the requested base64 budget".to_string());
        }
        shrink_max_dimension = (shrink_max_dimension.saturating_mul(3) / 4).max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::new(w, h));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn small_image_passes_through_unchanged() {
        let bytes = png_bytes(10, 10);
        let p = process_image(bytes.clone()).unwrap();
        assert_eq!(p.media_type, "image/png");
        assert!(p.resized.is_none());
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&p.base64)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn oversized_dimensions_are_resized_to_cap() {
        let bytes = png_bytes(3000, 1500);
        let p = process_image(bytes).unwrap();
        let (ow, oh, dw, dh) = p.resized.expect("resized");
        assert_eq!((ow, oh), (3000, 1500));
        assert!(dw <= IMAGE_MAX_DIM && dh <= IMAGE_MAX_DIM);
        assert_eq!(dw, 2000);
        assert_eq!(p.media_type, "image/jpeg");
    }

    #[test]
    fn base64_budget_is_enforced() {
        let p = process_image_with_base64_budget(png_bytes(3000, 1), 4_000).unwrap();
        assert!(p.base64.len() <= 4_000);
    }
}
