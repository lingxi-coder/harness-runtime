//! Shared image resize/downsample helpers used by multiple tool crates.
//!
//! This is the common claude-code-style image budget processor that used to
//! live only behind FileRead. Keeping it in `tool-api` lets `tool-file`,
//! `tool-shell`, and `tool-mcp` reuse the same implementation without adding
//! forbidden tool-to-tool dependency edges.

use base64::Engine;
use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat};

#[path = "image_budget_jpeg.rs"]
mod jpeg;

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
    /// Width and height of the delivered pixels, after orientation and resizing.
    pub dimensions: (u32, u32),
    /// `(orig_w, orig_h, disp_w, disp_h)` in upright coordinates, only when resized.
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

/// Decode, normalize JPEG orientation, then resize/re-encode to the image budget.
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
    let (img, (w, h), orientation) = if fmt == Some(ImageFormat::Jpeg) {
        let metadata = jpeg::metadata(&bytes);
        let mut decoder = JpegDecoder::new(std::io::Cursor::new(&bytes))
            .map_err(|e| format!("failed to decode image: {e}"))?;
        let (raw_width, raw_height) = decoder.dimensions();
        let original_dimensions = if (5..=8).contains(&metadata.orientation) {
            (raw_height, raw_width)
        } else {
            (raw_width, raw_height)
        };
        // Lossless JPEG has no DCT: jpeg-decoder's scale updates its advertised
        // dimensions but its lossless path still allocates full-size pixels.
        if metadata.can_scale && (raw_width > IMAGE_MAX_DIM || raw_height > IMAGE_MAX_DIM) {
            // IDCT sampling happens before pixel allocation. Loading the full original
            // first rejects ordinary high-resolution camera JPEGs at the decode limit.
            decoder
                .scale(IMAGE_MAX_DIM as u16, IMAGE_MAX_DIM as u16)
                .map_err(|e| format!("failed to decode image: {e}"))?;
        }
        // DynamicImage::from_decoder does not enforce the reader's allocation limit.
        // Progressive JPEGs also retain original-resolution coefficients even when
        // IDCT output is sampled. Four pixel buffers conservatively cover sampled
        // component planes, RGB/CMYK conversion, and the outer DynamicImage copy.
        // Other JPEG coding modes retain the original full-pixel allocation guard.
        let mut limits = image::io::Limits::default();
        let pixel_budget =
            decoder
                .total_bytes()
                .saturating_mul(if metadata.can_scale { 4 } else { 1 });
        limits
            .reserve(metadata.coefficient_bytes)
            .and_then(|()| limits.reserve(pixel_budget))
            .and_then(|()| decoder.set_limits(limits))
            .map_err(|e| format!("failed to decode image: {e}"))?;
        let img = DynamicImage::from_decoder(decoder)
            .map_err(|e| format!("failed to decode image: {e}"))?;
        (
            jpeg::orient(img, metadata.orientation),
            original_dimensions,
            metadata.orientation,
        )
    } else {
        let img =
            image::load_from_memory(&bytes).map_err(|e| format!("failed to decode image: {e}"))?;
        let dimensions = img.dimensions();
        (img, dimensions, 1)
    };

    if orientation == 1
        && bytes.len() <= target_raw_size
        && w <= IMAGE_MAX_DIM
        && h <= IMAGE_MAX_DIM
    {
        return Ok(ProcessedImage {
            base64: b64(&bytes),
            media_type,
            dimensions: (w, h),
            resized: None,
        });
    }

    let working = if img.width() > IMAGE_MAX_DIM || img.height() > IMAGE_MAX_DIM {
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
                    dimensions: (dw, dh),
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
                dimensions: (sdw, sdh),
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
    use std::io::Read;

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

    fn rotated_jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::new(width, height));
        let encoded = encode_jpeg(&img, 80).unwrap();
        // APP1 containing a little-endian TIFF IFD0 with orientation=6 (90 CW).
        let exif = [
            0xff, 0xe1, 0, 34, b'E', b'x', b'i', b'f', 0, 0, b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0,
            0x12, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
        ];
        [encoded[..2].as_ref(), exif.as_ref(), encoded[2..].as_ref()].concat()
    }

    #[test]
    fn orientation_alone_changes_delivered_dimensions_without_claiming_a_resize() {
        let processed = process_image(rotated_jpeg(210, 80)).unwrap();
        assert_eq!(processed.dimensions, (80, 210));
        assert_eq!(processed.resized, None);
    }

    #[test]
    fn resized_orientation_uses_upright_original_coordinates() {
        let processed = process_image(rotated_jpeg(2100, 800)).unwrap();
        assert_eq!(processed.dimensions, (762, 2000));
        assert_eq!(processed.resized, Some((800, 2100, 762, 2000)));
    }

    fn large_jpeg() -> Vec<u8> {
        let compressed = include_bytes!("image_budget_fixtures/large-baseline.jpg.gz");
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    #[test]
    fn high_resolution_jpeg_is_sampled_before_pixel_allocation() {
        let bytes = large_jpeg();
        // This is a valid 192MP camera-style baseline JPEG. Its full 576MB RGB
        // decode exceeds the image reader's default 512MiB allocation budget.
        let decoder = JpegDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(decoder.dimensions(), (16000, 12000));
        assert!(decoder.total_bytes() > image::io::Limits::default().max_alloc.unwrap());

        let processed = process_image(bytes).unwrap();
        assert_eq!(processed.resized, Some((16000, 12000, 2000, 1500)));
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(processed.base64)
            .unwrap();
        assert!(encoded.len() <= IMAGE_TARGET_RAW_SIZE);
        let decoded = image::load_from_memory(&encoded).unwrap();
        assert_eq!(decoded.dimensions(), (2000, 1500));
        let center = decoded.to_rgb8().get_pixel(1000, 750).0;
        assert!(center
            .into_iter()
            .all(|channel| (126..=130).contains(&channel)));
    }

    #[test]
    fn high_resolution_progressive_coefficients_are_checked_before_decode() {
        let mut bytes = large_jpeg();
        let marker = bytes
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .unwrap();
        bytes[marker + 1] = 0xc2;
        // The modified frame header advertises a progressive image; its
        // coefficient memory must be rejected before reading any scan data.
        assert!(jpeg::metadata(&bytes).coefficient_bytes > 512 * 1024 * 1024);
        let error = process_image(bytes)
            .err()
            .expect("coefficient budget must be enforced");
        assert!(error.contains("Memory limit exceeded"), "{error}");
    }

    #[test]
    fn lossless_jpeg_cannot_bypass_full_size_allocation_budget() {
        let mut bytes = large_jpeg();
        let marker = bytes
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .unwrap();
        bytes[marker + 1] = 0xc3;
        for component in 0..3 {
            bytes[marker + 12 + component * 3] = 0; // Lossless requires quantizer 0.
        }
        assert!(!jpeg::metadata(&bytes).can_scale);
        let error = process_image(bytes)
            .err()
            .expect("unsampled image budget must be enforced");
        assert!(error.contains("Memory limit exceeded"), "{error}");
    }

    #[test]
    fn extreme_sampled_dimensions_still_respect_decoder_working_memory_budget() {
        let mut bytes = large_jpeg();
        let marker = bytes
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .unwrap();
        bytes[marker + 5..marker + 7].copy_from_slice(&64000u16.to_be_bytes());
        bytes[marker + 7..marker + 9].copy_from_slice(&64000u16.to_be_bytes());
        // Even 1/8 IDCT sampling yields 8000x8000. The outer RGB image alone
        // fits 512MiB, but concurrent decoder component/pixel buffers do not.
        let error = process_image(bytes)
            .err()
            .expect("working buffers must fit the budget");
        assert!(error.contains("Memory limit exceeded"), "{error}");
    }
}
