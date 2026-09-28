//! FileRead image-reading shim over the shared image budget processor.
//!
//! FileRead keeps the extension gate locally, but the actual decode/resize
//! logic now lives in `tool-api` so other tool crates can reuse it without a
//! forbidden tool-to-tool dependency edge.

pub use tool_api::util::image_budget::{
    process_image, process_image_with_base64_budget, ProcessedImage, IMAGE_MAX_DIM,
};

/// Image extensions claude-code routes to the image path (FileReadTool.ts:188).
#[must_use]
pub fn is_image_extension(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

// ── FT-09: `q_l`'s two pre-decode guards (cc-238.js @294617599) ─────────────
//
// ```js
// let o=await Ar().readFileBytes(e,r),i=o.length;
// if(i===0)throw new ht(`Image file is empty: ${e}`,"Image file is empty");
// let s=Cle(o);
// if(s===null)throw new ht(`File has an image extension but its content is not a
//   valid PNG/JPEG/GIF/WebP. Detected: ${Kzn(o)}. …`,
//   "Image extension but invalid magic bytes");
// ```
//
// Both literals are present in 2.1.220 too (2 hits each) — a long-standing port
// gap, not 220→238 drift. LingXi previously fell through to `image` crate errors
// (`Image file is empty (0 bytes)` / `failed to decode image: …`).

/// `Cle` (cc-238.js @287754579) — magic-byte sniffer for the four media types
/// the image path accepts. Returns `None` when the bytes are not one of them.
pub use tool_api::util::image_sniff::sniff_image_media_type;

/// `t.toString("latin1").replace(/[^\x20-\x7e]/g,".")` — one char per byte, with
/// every non-printable-ASCII byte rendered as `.`.
fn latin1_printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// `N2a` (cc-238.js @287754579) — the non-image container sniff used by `Kzn`.
fn sniff_container(head: &[u8]) -> Option<&'static str> {
    if head.len() >= 4 && head[..4].eq_ignore_ascii_case(b"%PDF") {
        return Some("pdf");
    }
    if head.len() >= 4 && head[0] == 80 && head[1] == 75 && head[2] == 3 && head[3] == 4 {
        return Some("zip");
    }
    None
}

/// `Kzn` (cc-238.js @287755149) — describe what the first 32 bytes actually are,
/// for the invalid-magic-bytes message's `Detected: …` clause.
#[must_use]
pub fn describe_detected_bytes(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(32)];
    let printable = latin1_printable(head);
    let lower = printable.to_lowercase();
    // `r.slice(0,24)` — the sanitized string is one char per byte, so a byte
    // slice and a char slice agree.
    let excerpt: String = printable.chars().take(24).collect();
    if lower.contains("<!doctype") || lower.contains("<html") {
        return format!("HTML document (starts with \"{excerpt}\")");
    }
    if lower.starts_with("<?xml") || lower.starts_with("<svg") {
        return format!("XML/SVG document (starts with \"{excerpt}\")");
    }
    if lower.starts_with('{') || lower.starts_with('[') {
        return format!("JSON/text (starts with \"{excerpt}\")");
    }
    match sniff_container(head) {
        Some("pdf") => "PDF document".to_string(),
        Some("zip") => {
            "ZIP archive (Office documents such as .pptx/.docx/.xlsx are ZIPs)".to_string()
        }
        _ => {
            let hex = head[..head.len().min(8)]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("unrecognized bytes (hex: {hex})")
        }
    }
}

/// ``Image file is empty: ${e}`` — `e` is the resolved full path.
#[must_use]
pub fn format_image_empty(path: &std::path::Path) -> String {
    format!("Image file is empty: {}", path.display())
}

