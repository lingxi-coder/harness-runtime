//! Tests for `convert.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod convert_test;`.

pub use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{MediaAnalysis, MediaObservation, MessageId, ToolUseId};

    // ── to_llm_messages ───────────────────────────────────────────────────────

    #[test]
    fn text_block_maps_to_llm_text_with_no_cache_control() {
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::Text {
                text: "hello".to_string(),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::Text { text, cache_control: None } if text == "hello"
        ));
    }

    #[test]
    fn tool_use_block_maps_to_tool_call() {
        let id = ToolUseId::new();
        let id_str = id.to_string();
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::ToolUse {
                id,
                name: "Read".to_string(),
                input: serde_json::json!({"path": "/tmp/x"}),
                provider_id: None,
            }],
            stop_reason: None,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert_eq!(result[0].role, "assistant");
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ToolCall { id, name, input }
                if id == &id_str && name == "Read" && input["path"] == "/tmp/x"
        ));
    }

    #[test]
    fn tool_use_provider_id_replayed_verbatim_on_egress() {
        // P0: the canonical provider id (Anthropic `toolu_…`) carried in the
        // `ToolUseId` MUST be replayed verbatim as the egress `tool_call` id.
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::ToolUse {
                id: ToolUseId::from("toolu_01ABCDEF"),
                name: "Read".to_string(),
                input: serde_json::json!({"path": "/tmp/x"}),
                provider_id: None,
            }],
            stop_reason: None,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ToolCall { id, .. } if id == "toolu_01ABCDEF"
        ));
    }

    #[test]
    fn tool_result_provider_id_replayed_verbatim_on_egress() {
        // P0: the paired `tool_result` must echo the SAME verbatim canonical id.
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::ToolResult {
                tool_use_id: ToolUseId::from("toolu_01ABCDEF"),
                content: "file content".to_string(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ToolResult { tool_call_id, .. } if tool_call_id == "toolu_01ABCDEF"
        ));
    }

    #[test]
    fn tool_result_block_wraps_content_as_string_value() {
        let tool_use_id = ToolUseId::new();
        let tool_call_id_str = tool_use_id.to_string();
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::ToolResult {
                tool_use_id,
                content: "file content".to_string(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ToolResult { tool_call_id, output, is_error: false, cache_control: None, cache_reference: None }
                if tool_call_id == &tool_call_id_str && output == &Value::String("file content".to_string())
        ));
    }

    #[test]
    fn tool_result_error_flag_preserved() {
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "boom".to_string(),
                is_error: true,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn thinking_block_maps_to_reasoning() {
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::Thinking {
                thinking: "let me think".to_string(),
                signature: Some("sig_abc".to_string()),
            }],
            stop_reason: None,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::Reasoning { text, signature }
                if text == "let me think" && signature.as_deref() == Some("sig_abc")
        ));
    }

    #[test]
    fn image_base64_source_decoded_to_bytes() {
        // "hello" base64 encodes to "aGVsbG8="
        let raw = b"hello";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: encoded,
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::Image { media_type, bytes }
                if media_type == "image/png" && bytes.as_slice() == raw
        ));
    }

    #[test]
    fn image_url_source_maps_to_image_url() {
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::Image {
                source: ImageSource::Url {
                    url: "https://example.com/img.png".to_string(),
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ImageUrl { url } if url == "https://example.com/img.png"
        ));
    }

    #[test]
    fn media_analysis_block_is_lowered_to_model_visible_text() {
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::MediaAnalysis {
                analysis: MediaAnalysis {
                    question_key: "msg-123".into(),
                    media_fingerprints: vec!["fp-a".into()],
                    model: "deepseek-flash".into(),
                    prompt_version: 1,
                    created_at: std::time::UNIX_EPOCH,
                    task_findings: vec!["shows a receipt".into()],
                    media: vec![MediaObservation {
                        fingerprint: "fp-a".into(),
                        label: "receipt".into(),
                        description: "A printed store receipt.".into(),
                        ocr: Some("TOTAL 12.34".into()),
                        relevant_facts: vec!["total is 12.34".into()],
                        uncertainty: Some("merchant name is blurry".into()),
                    }],
                    cross_media_findings: vec!["only one image".into()],
                    truncated: false,
                },
            }],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        let LlmBlock::Text {
            text,
            cache_control,
        } = &result[0].content[0]
        else {
            panic!("media analysis should become text");
        };
        assert_eq!(cache_control, &None);
        assert!(text.contains("[Media analysis]"));
        assert!(text.contains("question_key: msg-123"));
        assert!(text.contains("shows a receipt"));
        assert!(text.contains("TOTAL 12.34"));
        assert!(text.contains("merchant name is blurry"));
    }

    #[test]
    fn redacted_thinking_replayed_verbatim() {
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::RedactedThinking {
                data: "enc==".to_string(),
            }],
            stop_reason: None,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::RedactedThinking { data } if data == "enc=="
        ));
    }

    #[test]
    fn server_tool_use_replayed_verbatim() {
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::ServerToolUse {
                id: "srvtoolu_01".to_string(),
                name: "web_search".to_string(),
                input: serde_json::json!({"query": "rust"}),
            }],
            stop_reason: None,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ServerToolUse { id, name, input }
                if id == "srvtoolu_01" && name == "web_search" && input["query"] == "rust"
        ));
    }

    #[test]
    fn connector_text_and_advisor_result_replayed_verbatim() {
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ProtoBlock::ConnectorText {
                    connector_text: "hi".to_string(),
                    signature: Some("sig".to_string()),
                },
                ProtoBlock::AdvisorToolResult {
                    tool_use_id: "srvtoolu_01".to_string(),
                    content: serde_json::json!("ok"),
                    is_error: true,
                },
            ],
            stop_reason: None,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::ConnectorText { connector_text, signature }
                if connector_text == "hi" && signature.as_deref() == Some("sig")
        ));
        assert!(matches!(
            &result[0].content[1],
            LlmBlock::AdvisorToolResult { tool_use_id, content, is_error }
                if tool_use_id == "srvtoolu_01" && content == "ok" && *is_error
        ));
    }

    #[test]
    fn multi_block_assistant_message_all_convert() {
        let id = ToolUseId::new();
        let id_str = id.to_string();
        let msg = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ProtoBlock::Text {
                    text: "sure".to_string(),
                },
                ProtoBlock::ToolUse {
                    id,
                    name: "Read".to_string(),
                    input: serde_json::json!({"path": "/x"}),
                    provider_id: None,
                },
            ],
            stop_reason: Some("tool_use".to_string()),
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content.len(), 2);
        assert!(matches!(&result[0].content[0], LlmBlock::Text { text, .. } if text == "sure"));
        assert!(matches!(
            &result[0].content[1],
            LlmBlock::ToolCall { id, name, .. } if id == &id_str && name == "Read"
        ));
    }

    #[test]
    fn empty_messages_vec_returns_empty() {
        let result = to_llm_messages(vec![]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn document_base64_source_decoded_to_bytes() {
        let raw = b"%PDF-1.4";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::Document {
                source: DocumentSource::Base64 {
                    media_type: "application/pdf".to_string(),
                    data: encoded,
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let result = to_llm_messages(vec![msg]).unwrap();
        assert!(matches!(
            &result[0].content[0],
            LlmBlock::Document { media_type, bytes }
                if media_type == "application/pdf" && bytes.as_slice() == raw
        ));
    }

    #[test]
    fn system_message_rejected_with_invalid_request() {
        let msg = ConversationMessage::System {
            id: MessageId::new(),
            content: "you are a helpful assistant".to_string(),
            subtype: None,
            compact_metadata: None,
            refusal_fallback: None,
        };
        let err = to_llm_messages(vec![msg]).unwrap_err();
        assert!(matches!(err, LlmError::InvalidRequest { .. }));
    }

    #[test]
    fn user_and_assistant_roles_mapped_correctly() {
        let user = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::Text {
                text: "hi".to_string(),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let assistant = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::Text {
                text: "hello".to_string(),
            }],
            stop_reason: Some("end_turn".to_string()),
        };
        let result = to_llm_messages(vec![user, assistant]).unwrap();
        assert_eq!(result[0].role, "user");
        assert_eq!(result[1].role, "assistant");
    }

    // ── to_tool_declarations ─────────────────────────────────────────────────

    #[test]
    fn valid_tool_declaration_converts_successfully() {
        let tools = vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
        })];
        let result = to_tool_declarations(tools).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Read");
        assert_eq!(result[0].description, "Read a file");
        assert_eq!(result[0].input_schema["type"], "object");
    }

    #[test]
    fn tool_declaration_missing_name_returns_error() {
        let tools = vec![serde_json::json!({
            "description": "Read a file",
            "input_schema": {"type": "object"}
        })];
        let err = to_tool_declarations(tools).unwrap_err();
        assert!(matches!(err, LlmError::InvalidRequest { message } if message.contains("name")));
    }

    #[test]
    fn tool_declaration_missing_description_returns_error() {
        let tools = vec![serde_json::json!({
            "name": "Read",
            "input_schema": {"type": "object"}
        })];
        let err = to_tool_declarations(tools).unwrap_err();
        assert!(
            matches!(err, LlmError::InvalidRequest { message } if message.contains("description"))
        );
    }

    #[test]
    fn tool_declaration_missing_input_schema_returns_error() {
        let tools = vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file"
        })];
        let err = to_tool_declarations(tools).unwrap_err();
        assert!(
            matches!(err, LlmError::InvalidRequest { message } if message.contains("input_schema"))
        );
    }

    #[test]
    fn tool_declaration_null_input_schema_returns_error() {
        let tools = vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file",
            "input_schema": null
        })];
        let err = to_tool_declarations(tools).unwrap_err();
        assert!(
            matches!(err, LlmError::InvalidRequest { message } if message.contains("input_schema"))
        );
    }

    #[test]
    fn image_bad_base64_returns_invalid_request() {
        let msg = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ProtoBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "not-valid-base64!!!".to_string(),
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let err = to_llm_messages(vec![msg]).unwrap_err();
        assert!(matches!(err, LlmError::InvalidRequest { message } if message.contains("base64")));
    }

    // ── normalize_messages_for_api ───────────────────────────────────────────

    fn user(id: MessageId, text: &str) -> ConversationMessage {
        ConversationMessage::User {
            id,
            content: vec![ProtoBlock::Text {
                text: text.to_string(),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ProtoBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: None,
        }
    }

    fn text_of(blocks: &[ProtoBlock]) -> Vec<&str> {
        blocks
            .iter()
            .map(|b| match b {
                ProtoBlock::Text { text } => text.as_str(),
                _ => panic!("expected text block"),
            })
            .collect()
    }

    #[test]
    fn two_consecutive_users_merge_into_one_keeping_first_id_and_order() {
        let first_id = MessageId::new();
        let out =
            normalize_messages_for_api(vec![user(first_id, "a"), user(MessageId::new(), "b")]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { id, content, .. } => {
                assert_eq!(id, &first_id, "merged message keeps the first message's id");
                // joinTextAtSeam inserts a `\n` on a's last text at a text|text seam.
                assert_eq!(text_of(content), vec!["a\n", "b"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn assistant_separates_users_no_merge() {
        let out = normalize_messages_for_api(vec![
            user(MessageId::new(), "a"),
            assistant("mid"),
            user(MessageId::new(), "b"),
        ]);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0], ConversationMessage::User { .. }));
        assert!(matches!(out[1], ConversationMessage::Assistant { .. }));
        assert!(matches!(out[2], ConversationMessage::User { .. }));
    }

    #[test]
    fn compact_boundary_drops_pre_boundary_history_and_marker() {
        let post_boundary_id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            user(MessageId::new(), "old"),
            ConversationMessage::System {
                id: MessageId::new(),
                content: "Conversation compacted".to_string(),
                subtype: None,
                compact_metadata: None,
                refusal_fallback: None,
            },
            user(post_boundary_id, "summary"),
        ]);
        assert_eq!(out.len(), 1, "pre-boundary history and marker are hidden");
        match &out[0] {
            ConversationMessage::User { id, content, .. } => {
                assert_eq!(id, &post_boundary_id);
                assert_eq!(text_of(content), vec!["summary"]);
            }
            other => panic!("expected post-boundary User, got {other:?}"),
        }
    }

    #[test]
    fn typed_compact_boundary_and_summary_metadata_never_reach_provider_wire() {
        let metadata: protocol::CompactBoundaryMetadata =
            serde_json::from_value(serde_json::json!({"trigger":"manual","preTokens":42})).unwrap();
        let summary =
            ConversationMessage::compact_summary(MessageId::new(), "typed summary".to_string());
        let normalized = normalize_messages_for_api(vec![
            user(MessageId::new(), "old"),
            ConversationMessage::compact_boundary(
                MessageId::new(),
                "localized boundary text".to_string(),
                metadata,
            ),
            summary,
        ]);
        assert_eq!(normalized.len(), 1);
        assert!(normalized[0].is_compact_summary());
        let provider_messages = to_llm_messages(normalized).unwrap();
        let wire = serde_json::to_string(&provider_messages).unwrap();
        assert!(wire.contains("typed summary"));
        assert!(!wire.contains("compact_boundary"));
        assert!(!wire.contains("compactMetadata"));
        assert!(!wire.contains("isCompactSummary"));
        assert!(!wire.contains("isVisibleInTranscriptOnly"));
    }

    #[test]
    fn most_recent_compact_boundary_wins() {
        let out = normalize_messages_for_api(vec![
            user(MessageId::new(), "old"),
            ConversationMessage::System {
                id: MessageId::new(),
                content: "Conversation compacted".to_string(),
                subtype: None,
                compact_metadata: None,
                refusal_fallback: None,
            },
            user(MessageId::new(), "first summary"),
            ConversationMessage::System {
                id: MessageId::new(),
                content: "Conversation compacted".to_string(),
                subtype: None,
                compact_metadata: None,
                refusal_fallback: None,
            },
            user(MessageId::new(), "latest summary"),
        ]);
        assert_eq!(out.len(), 1);
        let ConversationMessage::User { content, .. } = &out[0] else {
            panic!("expected latest summary")
        };
        assert_eq!(text_of(content), vec!["latest summary"]);
    }

    #[test]
    fn single_user_is_unchanged() {
        let id = MessageId::new();
        let out = normalize_messages_for_api(vec![user(id, "solo")]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User {
                id: got, content, ..
            } => {
                assert_eq!(got, &id);
                assert_eq!(text_of(content), vec!["solo"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn three_consecutive_users_merge_into_one_in_order() {
        let first_id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            user(first_id, "a"),
            user(MessageId::new(), "b"),
            user(MessageId::new(), "c"),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { id, content, .. } => {
                assert_eq!(id, &first_id);
                // Each text|text seam (a|b then b|c) gets its own `\n`.
                assert_eq!(text_of(content), vec!["a\n", "b\n", "c"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn assistant_user_user_assistant_merges_only_the_middle_pair() {
        let mid_id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            assistant("start"),
            user(mid_id, "a"),
            user(MessageId::new(), "b"),
            assistant("end"),
        ]);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0], ConversationMessage::Assistant { .. }));
        match &out[1] {
            ConversationMessage::User { id, content, .. } => {
                assert_eq!(id, &mid_id);
                assert_eq!(text_of(content), vec!["a\n", "b"]);
            }
            other => panic!("expected merged User, got {other:?}"),
        }
        assert!(matches!(out[2], ConversationMessage::Assistant { .. }));
    }

    // ── hoistToolResults + joinTextAtSeam (mergeUserMessages pipeline) ────────

    fn user_blocks(id: MessageId, content: Vec<ProtoBlock>) -> ConversationMessage {
        ConversationMessage::User {
            id,
            content,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn tool_result(content: &str) -> ProtoBlock {
        ProtoBlock::ToolResult {
            tool_use_id: ToolUseId::new(),
            content: content.to_string(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }
    }

    fn image() -> ProtoBlock {
        ProtoBlock::Image {
            source: ImageSource::Url {
                url: "https://example.com/i.png".to_string(),
            },
        }
    }

    /// Classify a block as one of a few coarse kinds for order assertions.
    fn kinds(blocks: &[ProtoBlock]) -> Vec<&'static str> {
        blocks
            .iter()
            .map(|b| match b {
                ProtoBlock::Text { .. } => "text",
                ProtoBlock::ToolResult { .. } => "tool_result",
                ProtoBlock::Image { .. } => "image",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn merge_two_text_users_inserts_newline_at_seam() {
        let id = MessageId::new();
        let out = normalize_messages_for_api(vec![user(id, "a"), user(MessageId::new(), "b")]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(text_of(content), vec!["a\n", "b"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn merge_hoists_tool_result_before_text() {
        // [User[Text"hi"], User[ToolResult, Text"after"]] → tool_result leads.
        let id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            user_blocks(
                id,
                vec![ProtoBlock::Text {
                    text: "hi".to_string(),
                }],
            ),
            user_blocks(
                MessageId::new(),
                vec![
                    tool_result("r"),
                    ProtoBlock::Text {
                        text: "after".to_string(),
                    },
                ],
            ),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { content, .. } => {
                // hoist: tool_result first, then the two text blocks in order.
                // No seam `\n` is added because b leads with a non-text block,
                // so the text|text adjacency never occurs at the seam.
                assert_eq!(kinds(content), vec!["tool_result", "text", "text"]);
                assert_eq!(text_of(&content[1..]), vec!["hi", "after"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn merge_toolresult_user_then_image_user_keeps_toolresult_leading() {
        // The real Read-image shape: [User[ToolResult], User[Image]] → [ToolResult, Image].
        let id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            user_blocks(id, vec![tool_result("file bytes")]),
            user_blocks(MessageId::new(), vec![image()]),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(kinds(content), vec!["tool_result", "image"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn hoist_preserves_relative_order_within_groups() {
        // [tr1, txtX, tr2, txtY] across two users → [tr1, tr2, txtX, txtY].
        let id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            user_blocks(
                id,
                vec![
                    tool_result("tr1"),
                    ProtoBlock::Text {
                        text: "X".to_string(),
                    },
                ],
            ),
            user_blocks(
                MessageId::new(),
                vec![
                    tool_result("tr2"),
                    ProtoBlock::Text {
                        text: "Y".to_string(),
                    },
                ],
            ),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(
                    kinds(content),
                    vec!["tool_result", "tool_result", "text", "text"]
                );
                // intra-group order preserved: tr1 before tr2, X before Y.
                match (&content[0], &content[1]) {
                    (
                        ProtoBlock::ToolResult { content: c0, .. },
                        ProtoBlock::ToolResult { content: c1, .. },
                    ) => {
                        assert_eq!(c0, "tr1");
                        assert_eq!(c1, "tr2");
                    }
                    _ => panic!("expected two leading tool_results"),
                }
                assert_eq!(text_of(&content[2..]), vec!["X", "Y"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn seam_no_newline_when_b_leads_with_non_text() {
        // [User[Text"a"], User[ToolResult]] → hoist runs, no seam `\n`.
        let id = MessageId::new();
        let out = normalize_messages_for_api(vec![
            user_blocks(
                id,
                vec![ProtoBlock::Text {
                    text: "a".to_string(),
                }],
            ),
            user_blocks(MessageId::new(), vec![tool_result("r")]),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(kinds(content), vec!["tool_result", "text"]);
                // text block kept its exact bytes — no trailing `\n`.
                assert_eq!(text_of(&content[1..]), vec!["a"]);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    // ── ensure_tool_result_pairing ────────────────────────────────────────────

    fn tu(id: &str) -> ProtoBlock {
        ProtoBlock::ToolUse {
            id: ToolUseId::from(id),
            name: "Read".into(),
            input: serde_json::json!({}),
            provider_id: None,
        }
    }
    fn tr(id: &str) -> ProtoBlock {
        ProtoBlock::ToolResult {
            tool_use_id: ToolUseId::from(id),
            content: "ok".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }
    }
    fn asst_blocks(blocks: Vec<ProtoBlock>) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: blocks,
            stop_reason: None,
        }
    }
    fn usr_blocks(blocks: Vec<ProtoBlock>) -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content: blocks,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    /// Clean turn → strict identity no-op.
    #[test]
    fn pairing_clean_turn_is_noop() {
        let out = ensure_tool_result_pairing(vec![
            usr_blocks(vec![ProtoBlock::Text { text: "go".into() }]),
            asst_blocks(vec![tu("toolu_a")]),
            usr_blocks(vec![tr("toolu_a")]),
        ]);
        assert_eq!(out.len(), 3);
        assert!(
            matches!(&out[1], ConversationMessage::Assistant { content, .. } if content.len() == 1)
        );
        assert!(matches!(&out[2], ConversationMessage::User { content, .. }
            if content.len() == 1 && matches!(content[0], ProtoBlock::ToolResult { .. })));
    }

    /// A tool_use with no matching tool_result → synthetic error result injected.
    #[test]
    fn pairing_missing_result_synthesizes_error() {
        let out = ensure_tool_result_pairing(vec![
            asst_blocks(vec![tu("toolu_x")]),
            usr_blocks(vec![ProtoBlock::Text {
                text: "next".into(),
            }]),
        ]);
        let ConversationMessage::User { content, .. } = &out[1] else {
            panic!("expected user");
        };
        match &content[0] {
            ProtoBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id.as_str(), "toolu_x");
                assert!(*is_error);
                assert_eq!(content, "[Tool result missing due to internal error]");
            }
            other => panic!("expected synthetic tool_result, got {other:?}"),
        }
    }

    /// An orphaned tool_result (no matching tool_use) → stripped.
    #[test]
    fn pairing_orphaned_result_is_stripped() {
        let out = ensure_tool_result_pairing(vec![
            asst_blocks(vec![tu("toolu_a")]),
            usr_blocks(vec![tr("toolu_a"), tr("toolu_ORPHAN")]),
        ]);
        let ConversationMessage::User { content, .. } = &out[1] else {
            panic!("expected user");
        };
        let ids: Vec<&str> = content
            .iter()
            .filter_map(|b| match b {
                ProtoBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["toolu_a"], "orphan stripped: {content:?}");
    }

    /// Leading orphaned tool_result (resume mid-turn, no preceding assistant)
    /// → replaced by the placeholder text.
    #[test]
    fn pairing_leading_orphan_stripped_to_placeholder() {
        let out = ensure_tool_result_pairing(vec![usr_blocks(vec![tr("toolu_gone")])]);
        assert_eq!(out.len(), 1);
        let ConversationMessage::User { content, .. } = &out[0] else {
            panic!("expected user");
        };
        assert!(matches!(&content[0], ProtoBlock::Text { text }
            if text == "[Orphaned tool result removed due to conversation resume]"));
    }

    // ── mergeAssistantMessages + stripAdvisorBlocks (followup) ────────────────

    /// Consecutive `Assistant` messages (the per-content-block streaming lines)
    /// re-collapse to one turn on the wire, keeping the first id and block order.
    #[test]
    fn consecutive_assistants_merge_into_one() {
        let first = MessageId::new();
        let out = normalize_messages_for_api(vec![
            ConversationMessage::Assistant {
                id: first,
                content: vec![ProtoBlock::Text { text: "hi".into() }],
                stop_reason: None,
            },
            asst_blocks(vec![tu("toolu_a")]),
        ]);
        assert_eq!(out.len(), 1, "split assistant lines re-merge: {out:?}");
        match &out[0] {
            ConversationMessage::Assistant { id, content, .. } => {
                assert_eq!(id, &first, "keeps the first line's id");
                assert!(matches!(content[0], ProtoBlock::Text { .. }));
                assert!(matches!(content[1], ProtoBlock::ToolUse { .. }));
            }
            other => panic!("expected merged Assistant, got {other:?}"),
        }
    }

    /// A tool_result `user` line between assistant turns is a real boundary —
    /// the assistants must NOT merge across it.
    #[test]
    fn tool_result_user_separates_assistant_turns_no_merge() {
        let out = normalize_messages_for_api(vec![
            asst_blocks(vec![ProtoBlock::Text { text: "t1".into() }]),
            usr_blocks(vec![tr("toolu_a")]),
            asst_blocks(vec![ProtoBlock::Text { text: "t2".into() }]),
        ]);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0], ConversationMessage::Assistant { .. }));
        assert!(matches!(out[1], ConversationMessage::User { .. }));
        assert!(matches!(out[2], ConversationMessage::Assistant { .. }));
    }

    /// `connector_text` + `advisor_tool_result` are stripped before the wire;
    /// `redacted_thinking` + `server_tool_use` are kept (they round-trip).
    #[test]
    fn advisor_and_connector_stripped_redacted_and_server_kept() {
        let out = normalize_messages_for_api(vec![asst_blocks(vec![
            ProtoBlock::Text { text: "x".into() },
            ProtoBlock::RedactedThinking { data: "op".into() },
            ProtoBlock::ServerToolUse {
                id: "s1".into(),
                name: "web_search".into(),
                input: serde_json::json!({}),
            },
            ProtoBlock::ConnectorText {
                connector_text: "c".into(),
                signature: None,
            },
            ProtoBlock::AdvisorToolResult {
                tool_use_id: "s1".into(),
                content: serde_json::json!({}),
                is_error: false,
            },
        ])]);
        let ConversationMessage::Assistant { content, .. } = &out[0] else {
            panic!("expected assistant");
        };
        assert_eq!(content.len(), 3, "connector+advisor stripped: {content:?}");
        assert!(matches!(content[0], ProtoBlock::Text { .. }));
        assert!(matches!(content[1], ProtoBlock::RedactedThinking { .. }));
        assert!(matches!(content[2], ProtoBlock::ServerToolUse { .. }));
    }

    fn tool_reference_result(names: &[&str]) -> ConversationMessage {
        usr_blocks(vec![ProtoBlock::ToolResult {
            tool_use_id: ToolUseId::from("toolu_search"),
            content: String::new(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: Some(
                names
                    .iter()
                    .map(|name| serde_json::json!({"type": "tool_reference", "tool_name": name}))
                    .collect(),
            ),
        }])
    }

    #[test]
    fn disabled_tool_search_strips_references_with_placeholder() {
        let out = normalize_messages_for_api_with_tool_search(
            vec![tool_reference_result(&["mcp__x__read"])],
            false,
            None,
        );
        let ConversationMessage::User { content, .. } = &out[0] else {
            panic!("expected user");
        };
        let ProtoBlock::ToolResult {
            content_blocks: Some(blocks),
            ..
        } = &content[0]
        else {
            panic!("expected structured tool result");
        };
        assert_eq!(
            blocks,
            &[serde_json::json!({
                "type": "text",
                "text": "[Tool references removed - tool search not enabled]"
            })]
        );
        assert_eq!(content.len(), 1, "disabled mode adds no turn boundary");
    }

    #[test]
    fn enabled_tool_search_filters_unavailable_refs_and_adds_boundary() {
        let available = std::collections::HashSet::from(["mcp__x__read".to_string()]);
        let out = normalize_messages_for_api_with_tool_search(
            vec![tool_reference_result(&["mcp__x__read", "mcp__gone__write"])],
            true,
            Some(&available),
        );
        let ConversationMessage::User { content, .. } = &out[0] else {
            panic!("expected user");
        };
        let ProtoBlock::ToolResult {
            content_blocks: Some(blocks),
            ..
        } = &content[0]
        else {
            panic!("expected structured tool result");
        };
        assert_eq!(
            blocks,
            &[serde_json::json!({
                "type": "tool_reference",
                "tool_name": "mcp__x__read"
            })]
        );
        assert!(matches!(
            &content[1],
            ProtoBlock::Text { text } if text == "Tool loaded."
        ));
    }

    #[test]
    fn historical_tool_reference_aliases_validate_against_canonical_tools() {
        let available = std::collections::HashSet::from([
            "Agent".to_string(),
            "TaskStop".to_string(),
            "TaskOutput".to_string(),
            "SendUserMessage".to_string(),
        ]);
        let out = normalize_messages_for_api_with_tool_search(
            vec![tool_reference_result(&[
                "Task",
                "KillShell",
                "BashOutputTool",
                "Brief",
            ])],
            true,
            Some(&available),
        );
        let ConversationMessage::User { content, .. } = &out[0] else {
            panic!("expected user")
        };
        let ProtoBlock::ToolResult {
            content_blocks: Some(blocks),
            ..
        } = &content[0]
        else {
            panic!("expected structured tool result")
        };
        assert_eq!(blocks.len(), 4, "legacy references must not be stripped");
    }
}
