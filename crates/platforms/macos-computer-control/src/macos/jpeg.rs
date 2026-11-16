//! Native computer-use JPEG encoding. The pinned 2.1.325 native continuations
//! use NSImage(CGImage, actual pixel size) → TIFFRepresentation →
//! NSBitmapImageRep(data:) → JPEG with NSImageCompressionFactor = 0.75.
//! Capture/filtering and resize occur before this PNG boundary; matching this
//! encoder does not establish identical ScreenCaptureKit pixels or resizing.
use super::ComputerError;
use core_foundation::{
    base::{CFType, CFTypeRef, TCFType},
    data::{CFData, CFDataRef},
    dictionary::CFDictionaryRef,
};
use objc2::{rc::autoreleasepool, runtime::AnyObject, AnyThread};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSImage,
    NSImageCompressionFactor,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSDictionary, NSNumber, NSSize};

const MAX_PIXELS: u64 = 33_554_432;
const MAX_PNG_BYTES: usize = 134_217_728;
#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithData(data: CFDataRef, options: CFDictionaryRef) -> CFTypeRef;
    fn CGImageSourceCreateImageAtIndex(
        source: CFTypeRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CFTypeRef;
}

fn png_dimensions(png: &[u8]) -> Result<(u32, u32), ComputerError> {
    if png.len() < 33
        || png.len() > MAX_PNG_BYTES
        || &png[..8] != b"\x89PNG\r\n\x1a\n"
        || png[8..12] != 13u32.to_be_bytes()
        || &png[12..16] != b"IHDR"
    {
        return Err(ComputerError::Other(
            "native computer JPEG requires a bounded PNG image".into(),
        ));
    }
    let width = u32::from_be_bytes(png[16..20].try_into().expect("PNG header length checked"));
    let height = u32::from_be_bytes(png[20..24].try_into().expect("PNG header length checked"));
    if width == 0
        || height == 0
        || width > 32768
        || height > 32768
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(ComputerError::Other(
            "native computer JPEG dimensions exceed budget".into(),
        ));
    }
    Ok((width, height))
}
fn failure(stage: &str) -> ComputerError {
    ComputerError::Other(format!("native computer JPEG encoding failed at {stage}"))
}

pub(super) async fn encode(png: &[u8]) -> Result<Vec<u8>, ComputerError> {
    let dimensions = png_dimensions(png)?;
    let png = png.to_vec();
    tokio::task::spawn_blocking(move || encode_sync(&png, dimensions, 0.75, false))
        .await
        .map_err(|error| {
            ComputerError::Other(format!("native computer JPEG worker failed: {error}"))
        })?
}
/// Internal background-capture path; public display encoding remains fixed at
/// the reference's 0.75. Caller quality is already a normalized factor.
pub(super) fn encode_png_with_quality(png: &[u8], quality: f64) -> Result<Vec<u8>, ComputerError> {
    if !quality.is_finite() || !(0.0..=1.0).contains(&quality) {
        return Err(ComputerError::Other(
            "native computer JPEG quality must be between 0 and 1".into(),
        ));
    }
    let dimensions = png_dimensions(png)?;
    encode_sync(png, dimensions, quality, true)
}
fn encode_sync(
    png: &[u8],
    (width, height): (u32, u32),
    quality: f64,
    window: bool,
) -> Result<Vec<u8>, ComputerError> {
    autoreleasepool(|_| {
        let data = CFData::from_buffer(png);
        let source =
            unsafe { CGImageSourceCreateWithData(data.as_concrete_TypeRef(), std::ptr::null()) };
        if source.is_null() {
            return Err(failure("PNG source"));
        }
        let source = unsafe { CFType::wrap_under_create_rule(source) };
        let image = unsafe {
            CGImageSourceCreateImageAtIndex(source.as_concrete_TypeRef(), 0, std::ptr::null())
        };
        if image.is_null() {
            return Err(failure("PNG image"));
        }
        let image_owner = unsafe { CFType::wrap_under_create_rule(image) };
        // ImageIO returns a retained CGImageRef. Keep its CF owner alive for the
        // borrowed objc2 CGImage reference through all AppKit calls below.
        let cg_image = unsafe { &*image_owner.as_concrete_TypeRef().cast::<CGImage>() };
        if CGImage::width(Some(cg_image)) != width as usize
            || CGImage::height(Some(cg_image)) != height as usize
        {
            return Err(failure("PNG dimensions"));
        }
        if window {
            encode_window_cgimage(cg_image, quality)
        } else {
            encode_cgimage_with_quality(cg_image, quality)
        }
    })
}

/// Direct CGImage path retains capture color space/alpha metadata through the
/// exact AppKit chain, without an intervening RGBA/PNG conversion.
pub(super) fn encode_cgimage(cg_image: &CGImage) -> Result<Vec<u8>, ComputerError> {
    encode_cgimage_with_quality(cg_image, 0.75)
}
fn encode_cgimage_with_quality(cg_image: &CGImage, quality: f64) -> Result<Vec<u8>, ComputerError> {
    autoreleasepool(|_| {
        let width = CGImage::width(Some(cg_image));
        let height = CGImage::height(Some(cg_image));
        if width == 0
            || height == 0
            || width > 32768
            || height > 32768
            || width as u64 * height as u64 > MAX_PIXELS
        {
            return Err(failure("CGImage dimensions"));
        }
        let image = NSImage::initWithCGImage_size(
            NSImage::alloc(),
            cg_image,
            NSSize::new(width as f64, height as f64),
        );
        let tiff = image
            .TIFFRepresentation()
            .ok_or_else(|| failure("TIFFRepresentation"))?;
        let bitmap = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &tiff)
            .ok_or_else(|| failure("NSBitmapImageRep"))?;
        let factor = NSNumber::numberWithDouble(quality);
        let number: &AnyObject = &factor;
        let properties = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::from_slices(
            &[unsafe { NSImageCompressionFactor }],
            &[number],
        );
        // The sole property has exactly the NSNumber type AppKit expects.
        let jpeg = unsafe {
            bitmap.representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties)
        }
        .ok_or_else(|| failure("JPEG representation"))?;
        Ok(jpeg.to_vec())
    })
}

