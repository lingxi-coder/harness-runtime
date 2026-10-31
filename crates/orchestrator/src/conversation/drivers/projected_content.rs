//! Invocation-owned native user content admitted by the shared turn driver.
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ImageSource};

use crate::error::OrchestratorError;

pub(super) struct ProjectedUserContent {
    pub(super) blocks: Vec<ContentBlock>,
    pub(super) text: Utf16JsonProjection,
    /// Root representation of the SDK input before its text was normalized
    /// into `ContentBlock`s. `false` means the original input was an array,
    /// including a one-text-block array.
    pub(super) source_was_json_string: bool,
}

pub(super) fn queued_human_content(
    content: &Utf16JsonProjection,
) -> Result<Vec<ContentBlock>, OrchestratorError> {
    let parsed = ProjectedUserContent::parse(content)?;
    let mut units: Vec<u16> = "The user sent a new message while you were working:\n"
        .encode_utf16()
        .collect();
    units.extend(parsed.text.string_units("").expect("parsed exact text"));
    units.extend(
        format!(
            "\n\nThis is how {} surfaces messages the user sends mid-turn — within the running turn, often alongside the next tool result, rather than as a separate conversation turn. Address the message above as you continue this turn.",
            branding::PRODUCT_NAME
        )
        .encode_utf16(),
    );
    let wrapped = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(invalid_content)?;
    let mut blocks = vec![text_block(wrapped, None)?];
    blocks.extend(
        parsed
            .blocks
            .into_iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. })),
    );
    Ok(blocks)
}

/// Native persisted attachment rendering. The API consumes the unwrapped
/// system body; transcript rendering retains the system-reminder envelope.
pub(super) fn queued_human_rendered(
    content: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, OrchestratorError> {
    let blocks = queued_human_content(content)?;
    let utf16_code_units = match &blocks[0] {
        ContentBlock::Text { text, .. } => text.encode_utf16().collect::<Vec<_>>(),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => utf16_code_units.clone(),
        _ => return Err(invalid_content("queued human text lost its exact carrier")),
    };
    let mut units: Vec<u16> = "<system-reminder>\n".encode_utf16().collect();
    units.extend(utf16_code_units);
    units.extend("\n</system-reminder>".encode_utf16());
    let text = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(invalid_content)?;
    let rendered_content = if content.string_units("").is_some() {
        text
    } else {
        let mut blocks = vec![Utf16JsonProjection::object(vec![
            ("type".into(), "text".into()),
            ("text".into(), text),
        ])
        .map_err(invalid_content)?];
        let count = content
            .array_len("")
            .ok_or_else(|| invalid_content("projected content must be a string or array"))?;
        for index in 0..count {
            let pointer = format!("/{index}");
            if content.string_at(&format!("{pointer}/type")) == Some("image") {
                blocks.push(content.subprojection(&pointer).map_err(invalid_content)?);
            }
        }
        Utf16JsonProjection::array(blocks).map_err(invalid_content)?
    };
    let rendered_user = Utf16JsonProjection::object(vec![("content".into(), rendered_content)])
        .map_err(invalid_content)?;
    Utf16JsonProjection::array(vec![rendered_user]).map_err(invalid_content)
}

impl ProjectedUserContent {
    pub(super) fn parse(content: &Utf16JsonProjection) -> Result<Self, OrchestratorError> {
        content.validate().map_err(invalid_content)?;
        let source_was_json_string = content.string_units("").is_some();
        let blocks = if source_was_json_string {
            vec![text_block(content.clone(), None)?]
        } else if let Some(count) = content.array_len("") {
            (0..count)
                .map(|index| {
                    let pointer = format!("/{index}");
                    if content.string_at(&format!("{pointer}/type")) == Some("text") {
                        let text = content
                            .subprojection(&format!("{pointer}/text"))
                            .map_err(invalid_content)?;
                        let citations =
                            nullable_projection_member(content, &format!("{pointer}/citations"))?;
                        text_block(text, citations)
                    } else {
                        let source = content
                            .subprojection(&pointer)
                            .and_then(|block| block.to_json_string())
                            .map_err(invalid_content)?;
                        serde_json::from_str(&source).map_err(invalid_content)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            return Err(OrchestratorError::Internal(
                "user content must be a string or content-block array".into(),
            ));
        };
        let text = exact_blocks_text(&blocks);
        Ok(Self {
            blocks,
            text,
            source_was_json_string,
        })
    }

    pub(super) fn display_text(&self) -> &str {
        self.text
            .string_at("")
            .expect("projected prompt text is a string")
    }

    pub(super) fn images(&self) -> Vec<ImageSource> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { source } => Some(source.clone()),
                _ => None,
            })
            .collect()
    }

    /// A Mod forwards the original text without collapsing its block layout.
    /// An actual text rewrite replaces the text portion while retaining media.
    pub(super) fn rewrite_text(
        &mut self,
        text: Utf16JsonProjection,
    ) -> Result<(), OrchestratorError> {
        if self.text == text {
            return Ok(());
        }
        let replacement = text_block(text.clone(), None)?;
        let first_text = self.blocks.iter().position(|block| {
            matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::TextJsUtf16 { .. }
            )
        });
        let mut rewritten = Vec::with_capacity(self.blocks.len() + 1);
        let mut replacement = Some(replacement);
        for (index, block) in self.blocks.drain(..).enumerate() {
            if first_text == Some(index) {
                rewritten.push(replacement.take().expect("first replacement"));
            }
            if !matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::TextJsUtf16 { .. }
            ) {
                rewritten.push(block);
            }
        }
        if let Some(replacement) = replacement {
            rewritten.insert(0, replacement);
        }
        self.blocks = rewritten;
        self.text = text;
        Ok(())
    }
}

