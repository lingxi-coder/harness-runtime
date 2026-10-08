//! Invocation-owned native user content admitted by the shared turn driver.
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ImageSource};
use serde_json::Value;

use crate::error::OrchestratorError;

pub(super) struct ProjectedUserContent {
    pub(super) blocks: Vec<ContentBlock>,
    pub(super) text: Utf16JsonProjection,
}

pub(super) fn queued_human_content(
    content: &Utf16JsonProjection,
) -> Result<Vec<ContentBlock>, OrchestratorError> {
    let parsed = ProjectedUserContent::parse(content)?;
    let mut units: Vec<u16> = "The user sent a new message while you were working:\n"
        .encode_utf16()
        .collect();
    units.extend(parsed.text.string_units("").expect("parsed exact text"));
    units.extend("\n\nThis is how Claude Code surfaces messages the user sends mid-turn — within the running turn, often alongside the next tool result, rather than as a separate conversation turn. Address the message above as you continue this turn.".encode_utf16());
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
    let rendered_content = if content.value.is_string() {
        text
    } else {
        let mut block =
            Utf16JsonProjection::plain(serde_json::json!({"type":"text","text":text.value}));
        block.set_pointer("/text", text).map_err(invalid_content)?;
        let mut rendered_blocks = vec![block];
        if let Some(values) = content.value.as_array() {
            for (index, value) in values.iter().enumerate() {
                if value.get("type").and_then(Value::as_str) == Some("image") {
                    rendered_blocks.push(
                        content
                            .subprojection(&format!("/{index}"))
                            .map_err(invalid_content)?,
                    );
                }
            }
        }
        let mut content = Utf16JsonProjection::plain(Value::Array(
            rendered_blocks
                .iter()
                .map(|block| block.value.clone())
                .collect(),
        ));
        for (index, block) in rendered_blocks.into_iter().enumerate() {
            content
                .set_pointer(&format!("/{index}"), block)
                .map_err(invalid_content)?;
        }
        content
    };
    let mut rendered =
        Utf16JsonProjection::plain(serde_json::json!([{"content":rendered_content.value}]));
    rendered
        .set_pointer("/0/content", rendered_content)
        .map_err(invalid_content)?;
    Ok(rendered)
}

impl ProjectedUserContent {
    pub(super) fn parse(content: &Utf16JsonProjection) -> Result<Self, OrchestratorError> {
        content.validate().map_err(invalid_content)?;
        let blocks = match &content.value {
            Value::String(_) => vec![text_block(content.clone(), None)?],
            Value::Array(blocks) => blocks
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    if value.get("type").and_then(Value::as_str) == Some("text") {
                        let text = content
                            .subprojection(&format!("/{index}/text"))
                            .map_err(invalid_content)?;
                        let citations = value
                            .get("citations")
                            .map(|value| (!value.is_null()).then(|| value.clone()));
                        text_block(text, citations)
                    } else {
                        serde_json::from_value(value.clone()).map_err(invalid_content)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(OrchestratorError::Internal(
                    "user content must be a string or content-block array".into(),
                ))
            }
        };
        let text = exact_blocks_text(&blocks);
        Ok(Self { blocks, text })
    }

    pub(super) fn display_text(&self) -> &str {
        self.text
            .value
            .as_str()
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
        _ => Utf16JsonProjection::plain(serde_json::json!("")),
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
    citations: Option<Option<Value>>,
) -> Result<ContentBlock, OrchestratorError> {
    let display = text
        .value
        .as_str()
        .ok_or_else(|| invalid_content("text block text must be a string"))?
        .to_owned();
    match text.strings.first() {
        Some(_) => Ok(ContentBlock::TextJsUtf16 {
            text: display,
            utf16_code_units: text.string_units("").expect("validated string projection"),
            citations,
        }),
        None => Ok(ContentBlock::Text {
            text: display,
            citations,
        }),
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
    use lingxi_core::host::OrchestratorHandle;
    use lingxi_core::types::{ConversationMessage, MessageId};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

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
        assert_eq!(
            rendered.value.pointer("/0/content/1/type"),
            Some(&serde_json::json!("image"))
        );
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
        let wrapped = queued_human_content(&Utf16JsonProjection::plain(serde_json::json!(
            "HEADLESS_MIDTOOL_B"
        )))
        .unwrap();
        let text = match &wrapped[0] {
            ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => text,
            _ => panic!("exact queued text"),
        };
        for capture in captures {
            let messages = capture["providerMessages"][1].as_array().unwrap();
            let system = messages.last().unwrap();
            assert_eq!(system["role"], "system");
            assert_eq!(system["content"][0]["text"], *text);
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
                    &Utf16JsonProjection::plain(serde_json::json!(text)),
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
            assert_eq!(native["content"][0]["text"], *text);
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
        let orch = crate::ConversationOrchestrator::new_with_streaming(
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
        .with_jsonl_writer(writer);
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

        // A subsequent failing turn publishes its own counter and reason;
        // neither comes from accumulated usage or the previous result row.
        assert!(orch
            .run_turn_streaming_with_cancel_projected_content(
                &Utf16JsonProjection::plain(serde_json::json!("next")),
                CancellationToken::new(),
                None,
            )
            .await
            .is_err());
        let metrics = orch.completed_turn_metrics().expect("error owner metrics");
        assert_eq!(metrics.num_turns, 1);
        assert_ne!(metrics.stop_reason.as_deref(), Some("end_turn"));
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