/// The reference CaptureWindow continuation uses NSBitmapImageRep(CGImage)
/// directly; display captures independently use the NSImage/TIFF chain.
pub(super) fn encode_window_cgimage(
    image: &CGImage,
    quality: f64,
) -> Result<Vec<u8>, ComputerError> {
    if !quality.is_finite() || !(0.0..=1.0).contains(&quality) {
        return Err(failure("window quality"));
    }
    autoreleasepool(|_| {
        let bitmap = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), image);
        let factor = NSNumber::numberWithDouble(quality);
        let number: &AnyObject = &factor;
        let properties = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::from_slices(
            &[unsafe { NSImageCompressionFactor }],
            &[number],
        );
        let jpeg = unsafe {
            bitmap.representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties)
        }
        .ok_or_else(|| failure("window JPEG representation"))?;
        Ok(jpeg.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn controlled_png(width: u32, height: u32) -> Vec<u8> {
        let pixels = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([
                (x * 17 + y * 3) as u8,
                (x * 5 + y * 23) as u8,
                (x * 29 + y * 7) as u8,
                255,
            ])
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }
    #[test]
    fn malformed_or_oversized_png_is_rejected_before_native_decoding() {
        for bytes in [
            vec![],
            b"not a PNG image".to_vec(),
            controlled_png(1, 1)[..20].to_vec(),
        ] {
            assert!(png_dimensions(&bytes).is_err());
        }
        let mut png = controlled_png(1, 1);
        png[16..20].copy_from_slice(&0u32.to_be_bytes());
        assert!(png_dimensions(&png).is_err());
        png[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(png_dimensions(&png).is_err());
    }
    #[tokio::test]
    async fn actual_native_encoder_preserves_controlled_image_dimensions() {
        let png = controlled_png(17, 13);
        let jpeg = encode(&png).await.unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]));
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (17, 13));
    }
    #[test]
    fn explicit_quality_bounds_are_validated_without_clamping_zero() {
        let png = controlled_png(1, 1);
        for quality in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert!(encode_png_with_quality(&png, quality).is_err());
        }
        assert!(encode_png_with_quality(&png, 0.0)
            .unwrap()
            .starts_with(&[0xff, 0xd8]));
        assert!(encode_png_with_quality(&png, 1.0)
            .unwrap()
            .starts_with(&[0xff, 0xd8]));
    }
    /// Run against independently generated same-host Swift/AppKit references.
    /// Fixtures are OS-codec-specific, so they are supplied explicitly rather
    /// than checking in one OS's JPEG output as a universal expected byte stream.
    #[tokio::test]
    #[ignore = "requires LINGXI_COMPUTER_JPEG_REFERENCE_DIR from the independent Swift encoder"]
    async fn native_seam_matches_independent_swift_appkit_bytes() {
        use lingxi_core::host::computer_control::ComputerControl;
        let root = std::path::PathBuf::from(
            std::env::var_os("LINGXI_COMPUTER_JPEG_REFERENCE_DIR")
                .expect("supply independent reference directory"),
        );
        let backend = super::super::MacosComputerControl::new();
        let mut cases = 0;
        for path in std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "png"))
        {
            let png = std::fs::read(&path).unwrap();
            let expected = std::fs::read(path.with_extension("jpg")).unwrap();
            let actual = backend
                .encode_computer_jpeg(&png)
                .await
                .unwrap()
                .expect("native macOS encoder");
            assert_eq!(
                actual,
                expected,
                "AppKit JPEG byte mismatch for {}",
                path.display()
            );
            let direct = autoreleasepool(|_| {
                let data = CFData::from_buffer(&png);
                let source = unsafe {
                    CGImageSourceCreateWithData(data.as_concrete_TypeRef(), std::ptr::null())
                };
                assert!(!source.is_null());
                let source = unsafe { CFType::wrap_under_create_rule(source) };
                let image = unsafe {
                    CGImageSourceCreateImageAtIndex(
                        source.as_concrete_TypeRef(),
                        0,
                        std::ptr::null(),
                    )
                };
                assert!(!image.is_null());
                let owner = unsafe { CFType::wrap_under_create_rule(image) };
                let image = unsafe { &*owner.as_concrete_TypeRef().cast::<CGImage>() };
                (
                    encode_cgimage(image).unwrap(),
                    encode_window_cgimage(image, 0.75).unwrap(),
                )
            });
            assert_eq!(
                direct.0,
                expected,
                "direct native CGImage/profile path mismatch for {}",
                path.display()
            );
            let window_expected = std::fs::read(path.with_extension("bitmap.jpg"))
                .expect("supply independent window bitmap reference");
            assert_eq!(
                direct.1,
                window_expected,
                "native window bitmap JPEG mismatch for {}",
                path.display()
            );
            cases += 1;
        }
        assert!(
            cases >= 4,
            "require at least four independently encoded controlled images"
        );
    }
}
