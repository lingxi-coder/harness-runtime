//! Current native close state for forwarded partial-message frames.
use lingxi_core::host::OutputStream;
use llm_runtime::{ContentBlock, HistoryEvent};

#[derive(Debug, Default)]
pub(crate) struct PartialStreamClose {
    message_open: bool,
    block: Option<(u32, bool)>,
}

impl PartialStreamClose {
    pub(crate) fn observe(&mut self, event: &HistoryEvent) {
        match event {
            HistoryEvent::MessageStart { .. } => {
                self.message_open = true;
                self.block = None;
            }
            HistoryEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let tool = match content_block {
                    ContentBlock::ToolCall { .. } | ContentBlock::ServerToolUse { .. } => true,
                    ContentBlock::ProviderContent { value, .. } => matches!(
                        value.get("type").and_then(serde_json::Value::as_str),
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
    }

    fn take_events(&mut self) -> Vec<String> {
        let state = std::mem::take(self);
        if !state.message_open {
            return Vec::new();
        }
        let mut events = Vec::with_capacity(2);
        if let Some((index, false)) = state.block {
            events.push(format!(
                "{{\"type\":\"content_block_stop\",\"index\":{index}}}"
            ));
        }
        events.push("{\"type\":\"message_stop\"}".to_string());
        events
    }

    /// Close only forwarded frames. Synthetic block stops never reach the
    /// accumulator or executor and cannot complete an unfinished tool input.
    pub(crate) async fn close(&mut self, output: &dyn OutputStream) {
        for event in self.take_events() {
            output.emit_stream_event(&event, false).await;
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
                                input: serde_json::json!({}),
                            },
                        },
                        "mcp_tool_use" => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: ContentBlock::ProviderContent {
                                protocol: "anthropic_messages".into(),
                                value: frame["content_block"].clone(),
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
