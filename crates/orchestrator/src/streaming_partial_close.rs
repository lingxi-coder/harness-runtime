//! Current native close state for forwarded partial-message frames.
use lingxi_core::host::orchestrator::{AbandonedStreamBlocks, PartialStreamEventMetadata};
use lingxi_core::host::OutputStream;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use llm_runtime::{ContentBlock, HistoryEvent};

#[derive(Debug, Default)]
pub(crate) struct PartialStreamClose {
    message_open: bool,
    block: Option<(u32, bool)>,
    api_message_id: Option<Utf16JsonProjection>,
    next_block_index: u64,
    thinking_display: Option<String>,
}

impl PartialStreamClose {
    pub(crate) fn observe_native_frame(
        &mut self,
        event: &lingxi_llm_client::protocol::JsonPayload,
        thinking_display: Option<&str>,
    ) -> PartialStreamEventMetadata {
        let kind = event.string_at("/type");
        let active = self.message_open || kind.as_deref() == Some("message_start");
        match kind.as_deref() {
            Some("message_start") => {
                self.message_open = true;
                self.block = None;
                self.api_message_id = event
                    .payload_at("/message/id")
                    .and_then(|id| id.root_string_code_units())
                    .filter(|units| !units.is_empty())
                    .map(|units| {
                        Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                            .expect("provider message id owns its exact units")
                    });
                self.thinking_display = thinking_display.map(str::to_owned);
            }
            Some("content_block_start") => {
                self.block = event
                    .unsigned_integer_at("/index")
                    .and_then(|index| u32::try_from(index).ok())
                    .map(|index| {
                        let tool = matches!(
                            event.string_at("/content_block/type").as_deref(),
                            Some("tool_use" | "server_tool_use" | "mcp_tool_use")
                        );
                        (index, tool)
                    });
            }
            Some("content_block_stop") => self.block = None,
            Some("message_stop") => {
                self.message_open = false;
                self.block = None;
            }
            _ => {}
        }
        let marks_thinking = kind.as_deref() == Some("message_start")
            || kind.as_deref() == Some("content_block_start")
                && event.string_at("/content_block/type").as_deref() == Some("thinking")
            || kind.as_deref() == Some("content_block_delta")
                && event.string_at("/delta/type").as_deref() == Some("thinking_delta");
        PartialStreamEventMetadata {
            api_message_id: active.then(|| self.api_message_id.clone()).flatten(),
            thinking_display: marks_thinking
                .then(|| self.thinking_display.clone())
                .flatten(),
            ..Default::default()
        }
    }

