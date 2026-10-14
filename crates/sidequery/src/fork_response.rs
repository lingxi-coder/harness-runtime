//! Lossless host history projection for isolated fork continuations.
use crate::side_query::{ForkedSideQueryResponse, SideQueryResponse};
use lingxi_core::types::{ContentBlock, ToolUseId};

pub(crate) fn synthesize(response: SideQueryResponse) -> ForkedSideQueryResponse {
    let candidates: Vec<_> = response.text.iter().cloned().collect();
    let mut assistant_content: Vec<_> = candidates
        .iter()
        .map(|text| ContentBlock::Text {
            text: text.clone(),
            citations: None,
        })
        .collect();
    assistant_content.extend(response.tool_calls.iter().filter_map(|call| {
        Some(ContentBlock::ToolUse {
            id: ToolUseId::from(call.get("id")?.as_str()?.to_owned()),
            name: call.get("name")?.as_str()?.to_owned(),
            input: call
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            input_projection: None,
            provider_id: None,
        })
    }));
    ForkedSideQueryResponse {
        response,
        assistant_content,
        candidates,
        request_id: None,
    }
}

pub(crate) fn translate_response_blocks(
    content: &[llm_runtime::ContentBlock],
) -> Vec<lingxi_core::types::ContentBlock> {
    content
        .iter()
        .filter_map(|b| match b {
            llm_runtime::ContentBlock::ProviderContent { protocol, value } => Some(lingxi_core::types::ContentBlock::ProviderContent { protocol: protocol.clone(), value: value.clone() }),

            llm_runtime::ContentBlock::Text { text, citations, .. } => {
                Some(lingxi_core::types::ContentBlock::Text {
                    text: text.clone(),
                    citations: citations.clone(),
                })
            }
            llm_runtime::ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
                ..
            } => Some(lingxi_core::types::ContentBlock::TextJsUtf16 {
                text: text.clone(),
                utf16_code_units: utf16_code_units.clone(),
                citations: citations.clone(),
            }),
            llm_runtime::ContentBlock::ToolCall { id, name, input, input_projection } => {
                // (cc 2.1.218 `jYd`) Same literal-`\uXXXX` repair the orchestrator
                // applies — a subagent's tool inputs must be normalized too.
                let (input, _stats) =
                    llm_runtime::unicode_repair::repair_tool_input(name, input);
                let mut input_projection = input_projection.clone();
                if let Some(projection) = &mut input_projection {
                    projection.rebase_display_value(input.clone()).expect("valid tool projection remains valid after repair");
                }
                Some(lingxi_core::types::ContentBlock::ToolUse {
                    input_projection,
                    // The provider-issued id IS the canonical ToolUseId (byte
                    // parity with claude-code); the provider_id sidecar stays None.
                    id: lingxi_core::types::ToolUseId::from(id.clone()),
                    name: name.clone(),
                    input,
                    provider_id: None,
                })
            }
            llm_runtime::ContentBlock::Reasoning { text, signature } => {
                Some(lingxi_core::types::ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                })
            }
            // Low-frequency server-side blocks: PRESERVED verbatim for resume/replay
            // byte parity (matches orchestrator::turn_loop::translate_response_blocks).
            llm_runtime::ContentBlock::RedactedThinking { data } => {
                Some(lingxi_core::types::ContentBlock::RedactedThinking { data: data.clone() })
            }
            llm_runtime::ContentBlock::ServerToolUse { id, name, input } => {
                Some(lingxi_core::types::ContentBlock::ServerToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            }
            llm_runtime::ContentBlock::ConnectorText {
                connector_text,
                signature,
            } => Some(lingxi_core::types::ContentBlock::ConnectorText {
                connector_text: connector_text.clone(),
                signature: signature.clone(),
            }),
            llm_runtime::ContentBlock::AdvisorToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(lingxi_core::types::ContentBlock::AdvisorToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            // Input-only / non-output variants remain dropped on the response path.
            llm_runtime::ContentBlock::Image { .. }
            | llm_runtime::ContentBlock::ImageUrl { .. }
            | llm_runtime::ContentBlock::Document { .. }
            | llm_runtime::ContentBlock::ToolResult { .. }
            // cache_edits is a request-only directive — never in a response.
            | llm_runtime::ContentBlock::CacheEdits { .. } => None,
        })
        .collect()
}