/// The invalid-magic-bytes message. `${Oi}` ("Bash") is an interpolation SLOT,
/// rendered here as "a registered shell tool" per the port-wide convention
/// (LingXi registers Bash AND PowerShell) — do NOT hard-code `Bash`.
#[must_use]
pub fn format_image_bad_magic(path: &std::path::Path, bytes: &[u8]) -> String {
    format!(
        "File has an image extension but its content is not a valid PNG/JPEG/GIF/WebP. Detected: {detected}. This usually means a download saved an error/login page instead of the image. Use `file \"{path}\"` to confirm, or read it as text with a registered shell tool (e.g. `head -c 500`).",
        detected = describe_detected_bytes(bytes),
        path = path.display(),
    )
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
    fn detects_image_extensions() {
        for ok in ["a.png", "a.jpg", "a.JPEG", "a.gif", "a.webp"] {
            assert!(is_image_extension(std::path::Path::new(ok)), "{ok}");
        }
        for no in ["a.txt", "a.rs", "a", "a.tar.gz"] {
            assert!(!is_image_extension(std::path::Path::new(no)), "{no}");
        }
    }

    #[test]
    fn small_image_passes_through_unchanged() {
        use base64::Engine;
        let bytes = png_bytes(10, 10);
        let p = process_image(bytes.clone()).unwrap();
        assert_eq!(p.media_type, "image/png");
        assert!(p.resized.is_none(), "small image not resized");
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

    // ── FT-09: `Cle` / `Kzn` and the two `q_l` guards ────────────────────────

    #[test]
    fn sniffs_the_four_accepted_magic_numbers() {
        assert_eq!(
            sniff_image_media_type(&png_bytes(2, 2)),
            Some("image/png"),
            "a real PNG"
        );
        assert_eq!(
            sniff_image_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_media_type(b"GIF87a"), Some("image/gif"));
        assert_eq!(sniff_image_media_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(
            sniff_image_media_type(b"RIFF\0\0\0\0WEBP"),
            Some("image/webp")
        );
        // Rejections: too short, a truncated RIFF, an HTML login page.
        assert_eq!(sniff_image_media_type(b"GIF"), None);
        assert_eq!(sniff_image_media_type(b"RIFF\0\0\0\0WEB"), None);
        assert_eq!(sniff_image_media_type(b"<!DOCTYPE html>"), None);
        assert_eq!(sniff_image_media_type(b""), None);
    }

    #[test]
    fn describes_detected_bytes_like_kzn() {
        // `n.includes("<!doctype")` — a match anywhere in the first 32 bytes.
        assert_eq!(
            describe_detected_bytes(b"<!DOCTYPE html><html><head><title>Login"),
            "HTML document (starts with \"<!DOCTYPE html><html><he\")"
        );
        assert_eq!(
            describe_detected_bytes(b"<?xml version=\"1.0\"?><svg/>"),
            "XML/SVG document (starts with \"<?xml version=\"1.0\"?><sv\")"
        );
        assert_eq!(
            describe_detected_bytes(b"{\"error\":\"not found\"}"),
            "JSON/text (starts with \"{\"error\":\"not found\"}\")"
        );
        assert_eq!(describe_detected_bytes(b"%PDF-1.7\n"), "PDF document");
        assert_eq!(
            describe_detected_bytes(&[0x50, 0x4B, 0x03, 0x04, 0x14, 0x00]),
            "ZIP archive (Office documents such as .pptx/.docx/.xlsx are ZIPs)"
        );
        // `t.subarray(0,8).toString("hex").replace(/(..)/g,"$1 ").trim()`.
        assert_eq!(
            describe_detected_bytes(&[0x00, 0x01, 0xFE, 0xFF, 0x10, 0x20, 0x30, 0x40, 0x50]),
            "unrecognized bytes (hex: 00 01 fe ff 10 20 30 40)"
        );
        // Non-printable bytes become `.` inside the excerpt.
        assert_eq!(
            describe_detected_bytes(b"{\x00\x01ab"),
            "JSON/text (starts with \"{..ab\")"
        );
    }

    #[test]
    fn image_guard_messages_are_byte_locked() {
        let p = std::path::Path::new("/tmp/shot.png");
        assert_eq!(format_image_empty(p), "Image file is empty: /tmp/shot.png");
        assert_eq!(
            format_image_bad_magic(p, b"<!DOCTYPE html><html><head><title>Login"),
            "File has an image extension but its content is not a valid PNG/JPEG/GIF/WebP. Detected: HTML document (starts with \"<!DOCTYPE html><html><he\"). This usually means a download saved an error/login page instead of the image. Use `file \"/tmp/shot.png\"` to confirm, or read it as text with a registered shell tool (e.g. `head -c 500`)."
        );
    }

    #[test]
    fn base64_budget_is_enforced() {
        let p = process_image_with_base64_budget(png_bytes(3000, 1), 4_000).unwrap();
        assert!(p.base64.len() <= 4_000);
    }
}
