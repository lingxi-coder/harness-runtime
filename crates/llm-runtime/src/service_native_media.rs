//! Claude Code 2.1.319 Kvn/Pvn media projection. Input is already normalized
//! and tool-paired; this pass changes only the outgoing request clone.
use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::HashSet;

const MEDIA_REMOVED: &str = "[media removed: request limit]";

#[derive(Clone, Copy, Debug)]
pub(super) struct NativeMediaLimits {
    pub(super) count: usize,
    pub(super) count_headroom: usize,
    pub(super) bytes: usize,
    pub(super) byte_headroom: usize,
    pub(super) document_bytes: usize,
}

impl NativeMediaLimits {
    pub(super) fn for_model(
        model: &str,
        context_1m_disabled: bool,
        context_1m_beta: bool,
        constrained_endpoint: bool,
    ) -> Self {
        // Native Ovn's ZOr/fT/catalog branch. Explicit custom CLI beta
        // membership remains independent of the disable-1m model gates.
        let canonical = lingxi_core::host::model_capabilities::normalize_model_id(model);
        let native_1m = matches!(
            canonical.as_str(),
            "claude-haiku-5-5"
                | "claude-sonnet-5"
                | "claude-sonnet-5-5"
                | "claude-opus-4-7"
                | "claude-opus-4-8"
                | "claude-opus-5"
                | "claude-opus-5-5"
                | "claude-fable-5"
                | "claude-fable-5-1"
                | "claude-mythos-5"
                | "claude-mythos-5-1"
        );
        let long_context = context_1m_beta
            || (!context_1m_disabled && (model.to_ascii_lowercase().contains("[1m]") || native_1m));
        Self {
            count: if long_context { 600 } else { 100 },
            count_headroom: 20,
            bytes: if constrained_endpoint {
                25_165_824
            } else {
                78_643_200
            },
            byte_headroom: 10_485_760,
            document_bytes: if constrained_endpoint { 25_165_824 } else { 0 },
        }
    }
}

#[derive(Clone, Copy)]
struct MediaItem {
    bytes: usize,
    document_bytes: usize,
}

fn media_item(kind: &str, bytes: usize) -> Option<MediaItem> {
    if !matches!(kind, "image" | "document") {
        return None;
    }
    Some(MediaItem {
        bytes,
        document_bytes: if kind == "document" { bytes } else { 0 },
    })
}

fn projected_media_item(value: &Utf16JsonProjection) -> Option<MediaItem> {
    let kind = value.string_at("/type")?;
    // Native project-read text documents are context carriers, not media.
    if kind == "document"
        && value.string_at("/source/type") == Some("text")
        && value
            .string_at("/context")
            .is_some_and(|text| text.starts_with("A doc of this project."))
    {
        return None;
    }
    let data = (value.string_at("/source/type") == Some("base64"))
        .then(|| {
            value
                .at_pointer("/source/data")
                .and_then(|data| data.code_units())
        })
        .flatten();
    media_item(kind, data.map_or(0, <[u16]>::len))
}

fn block_media_item(block: &ContentBlock) -> Option<MediaItem> {
    match block {
        ContentBlock::Image { source } => match source {
            lingxi_core::types::ImageSource::Base64 { data, .. } => {
                media_item("image", data.encode_utf16().count())
            }
            lingxi_core::types::ImageSource::Url { .. } => media_item("image", 0),
        },
        ContentBlock::Document { source } => match source {
            lingxi_core::types::DocumentSource::Base64 { data, .. } => {
                media_item("document", data.encode_utf16().count())
            }
        },
        ContentBlock::ProviderContent { protocol, value } if protocol == "anthropic_messages" => {
            projected_media_item(value)
        }
        _ => None,
    }
}

fn nested_content(block: &ContentBlock) -> Option<&Utf16JsonProjection> {
    match block {
        ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } => {
            let value = content_blocks.as_ref().unwrap_or(content);
            value.array_len("").map(|_| value)
        }
        ContentBlock::ProviderContent { protocol, value }
            if protocol == "anthropic_messages"
                && value.string_at("/type") == Some("tool_result") =>
        {
            value.array_len("/content").map(|_| value)
        }
        _ => None,
    }
}

fn content_pointer(block: &ContentBlock) -> &'static str {
    if matches!(block, ContentBlock::ProviderContent { .. }) {
        "/content"
    } else {
        ""
    }
}

fn removed_projection() -> Utf16JsonProjection {
    Utf16JsonProjection::parse(r#"{"type":"text","text":"[media removed: request limit]"}"#)
        .expect("native media placeholder is valid JSON")
}

fn content_mut(message: &mut ConversationMessage) -> Option<&mut Vec<ContentBlock>> {
    match message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => Some(content),
        ConversationMessage::System {
            api_system: Some(message),
            ..
        } => Some(&mut message.content),
        _ => None,
    }
}

pub(super) fn strip_native_media_with_sources(
    messages: ConversationMessagesWithSources,
    limits: NativeMediaLimits,
) -> ConversationMessagesWithSources {
    // No LingXi producer owns Native's private rejected/unsent source state.
    // Source text is never evidence that a host rejected this media item.
    strip_native_media_with_host_rejections(messages, limits, &[])
}