    pub(crate) fn observe(&mut self, event: &HistoryEvent) -> PartialStreamEventMetadata {
        let active = self.message_open || matches!(event, HistoryEvent::MessageStart { .. });
        match event {
            HistoryEvent::MessageStart { response } => {
                self.message_open = true;
                self.block = None;
                self.api_message_id = (!response.id.is_empty())
                    .then(|| Utf16JsonProjection::from(response.id.clone()));
                self.thinking_display = response
                    .provider_metadata
                    .string_at("/llm_client/thinking_display");
            }
            HistoryEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let tool = match content_block {
                    ContentBlock::ToolCall { .. } | ContentBlock::ServerToolUse { .. } => true,
                    ContentBlock::ProviderContent { value, .. } => matches!(
                        value.string_at("/type").as_deref(),
                        Some("tool_use" | "server_tool_use" | "mcp_tool_use")
                    ),
                    _ => false,
                };
                self.block = Some((*index, tool));
            }
            HistoryEvent::ContentBlockStop { .. } => self.block = None,
            HistoryEvent::MessageStop | HistoryEvent::Completed { .. } => {
                self.message_open = false;
                self.block = None;
            }
            _ => {}
        }
        PartialStreamEventMetadata {
            api_message_id: active.then(|| self.api_message_id.clone()).flatten(),
            thinking_display: if matches!(
                event,
                HistoryEvent::MessageStart { .. }
                    | HistoryEvent::ContentBlockStart {
                        content_block: ContentBlock::Reasoning { .. },
                        ..
                    }
                    | HistoryEvent::ContentBlockDelta {
                        delta: llm_runtime::HistoryContentDelta::ThinkingDelta { .. },
                        ..
                    }
            ) {
                self.thinking_display.clone()
            } else {
                None
            },
            ..Default::default()
        }
    }

    pub(crate) fn record_completed_block(&mut self, index: u32) {
        self.next_block_index = self.next_block_index.max(u64::from(index) + 1);
    }

    fn take_frames(&mut self, unfinished: bool) -> Vec<(String, PartialStreamEventMetadata)> {
        if !self.message_open {
            return Vec::new();
        }
        self.message_open = false;
        let block = self.block.take();
        let mut events = Vec::with_capacity(2);
        let mut metadata = PartialStreamEventMetadata {
            api_message_id: self.api_message_id.clone(),
            ..Default::default()
        };
        if let Some((index, false)) = block {
            events.push((
                format!("{{\"type\":\"content_block_stop\",\"index\":{index}}}"),
                metadata.clone(),
            ));
        }
        if unfinished {
            metadata.abandoned_blocks =
                self.api_message_id
                    .as_ref()
                    .map(|id| AbandonedStreamBlocks {
                        api_message_id: id.clone(),
                        from_block_index: self.next_block_index,
                    });
        }
        events.push(("{\"type\":\"message_stop\"}".to_string(), metadata));
        events
    }

    #[cfg(test)]
    fn take_events(&mut self) -> Vec<String> {
        self.take_frames(true)
            .into_iter()
            .map(|(event, _)| event)
            .collect()
    }

    /// Close only forwarded frames. Synthetic block stops never reach the
    /// accumulator or executor and cannot complete an unfinished tool input.
    pub(crate) async fn close(&mut self, output: &dyn OutputStream) {
        self.close_with_state(output, true).await;
    }

    pub(crate) async fn close_complete(&mut self, output: &dyn OutputStream) {
        self.close_with_state(output, false).await;
    }

    async fn close_with_state(&mut self, output: &dyn OutputStream, unfinished: bool) {
        for (event, metadata) in self.take_frames(unfinished) {
            output
                .emit_stream_event_with_metadata(&event, false, &metadata)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support_stream::{
        content_block_start_text, content_block_start_thinking, content_block_start_tool_use,
        content_block_stop, message_delta_stop, message_start, message_stop,
    };
    use serde_json::Value;

    #[test]
    fn native_319_close_identity_abandonment_and_event_order_match_source() {
        let oracle = Utf16JsonProjection::parse(include_str!(
            "../tests/fixtures/partial_stream_2_1_319.json"
        ))
        .unwrap();
        for index in 0..oracle.array_len("/cases").unwrap() {
            // These original close vectors contain ordinary scalar IDs. Exact
            // non-scalar identity vectors below stay in Source throughout.
            let case: Value = serde_json::from_str(
                &oracle
                    .subprojection(&format!("/cases/{index}"))
                    .unwrap()
                    .to_json_string()
                    .unwrap(),
            )
            .unwrap();
            let mut state = PartialStreamClose::default();
            for frame in case["events"].as_array().unwrap() {
                let index = frame["index"].as_u64().unwrap_or_default() as u32;
                let event = match frame["type"].as_str().unwrap() {
                    "message_start" => {
                        message_start(frame["message"]["id"].as_str().unwrap(), "model")
                    }
                    "content_block_start" => match frame["content_block"]["type"].as_str().unwrap()
                    {
                        "text" => content_block_start_text(index),
                        "thinking" => content_block_start_thinking(index),
                        "tool_use" => content_block_start_tool_use(index, "tool".into(), "Read"),
                        _ => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: ContentBlock::ProviderContent {
                                protocol: "anthropic_messages".into(),
                                value:
                                    lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                                        &frame["content_block"],
                                    )
                                    .unwrap(),
                            },
                        },
                    },
                    "content_block_stop" => content_block_stop(index),
                    "message_stop" => message_stop(),
                    other => panic!("unknown native close event {other}"),
                };
                state.observe(&event);
                if frame["type"] == "content_block_stop" {
                    state.record_completed_block(index);
                }
            }
            let actual = state.take_frames(case["unfinished"].as_bool().unwrap());
            let expected = case["expected"].as_array().unwrap();
            assert_eq!(actual.len(), expected.len(), "{}", case["name"]);
            for ((event, metadata), row) in actual.iter().zip(expected) {
                assert_eq!(serde_json::from_str::<Value>(event).unwrap(), row["event"]);
                assert_eq!(
                    metadata
                        .api_message_id
                        .as_ref()
                        .and_then(|id| id.string_at("")),
                    row["apiMessageId"].as_str()
                );
                assert_eq!(
                    metadata
                        .abandoned_blocks
                        .as_ref()
                        .map(|value| value.from_block_index),
                    row["abandonedBlocks"]["fromBlockIndex"].as_u64()
                );
            }
            assert!(state.take_frames(true).is_empty());
        }
    }

    #[test]
    fn thinking_display_comes_from_the_request_snapshot_and_uses_native_event_gates() {
        let mut state = PartialStreamClose::default();
        let mut start = message_start("message", "model");
        if let HistoryEvent::MessageStart { response } = &mut start {
            response.provider_metadata = lingxi_llm_client::protocol::JsonPayload::parse(
                r#"{"llm_client":{"thinking_display":"updates"}}"#,
            )
            .unwrap();
        }
        assert_eq!(
            state.observe(&start).thinking_display.as_deref(),
            Some("updates")
        );
        assert_eq!(
            state
                .observe(&content_block_start_thinking(0))
                .thinking_display
                .as_deref(),
            Some("updates")
        );
        assert!(state
            .observe(&content_block_start_text(1))
            .thinking_display
            .is_none());
        assert!(state.observe(&message_stop()).thinking_display.is_none());
    }

    #[test]
    fn native_319_message_identity_keeps_lone_units_distinct_from_replacement_text() {
        let oracle = Utf16JsonProjection::parse(include_str!(
            "../tests/fixtures/partial_stream_2_1_319.json"
        ))
        .unwrap();
        let mut identities = Vec::new();
        for index in 0..oracle.array_len("/sourceIdentityCases").unwrap() {
            let case = oracle
                .subprojection(&format!("/sourceIdentityCases/{index}"))
                .unwrap();
            let events = lingxi_llm_client::protocol::JsonPayload::parse(
                case.string_at("/eventsSource").unwrap(),
            )
            .unwrap();
            let expected =
                Utf16JsonProjection::parse(case.string_at("/expectedInternalJson").unwrap())
                    .unwrap();
            let mut state = PartialStreamClose::default();
            for event in 0..events.array_len("").unwrap() {
                state.observe_native_frame(&events.array_item(event).unwrap(), None);
            }
            let frames = state.take_frames(true);
            for (frame_index, (_, metadata)) in frames.iter().enumerate() {
                let units = metadata
                    .api_message_id
                    .as_ref()
                    .unwrap()
                    .string_units("")
                    .unwrap();
                assert_eq!(
                    expected.string_units(&format!("/{frame_index}/apiMessageId")),
                    Some(units.clone())
                );
                if let Some(abandoned) = &metadata.abandoned_blocks {
                    assert_eq!(
                        abandoned.api_message_id.string_units(""),
                        Some(units.clone())
                    );
                    assert_eq!(
                        expected
                            .string_units(&format!("/{frame_index}/abandonedBlocks/apiMessageId")),
                        Some(units)
                    );
                }
            }
            identities.push(
                frames[0]
                    .1
                    .api_message_id
                    .as_ref()
                    .unwrap()
                    .string_units("")
                    .unwrap(),
            );
        }
        assert_eq!(identities.len(), 2);
        assert_eq!(identities[0].last(), Some(&0xd800));
        assert_eq!(identities[1].last(), Some(&0xfffd));
        assert_ne!(identities[0], identities[1]);
    }

    #[test]
    fn forwarded_partial_close_matches_actual_native_287_states_and_json_bytes() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/partial_stream_close_2_1_287.json"
        ))
        .unwrap();
        let cases = oracle["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 14);
        for case in cases {
            let mut state = PartialStreamClose::default();
            for frame in case["events"].as_array().unwrap() {
                let index = frame["index"].as_u64().unwrap_or(0) as u32;
                let event = match frame["type"].as_str().unwrap() {
                    "message_start" => message_start("fixture", "fixture-model"),
                    "content_block_start" => match frame["content_block"]["type"].as_str().unwrap()
                    {
                        "text" => content_block_start_text(index),
                        "thinking" => content_block_start_thinking(index),
                        "tool_use" => content_block_start_tool_use(index, "tool-1".into(), "Read"),
                        "redacted_thinking" => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: ContentBlock::RedactedThinking {
                                data: "opaque".into(),
                            },
                        },
                        "server_tool_use" => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: ContentBlock::ServerToolUse {
                                id: "server-1".into(),
                                name: "web_search".into(),
                                input:
                                    lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                                        &serde_json::json!({}),
                                    )
                                    .unwrap(),
                            },
                        },
                        "mcp_tool_use" => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: ContentBlock::ProviderContent {
                                protocol: "anthropic_messages".into(),
                                value:
                                    lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                                        &frame["content_block"],
                                    )
                                    .unwrap(),
                            },
                        },
                        unknown => panic!("uncovered actual native kind {unknown}"),
                    },
                    "content_block_stop" => content_block_stop(index),
                    "message_delta" => {
                        message_delta_stop(frame["delta"]["stop_reason"].as_str().unwrap())
                    }
                    "message_stop" => message_stop(),
                    unknown => panic!("uncovered actual native event {unknown}"),
                };
                state.observe(&event);
            }
            assert_eq!(
                serde_json::json!(state.take_events()),
                case["expected"]["eventJson"],
                "{}",
                case["name"]
            );
            assert!(state.take_events().is_empty(), "{}", case["name"]);
        }
    }
}
