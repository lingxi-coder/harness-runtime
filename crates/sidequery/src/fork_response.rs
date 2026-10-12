//! Lossless host history projection for isolated fork continuations.
use crate::side_query::{ForkedSideQueryResponse, SideQueryResponse};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ContentBlock, ToolUseId};

pub(crate) fn synthesize(response: SideQueryResponse) -> ForkedSideQueryResponse {
    let candidates = response
        .text
        .iter()
        .filter_map(|text| String::from_utf16(&text.string_units("")?).ok())
        .collect();
    let mut assistant_content: Vec<_> = response
        .text
        .iter()
        .map(|text| {
            let units = text
                .string_units("")
                .expect("side-query text is a source string");
            match String::from_utf16(&units) {
                Ok(text) => ContentBlock::Text {
                    text,
                    citations: None,
                },
                Err(_) => ContentBlock::TextJsUtf16 {
                    text: String::from_utf16_lossy(&units),
                    utf16_code_units: units,
                    citations: None,
                },
            }
        })
        .collect();
    assistant_content.extend(response.tool_calls.iter().filter_map(|call| {
        Some(ContentBlock::ToolUse {
            id: ToolUseId::from(call.string_at("/id")?.to_owned()),
            name: call.string_at("/name")?.to_owned(),
            input: call
                .subprojection("/input")
                .unwrap_or_else(|_| Utf16JsonProjection::from(serde_json::Value::Null)),
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
    fn source(
        payload: &llm_runtime::services::sdk::protocol::JsonPayload,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(payload.as_json_source())
            .expect("SDK JSON payload retains valid JSON source")
    }
    fn source_citations(
        value: &Option<Option<llm_runtime::services::sdk::protocol::JsonPayload>>,
    ) -> Option<Option<lingxi_core::types::utf16_json::Utf16JsonProjection>> {
        value.as_ref().map(|value| value.as_ref().map(source))
    }
    content
        .iter()
        .filter_map(|b| match b {
            llm_runtime::ContentBlock::ProviderContent { protocol, value } => Some(lingxi_core::types::ContentBlock::ProviderContent { protocol: protocol.clone(), value: source(value) }),

            llm_runtime::ContentBlock::Text { text, citations, .. } => {
                Some(lingxi_core::types::ContentBlock::Text {
                    text: text.clone(),
                    citations: source_citations(citations),
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
                citations: source_citations(citations),
            }),
            llm_runtime::ContentBlock::ToolCall { id, name, input } => {
                // (cc 2.1.218 `jYd`) Same literal-`\uXXXX` repair the orchestrator
                // applies — a subagent's tool inputs must be normalized too.
                let source_input = source(input);
                let (input, _stats) =
                    llm_runtime::unicode_repair::repair_tool_input(name, &source_input);
                Some(lingxi_core::types::ContentBlock::ToolUse {
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
                    input: source(input),
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
                content: source(content),
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