fn strip_native_media_with_host_rejections(
    mut messages: ConversationMessagesWithSources,
    limits: NativeMediaLimits,
    rejected_media_indices: &[usize],
) -> ConversationMessagesWithSources {
    let mut items = Vec::new();
    for message in &mut messages.messages {
        let Some(content) = content_mut(message) else {
            continue;
        };
        for block in content {
            if let Some(item) = block_media_item(block) {
                items.push(item);
            } else if let Some(value) = nested_content(block) {
                let pointer = content_pointer(block);
                for index in 0..value.array_len(pointer).unwrap_or(0) {
                    if let Some(item) = value
                        .subprojection(&format!("{pointer}/{index}"))
                        .ok()
                        .as_ref()
                        .and_then(projected_media_item)
                    {
                        items.push(item);
                    }
                }
            }
        }
    }
    let mut removed = vec![false; items.len()];
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    let mut document_bytes = 0usize;
    for (index, item) in items.iter().enumerate() {
        kept.push(index);
        bytes += item.bytes;
        document_bytes += item.document_bytes;
        if kept.len() > limits.count {
            let count = kept.len() - limits.count.saturating_sub(limits.count_headroom);
            for old in kept.drain(..count) {
                removed[old] = true;
                bytes -= items[old].bytes;
                document_bytes -= items[old].document_bytes;
            }
        }
        if limits.document_bytes > 0
            && limits.document_bytes < limits.bytes
            && document_bytes > limits.document_bytes
        {
            let mut cursor = 0;
            while cursor < kept.len()
                && document_bytes > limits.document_bytes.saturating_sub(limits.byte_headroom)
            {
                let old = kept[cursor];
                if items[old].document_bytes == 0 {
                    cursor += 1;
                    continue;
                }
                kept.remove(cursor);
                removed[old] = true;
                bytes -= items[old].bytes;
                document_bytes -= items[old].document_bytes;
            }
        }
        if limits.bytes > 0 && bytes > limits.bytes {
            let mut cursor = 0;
            while cursor < kept.len() && bytes > limits.bytes.saturating_sub(limits.byte_headroom) {
                let old = kept[cursor];
                if items[old].bytes == 0 {
                    cursor += 1;
                    continue;
                }
                kept.remove(cursor);
                removed[old] = true;
                bytes -= items[old].bytes;
                document_bytes -= items[old].document_bytes;
            }
        }
    }
    for &index in rejected_media_indices {
        if let Some(removed) = removed.get_mut(index) {
            *removed = true;
        }
    }
    if !removed.iter().any(|removed| *removed) {
        return messages;
    }
    let mut index = 0;
    for (message, sources) in messages
        .messages
        .iter_mut()
        .zip(&mut messages.block_sources)
    {
        let Some(content) = content_mut(message) else {
            continue;
        };
        let mut retained = Vec::with_capacity(content.len());
        let mut retained_sources = Vec::with_capacity(sources.len());
        let mut dropped_sources = HashSet::new();
        let mut top_level_removed = false;
        for (mut block, source) in content.drain(..).zip(sources.drain(..)) {
            if block_media_item(&block).is_some() {
                let drop = removed[index];
                index += 1;
                if drop {
                    top_level_removed = true;
                    dropped_sources.extend(source);
                    continue;
                }
            } else if let Some(value) = nested_content(&block) {
                let pointer = content_pointer(&block);
                let mut changed = false;
                let nested = (0..value.array_len(pointer).unwrap_or(0))
                    .map(|offset| {
                        let nested = value
                            .subprojection(&format!("{pointer}/{offset}"))
                            .expect("native nested content is valid JSON");
                        if projected_media_item(&nested).is_some() {
                            let drop = removed[index];
                            index += 1;
                            if drop {
                                changed = true;
                                return removed_projection();
                            }
                        }
                        nested
                    })
                    .collect::<Vec<_>>();
                if changed {
                    let replacement = Utf16JsonProjection::array(nested)
                        .expect("native nested content is a JSON array");
                    match &mut block {
                        ContentBlock::ToolResult {
                            content,
                            content_blocks,
                            ..
                        } => {
                            if content_blocks.is_some() {
                                *content_blocks = Some(replacement);
                            } else {
                                *content = replacement;
                            }
                        }
                        ContentBlock::ProviderContent { value, .. } => {
                            value
                                .replace_pointer_projection("/content", replacement)
                                .expect("native result has a content array");
                        }
                        _ => unreachable!("nested_content accepts only tool results"),
                    }
                }
            }
            retained.push(block);
            retained_sources.push(source);
        }
        if retained.is_empty() && top_level_removed {
            retained.push(ContentBlock::Text {
                text: MEDIA_REMOVED.into(),
                citations: None,
            });
            retained_sources.push(dropped_sources);
        }
        *content = retained;
        *sources = retained_sources;
    }
    messages
}

#[cfg(test)]
#[path = "service_media_319_test.rs"]
mod tests;
