//! Load a pasted image file into a canonical [`lingxi_core::types::ImageSource::Base64`].
//!
//! The TUI records pasted image *paths*; the orchestrator reads + base64-encodes
//! them here, just before building the outgoing user message, so a read failure
//! surfaces as a turn error (and the TUI never blocks on disk I/O during paste).

use crate::error::OrchestratorError;
use lingxi_core::types::ImageSource;
use std::path::Path;

/// Detect the image MIME type from a file extension. `None` for unsupported
/// extensions.
///
/// This is only a *fallback hint*: the real media type comes from sniffing the
/// file's magic bytes ([`detect_media_type_from_bytes`]), mirroring TS where the
/// sent media type is `detectImageFormatFromBase64` (imagePaste.ts:409) and the
/// extension is only used as a `metadata.format ?? ext` fallback
/// (imageResizer.ts:186) with `jpg` normalized to `jpeg` (imageResizer.ts:188).
fn media_type_for(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        // `jpg` → `jpeg` normalization (imageResizer.ts:188).
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Detect the image MIME type from the file's leading *magic bytes*, the way the
/// model sees it — so a file whose extension lies about its real format (e.g. a
/// `.png` that actually holds JPEG bytes) is sent with the correct media type.
///
/// Mirrors TS `detectImageFormatFromBuffer` (imageResizer.ts:769-812): PNG
/// `89 50 4e 47`, JPEG `ff d8 ff`, GIF `47 49 46`, WEBP `RIFF`(`52 49 46 46`)…
/// `WEBP`(`57 45 42 50`) at offset 8. Returns `None` for anything unrecognized
/// (or fewer than 4 bytes); the caller then falls back to the extension hint and
/// finally to `image/png`, matching the TS default.
fn detect_media_type_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    // PNG signature: 89 50 4e 47
    if bytes[0] == 0x89 && bytes[1] == 0x50 && bytes[2] == 0x4e && bytes[3] == 0x47 {
        return Some("image/png");
    }
    // JPEG signature: ff d8 ff
    if bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    // GIF signature: 47 49 46 ("GIF", covers GIF87a / GIF89a)
    if bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 {
        return Some("image/gif");
    }
    // WEBP signature: "RIFF" (52 49 46 46) then "WEBP" (57 45 42 50) at offset 8.
    if bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes.len() >= 12
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
    {
        return Some("image/webp");
    }
    None
}

/// Standard base64 (RFC 4648) encoder with padding. No `data:` prefix.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Read an image file from disk and build a base64 [`ImageSource`].
///
/// The media type is determined from the file's actual *magic bytes*
/// ([`detect_media_type_from_bytes`], the authoritative source — TS
/// imagePaste.ts:409), falling back to the extension hint
/// ([`media_type_for`] — TS imageResizer.ts:186 `metadata.format ?? ext`) and
/// finally to `image/png` (the TS default). This means a `.png` file that
/// actually holds JPEG bytes is sent with `image/jpeg`, and an unrecognized file
/// is never rejected for "unsupported type" — it defaults to PNG like TS.
///
/// # Errors
/// Returns [`OrchestratorError::Internal`] only if the file cannot be read.
pub fn load_image_source(path: &Path) -> Result<ImageSource, OrchestratorError> {
    const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(OrchestratorError::Internal(format!(
                "image {} exceeds {MAX_IMAGE_BYTES} bytes",
                path.display()
            )));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| {
        OrchestratorError::Internal(format!("could not read image {}: {e}", path.display()))
    })?;
    // Magic bytes win; extension is only a fallback hint; default PNG.
    let media_type = detect_media_type_from_bytes(&bytes)
        .or_else(|| media_type_for(path))
        .unwrap_or("image/png");
    Ok(ImageSource::Base64 {
        media_type: media_type.to_string(),
        data: base64_encode(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn base64_encodes_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello world!"), "aGVsbG8gd29ybGQh");
    }

    #[test]
    fn media_type_detection() {
        // Extension hint map (jpg normalized to jpeg).
        assert_eq!(media_type_for(Path::new("a.PNG")), Some("image/png"));
        assert_eq!(media_type_for(Path::new("a.jpg")), Some("image/jpeg"));
        assert_eq!(media_type_for(Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(media_type_for(Path::new("a.webp")), Some("image/webp"));
        assert_eq!(media_type_for(Path::new("a.txt")), None);
        assert_eq!(media_type_for(Path::new("noext")), None);
    }

    // Magic-byte prefixes for each recognized format (TS imageResizer.ts:769-812).
    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG_MAGIC: &[u8] = b"\xff\xd8\xff\xe0";
    const GIF_MAGIC: &[u8] = b"GIF89a";
    // "RIFF" + 4-byte size + "WEBP".
    const WEBP_MAGIC: &[u8] = b"RIFF\x00\x00\x00\x00WEBP";

    #[test]
    fn sniffs_each_format_from_bytes() {
        assert_eq!(detect_media_type_from_bytes(PNG_MAGIC), Some("image/png"));
        assert_eq!(detect_media_type_from_bytes(JPEG_MAGIC), Some("image/jpeg"));
        assert_eq!(detect_media_type_from_bytes(GIF_MAGIC), Some("image/gif"));
        assert_eq!(detect_media_type_from_bytes(WEBP_MAGIC), Some("image/webp"));
        // Unrecognized / too short → None (caller defaults to PNG).
        assert_eq!(detect_media_type_from_bytes(b"not an image"), None);
        assert_eq!(detect_media_type_from_bytes(b"ab"), None);
        // "RIFF" without the "WEBP" fourcc is not WebP.
        assert_eq!(
            detect_media_type_from_bytes(b"RIFF\x00\x00\x00\x00AVI "),
            None
        );
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("lingxi_img_test_{name}"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    fn loaded_media_type(p: &Path) -> String {
        match load_image_source(p).unwrap() {
            ImageSource::Base64 { media_type, data } => {
                assert!(!data.is_empty());
                media_type
            }
            ImageSource::Url { .. } => panic!("expected base64"),
        }
    }

    #[test]
    fn png_extension_holding_jpeg_bytes_is_sent_as_jpeg() {
        // The key parity case: the extension lies, the bytes are authoritative.
        let p = write_temp("liar.png", JPEG_MAGIC);
        assert_eq!(loaded_media_type(&p), "image/jpeg");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn each_format_sniffed_through_load() {
        for (name, magic, expect) in [
            ("a.png", PNG_MAGIC, "image/png"),
            ("a.jpg", JPEG_MAGIC, "image/jpeg"),
            ("a.gif", GIF_MAGIC, "image/gif"),
            ("a.webp", WEBP_MAGIC, "image/webp"),
        ] {
            let p = write_temp(name, magic);
            assert_eq!(loaded_media_type(&p), expect, "for {name}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn unknown_bytes_default_to_png() {
        // Unrecognized bytes + unrecognized extension → PNG default (TS default).
        let p = write_temp("mystery.dat", b"definitely not an image");
        assert_eq!(loaded_media_type(&p), "image/png");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_bytes_fall_back_to_extension_hint() {
        // Unrecognized bytes but a known extension → extension hint is used.
        let p = write_temp("hint.gif", b"xx");
        assert_eq!(loaded_media_type(&p), "image/gif");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unreadable_file_errors() {
        // Read failure (missing file) is still the only error path.
        assert!(load_image_source(Path::new("/tmp/lingxi_nonexistent_xyz.bmp")).is_err());
    }
}
