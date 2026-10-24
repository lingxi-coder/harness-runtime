//! Shared bounded, path-free attachment decoding for Local App Agent turns.
use base64::Engine;
use lingxi_core::types::{ContentBlock, DocumentSource, ImageSource};
use local_app::v2::agent::{AttachmentMetadata, Input};

/// Decoded model-ready content with safe metadata for host-owned history.
#[derive(Debug)]
pub struct PreparedInput {
    /// Ordered message blocks, including the user's prompt and file boundaries.
    pub blocks: Vec<ContentBlock>,
    /// Name, MIME type and byte count for each submitted file.
    pub attachments: Vec<AttachmentMetadata>,
    /// True when the model must support image/PDF input.
    pub uses_vision: bool,
}

/// Decode and validate all uploaded files without resolving or opening host paths.
pub async fn prepare(input: &Input) -> Result<PreparedInput, String> {
    let mut blocks = vec![ContentBlock::Text {
        text: input.prompt.clone(),
        citations: None,
    }];
    let mut attachments = Vec::with_capacity(input.attachments.len());
    let mut uses_vision = false;

    for attachment in &input.attachments {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&attachment.base64)
            .map_err(|_| "invalid_attachments")?;
        if bytes.is_empty() || bytes.len() > 25 * 1024 * 1024 {
            return Err("invalid_attachments".into());
        }
        let media_type = attachment.media_type.to_ascii_lowercase();
        let label = format!("Attachment: {} ({media_type})", attachment.name);
        blocks.push(ContentBlock::Text {
            text: label,
            citations: None,
        });

        if media_type.starts_with("image/") {
            if !image_signature(&bytes, &media_type) {
                return Err("invalid_attachments".into());
            }
            let processed = tokio::task::spawn_blocking(move || {
                tool_api::util::image_budget::process_image(bytes)
            })
            .await
            .map_err(|_| "invalid_attachments")?
            .map_err(|_| "invalid_attachments")?;
            blocks.push(ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: processed.media_type,
                    data: processed.base64,
                },
            });
            uses_vision = true;
        } else if media_type == "application/pdf" {
            if !bytes.starts_with(b"%PDF-") {
                return Err("invalid_attachments".into());
            }
            blocks.push(ContentBlock::Document {
                source: DocumentSource::Base64 {
                    media_type,
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                },
            });
            uses_vision = true;
        } else if is_text_media(&media_type) {
            let text = String::from_utf8(bytes).map_err(|_| "invalid_attachments")?;
            if text.len() > 25 * 1024 * 1024 {
                return Err("invalid_attachments".into());
            }
            blocks.push(ContentBlock::Text {
                text,
                citations: None,
            });
        } else {
            return Err("attachment_type_unsupported".into());
        }

        attachments.push(AttachmentMetadata {
            name: attachment.name.clone(),
            media_type: attachment.media_type.clone(),
            size: bytes_len(&attachment.base64)?,
        });
    }

    Ok(PreparedInput {
        blocks,
        attachments,
        uses_vision,
    })
}

fn bytes_len(encoded: &str) -> Result<u64, String> {
    let padding = encoded
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    let bytes = encoded
        .len()
        .checked_div(4)
        .and_then(|blocks| blocks.checked_mul(3))
        .and_then(|size| size.checked_sub(padding))
        .ok_or("invalid_attachments")?;
    u64::try_from(bytes).map_err(|_| "invalid_attachments".into())
}

fn is_text_media(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/xml" | "application/csv" | "application/x-yaml"
        )
}

fn image_signature(bytes: &[u8], mime: &str) -> bool {
    match mime {
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(media_type: &str, name: &str, bytes: &[u8]) -> Input {
        Input {
            request_id: "request-1".into(),
            prompt: "inspect".into(),
            attachments: vec![local_app::v2::agent::Attachment {
                name: name.into(),
                media_type: media_type.into(),
                base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            }],
            output_schema: None,
            tools: Vec::new(),
            skills: Vec::new(),
            max_turns: None,
            timeout_ms: None,
        }
    }

    #[tokio::test]
    async fn supports_text_pdf_and_verified_images_without_paths() {
        let text = prepare(&input("text/plain", "notes.txt", b"hello"))
            .await
            .unwrap();
        assert!(!text.uses_vision);
        assert!(
            matches!(text.blocks.last(), Some(ContentBlock::Text { text, .. }) if text == "hello")
        );

        let pdf = prepare(&input("application/pdf", "doc.pdf", b"%PDF-1.7"))
            .await
            .unwrap();
        assert!(pdf.uses_vision);
        assert!(matches!(
            pdf.blocks.last(),
            Some(ContentBlock::Document { .. })
        ));
        assert_eq!(
            prepare(&input("application/pdf", "spoof.pdf", b"plain text"))
                .await
                .unwrap_err(),
            "invalid_attachments"
        );

        let png = prepare(&input("image/png", "pixel.png", b"\x89PNG\r\n\x1a\n")).await;
        assert!(png.is_err()); // malformed image payload is rejected by image processing.
    }

    #[tokio::test]
    async fn rejects_unknown_or_mislabeled_files() {
        assert_eq!(
            prepare(&input("application/zip", "archive.zip", b"PK\x03\x04"))
                .await
                .unwrap_err(),
            "attachment_type_unsupported"
        );
        assert_eq!(
            prepare(&input("image/png", "spoof.png", b"plain text"))
                .await
                .unwrap_err(),
            "invalid_attachments"
        );
    }
}
