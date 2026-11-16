//! Apply a single content transformation to all host request coordinates.
//!
//! The SDK map owns structural positions; host admission identities stay in
//! ExecutionContext. Nothing here encodes input, grants authority, or mutates
//! canonical history. Build and validate all metadata before committing it.
use crate::{LlmError, LlmRequest};
use lingxi_llm_client::message_projection::MessageProjectionMap;

fn invalid(message: &str) -> LlmError {
    LlmError::InvalidRequest {
        message: message.to_owned(),
    }
}

pub(crate) fn apply(
    original: &LlmRequest,
    projected: &mut LlmRequest,
    map: &MessageProjectionMap,
) -> Result<(), LlmError> {
    for (mi, message) in original.input.messages.iter().enumerate() {
        if map
            .message_index(mi)
            .is_some_and(|index| index >= projected.input.messages.len())
        {
            return Err(invalid(
                "message projection points outside the projected request",
            ));
        }
        for bi in 0..message.content.len() {
            for &(target_message, target_block) in
                map.outputs((mi, bi)).map_err(crate::upstream::error)?
            {
                if map
                    .message_index(mi)
                    .is_some_and(|index| index != target_message)
                {
                    return Err(invalid("whole-message and block projections disagree"));
                }
                if projected
                    .input
                    .messages
                    .get(target_message)
                    .and_then(|message| message.content.get(target_block))
                    .is_none()
                {
                    return Err(invalid(
                        "block projection points outside the projected request",
                    ));
                }
            }
        }
    }
    // Protocol adaptation may deliberately clear source-family cache policy.
    // Only remap the controls retained by the projected request.
    let mut points = projected.input.prompt_cache.breakpoints.clone();
    for point in &mut points {
        point.position = map
            .cache_position(point.position)
            .map_err(crate::upstream::error)?
            .ok_or_else(|| invalid("request projection removed an active cache endpoint"))?;
    }
    let mut layout = original.execution.classifier_cache_layout.clone();
    if let Some(layout) = &mut layout {
        let remap = |position| {
            map.cache_position(position)
                .map_err(crate::upstream::error)?
                .ok_or_else(|| invalid("request projection removed a classifier cache endpoint"))
        };
        layout.history = layout.history.map(remap).transpose()?;
        layout.action = layout.action.map(remap).transpose()?;
    }
    let exact = map
        .string_overrides(&original.execution.message_json_string_overrides)
        .map_err(crate::upstream::error)?;
    let mut indices = original.execution.request_message_source_indices.clone();
    indices.retain(|_, index| {
        if let Some(target) = map.message_index(*index) {
            *index = target;
            true
        } else {
            false
        }
    });
    let sources = &original.execution.request_content_source_ids;
    let mapped_sources = if sources.is_empty() {
        Vec::new()
    } else {
        if sources.len() != original.input.messages.len()
            || sources
                .iter()
                .zip(&original.input.messages)
                .any(|(sources, message)| sources.len() != message.content.len())
        {
            return Err(invalid("request projection has ambiguous block provenance"));
        }
        let mut mapped = projected
            .input
            .messages
            .iter()
            .map(|message| vec![std::collections::HashSet::new(); message.content.len()])
            .collect::<Vec<_>>();
        for (mi, row) in sources.iter().enumerate() {
            for (bi, source) in row.iter().enumerate() {
                for &(target_message, target_block) in
                    map.outputs((mi, bi)).map_err(crate::upstream::error)?
                {
                    mapped[target_message][target_block].extend(source.iter().copied());
                }
            }
        }
        mapped
    };
    projected.input.prompt_cache.breakpoints = points;
    projected.execution.classifier_cache_layout = layout;
    projected.execution.message_json_string_overrides = exact;
    projected.execution.request_message_source_indices = indices;
    projected.execution.request_content_source_ids = mapped_sources;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::MessageId;
    use lingxi_llm_client::protocol::{
        CacheBreakpoint, CachePosition, CacheTtl, ConversationMessage,
    };

    #[test]
    fn metadata_moves_together_and_merged_blocks_union_existing_authority() {
        let a = MessageId::new();
        let b = MessageId::new();
        let mut source = LlmRequest::new("fixture");
        source.input.messages = vec![
            ConversationMessage::user_text("�"),
            ConversationMessage::user_text("tail"),
        ];
        source.execution.request_message_source_ids.extend([a, b]);
        source.execution.request_content_source_ids = vec![vec![[a].into()], vec![[b].into()]];
        source
            .execution
            .request_message_source_indices
            .extend([(a, 0), (b, 1)]);
        source
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);
        source.input.prompt_cache.breakpoints.push(CacheBreakpoint {
            position: CachePosition::Message { index: 0, block: 0 },
            ttl: CacheTtl::ThirtyMinutes,
            scope: None,
        });
        let mut projected = source.clone();
        projected.input.messages = vec![ConversationMessage::user_text("�")];
        let mut map = MessageProjectionMap::default();
        map.insert((0, 0), vec![(0, 0)]).unwrap();
        map.insert((1, 0), vec![(0, 0)]).unwrap();
        apply(&source, &mut projected, &map).unwrap();
        assert_eq!(
            projected.execution.request_content_source_ids,
            vec![vec![[a, b].into()]]
        );
        assert_eq!(projected.execution.request_message_source_indices[&b], 0);
        assert_eq!(
            projected.execution.request_message_source_ids,
            source.execution.request_message_source_ids
        );
        assert_eq!(
            projected.execution.message_json_string_overrides,
            source.execution.message_json_string_overrides
        );
        assert_eq!(projected.input.prompt_cache, source.input.prompt_cache);
    }

    #[test]
    fn invalid_provenance_does_not_partially_change_request_metadata() {
        let mut source = LlmRequest::new("fixture").with_user_text("task");
        source.execution.request_content_source_ids = vec![vec![]];
        let mut projected = source.clone();
        let mut map = MessageProjectionMap::default();
        map.insert((0, 0), vec![(0, 0)]).unwrap();
        assert!(apply(&source, &mut projected, &map).is_err());
        assert_eq!(
            projected.execution.request_content_source_ids,
            source.execution.request_content_source_ids
        );
    }

    #[test]
    fn contradictory_message_identity_is_rejected_before_metadata_changes() {
        let source = LlmRequest::new("fixture")
            .with_user_text("first")
            .with_user_text("second");
        let mut projected = source.clone();
        let mut map = MessageProjectionMap::default();
        map.insert((0, 0), vec![(0, 0)]).unwrap();
        map.insert_message(0, vec![1]).unwrap();
        map.insert((1, 0), vec![(1, 0)]).unwrap();
        assert!(apply(&source, &mut projected, &map).is_err());
        assert_eq!(projected.input.messages, source.input.messages);
    }

    #[test]
    fn protocol_retry_reindexes_exact_text_and_authority_without_restoring_foreign_cache_policy() {
        use lingxi_llm_client::protocol::{ContentBlock, JsonPayload, MessageRole, ProtocolFamily};
        let hidden = MessageId::new();
        let visible = MessageId::new();
        let mut source = LlmRequest::new("fixture");
        source.input.messages = vec![
            ConversationMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ProviderContent {
                    protocol: ProtocolFamily::OpenAiResponses,
                    value: JsonPayload::parse(
                        r#"{"type":"reasoning","id":"rs_fixture","encrypted_content":"opaque"}"#,
                    )
                    .unwrap(),
                }],
                native_options: Vec::new(),
            },
            ConversationMessage::user_text("�"),
        ];
        source.execution.request_content_source_ids =
            vec![vec![[hidden].into()], vec![[visible].into()]];
        source
            .execution
            .request_message_source_indices
            .extend([(hidden, 0), (visible, 1)]);
        source
            .execution
            .message_json_string_overrides
            .insert("/messages/1/content/0/text".into(), vec![0xd800]);
        source.input.prompt_cache.breakpoints.push(CacheBreakpoint {
            position: CachePosition::Message { index: 1, block: 0 },
            ttl: CacheTtl::ThirtyMinutes,
            scope: None,
        });
        source.execution.classifier_cache_layout = Some(crate::ClassifierCacheLayout {
            logical_id: "retry-fixture".into(),
            stage: "fast",
            history: Some(CachePosition::Message { index: 1, block: 0 }),
            action: None,
        });
        let projected = crate::upstream::adapt_request(
            &source,
            ProtocolFamily::OpenAiResponses,
            ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(projected.input.messages.len(), 1);
        assert_eq!(projected.input.prompt_cache, Default::default());
        assert_eq!(
            projected.execution.message_json_string_overrides,
            [("/messages/0/content/0/text".into(), vec![0xd800])].into()
        );
        assert_eq!(
            projected.execution.request_content_source_ids,
            vec![vec![[visible].into()]]
        );
        assert!(!projected
            .execution
            .request_message_source_indices
            .contains_key(&hidden));
        assert_eq!(
            projected.execution.request_message_source_indices[&visible],
            0
        );
        assert_eq!(
            projected.execution.classifier_cache_layout.unwrap().history,
            Some(CachePosition::Message { index: 0, block: 0 })
        );
        assert_eq!(source.input.messages.len(), 2);
        assert_eq!(source.input.prompt_cache.breakpoints.len(), 1);
    }
}
