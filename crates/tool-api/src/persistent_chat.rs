//! The bounded, self-contained result contract for host-owned Local App Chat.

use base64::Engine as _;
use lingxi_core::host::tool_invoker::LOCAL_APP_CHAT_MAX_INLINE_BYTES;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

/// A result shape cannot be restored solely from the captured transcript.
pub const INLINE_RESULT_ERROR: &str = "chat_transcript_external_result_unmanaged";
/// The exact serialized result exceeds the current inline budget.
pub const INLINE_RESULT_TOO_LARGE: &str = "local_app_chat_inline_result_too_large";

/// Bound the exact serialized projection, including metadata and media bytes.
pub fn validate_inline_size(value: &Source) -> Result<(), &'static str> {
    if value
        .to_json_string()
        .map_err(|_| INLINE_RESULT_ERROR)?
        .len()
        > LOCAL_APP_CHAT_MAX_INLINE_BYTES
    {
        return Err(INLINE_RESULT_TOO_LARGE);
    }
    Ok(())
}

/// Only provider-ready text, base64 images, and base64 PDF documents are
/// self-contained. Resource references, URLs and opaque provider blocks have
/// no implicit read-back path under this profile.
pub fn validate_inline_blocks(blocks: &Source) -> Result<(), &'static str> {
    validate_inline_size(blocks)?;
    let count = blocks.array_len("").ok_or(INLINE_RESULT_ERROR)?;
    for index in 0..count {
        let block = blocks
            .subprojection(&format!("/{index}"))
            .map_err(|_| INLINE_RESULT_ERROR)?;
        match block.string_at("/type") {
            Some("text") if block.string_units("/text").is_some() => {}
            Some("image" | "document") => {
                let kind = block.string_at("/type");
                let mime = block.string_at("/source/media_type");
                let valid_mime = if kind == Some("image") {
                    matches!(
                        mime,
                        Some("image/jpeg" | "image/png" | "image/gif" | "image/webp")
                    )
                } else {
                    mime == Some("application/pdf")
                };
                if block.string_at("/source/type") != Some("base64") || !valid_mime {
                    return Err(INLINE_RESULT_ERROR);
                }
                let data = block.string_at("/source/data").ok_or(INLINE_RESULT_ERROR)?;
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| INLINE_RESULT_ERROR)?;
            }
            _ => return Err(INLINE_RESULT_ERROR),
        }
    }
    Ok(())
}

/// Validate both projections of one canonical tool result before persistence.
pub fn validate_inline_result(
    content: &Source,
    content_blocks: Option<&Source>,
) -> Result<(), &'static str> {
    validate_inline_size(content)?;
    if let Some(blocks) = content_blocks {
        validate_inline_blocks(blocks)?;
    } else if content.string_units("").is_none() {
        return Err(INLINE_RESULT_ERROR);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_inline_text_image_and_pdf_but_rejects_external_shapes() {
        let content = Source::parse(r#"[{"type":"text","text":"inline"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}},{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERg=="}}]"#).unwrap();
        assert_eq!(validate_inline_blocks(&content), Ok(()));
        for external in [
            r#"[{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]"#,
            r#"[{"type":"resource_link","uri":"file:///tmp/blob"}]"#,
            r#"[{"type":"tool_reference","tool_name":"unknown"}]"#,
            r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"invalid!"}}]"#,
        ] {
            assert!(validate_inline_blocks(&Source::parse(external).unwrap()).is_err());
        }
    }

    #[test]
    fn limits_serialized_text_and_media_including_metadata() {
        let large: Source = "x".repeat(LOCAL_APP_CHAT_MAX_INLINE_BYTES).into();
        assert_eq!(validate_inline_size(&large), Err(INLINE_RESULT_TOO_LARGE));
        let metadata = Source::plain(serde_json::json!([{
            "type":"text", "text":"small", "_meta":{"huge":"x".repeat(LOCAL_APP_CHAT_MAX_INLINE_BYTES)}
        }]));
        assert_eq!(
            validate_inline_blocks(&metadata),
            Err(INLINE_RESULT_TOO_LARGE)
        );
    }
}