pub(super) fn exact_message_text(
    message: &lingxi_core::types::ConversationMessage,
) -> Utf16JsonProjection {
    match message {
        lingxi_core::types::ConversationMessage::User { content, .. } => exact_blocks_text(content),
        _ => Utf16JsonProjection::root_string(String::new(), Vec::new())
            .expect("empty string projection is valid"),
    }
}

fn exact_blocks_text(blocks: &[ContentBlock]) -> Utf16JsonProjection {
    let mut units = Vec::new();
    let mut first = true;
    for block in blocks {
        let exact = match block {
            ContentBlock::Text { text, .. } => text.encode_utf16().collect(),
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => utf16_code_units.clone(),
            _ => continue,
        };
        if !first {
            units.push(u16::from(b'\n'));
        }
        first = false;
        units.extend(exact);
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("text blocks provide a valid display projection")
}

fn text_block(
    text: Utf16JsonProjection,
    citations: Option<Option<Utf16JsonProjection>>,
) -> Result<ContentBlock, OrchestratorError> {
    let display = text
        .string_at("")
        .ok_or_else(|| invalid_content("text block text must be a string"))?
        .to_owned();
    let units = text
        .string_units("")
        .expect("validated root string projection");
    if display.encode_utf16().ne(units.iter().copied()) {
        Ok(ContentBlock::TextJsUtf16 {
            text: display,
            utf16_code_units: units,
            citations,
        })
    } else {
        Ok(ContentBlock::Text {
            text: display,
            citations,
        })
    }
}

fn nullable_projection_member(
    source: &Utf16JsonProjection,
    pointer: &str,
) -> Result<Option<Option<Utf16JsonProjection>>, OrchestratorError> {
    let Some(value) = source.at_pointer(pointer) else {
        return Ok(None);
    };
    if value.is_null() {
        Ok(Some(None))
    } else {
        source
            .subprojection(pointer)
            .map(|value| Some(Some(value)))
            .map_err(invalid_content)
    }
}

fn invalid_content(error: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::Internal(format!("invalid projected user content: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
        MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
    };
    use lingxi_core::types::{ConversationMessage, MessageId, SessionId};
    use serde_json::Value;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn app_chat_plain_text_has_the_same_carrier_after_jsonl_replay() {
        let image = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AQID".into(),
        };
        for source in [r#""plain text \ud83e\udd80""#, r#""unpaired \ud800""#] {
            let source = Utf16JsonProjection::parse(source).unwrap();
            for app_chat in [false, true] {
                let id = MessageId::new();
                let message = super::super::user_message_from_projection(
                    id,
                    &source,
                    false,
                    vec![image.clone()],
                    app_chat,
                );
                let ConversationMessage::User { content, .. } = &message else {
                    unreachable!()
                };
                let units = source.string_units("").unwrap();
                assert_eq!(message.id(), id);
                assert_eq!(
                    content[1],
                    ContentBlock::Image {
                        source: image.clone()
                    }
                );
                if app_chat && String::from_utf16(&units).is_ok() {
                    assert!(matches!(&content[0], ContentBlock::Text { text, .. }
                        if text.encode_utf16().eq(units.iter().copied())));
                } else {
                    assert!(
                        matches!(&content[0], ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                        if utf16_code_units == &units)
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn app_chat_plain_turns_checkpoint_exact_live_user_and_assistant_rows() {
        use lingxi_core::host::json_schema_validator::{
            CompiledJsonSchema, JsonSchemaValidation, SchemaWorkerError,
        };
        struct UnusedPlan;
        #[async_trait::async_trait]
        impl CompiledJsonSchema for UnusedPlan {
            async fn validate(
                &self,
                _: &Utf16JsonProjection,
            ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
                panic!("this turn advertises but does not call the tool")
            }
        }
        for (streaming, unpaired, default_prompt) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, false, true),
            (true, false, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let session_id = SessionId::new();
            let directory = root
                .path()
                .join("apps/test-app/agent/agui-v1/chats")
                .join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join(format!("{}.jsonl", session_id.as_uuid()));
            let writer = Arc::new(
                session::JsonlWriter::open_persistent_chat(
                    path,
                    Arc::new(platform_posix::fs::PosixFileSystem::new(
                        root.path().to_path_buf(),
                    )),
                    None,
                )
                .await
                .unwrap(),
            );
            let mut answer_units = "checkpoint answer ".encode_utf16().collect::<Vec<_>>();
            answer_units.push(0xd83e);
            if !unpaired {
                answer_units.push(0xdd80);
            }
            let api = Arc::new(MockApiClient::new(vec![
                crate::test_support::mock_message_response(
                    vec![llm_runtime::ContentBlock::TextJsUtf16 {
                        text: String::from_utf16_lossy(&answer_units),
                        utf16_code_units: answer_units.clone(),
                        cache_control: None,
                        citations: None,
                    }],
                    Some("end_turn"),
                ),
            ]));
            let mut events = vec![
                message_start("assistant", "claude-sonnet-5"),
                content_block_start_text(0),
                text_delta(0, "checkpoint answer "),
            ];
            // A surrogate pair can straddle provider deltas. Canonicalize
            // only after the complete accepted row is available.
            for unit in &answer_units["checkpoint answer ".len()..] {
                events.push(llm_runtime::HistoryEvent::ContentBlockDelta {
                    index: 0,
                    delta: llm_runtime::HistoryContentDelta::TextJsUtf16Delta {
                        text: String::from_utf16_lossy(&[*unit]),
                        utf16_code_units: vec![*unit],
                    },
                });
            }
            events.extend([
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]);
            let stream = Arc::new(MockStreamingApiClient::with_turns(vec![events]));
            let mut tools = tool_api::registry::ToolRegistry::new();
            tools.register_builtin(Arc::new(
                crate::structured_output::StructuredOutputTool::new(
                    Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
                    Arc::new(UnusedPlan),
                    Arc::new(std::sync::Mutex::new(None)),
                ),
            ));
            let orch = crate::ConversationOrchestrator::new_side_chat(
                crate::OrchestratorConfig {
                    system_prompt_override: (!default_prompt).then(|| "Test Chat.".into()),
                    append_system_prompt: Some("Explicit App Skill instructions.".into()),
                    exclude_dynamic_system_prompt_sections: true,
                    ..Default::default()
                },
                api.clone(),
                stream.clone(),
                Arc::new(tools),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_session_id(session_id)
            .with_jsonl_writer(writer.clone())
            .with_persistent_app_chat();
            let orch = crate::ConversationOrchestrator::into_shared(orch);
            assert!(!orch.config.bare);
            if streaming {
                orch.run_turn_streaming_with_cancel(
                    "checkpoint prompt 🦀",
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            } else {
                orch.run_turn("checkpoint prompt 🦀").await.unwrap();
            }
            let (system, messages, tool_names) = if streaming {
                let calls = stream.captured_calls().await;
                assert_eq!(calls.len(), 1);
                (
                    calls[0].system.as_ref().unwrap().display_text(),
                    calls[0].messages.clone(),
                    calls[0]
                        .tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>(),
                )
            } else {
                let requests = api.captured_requests().await;
                assert_eq!(requests.len(), 1);
                let crate::OrchestratorApiRequest::Main(request) = &requests[0] else {
                    panic!("expected the actual main request")
                };
                (
                    request.system.as_ref().unwrap().display_text(),
                    request.messages.clone(),
                    request
                        .tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>(),
                )
            };
            if default_prompt {
                assert!(system.contains("Explicit App Skill instructions."));
            } else {
                assert_eq!(system, "Test Chat.\n\nExplicit App Skill instructions.");
            }
            assert_eq!(tool_names, ["StructuredOutput"]);
            assert_eq!(messages.len(), 1, "ambient context reached the provider");
            let live = orch.snapshot_history().await;
            assert_eq!(live.len(), 2, "ambient context entered canonical history");
            assert!(matches!(&live[0], ConversationMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "checkpoint prompt 🦀")));
            let before_checkpoint = writer
                .capture_persistent_chat_snapshot(false)
                .await
                .unwrap();
            let captured = orch
                .verify_persistent_chat_checkpoint(true)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{error}; streaming={streaming}; live={live:?}; durable={}",
                        before_checkpoint.source()
                    )
                });
            let replayed =
                crate::ConversationOrchestrator::replay_persistent_chat_snapshot(&captured)
                    .unwrap();
            assert_eq!(live, replayed.state.history, "streaming={streaming}");
            assert_eq!(live, replayed.display_history);
            if streaming {
                let written = session::jsonl::route_lines(captured.source());
                let row = written
                    .messages_in_order
                    .iter()
                    .find(|row| row.message_type == "assistant")
                    .unwrap();
                let logical_id = live[1].id().as_uuid().to_string();
                assert_eq!(row.message.string_at("/id"), Some(logical_id.as_str()));
                assert_eq!(row.extra.string_at("/providerMessageId"), Some("assistant"));
            }
            let ConversationMessage::Assistant {
                content,
                stop_reason,
                ..
            } = &live[1]
            else {
                panic!("expected the real assistant row")
            };
            assert_eq!(stop_reason.as_deref(), Some("end_turn"));
            if unpaired {
                assert!(
                    matches!(&content[0], ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                    if utf16_code_units == &answer_units)
                );
            } else {
                assert!(matches!(&content[0], ContentBlock::Text { text, .. }
                    if text.encode_utf16().eq(answer_units.iter().copied())));
            }
        }
    }

    #[tokio::test]
    async fn ordinary_plain_chat_still_collects_ambient_context() {
        let api = Arc::new(MockApiClient::new(vec![
            crate::test_support::mock_message_response(
                vec![llm_runtime::ContentBlock::TextJsUtf16 {
                    text: "ordinary answer 🦀".into(),
                    utf16_code_units: "ordinary answer 🦀".encode_utf16().collect(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            ),
        ]));
        let root = tempfile::tempdir().unwrap();
        let orch = crate::ConversationOrchestrator::new_side_chat(
            crate::OrchestratorConfig {
                system_prompt_override: Some("Test Chat.".into()),
                ..Default::default()
            },
            api.clone(),
            Arc::new(MockStreamingApiClient::empty()),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        );
        orch.run_turn("ordinary prompt").await.unwrap();
        assert!(api.captured_msgs().await[0]
            .iter()
            .any(|message| matches!(message, ConversationMessage::User { is_meta: true, .. })));
        let live = orch.snapshot_history().await;
        assert!(matches!(&live[0], ConversationMessage::User { content, .. }
            if matches!(&content[0], ContentBlock::TextJsUtf16 { .. })));
        assert!(live
            .iter()
            .any(|message| matches!(message, ConversationMessage::User { is_meta: true, .. })));
        assert!(
            matches!(live.last(), Some(ConversationMessage::Assistant { content, .. })
            if matches!(&content[0], ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                if utf16_code_units.iter().copied().eq("ordinary answer 🦀".encode_utf16())))
        );
    }

    #[test]
    fn projected_user_blocks_keep_utf16_and_media_order() {
        let incoming = Utf16JsonProjection::parse(
            r#"[{"type":"text","text":"before\ud800"},{"type":"image","source":{"type":"url","url":"https://example.test/image.png"}},{"type":"text","text":"after\udfff"}]"#,
        ).unwrap();
        let content = ProjectedUserContent::parse(&incoming).unwrap();
        assert!(
            matches!(&content.blocks[0], ContentBlock::TextJsUtf16 { utf16_code_units, .. } if utf16_code_units.last()==Some(&0xd800))
        );
        assert!(matches!(&content.blocks[1], ContentBlock::Image { .. }));
        assert!(
            matches!(&content.blocks[2], ContentBlock::TextJsUtf16 { utf16_code_units, .. } if utf16_code_units.last()==Some(&0xdfff))
        );
        assert_eq!(
            content.text.to_json_string().unwrap(),
            r#""before\ud800\nafter\udfff""#
        );
    }

    #[test]
    fn queued_human_rendering_keeps_exact_text_and_media_per_delivery() {
        let content = Utf16JsonProjection::parse(r#"[{"type":"text","text":"first\ud800"},{"type":"image","source":{"type":"url","url":"https://example.test/image.png"}},{"type":"text","text":"last\udfff"}]"#).unwrap();
        let wrapped = queued_human_content(&content).unwrap();
        assert_eq!(wrapped.len(), 2);
        assert!(matches!(&wrapped[1], ContentBlock::Image { .. }));
        let ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } = &wrapped[0]
        else {
            panic!("lost exact queued text")
        };
        assert!(utf16_code_units
            .windows(3)
            .any(|units| units == [0xd800, 10, b'l' as u16]));
        assert!(utf16_code_units.contains(&0xdfff));
        let rendered = queued_human_rendered(&content).unwrap();
        let exact = rendered
            .subprojection("/0/content/0/text")
            .unwrap()
            .to_json_string()
            .unwrap();
        assert!(exact.contains(r"first\ud800\nlast\udfff"));
        assert_eq!(rendered.string_at("/0/content/1/type"), Some("image"));
    }

    #[test]
    fn queued_human_wrapper_matches_both_pinned_native_requests() {
        let receipt: Value = serde_json::from_str(include_str!(
            "../../../../../scripts/tests/headless-fixtures/native-2.1.293-input-queue-probes.json"
        ))
        .unwrap();
        let cases = receipt["cases"].as_object().unwrap();
        let captures = cases
            .iter()
            .find(|(name, _)| name.contains("midtool"))
            .unwrap()
            .1
            .as_array()
            .unwrap();
        assert_eq!(captures.len(), 2);
        let wrapped = queued_human_content(
            &Utf16JsonProjection::from_serde_value(&serde_json::json!("HEADLESS_MIDTOOL_B"))
                .unwrap(),
        )
        .unwrap();
        let text = match &wrapped[0] {
            ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => text,
            _ => panic!("exact queued text"),
        };
        for capture in captures {
            let messages = capture["providerMessages"][1].as_array().unwrap();
            let system = messages.last().unwrap();
            assert_eq!(system["role"], "system");
            let native_text = system["content"][0]["text"]
                .as_str()
                .expect("native queued-message text");
            let expected = native_text.replace("Claude Code", branding::PRODUCT_NAME);
            assert_eq!(expected, *text);
            assert_eq!(capture["observation"]["resultCount"], 1);
        }
    }

    #[test]
    fn per_delivery_projection_matches_both_native_multifold_requests() {
        let receipt: Value = serde_json::from_str(include_str!(
            "../../../../../scripts/tests/headless-fixtures/native-2.1.293-input-multifold-probes.json"
        )).unwrap();
        let messages = ["HEADLESS_MULTIFOLD_B", "HEADLESS_MULTIFOLD_C"]
            .into_iter()
            .map(|text| {
                crate::ConversationOrchestrator::queued_human_attachment_projection(
                    MessageId::new(),
                    &Utf16JsonProjection::from_serde_value(&serde_json::json!(text)).unwrap(),
                )
                .unwrap()
            })
            .collect();
        let model = llm_runtime::convert::to_llm_messages(
            llm_runtime::convert::normalize_messages_for_api(messages),
        )
        .unwrap();
        assert_eq!(model.len(), 1);
        assert_eq!(model[0].role, "system");
        assert_eq!(model[0].content.len(), 1);
        let text = match &model[0].content[0] {
            llm_runtime::ContentBlock::Text { text, .. }
            | llm_runtime::ContentBlock::TextJsUtf16 { text, .. } => text,
            _ => panic!("native merged system text"),
        };
        let runs = receipt["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        for run in runs {
            let native = run["providerMessages"][1]
                .as_array()
                .unwrap()
                .last()
                .unwrap();
            assert_eq!(native["role"], "system");
            let native_text = native["content"][0]["text"]
                .as_str()
                .expect("native multifold text");
            let expected = native_text.replace("Claude Code", branding::PRODUCT_NAME);
            assert_eq!(expected, *text);
        }
    }

    #[test]
    fn mod_forward_keeps_blocks_and_a_rewrite_replaces_only_text() {
        let incoming = Utf16JsonProjection::parse(
            r#"[{"type":"text","text":"before\ud800"},{"type":"image","source":{"type":"url","url":"https://example.test/image.png"}},{"type":"text","text":"after"}]"#,
        ).unwrap();
        let mut content = ProjectedUserContent::parse(&incoming).unwrap();
        content.rewrite_text(content.text.clone()).unwrap();
        assert_eq!(content.blocks.len(), 3);
        content
            .rewrite_text(Utf16JsonProjection::parse(r#""changed\udfff""#).unwrap())
            .unwrap();
        assert_eq!(content.blocks.len(), 2);
        assert!(
            matches!(&content.blocks[0], ContentBlock::TextJsUtf16 { utf16_code_units, .. } if utf16_code_units.last()==Some(&0xdfff))
        );
        assert!(matches!(&content.blocks[1], ContentBlock::Image { .. }));
    }

    #[tokio::test]
    async fn projected_user_turn_reaches_existing_model_driver_and_exact_jsonl() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript.jsonl");
        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("assistant", "claude-sonnet-5"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let orch = crate::ConversationOrchestrator::into_shared(
            crate::ConversationOrchestrator::new_with_streaming(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                stream.clone(),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_jsonl_writer(writer),
        );
        let incoming = Utf16JsonProjection::parse(
            r#"[{"type":"text","text":"first\ud800"},{"type":"text","text":"second"}]"#,
        )
        .unwrap();
        let id = MessageId::new();
        orch.run_turn_streaming_with_cancel_projected_content(
            &incoming,
            CancellationToken::new(),
            Some(id),
        )
        .await
        .unwrap();
        let metrics = orch
            .completed_turn_metrics()
            .expect("completed owner metrics");
        assert_eq!(metrics.num_turns, 1);
        assert_eq!(metrics.stop_reason.as_deref(), Some("end_turn"));
        let calls = stream.captured_calls().await;
        assert_eq!(calls.len(), 1);
        let user = calls[0]
            .messages
            .iter()
            .find(|message| message.id() == id)
            .unwrap();
        let ConversationMessage::User { content, .. } = user else {
            panic!("expected user input")
        };
        assert_eq!(content.len(), 2);
        assert!(
            matches!(&content[0], ContentBlock::TextJsUtf16 { utf16_code_units, .. } if utf16_code_units.last()==Some(&0xd800))
        );
        let rows = std::fs::read_to_string(path).unwrap();
        assert!(rows.contains(r#""text":"first\ud800""#), "{rows}");
        let history = orch.snapshot_history().await;
        assert!(history.iter().any(|message| message.id() == id));

        // A generic connect error is surfaced as a model-error message and a
        // graceful turn completion. Its metrics must replace the prior turn's
        // end_turn reason and counter.
        let outcome = orch
            .run_turn_streaming_with_cancel_projected_content(
                &Utf16JsonProjection::from_serde_value(&serde_json::json!("next")).unwrap(),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("generic connect error completes through the model-error path");
        assert_eq!(outcome, crate::TurnOutcome::EndTurn);
        let metrics = orch.completed_turn_metrics().expect("error owner metrics");
        assert_eq!(metrics.num_turns, 1);
        assert_eq!(metrics.stop_reason.as_deref(), Some("model_error"));
    }

    #[tokio::test]
    async fn externally_persisted_chat_user_rows_are_not_appended_twice() {
        let root = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let path = root.path().join(format!("{}.jsonl", session_id.as_uuid()));
        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![
            crate::scripted![
                message_start("assistant-one", "claude-sonnet-5"),
                content_block_start_text(0),
                text_delta(0, "first answer"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ],
            crate::scripted![
                message_start("assistant-two", "claude-sonnet-5"),
                content_block_start_text(0),
                text_delta(0, "second answer"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ],
        ]));
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let orch = crate::ConversationOrchestrator::into_shared(
            crate::ConversationOrchestrator::new_with_streaming(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                stream,
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_jsonl_writer(writer)
            .with_session_id(session_id),
        );

        let first_id = MessageId::new();
        let first = Utf16JsonProjection::from_serde_value(&serde_json::json!([
            {"type":"text","text":"first prompt"}
        ]))
        .unwrap();
        orch.append_external_history_message(ConversationMessage::user(
            first_id,
            "first prompt".into(),
        ))
        .await;
        assert_eq!(count_user_rows(&path, first_id), 1);
        orch.run_turn_streaming_with_cancel_projected_content(
            &first,
            CancellationToken::new(),
            Some(first_id),
        )
        .await
        .unwrap();

        let second_id = MessageId::new();
        let second = Utf16JsonProjection::from_serde_value(&serde_json::json!([
            {"type":"text","text":"second prompt"}
        ]))
        .unwrap();
        orch.append_external_history_message(ConversationMessage::user(
            second_id,
            "second prompt".into(),
        ))
        .await;
        assert_eq!(count_user_rows(&path, first_id), 1);
        assert_eq!(count_user_rows(&path, second_id), 1);
        orch.run_turn_streaming_with_cancel_projected_content(
            &second,
            CancellationToken::new(),
            Some(second_id),
        )
        .await
        .unwrap();

        // A failed provider request must leave the already-persisted user row
        // intact without sending a second copy through the ordinary turn path.
        let third_id = MessageId::new();
        let third = Utf16JsonProjection::from_serde_value(&serde_json::json!([
            {"type":"text","text":"provider failure prompt"}
        ]))
        .unwrap();
        orch.append_external_history_message(ConversationMessage::user(
            third_id,
            "provider failure prompt".into(),
        ))
        .await;
        assert_eq!(count_user_rows(&path, third_id), 1);
        assert_eq!(
            orch.run_turn_streaming_with_cancel_projected_content(
                &third,
                CancellationToken::new(),
                Some(third_id),
            )
            .await
            .unwrap(),
            crate::TurnOutcome::EndTurn
        );

        assert_eq!(count_user_rows(&path, first_id), 1);
        assert_eq!(count_user_rows(&path, second_id), 1);
        assert_eq!(count_user_rows(&path, third_id), 1);
        let replayed = crate::resume::replay_session_state_from_path(
            &path,
            session_id.as_uuid(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        )
        .await
        .unwrap();
        for id in [first_id, second_id, third_id] {
            assert_eq!(
                replayed
                    .display_history
                    .iter()
                    .filter(|row| {
                        row.id() == id && row.role() == lingxi_core::types::MessageRole::User
                    })
                    .count(),
                1,
                "cold replay should retain one user row for {id}"
            );
        }
    }

    fn count_user_rows(path: &std::path::Path, id: MessageId) -> usize {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|row| row["type"] == "user" && row["uuid"] == id.as_uuid().to_string())
            .count()
    }

    #[tokio::test]
    async fn sdk_projected_single_text_block_keeps_jsonl_array_shape() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript.jsonl");
        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("assistant", "claude-sonnet-5"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let orch = crate::ConversationOrchestrator::into_shared(
            crate::ConversationOrchestrator::new_with_streaming(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                stream,
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_jsonl_writer(writer),
        );
        let incoming =
            Utf16JsonProjection::parse(r#"[{"type":"text","text":"one block"}]"#).unwrap();

        crate::native_user_message_source::with_sdk_source(
            orch.run_turn_streaming_with_cancel_projected_content(
                &incoming,
                CancellationToken::new(),
                None,
            ),
        )
        .await
        .unwrap();

        let raw = std::fs::read_to_string(path).unwrap();
        let row: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(row["promptSource"], "sdk");
        let content = row["message"]["content"]
            .as_array()
            .expect("projected SDK content retains its original array shape");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "one block");
    }

    #[tokio::test]
    async fn cancelled_projected_admission_does_not_wait_for_or_change_a_running_turn() {
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let stream = Arc::new(MockStreamingApiClient::empty());
        let orch = crate::ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig::default(),
            api.clone(),
            stream.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let _owner = orch.turn_gate.lock().await;
        let cancel = CancellationToken::new();
        let incoming = Utf16JsonProjection::parse(r#""queued\ud800""#).unwrap();
        let waiter =
            orch.run_turn_streaming_with_cancel_projected_content(&incoming, cancel.clone(), None);
        tokio::pin!(waiter);
        tokio::select! {
            biased;
            result = &mut waiter => panic!("waiter bypassed owner: {result:?}"),
            () = tokio::task::yield_now() => {},
        }
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, crate::TurnOutcome::Cancelled));
        assert!(orch.snapshot_history().await.is_empty());
        assert!(stream.captured_calls().await.is_empty());
        assert!(api.captured_msgs().await.is_empty());
    }
}
