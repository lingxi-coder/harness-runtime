//! Image magic-byte sniffing — a 1:1 port of claude-code's `Wfe`.
//!
//! Used by the Bash tool's image-output gate and the dispatch loop's Bash
//! image result mapper (`hKn`): a `data:` URI's payload only becomes a
//! tool_result image block when its DECODED bytes carry a recognized image
//! magic, and the block's `media_type` is the SNIFFED type (never the URI's
//! claimed one).

/// Sniff an image's media type from its magic bytes (claude `Wfe`):
/// PNG (`89 50 4E 47`), JPEG (`FF D8 FF`), GIF87a/GIF89a, RIFF-WEBP.
/// `None` for anything else — callers treat the output as text.
#[must_use]
pub fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes[0] == 137 && bytes[1] == 80 && bytes[2] == 78 && bytes[3] == 71 {
        return Some("image/png");
    }
    if bytes[0] == 255 && bytes[1] == 216 && bytes[2] == 255 {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6
        && bytes[0] == 71
        && bytes[1] == 73
        && bytes[2] == 70
        && bytes[3] == 56
        && (bytes[4] == 55 || bytes[4] == 57)
        && bytes[5] == 97
    {
        return Some("image/gif");
    }
    if bytes[0] == 82
        && bytes[1] == 73
        && bytes[2] == 70
        && bytes[3] == 70
        && bytes.len() >= 12
        && bytes[8] == 87
        && bytes[9] == 69
        && bytes[10] == 66
        && bytes[11] == 80
    {
        return Some("image/webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::sniff_image_media_type;

    #[test]
    fn recognizes_the_wfe_magic_set() {
        assert_eq!(
            sniff_image_media_type(&[137, 80, 78, 71, 13, 10, 26, 10]),
            Some("image/png")
        );
        assert_eq!(
            sniff_image_media_type(&[255, 216, 255, 224]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_media_type(b"GIF87a"), Some("image/gif"));
        assert_eq!(sniff_image_media_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(
            sniff_image_media_type(b"RIFF\x00\x00\x00\x00WEBP"),
            Some("image/webp")
        );
    }

    #[test]
    fn rejects_short_and_unrecognized_bytes() {
        assert_eq!(sniff_image_media_type(b""), None);
        assert_eq!(sniff_image_media_type(b"GIF"), None); // < 4 bytes
        assert_eq!(sniff_image_media_type(b"hello world"), None);
        // RIFF but not WEBP (e.g. WAV) is NOT an image.
        assert_eq!(sniff_image_media_type(b"RIFF\x00\x00\x00\x00WAVE"), None);
        // GIF with a bad version byte.
        assert_eq!(sniff_image_media_type(b"GIF88a"), None);
    }
}
