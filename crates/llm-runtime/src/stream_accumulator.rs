//! Drive a streaming [`HistoryEvent`] sequence to a single [`HistoryResponse`] — the
//! exact value the non-streaming round-trip returns — so a caller's downstream
//! logic is byte-identical regardless of which transport produced the turn.
//!
//! Byte-locked against `orchestrator::sse`'s `BlockAccumulator` +
//! `event_router` (source semantics: `claude.ts:1995-2300`). It lived in the
//! `agent` crate because that crate cannot depend on the orchestrator without
//! closing a dependency cycle; it now lives HERE, next to the `HistoryEvent` and
//! `HistoryResponse` it is defined in terms of, so every consumer of a stream gets
//! the same assembly instead of growing a second, weaker one. `agent`
//! re-exports it and is otherwise unchanged.
//!
//! [`response_to_stream_events`] is the inverse: it synthesizes a lossless
//! [`HistoryEvent`] sequence from an `HistoryResponse`, so a client that only
//! implements the non-streaming round-trip can still present a streaming seam.
//! The two round-trip exactly (see the `round_trip_*` tests).
#![forbid(unsafe_code)]

use crate::{
    ContentBlock, ExecutionUsage, HistoryContentDelta, HistoryEvent, HistoryMessageDelta,
    HistoryResponse, LlmError, UsageState,
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::Value;
use std::collections::HashMap;

fn projected_response_model(metadata: &Value) -> Option<&str> {
    metadata.get("llm_client")?.get("response_model")?.as_str()
}

// The SDK owns text/signature/JSON assembly. This map only remembers the host
// presentation kind and lifecycle so invalid UI event sequences fail locally.
#[derive(Debug, Clone)]
enum BlockKind {
    Text,
    ToolCall {
        input_projection: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        id: String,
        name: String,
        server: Option<Value>,
    },
    Reasoning,
    PreservedText(ContentBlock),
    Preserved(ContentBlock),
    Other,
}

#[derive(Debug, Default)]
struct BlockAccumulator {
    sdk: lingxi_llm_client::stream_assembly::StreamAccumulator,
    kinds: HashMap<u32, BlockKind>,
    text_citations: HashMap<u32, Option<Option<Value>>>,
    tool_calls_started: usize,
    tool_deltas: std::collections::HashSet<u32>,
}
impl BlockAccumulator {
    fn new() -> Self {
        Self::default()
    }
    fn observe(&mut self, event: lingxi_llm_client::protocol::StreamEvent) {
        self.sdk.observe(&event);
    }
    fn start_block(&mut self, index: u32, kind: BlockKind) {
        use lingxi_llm_client::protocol::{StreamEvent, ToolUseId};
        match &kind {
            BlockKind::Text => self.observe(StreamEvent::TextDelta {
                block: index as usize,
                text: String::new(),
            }),
            BlockKind::Reasoning => self.observe(StreamEvent::ReasoningDelta {
                block: index as usize,
                text: String::new(),
            }),
            BlockKind::ToolCall { id, name, .. } => {
                self.tool_calls_started += 1;
                self.observe(StreamEvent::ToolCallDelta {
                    block: index as usize,
                    id: ToolUseId::new(id),
                    provider_id: None,
                    caller: None,
                    toolset_name: None,
                    name: name.clone(),
                    arguments_fragment: String::new(),
                });
            }
            _ => {}
        }
        self.kinds.insert(index, kind);
    }
    fn append_text(&mut self, index: u32, text: &str) -> Result<(), LlmError> {
        use lingxi_llm_client::protocol::StreamEvent;
        match self.kinds.get(&index) {
            Some(BlockKind::Text) => self.observe(StreamEvent::TextDelta {
                block: index as usize,
                text: text.into(),
            }),
            Some(BlockKind::Reasoning) => self.observe(StreamEvent::ReasoningDelta {
                block: index as usize,
                text: text.into(),
            }),
            Some(BlockKind::PreservedText(_)) => {}
            Some(_) => return Err(type_mismatch(index, "text", "text_delta")),
            None => return Err(block_not_found(index)),
        }
        Ok(())
    }
    fn append_text_utf16(
        &mut self,
        index: u32,
        text: &str,
        utf16_code_units: Vec<u16>,
    ) -> Result<(), LlmError> {
        use lingxi_llm_client::protocol::StreamEvent;
        match self.kinds.get(&index) {
            Some(BlockKind::Text) => self.observe(StreamEvent::TextDeltaJsUtf16 {
                block: index as usize,
                text: text.into(),
                utf16_code_units,
            }),
            Some(BlockKind::PreservedText(_)) => {}
            Some(_) => return Err(type_mismatch(index, "text", "text_delta")),
            None => return Err(block_not_found(index)),
        }
        Ok(())
    }
    fn append_json(&mut self, index: u32, partial: &str) -> Result<(), LlmError> {
        use lingxi_llm_client::protocol::{StreamEvent, ToolUseId};
        match self.kinds.get(&index).cloned() {
            Some(BlockKind::ToolCall { id, name, .. }) => {
                if !partial.is_empty() {
                    self.tool_deltas.insert(index);
                }
                self.observe(StreamEvent::ToolCallDelta {
                    block: index as usize,
                    id: ToolUseId::new(id),
                    provider_id: None,
                    caller: None,
                    toolset_name: None,
                    name,
                    arguments_fragment: partial.into(),
                })
            }
            Some(BlockKind::PreservedText(_)) | Some(BlockKind::Preserved(_)) => {}
            Some(_) => return Err(type_mismatch(index, "tool_call", "input_json_delta")),
            None => return Err(block_not_found(index)),
        }
        Ok(())
    }
    fn append_thinking(&mut self, index: u32, text: &str) {
        if matches!(self.kinds.get(&index), Some(BlockKind::Reasoning)) {
            self.observe(lingxi_llm_client::protocol::StreamEvent::ReasoningDelta {
                block: index as usize,
                text: text.into(),
            });
        }
    }
    fn set_text_citations(
        &mut self,
        index: u32,
        citations: Option<Option<Value>>,
    ) -> Result<(), LlmError> {
        match self.kinds.get(&index) {
            Some(BlockKind::Text) => {
                self.text_citations.insert(index, citations);
                Ok(())
            }
            Some(_) => Err(type_mismatch(index, "text", "text_citations")),
            None => Err(block_not_found(index)),
        }
    }
    fn set_provider_content_snapshot(&mut self, index: u32, value: Value) -> Result<(), LlmError> {
        let Some(kind) = self.kinds.get_mut(&index) else {
            return Err(block_not_found(index));
        };
        match kind {
            BlockKind::PreservedText(ContentBlock::ProviderContent {
                protocol,
                value: current,
            }) if protocol == "anthropic_messages" => {
                *current = value;
                Ok(())
            }
            _ => Err(type_mismatch(
                index,
                "opaque_text",
                "provider_content_snapshot",
            )),
        }
    }

    fn set_signature(&mut self, index: u32, signature: &str) -> Result<(), LlmError> {
        match self.kinds.get(&index) {
            Some(BlockKind::Reasoning) => {
                self.observe(lingxi_llm_client::protocol::StreamEvent::ThoughtSignature {
                    block: index as usize,
                    signature: signature.into(),
                })
            }
            Some(_) => return Err(type_mismatch(index, "reasoning", "signature_delta")),
            None => return Err(block_not_found(index)),
        }
        Ok(())
    }
    fn stop_block(&mut self, index: u32) -> Result<Option<ContentBlock>, LlmError> {
        use lingxi_llm_client::protocol::{ContentBlock as SdkBlock, StreamEvent};
        let kind = self
            .kinds
            .remove(&index)
            .ok_or_else(|| double_stop(index))?;
        self.observe(StreamEvent::BlockEnd {
            block: index as usize,
        });
        let content = self.sdk.content_at(index as usize);
        let content = content.as_ref();
        Ok(match kind {
            BlockKind::PreservedText(block) => Some(block),
            BlockKind::Preserved(block) => Some(block),
            BlockKind::Other => None,
            BlockKind::Text => match content {
                Some(SdkBlock::Text { text, .. }) => Some(ContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                    citations: self.text_citations.remove(&index).unwrap_or(None),
                }),
                Some(SdkBlock::TextJsUtf16 {
                    text,
                    utf16_code_units,
                    ..
                }) => Some(ContentBlock::TextJsUtf16 {
                    text: text.clone(),
                    utf16_code_units: utf16_code_units.clone(),
                    cache_control: None,
                    citations: self.text_citations.remove(&index).unwrap_or(None),
                }),
                _ => None,
            },
            BlockKind::Reasoning => match content {
                Some(SdkBlock::Thinking { text, signature }) => Some(ContentBlock::Reasoning {
                    text: text.clone(),
                    signature: signature.clone(),
                }),
                _ => None,
            },
            BlockKind::ToolCall { id, name, server, input_projection } => match content {
                Some(SdkBlock::ToolUse { input, input_json, .. }) => Some(if let Some(initial) = server {
                    ContentBlock::ServerToolUse {
                        id,
                        name,
                        input: if self.tool_deltas.contains(&index) {
                            input.clone()
                        } else {
                            initial
                        },
                    }
                } else {
                    let projection = match input_json {
                        Some(raw) => Some(lingxi_core::types::utf16_json::Utf16JsonProjection::parse(raw).map_err(|error| LlmError::InvalidRequest { message: error.to_string() })?),
                        None => input_projection,
                    };
                    ContentBlock::ToolCall {
                        input: projection.as_ref().map_or_else(|| input.clone(), |projection| projection.value.clone()),
                        input_projection: projection,
                        id,
                        name,
                    }
                }),
                _ if server.is_some() => Some(ContentBlock::ServerToolUse {
                    id,
                    name,
                    input: server.expect("server initial input"),
                }),
                _ => {
                    let input_bytes = self
                        .sdk
                        .snapshot()
                        .incomplete_tools
                        .iter()
                        .find(|tool| tool.block == index as usize)
                        .map_or(0, |tool| tool.input_bytes);
                    return Err(LlmError::MalformedToolInput {
                        tool_name: name,
                        block_index: index,
                        reason: "invalid JSON tool input".into(),
                        input_bytes,
                        has_other_tool_calls: self.tool_calls_started > 1,
                    });
                }
            },
        })
    }
}

fn block_not_found(index: u32) -> LlmError {
    LlmError::StreamInterrupted {
        message: format!("streaming: delta for block index {index} without prior start"),
    }
}
fn double_stop(index: u32) -> LlmError {
    LlmError::StreamInterrupted {
        message: format!("streaming: double stop for block index {index}"),
    }
}
fn type_mismatch(index: u32, expected: &str, got: &str) -> LlmError {
    LlmError::StreamInterrupted {
        message: format!(
            "streaming: type mismatch on block {index}: expected {expected}, got {got}"
        ),
    }
}
fn block_kind_of(block: &ContentBlock) -> BlockKind {
    match block {
        ContentBlock::Text { .. } | ContentBlock::TextJsUtf16 { .. } => BlockKind::Text,
        ContentBlock::ToolCall { id, name, input_projection, .. } => BlockKind::ToolCall { input_projection: input_projection.clone(),
            id: id.clone(),
            name: name.clone(),
            server: None,
        },
        ContentBlock::ServerToolUse { id, name, input } => BlockKind::ToolCall { input_projection: None,
            id: id.clone(),
            name: name.clone(),
            server: Some(input.clone()),
        },
        ContentBlock::Reasoning { .. } => BlockKind::Reasoning,
        ContentBlock::ProviderContent { protocol, value }
            if protocol == "anthropic_messages" && value["type"] == "text" =>
        {
            BlockKind::PreservedText(block.clone())
        }
        ContentBlock::RedactedThinking { .. }
        | ContentBlock::ConnectorText { .. }
        | ContentBlock::ProviderContent { .. }
        | ContentBlock::AdvisorToolResult { .. } => BlockKind::Preserved(block.clone()),
        _ => BlockKind::Other,
    }
}

/// Keep the most recent canonical SDK usage report. Its state distinguishes a
/// partial observation from a complete one; the host never infers omitted
/// provider fields from zero counters.
pub(crate) fn merge_usage(seed: &ExecutionUsage, delta: &ExecutionUsage) -> ExecutionUsage {
    let has_new_report = delta.report.usage.is_some() || delta.report.state != UsageState::Missing;
    let report = if has_new_report {
        delta.report.clone()
    } else {
        seed.report.clone()
    };
    let replacement_total = has_new_report
        .then(|| report.usage.map(|counts| counts.total()))
        .flatten();
    ExecutionUsage {
        report,
        inference: if delta.inference == Default::default() {
            seed.inference.clone()
        } else {
            delta.inference.clone()
        },
        context_tokens: if has_new_report {
            delta.context_tokens.or(replacement_total)
        } else {
            delta.context_tokens.or(seed.context_tokens)
        },
        provider_reported_total_tokens: if has_new_report {
            delta.provider_reported_total_tokens.or(replacement_total)
        } else {
            delta
                .provider_reported_total_tokens
                .or(seed.provider_reported_total_tokens)
        },
        provider_metadata: if delta.provider_metadata.is_null() {
            seed.provider_metadata.clone()
        } else {
            delta.provider_metadata.clone()
        },
        cost_estimate: delta
            .cost_estimate
            .clone()
            .or_else(|| seed.cost_estimate.clone()),
    }
}

/// Drive `stream` to completion, accumulating SSE events into a single
/// [`HistoryResponse`] — the streaming analog of one non-streaming
/// `messages_create` round-trip. `id` / `model` / the usage seed come from
/// `message_start`; the final `stop_reason` + usage come from `message_delta`.
///
/// If the stream yields a [`HistoryEvent::Completed`] event, the contained
/// response is returned immediately without waiting for `MessageStop` (it
/// already is the final complete response).
///
/// # Errors
/// - The transport [`LlmError`] verbatim if the stream yields `Err`.
/// - [`LlmError::MalformedToolInput`] if a tool argument is not valid JSON.
/// - [`LlmError::StreamInterrupted`] if the event sequence violates the
///   per-block protocol (delta before start, double stop, type mismatch).
/// - [`LlmError::StreamInterrupted`] if the stream ends before `message_stop`
///   or `completed`.
// Test-only thin wrapper over the salvaging variant — drops the partial content
// the salvage carries so the accumulator's own unit tests keep asserting the
// `Result<_, LlmError>` shape. Production drives `accumulate_stream_salvaging`
// directly (the runner needs the salvaged partial), so this is `cfg(test)`.
#[cfg(test)]
pub async fn accumulate_stream(
    stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
) -> Result<HistoryResponse, LlmError> {
    accumulate_stream_salvaging(stream)
        .await
        .map_err(|(_partial, e)| e)
}

/// Like [`accumulate_stream`], but on ANY mid-stream error returns the content
/// blocks completed BEFORE the error alongside the error, so the subagent runner
/// can SALVAGE the partial output (CC 2.1.207 `api_error_partial` recovery — the
/// query-loop finalizes the partial into the transcript, and the sync-agent
/// caller recovers it with an incomplete-response notice rather than failing
/// the whole tool call). Only blocks whose `content_block_stop` was already
/// seen are salvaged — an in-flight (unstopped) block is dropped exactly as CC's
/// `blocks_yielded` counts only completed blocks.
pub async fn accumulate_stream_salvaging(
    stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
) -> Result<HistoryResponse, (Vec<ContentBlock>, LlmError)> {
    accumulate_stream_salvaging_remaining(stream)
        .await
        .map(|(response, _remaining)| response)
}

/// Accumulate one assistant response and return the unread event stream. Mod
/// `turn.step` can emit multiple assistant responses from one request; its
/// caller must process each response without opening another provider call.
#[derive(Debug)]
pub enum ResponseAccumulatorUpdate {
    /// One content block has been fully assembled. This is emitted at the
    /// `content_block_stop` boundary, so live consumers can act on a complete
    /// tool call while the provider stream is still open.
    Continue {
        completed_block: Option<(u32, ContentBlock)>,
    },
    /// The current response reached `message_stop` or a complete snapshot.
    Completed(HistoryResponse),
}

/// Incremental form of the response accumulator used by consumers that need
/// completed block boundaries before the provider stream ends. It is the same
/// assembler used by [`accumulate_stream_salvaging_remaining`]; callers should
/// not decode provider deltas themselves.
#[derive(Debug)]
pub struct ResponseAccumulator {
    acc: BlockAccumulator,
    malformed_input: Option<LlmError>,
    content: Vec<ContentBlock>,
    content_indices: Vec<(u32, bool)>,
    id: String,
    model: String,
    usage: ExecutionUsage,
    stop_reason: Option<String>,
    stop_details: Option<crate::HistoryStopDetails>,
    cost: Option<crate::CostEstimate>,
    provider_metadata: Value,
}

impl Default for ResponseAccumulator {
    fn default() -> Self {
        Self {
            acc: BlockAccumulator::new(),
            malformed_input: None,
            content: Vec::new(),
            content_indices: Vec::new(),
            id: String::new(),
            model: String::new(),
            usage: ExecutionUsage::default(),
            stop_reason: None,
            stop_details: None,
            cost: None,
            provider_metadata: Value::Null,
        }
    }
}

impl ResponseAccumulator {
    /// Apply one provider-neutral HistoryEvent. A fallback observation filters
    /// the same accumulated block indexes as the one-shot path; the raw event
    /// remains available to the caller for host-owned admission/cancellation.
    pub fn observe(
        &mut self,
        event: HistoryEvent,
    ) -> Result<ResponseAccumulatorUpdate, (Vec<ContentBlock>, LlmError)> {
        if self.malformed_input.is_some() {
            match event {
                HistoryEvent::ContentBlockStart { content_block, .. }
                    if matches!(
                        content_block,
                        ContentBlock::ToolCall { .. } | ContentBlock::ServerToolUse { .. }
                    ) =>
                {
                    self.acc.tool_calls_started += 1;
                }
                HistoryEvent::Completed { .. } => {
                    return Err((
                        std::mem::take(&mut self.content),
                        self.malformed_input
                            .take()
                            .expect("pending malformed input"),
                    ));
                }
                HistoryEvent::MessageStop => {
                    if let Some(LlmError::MalformedToolInput {
                        has_other_tool_calls,
                        ..
                    }) = self.malformed_input.as_mut()
                    {
                        *has_other_tool_calls = self.acc.tool_calls_started > 1;
                    }
                    return Err((
                        std::mem::take(&mut self.content),
                        self.malformed_input
                            .take()
                            .expect("pending malformed input"),
                    ));
                }
                _ => {}
            }
            return Ok(ResponseAccumulatorUpdate::Continue {
                completed_block: None,
            });
        }

        match event {
            HistoryEvent::WebSearch { .. } => {} // Metadata is retained by the terminal snapshot.
            HistoryEvent::ResponseObserved {
                model: observed,
                response_id,
            } => {
                self.model = observed;
                if let Some(observed_id) = response_id {
                    self.id = observed_id;
                }
            }
            HistoryEvent::CostQuoteObserved { estimate, .. } => {
                // A quote is a settlement fact, not a usage report. Keep it
                // even when the provider omitted aggregate token counters.
                // `None` is authoritative too: it suppresses stale aggregate
                // estimates for a native multi-iteration quote that could not
                // be fully priced from the frozen catalog.
                self.cost = estimate;
            }
            HistoryEvent::ServerFallback {
                event,
                profile,
                lane,
            } => {
                self.acc.sdk.apply_server_fallback(&event);
                self.acc.kinds.retain(|index, _| {
                    !event
                        .discarded_blocks
                        .contains(&((*index & 0x7fff_ffff) as usize))
                });
                self.acc.tool_deltas.retain(|index| {
                    !event
                        .discarded_blocks
                        .contains(&((*index & 0x7fff_ffff) as usize))
                });
                self.model.clone_from(&event.to_model);
                let mut retained_content = Vec::new();
                let mut retained_indices = Vec::new();
                for (block, index) in std::mem::take(&mut self.content)
                    .into_iter()
                    .zip(std::mem::take(&mut self.content_indices))
                {
                    if !event.discarded_blocks.contains(&(index.0 as usize)) {
                        retained_content.push(block);
                        retained_indices.push(index);
                    }
                }
                self.content = retained_content;
                self.content_indices = retained_indices;
                crate::history_projection::append_observation(
                    &mut self.provider_metadata,
                    "server_fallback_events",
                    serde_json::to_value(crate::history::HistoryServerFallback {
                        event: *event,
                        profile,
                        lane,
                    })
                    .expect("normalized fallback event"),
                );
            }
            HistoryEvent::MessageStart { response } => {
                // Capture id/model + the usage seed from the start snapshot.
                self.id = response.id;
                self.model = response.model;
                self.usage = response.usage;
                self.cost = response.cost;
                self.provider_metadata = response.provider_metadata;
                self.stop_details = response.stop_details;
            }
            HistoryEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.acc.start_block(index, block_kind_of(&content_block));
                let citations = match &content_block {
                    ContentBlock::Text { citations, .. }
                    | ContentBlock::TextJsUtf16 { citations, .. } => Some(citations.clone()),
                    _ => None,
                };
                if let Some(citations) = citations {
                    self.acc.text_citations.insert(index, citations);
                }
            }
            HistoryEvent::ContentBlockDelta { index, delta } => {
                let result = match delta {
                    HistoryContentDelta::TextDelta { text } => self.acc.append_text(index, &text),
                    HistoryContentDelta::TextJsUtf16Delta {
                        text,
                        utf16_code_units,
                    } => self.acc.append_text_utf16(index, &text, utf16_code_units),
                    HistoryContentDelta::InputJsonDelta { partial_json } => {
                        self.acc.append_json(index, &partial_json)
                    }
                    HistoryContentDelta::ThinkingDelta { thinking } => {
                        // No-op on a non-thinking block (e.g. `redacted_thinking`),
                        // never a stream error — see [`append_thinking`].
                        self.acc.append_thinking(index, &thinking);
                        Ok(())
                    }
                    HistoryContentDelta::SignatureDelta { signature } => {
                        self.acc.set_signature(index, &signature)
                    }
                    HistoryContentDelta::TextCitations { citations } => {
                        self.acc.set_text_citations(index, citations)
                    }
                    HistoryContentDelta::ProviderContentSnapshot { value } => {
                        self.acc.set_provider_content_snapshot(index, value)
                    }
                    // Dropped at the `translate_response_blocks` boundary.
                    HistoryContentDelta::CitationsDelta { .. }
                    | HistoryContentDelta::ConnectorTextDelta { .. } => Ok(()),
                };
                if let Err(error) = result {
                    return Err((std::mem::take(&mut self.content), error));
                }
            }
            HistoryEvent::ContentBlockStop { index } => {
                match self.acc.stop_block(index) {
                    Ok(block) => {
                        if let Some(block) = block {
                            if let ContentBlock::ProviderContent { value, .. } = &block {
                                if index >= 0x8000_0000
                                    && value["type"] == "lingxi_observation"
                                    && value["metadata"]["llm_client"].is_object()
                                {
                                    if let Some(metadata) = value.get("metadata") {
                                        if let Some(response_model) =
                                            projected_response_model(metadata)
                                        {
                                            self.model = response_model.to_owned();
                                        }
                                        self.provider_metadata = metadata.clone();
                                    }
                                    return Ok(ResponseAccumulatorUpdate::Continue {
                                        completed_block: None,
                                    });
                                }
                            }
                            let key = crate::stream_content_order(index);
                            let position = self
                                .content_indices
                                .partition_point(|existing| existing <= &key);
                            self.content_indices.insert(position, key);
                            self.content.insert(position, block.clone());
                            return Ok(ResponseAccumulatorUpdate::Continue {
                                completed_block: Some((index, block)),
                            });
                        }
                    }
                    Err(mut error @ LlmError::MalformedToolInput { .. }) => {
                        // Until a terminal event proves the response complete, fail closed:
                        // a later tool call may have performed server-side work.
                        if let LlmError::MalformedToolInput {
                            has_other_tool_calls,
                            ..
                        } = &mut error
                        {
                            *has_other_tool_calls = true;
                        }
                        self.malformed_input = Some(error);
                    }
                    Err(error) => return Err((std::mem::take(&mut self.content), error)),
                }
            }
            HistoryEvent::MessageDelta {
                delta,
                usage: delta_usage,
            } => {
                if let Some(sr) = delta.stop_reason {
                    self.stop_reason = Some(sr);
                }
                if delta.stop_details.is_some() {
                    self.stop_details = delta.stop_details;
                }
                if let Some(mut usage) = delta_usage {
                    if let Some(metadata) = usage.provider_metadata.get("stream") {
                        if let Some(response_model) = projected_response_model(metadata) {
                            self.model = response_model.to_owned();
                        }
                        self.provider_metadata = metadata.clone();
                    }
                    if let Some(estimate) = usage.cost_estimate.take() {
                        self.cost = Some(estimate);
                    }
                    self.usage = merge_usage(&self.usage, &usage);
                }
            }
            HistoryEvent::MessageStop => {
                return Ok(ResponseAccumulatorUpdate::Completed(HistoryResponse {
                    id: std::mem::take(&mut self.id),
                    model: std::mem::take(&mut self.model),
                    content: std::mem::take(&mut self.content),
                    stop_reason: self.stop_reason.take(),
                    stop_details: self.stop_details.take(),
                    usage: std::mem::take(&mut self.usage),
                    cost: self.cost.take(),
                    provider_metadata: std::mem::replace(&mut self.provider_metadata, Value::Null),
                }));
            }
            // Short-circuit: the stream provider emits a fully-assembled
            // response in the `Completed` event — return it directly.
            // This is the canonical terminal for llm-runtime streams
            // (llm-runtime protocol.rs:302; drops Ping/Error from api-client).
            HistoryEvent::Completed { response } => {
                return Ok(ResponseAccumulatorUpdate::Completed(*response));
            }
        }
        Ok(ResponseAccumulatorUpdate::Continue {
            completed_block: None,
        })
    }

    /// Salvage only blocks whose stop event has already been observed.
    pub fn partial_content(&self) -> &[ContentBlock] {
        &self.content
    }

    /// Snapshot the response facts observed so far without manufacturing a
    /// terminal event. Host controllers use this when a routing decision ends
    /// a stream before the provider sends its message delta/stop pair.
    #[must_use]
    pub fn partial_snapshot(&self) -> HistoryResponse {
        HistoryResponse {
            id: self.id.clone(),
            model: self.model.clone(),
            content: self.content.clone(),
            stop_reason: self.stop_reason.clone(),
            stop_details: self.stop_details.clone(),
            usage: self.usage.clone(),
            cost: self.cost.clone(),
            provider_metadata: self.provider_metadata.clone(),
        }
    }

    /// If malformed tool input was observed before a transport error, it is
    /// the authoritative failure to surface, matching the one-shot adapter.
    pub fn take_malformed_error(&mut self) -> Option<LlmError> {
        self.malformed_input.take()
    }

    /// Report the canonical incomplete-stream error at end of input.
    pub fn finish(self) -> (Vec<ContentBlock>, LlmError) {
        if let Some(error) = self.malformed_input {
            return (self.content, error);
        }
        (
            self.content,
            LlmError::StreamInterrupted {
                message: "stream ended without message_stop or completed event".to_string(),
            },
        )
    }
}

pub async fn accumulate_stream_salvaging_remaining(
    mut stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
) -> Result<
    (
        HistoryResponse,
        BoxStream<'static, Result<HistoryEvent, LlmError>>,
    ),
    (Vec<ContentBlock>, LlmError),
> {
    let mut accumulator = ResponseAccumulator::default();
    while let Some(item) = stream.next().await {
        // Transport-level error: salvage the completed blocks + surface it.
        let event = match item {
            Ok(event) => event,
            Err(error) => {
                let partial = accumulator.partial_content().to_vec();
                let malformed = accumulator.take_malformed_error();
                return Err((partial, malformed.unwrap_or(error)));
            }
        };
        match accumulator.observe(event) {
            Ok(ResponseAccumulatorUpdate::Continue { .. }) => {}
            Ok(ResponseAccumulatorUpdate::Completed(response)) => {
                return Ok((response, stream));
            }
            Err(error) => return Err(error),
        }
    }
    Err(accumulator.finish())
}

/// Synthesize a [`HistoryEvent`] sequence that reconstructs `resp` exactly when
/// fed back through [`accumulate_stream`].
///
/// Scripted clients can explicitly adapt complete responses to event streams.
/// The round-trip is lossless: `text` /
/// `reasoning` bodies ride a single delta (the `content_block_start` payload is
/// empty, exactly as on the wire), `tool_call` input rides one `input_json_delta`
/// (re-parsed on stop), and the full usage is seeded on `message_start` so the
/// [`merge_usage`] reconstruction reproduces `resp.usage`.
pub fn response_to_stream_events(resp: HistoryResponse) -> Vec<HistoryEvent> {
    let mut events = Vec::with_capacity(resp.content.len() * 3 + 3);
    // `message_start` carries id/model + a usage seed. On the wire the seed
    // holds input/cache with `output_tokens == 0`; here we seed the FULL usage
    // so the round-trip is exact regardless of the field split.
    events.push(HistoryEvent::MessageStart {
        response: Box::new(HistoryResponse {
            id: resp.id.clone(),
            model: resp.model.clone(),
            content: Vec::new(),
            stop_reason: None,
            stop_details: None,
            // `resp.usage` is reused below for the final `message_delta`; clone
            // here since `Usage` is not `Copy`.
            usage: resp.usage.clone(),
            cost: resp.cost.clone(),
            provider_metadata: resp.provider_metadata.clone(),
        }),
    });
    for (i, block) in resp.content.into_iter().enumerate() {
        // Content-block counts never approach `u32::MAX`; saturate rather than
        // panic on the (unreachable) overflow.
        let index = u32::try_from(i).unwrap_or(u32::MAX);
        match block {
            ContentBlock::Text { text, .. } => {
                events.push(HistoryEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::Text {
                        text: String::new(),
                        cache_control: None,
                        citations: None,
                    },
                });
                events.push(HistoryEvent::ContentBlockDelta {
                    index,
                    delta: HistoryContentDelta::TextDelta { text },
                });
            }
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
                cache_control,
            } => {
                events.push(HistoryEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::Text {
                        text: String::new(),
                        cache_control,
                        citations,
                    },
                });
                events.push(HistoryEvent::ContentBlockDelta {
                    index,
                    delta: HistoryContentDelta::TextJsUtf16Delta {
                        text,
                        utf16_code_units,
                    },
                });
            }
            ContentBlock::Reasoning { text, signature } => {
                events.push(HistoryEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::Reasoning {
                        text: String::new(),
                        signature: None,
                    },
                });
                events.push(HistoryEvent::ContentBlockDelta {
                    index,
                    delta: HistoryContentDelta::ThinkingDelta { thinking: text },
                });
                if let Some(sig) = signature {
                    events.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::SignatureDelta { signature: sig },
                    });
                }
            }
            ContentBlock::ToolCall { id, name, input , .. } => {
                events.push(HistoryEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolCall { input_projection: None,
                        id: id.clone(),
                        name: name.clone(),
                        input: Value::Null,
                    },
                });
                // The accumulator reassembles + parses this back to `input`.
                events.push(HistoryEvent::ContentBlockDelta {
                    index,
                    delta: HistoryContentDelta::InputJsonDelta {
                        partial_json: input.to_string(),
                    },
                });
            }
            // Low-frequency server-side variants: emit only a start so the index
            // is consumed. The accumulator captures the full block from this
            // start event and yields `Preserved` on stop, so it round-trips
            // verbatim (matching `translate_response_blocks` preservation).
            other => {
                events.push(HistoryEvent::ContentBlockStart {
                    index,
                    content_block: other,
                });
            }
        }
        events.push(HistoryEvent::ContentBlockStop { index });
    }
    events.push(HistoryEvent::MessageDelta {
        delta: HistoryMessageDelta {
            stop_reason: resp.stop_reason,
            stop_details: resp.stop_details,
        },
        usage: Some(resp.usage),
    });
    events.push(HistoryEvent::MessageStop);
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use lingxi_llm_client::protocol as wire;

    fn boxed(events: Vec<HistoryEvent>) -> BoxStream<'static, Result<HistoryEvent, LlmError>> {
        stream::iter(events.into_iter().map(Ok)).boxed()
    }

    fn message_start(id: &str, model: &str) -> HistoryEvent {
        HistoryEvent::MessageStart {
            response: Box::new(HistoryResponse {
                id: id.to_string(),
                model: model.to_string(),
                content: Vec::new(),
                stop_reason: None,
                stop_details: None,
                usage: ExecutionUsage::default(),
                cost: None,
                provider_metadata: Value::Null,
            }),
        }
    }

    #[test]
    fn canonical_zero_snapshots_replace_prior_counters_without_protocol_merging() {
        let seed = ExecutionUsage::from_counts(wire::Usage {
            input_tokens: 42,
            output_tokens: 99,
            cache_read_tokens: 3,
            ..Default::default()
        });
        for state in [
            wire::UsageState::Partial,
            wire::UsageState::Complete,
            wire::UsageState::Invalid,
        ] {
            let next = ExecutionUsage {
                report: wire::UsageReport::measured(wire::Usage::default(), state),
                ..Default::default()
            };
            let merged = merge_usage(&seed, &next);
            assert_eq!(
                merged.report, next.report,
                "SDK state {state:?} remains authoritative"
            );
            assert_eq!(merged.counts().input_tokens, 0);
            assert_eq!(merged.counts().output_tokens, 0);
        }
        let invalid = ExecutionUsage {
            report: wire::UsageReport {
                usage: None,
                state: wire::UsageState::Invalid,
            },
            ..Default::default()
        };
        assert_eq!(merge_usage(&seed, &invalid).report, invalid.report);
        assert_eq!(
            merge_usage(&seed, &ExecutionUsage::default()).report,
            seed.report
        );
    }

    #[tokio::test]
    async fn host_observations_update_response_metadata_without_token_usage() {
        let metadata = serde_json::json!({"llm_client":{"web_search":[{"citations":[{"url":"https://example.com"}]}]}});
        let response = accumulate_stream(boxed(vec![
            message_start("m", "model"),
            HistoryEvent::ContentBlockStart {
                index: u32::MAX,
                content_block: ContentBlock::ProviderContent {
                    protocol: "open_ai_responses".into(),
                    value: serde_json::json!({"type":"lingxi_observation","metadata":metadata}),
                },
            },
            HistoryEvent::ContentBlockStop { index: u32::MAX },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ]))
        .await
        .unwrap();
        assert_eq!(response.provider_metadata, metadata);
        assert_eq!(response.usage, ExecutionUsage::default());
        assert!(response.content.is_empty());
    }

    #[tokio::test]
    async fn native_cost_quote_survives_without_manufacturing_a_usage_report() {
        let estimate = crate::CostEstimate {
            pricing_model: crate::PricingModelRef {
                pricing_provider_id: crate::ProviderId::AnthropicFirstParty,
                billing_model: "claude-opus-4-7".into(),
                request_model: "claude-opus-4-7".into(),
                display_model: "claude-opus-4-7".into(),
            },
            total_cost_usd: Some(0.0),
            input_cost_usd: Some(0.0),
            output_cost_usd: Some(0.0),
            cache_read_cost_usd: Some(0.0),
            cache_write_cost_usd: Some(0.0),
            reasoning_cost_usd: Some(0.0),
            estimated: true,
            pricing_source: Some("captured-sdk-profile".into()),
        };
        let metadata = serde_json::json!({
            "llm_client": {
                "server_fallback_cost_quote": {
                    "kind":"anthropic_server_fallback_per_iteration",
                    "completeness":"complete"
                }
            }
        });
        let response = accumulate_stream(boxed(vec![
            message_start("m", "claude-opus-4-7"),
            HistoryEvent::CostQuoteObserved {
                estimate: Some(estimate.clone()),
                native_server_fallback: true,
                summary_model: Some("claude-opus-4-7".into()),
            },
            HistoryEvent::ContentBlockStart {
                index: u32::MAX,
                content_block: ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: serde_json::json!({
                        "type":"lingxi_observation",
                        "metadata":metadata
                    }),
                },
            },
            HistoryEvent::ContentBlockStop { index: u32::MAX },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ]))
        .await
        .unwrap();
        assert_eq!(response.usage, ExecutionUsage::default());
        assert_eq!(response.cost, Some(estimate));
        assert_eq!(
            response.server_fallback_cost_quote().unwrap()["completeness"],
            "complete"
        );
        assert!(response.content.is_empty());
    }

    #[tokio::test]
    async fn terminal_stream_usage_promotes_frozen_quote_to_response_cost() {
        let mut quote = crate::CostEstimate::unestimated(crate::PricingModelRef {
            pricing_provider_id: crate::ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            billing_model: "deepseek-flash".into(),
            request_model: "deepseek-flash".into(),
            display_model: "deepseek-flash".into(),
        });
        quote.estimated = true;
        quote.total_cost_usd = Some(0.00075);
        let mut usage = ExecutionUsage::default();
        usage.cost_estimate = Some(quote);
        let response = accumulate_stream(boxed(vec![
            message_start("m", "deepseek-flash"),
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: Some(usage),
            },
            HistoryEvent::MessageStop,
        ]))
        .await
        .unwrap();
        assert_eq!(response.cost.unwrap().total_cost_usd, Some(0.00075));
        assert!(response.usage.cost_estimate.is_none());
    }

    #[tokio::test]
    async fn text_stream_accumulates_one_text_block() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                    citations: None,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta { text: "he".into() },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta { text: "llo".into() },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ];
        let resp = accumulate_stream(boxed(evs)).await.expect("accumulate");
        assert_eq!(resp.id, "m1");
        assert_eq!(resp.model, "claude-mock");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn utf16_text_stream_pairs_surrogate_halves_and_keeps_units_off_event_json() {
        let high = HistoryContentDelta::TextJsUtf16Delta {
            text: "�".into(),
            utf16_code_units: vec![0xd83d],
        };
        let serialized = serde_json::to_value(&high).unwrap();
        assert!(serialized.get("utf16_code_units").is_none());
        let response = accumulate_stream(boxed(vec![
            message_start("m-utf16", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                    citations: None,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: high,
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextJsUtf16Delta {
                    text: "�".into(),
                    utf16_code_units: vec![0xde00],
                },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ]))
        .await
        .expect("utf16 stream accumulation");
        assert!(matches!(
            response.content.as_slice(),
            [ContentBlock::TextJsUtf16 { text, utf16_code_units, .. }]
                if text == "😀" && utf16_code_units == &[0xd83d, 0xde00]
        ));
    }

    #[tokio::test]
    async fn tool_call_stream_reassembles_input_json() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolCall { input_projection: None,
                    id: "tc-1".to_string(),
                    name: "Read".into(),
                    input: Value::Null,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 1,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "{\"file".into(),
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 1,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "_path\":\"foo.rs\"}".into(),
                },
            },
            HistoryEvent::ContentBlockStop { index: 1 },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("tool_use".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ];
        let resp = accumulate_stream(boxed(evs)).await.expect("accumulate");
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::ToolCall { id, name, input , .. } => {
                assert_eq!(id, "tc-1");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "foo.rs");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn incremental_accumulator_exposes_a_complete_tool_call_before_terminal_event() {
        let mut accumulator = ResponseAccumulator::default();
        accumulator
            .observe(message_start("m-live", "claude-mock"))
            .unwrap();
        accumulator
            .observe(HistoryEvent::ContentBlockStart {
                index: 4,
                content_block: ContentBlock::ToolCall { input_projection: None,
                    id: "tool-live".into(),
                    name: "Agent".into(),
                    input: Value::Null,
                },
            })
            .unwrap();
        accumulator
            .observe(HistoryEvent::ContentBlockDelta {
                index: 4,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: r#"{"prompt":"run child"}"#.into(),
                },
            })
            .unwrap();

        let update = accumulator
            .observe(HistoryEvent::ContentBlockStop { index: 4 })
            .unwrap();
        let ResponseAccumulatorUpdate::Continue {
            completed_block: Some((index, block)),
        } = update
        else {
            panic!("content_block_stop must publish the assembled block");
        };
        assert_eq!(index, 4);
        assert!(matches!(
            block,
            ContentBlock::ToolCall { id, name, input , .. }
                if id == "tool-live" && name == "Agent" && input["prompt"] == "run child"
        ));
        assert_eq!(accumulator.partial_content().len(), 1);

        let ResponseAccumulatorUpdate::Continue { .. } = accumulator
            .observe(HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("tool_use".into()),
                    stop_details: None,
                },
                usage: None,
            })
            .unwrap()
        else {
            panic!("message_delta is not terminal");
        };
        let ResponseAccumulatorUpdate::Completed(response) =
            accumulator.observe(HistoryEvent::MessageStop).unwrap()
        else {
            panic!("message_stop must complete the response");
        };
        assert_eq!(response.id, "m-live");
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        assert!(matches!(
            response.content.as_slice(),
            [ContentBlock::ToolCall { id, .. }] if id == "tool-live"
        ));
    }

    #[tokio::test]
    async fn reasoning_stream_accumulates_body_and_signature() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Reasoning {
                    text: String::new(),
                    signature: None,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::ThinkingDelta {
                    thinking: "ponder".into(),
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::SignatureDelta {
                    signature: "sig-1".into(),
                },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ];
        let resp = accumulate_stream(boxed(evs)).await.expect("accumulate");
        match &resp.content[0] {
            ContentBlock::Reasoning { text, signature } => {
                assert_eq!(text, "ponder");
                assert_eq!(signature.as_deref(), Some("sig-1"));
            }
            other => panic!("expected Reasoning, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_usage_uses_sdk_cumulative_snapshot() {
        let evs = vec![
            HistoryEvent::MessageStart {
                response: Box::new(HistoryResponse {
                    id: "m1".into(),
                    model: "claude-mock".into(),
                    content: Vec::new(),
                    stop_reason: None,
                    stop_details: None,
                    usage: ExecutionUsage {
                        report: wire::UsageReport::measured(
                            wire::Usage {
                                input_tokens: 42,
                                output_tokens: 0,
                                cache_write_tokens: 7,
                                cache_read_tokens: 3,
                                reasoning_tokens: 0,

                                ..Default::default()
                            },
                            wire::UsageState::Partial,
                        ),
                        ..ExecutionUsage::default()
                    },
                    cost: None,
                    provider_metadata: Value::Null,
                }),
            },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: Some(ExecutionUsage {
                    report: wire::UsageReport::measured(
                        wire::Usage {
                            input_tokens: 42,
                            output_tokens: 99,
                            cache_write_tokens: 7,
                            cache_read_tokens: 3,
                            reasoning_tokens: 0,

                            ..Default::default()
                        },
                        wire::UsageState::Complete,
                    ),
                    ..ExecutionUsage::default()
                }),
            },
            HistoryEvent::MessageStop,
        ];
        let resp = accumulate_stream(boxed(evs)).await.expect("accumulate");
        // The SDK supplies cumulative counters; runtime consumes the complete snapshot.
        assert_eq!(resp.usage.counts().input_tokens, 42);
        assert_eq!(resp.usage.counts().output_tokens, 99);
        assert_eq!(resp.usage.counts().cache_write_tokens, 7);
        assert_eq!(resp.usage.counts().cache_read_tokens, 3);
    }

    #[tokio::test]
    async fn stream_without_message_stop_errors() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                    citations: None,
                },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            // no message_stop
        ];
        let err = accumulate_stream(boxed(evs)).await.expect_err("no stop");
        assert!(matches!(err, LlmError::StreamInterrupted { .. }));
    }

    #[tokio::test]
    async fn delta_before_start_is_malformed() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta {
                    text: "oops".into(),
                },
            },
            HistoryEvent::MessageStop,
        ];
        let err = accumulate_stream(boxed(evs)).await.expect_err("malformed");
        match err {
            LlmError::StreamInterrupted { message } => {
                assert!(message.contains("block index 0"), "{message}");
            }
            other => panic!("expected StreamInterrupted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn type_mismatch_is_malformed() {
        // `input_json_delta` on a text block.
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                    citations: None,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "{}".into(),
                },
            },
            HistoryEvent::MessageStop,
        ];
        let err = accumulate_stream(boxed(evs)).await.expect_err("mismatch");
        match err {
            LlmError::StreamInterrupted { message } => {
                assert!(message.contains("type mismatch"), "{message}");
            }
            other => panic!("expected StreamInterrupted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn thinking_delta_on_redacted_thinking_is_ignored() {
        // The API streams `thinking_delta` (estimated_tokens pings) during the
        // redacted-thinking phase, so one legitimately lands on a
        // `redacted_thinking` block. CC no-ops it (`if(n?.type==="thinking")`);
        // the port must NOT terminate the stream with a type mismatch.
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::ThinkingDelta {
                    thinking: "leak".into(),
                },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::MessageStop,
        ];
        let resp = accumulate_stream(boxed(evs))
            .await
            .expect("redacted-thinking stream must succeed");
        // The redacted_thinking block is preserved unchanged; the stray
        // thinking_delta text is dropped (not appended anywhere).
        assert!(
            resp.content
                .iter()
                .any(|b| matches!(b, ContentBlock::RedactedThinking { .. })),
            "redacted_thinking block preserved"
        );
        assert!(
            !resp
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("leak"))),
            "stray thinking_delta must not materialize as text"
        );
    }

    #[tokio::test]
    async fn bad_tool_call_json_is_malformed() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolCall { input_projection: None,
                    id: "tc-1".to_string(),
                    name: "Read".into(),
                    input: Value::Null,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "{not json".into(),
                },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::MessageStop,
        ];
        let err = accumulate_stream(boxed(evs)).await.expect_err("bad json");
        match err {
            LlmError::MalformedToolInput {
                tool_name,
                block_index,
                input_bytes,
                has_other_tool_calls,
                ..
            } => {
                assert_eq!(tool_name, "Read");
                assert_eq!(block_index, 0);
                assert_eq!(input_bytes, 9);
                assert!(!has_other_tool_calls);
            }
            other => panic!("expected MalformedToolInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_structured_output_is_redacted_and_tracks_all_started_tools() {
        for other_tool in [None, Some(false), Some(true)] {
            for completed in [false, true] {
                let mut evs = vec![message_start("m1", "mock")];
                if let Some(server) = other_tool {
                    let content_block = if server {
                        ContentBlock::ServerToolUse {
                            id: "other".into(),
                            name: "search".into(),
                            input: Value::Null,
                        }
                    } else {
                        ContentBlock::ToolCall { input_projection: None,
                            id: "other".into(),
                            name: "Read".into(),
                            input: Value::Null,
                        }
                    };
                    evs.push(HistoryEvent::ContentBlockStart {
                        index: 0,
                        content_block,
                    });
                    if completed {
                        evs.push(HistoryEvent::ContentBlockStop { index: 0 });
                    }
                }
                let input = r#"{"secret":"never-print-this""#;
                evs.extend([
                    HistoryEvent::ContentBlockStart {
                        index: 1,
                        content_block: ContentBlock::ToolCall { input_projection: None,
                            id: "output".into(),
                            name: "StructuredOutput".into(),
                            input: Value::Null,
                        },
                    },
                    HistoryEvent::ContentBlockDelta {
                        index: 1,
                        delta: HistoryContentDelta::InputJsonDelta {
                            partial_json: input.into(),
                        },
                    },
                    HistoryEvent::ContentBlockStop { index: 1 },
                    HistoryEvent::MessageStop,
                ]);
                let (_, err) = accumulate_stream_salvaging(boxed(evs))
                    .await
                    .expect_err("malformed JSON");
                assert!(!err.to_string().contains("never-print-this"));
                assert!(!format!("{err:?}").contains("never-print-this"));
                assert_eq!(
                    crate::retry::RetryPolicy.classify_error(&err),
                    crate::retry::RetryDecision::DoNotRetry
                );
                match err {
                    LlmError::MalformedToolInput {
                        tool_name,
                        block_index,
                        reason,
                        input_bytes,
                        has_other_tool_calls,
                    } => {
                        assert_eq!(tool_name, "StructuredOutput");
                        assert_eq!(block_index, 1);
                        assert_eq!(input_bytes, input.len());
                        assert!(!reason.is_empty());
                        assert_eq!(has_other_tool_calls, other_tool.is_some());
                    }
                    other => panic!("expected malformed tool input, got {other:?}"),
                }
            }
        }
    }

    #[tokio::test]
    async fn malformed_output_checks_later_tools_and_incomplete_streams() {
        for later_tool in [false, true] {
            for terminal in [false, true] {
                let mut evs = vec![
                    message_start("m1", "mock"),
                    HistoryEvent::ContentBlockStart {
                        index: 0,
                        content_block: ContentBlock::ToolCall { input_projection: None,
                            id: "output".into(),
                            name: "StructuredOutput".into(),
                            input: Value::Null,
                        },
                    },
                    HistoryEvent::ContentBlockDelta {
                        index: 0,
                        delta: HistoryContentDelta::InputJsonDelta {
                            partial_json: "{".into(),
                        },
                    },
                    HistoryEvent::ContentBlockStop { index: 0 },
                ];
                if later_tool {
                    evs.push(HistoryEvent::ContentBlockStart {
                        index: 1,
                        content_block: ContentBlock::ToolCall { input_projection: None,
                            id: "later".into(),
                            name: "Write".into(),
                            input: Value::Null,
                        },
                    });
                }
                if terminal {
                    evs.push(HistoryEvent::MessageStop);
                }
                let err = accumulate_stream(boxed(evs))
                    .await
                    .expect_err("malformed output");
                assert!(
                    matches!(err, LlmError::MalformedToolInput { has_other_tool_calls, .. } if has_other_tool_calls == (later_tool || !terminal))
                );
            }
        }
    }

    #[tokio::test]
    async fn transport_error_passes_through_verbatim() {
        let s: BoxStream<'static, Result<HistoryEvent, LlmError>> = stream::iter(vec![
            Ok(message_start("m1", "claude-mock")),
            Err(LlmError::Transport {
                message: "dropped".into(),
            }),
        ])
        .boxed();
        let err = accumulate_stream(s).await.expect_err("transport");
        assert!(matches!(err, LlmError::Transport { .. }));
    }

    #[tokio::test]
    async fn accumulator_remainder_preserves_later_assistant_responses() {
        let response = |id: &str, text: &str| HistoryResponse {
            id: id.into(),
            model: "mock-model".into(),
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: ExecutionUsage::default(),
            cost: None,
            provider_metadata: Value::Null,
        };
        let mut events = response_to_stream_events(response("first-id", "first"));
        events.extend(response_to_stream_events(response("second-id", "second")));
        let (first, rest) = accumulate_stream_salvaging_remaining(boxed(events))
            .await
            .expect("first response");
        assert_eq!(first.id, "first-id");
        let (second, mut rest) = accumulate_stream_salvaging_remaining(rest)
            .await
            .expect("second response");
        assert_eq!(second.id, "second-id");
        assert!(rest.next().await.is_none());
    }

    #[tokio::test]
    async fn physical_quote_is_not_reapplied_to_a_later_mod_response() {
        let estimate = crate::CostEstimate {
            pricing_model: crate::PricingModelRef {
                pricing_provider_id: crate::ProviderId::AnthropicFirstParty,
                billing_model: "claude-opus-4-7".into(),
                request_model: "claude-opus-4-7".into(),
                display_model: "claude-opus-4-7".into(),
            },
            total_cost_usd: Some(0.002),
            input_cost_usd: Some(0.001),
            output_cost_usd: Some(0.001),
            cache_read_cost_usd: Some(0.0),
            cache_write_cost_usd: Some(0.0),
            reasoning_cost_usd: Some(0.0),
            estimated: false,
            pricing_source: Some("captured-sdk-profile".into()),
        };
        let mut usage = ExecutionUsage::default();
        usage.cost_estimate = Some(estimate.clone());
        let events = vec![
            message_start("first", "claude-opus-4-7"),
            HistoryEvent::CostQuoteObserved {
                estimate: Some(estimate.clone()),
                native_server_fallback: true,
                summary_model: Some("claude-opus-4-7".into()),
            },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: Some(usage),
            },
            HistoryEvent::MessageStop,
            message_start("second", "claude-opus-4-7"),
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ];

        let (first, rest) = accumulate_stream_salvaging_remaining(boxed(events))
            .await
            .expect("the first response consumes its physical quote once");
        assert_eq!(first.cost, Some(estimate));
        let (second, mut rest) = accumulate_stream_salvaging_remaining(rest)
            .await
            .expect("the later Mod response has its own settlement scope");
        assert_eq!(second.id, "second");
        assert!(second.cost.is_none());
        assert!(rest.next().await.is_none());
    }

    #[tokio::test]
    async fn completed_event_short_circuits_response() {
        // A `Completed{response}` event immediately returns the contained
        // response without waiting for `MessageStop`.
        let resp = HistoryResponse {
            id: "cmp-1".into(),
            model: "claude-mock".into(),
            content: vec![ContentBlock::Text {
                text: "direct answer".into(),
                cache_control: None,
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: ExecutionUsage::default(),
            cost: None,
            provider_metadata: Value::Null,
        };
        let events = vec![HistoryEvent::Completed {
            response: Box::new(resp.clone()),
        }];
        let got = accumulate_stream(boxed(events)).await.expect("completed");
        assert_eq!(got.id, "cmp-1");
        assert_eq!(got.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(got.content.len(), 1);
        match &got.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "direct answer"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_side_blocks_preserved_through_round_trip() {
        // A `server_tool_use` block is now PRESERVED verbatim through the
        // streaming accumulator (captured from the `ContentBlockStart` event) so
        // resume/replay JSONL bytes stay intact — matching the non-streaming
        // `translate_response_blocks` preservation.
        let resp = HistoryResponse {
            id: "m1".into(),
            model: "claude-mock".into(),
            content: vec![
                ContentBlock::ServerToolUse {
                    id: "srv-1".into(),
                    name: "advisor".into(),
                    input: Value::Null,
                },
                ContentBlock::Text {
                    text: "kept".into(),
                    cache_control: None,
                    citations: None,
                },
            ],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: ExecutionUsage::default(),
            cost: None,
            provider_metadata: Value::Null,
        };
        let round = accumulate_stream(boxed(response_to_stream_events(resp)))
            .await
            .expect("round-trip");
        // Both the server_tool_use block and the text block survive.
        assert_eq!(round.content.len(), 2);
        assert!(matches!(
            &round.content[0],
            ContentBlock::ServerToolUse { id, name, .. } if id == "srv-1" && name == "advisor"
        ));
        assert!(matches!(round.content[1], ContentBlock::Text { .. }));
    }

    async fn assert_round_trips(resp: HistoryResponse) {
        // Drive the synthetic stream back through the accumulator.
        let events = response_to_stream_events(resp.clone());
        let round = accumulate_stream(boxed(events))
            .await
            .expect("round-trip accumulate");
        assert_eq!(round.id, resp.id);
        assert_eq!(round.model, resp.model);
        assert_eq!(round.stop_reason, resp.stop_reason);
        assert_eq!(
            serde_json::to_value(&round.content).unwrap(),
            serde_json::to_value(&resp.content).unwrap()
        );
        assert_eq!(
            round.usage.counts().input_tokens,
            resp.usage.counts().input_tokens
        );
        assert_eq!(
            round.usage.counts().output_tokens,
            resp.usage.counts().output_tokens
        );
        assert_eq!(
            round.usage.counts().cache_write_tokens,
            resp.usage.counts().cache_write_tokens
        );
        assert_eq!(
            round.usage.counts().cache_read_tokens,
            resp.usage.counts().cache_read_tokens
        );
    }

    #[tokio::test]
    async fn round_trip_text_and_tool_call_and_reasoning() {
        assert_round_trips(HistoryResponse {
            id: "m1".into(),
            model: "claude-mock".into(),
            content: vec![
                ContentBlock::Text {
                    text: "answer".into(),
                    cache_control: None,
                    citations: None,
                },
                ContentBlock::Reasoning {
                    text: "reason".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::ToolCall { input_projection: None,
                    id: "tc-abc".to_string(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "x.rs", "limit": 10}),
                },
            ],
            stop_reason: Some("tool_use".into()),
            stop_details: None,
            usage: ExecutionUsage {
                report: wire::UsageReport::measured(
                    wire::Usage {
                        input_tokens: 11,
                        output_tokens: 22,
                        cache_write_tokens: 1,
                        cache_read_tokens: 2,
                        reasoning_tokens: 0,

                        ..Default::default()
                    },
                    wire::UsageState::Partial,
                ),
                ..ExecutionUsage::default()
            },
            cost: None,
            provider_metadata: Value::Null,
        })
        .await;
    }

    #[tokio::test]
    async fn refusal_details_survive_the_history_stream_edge() {
        let mut response = HistoryResponse {
            id: "refusal".into(),
            model: "model".into(),
            content: Vec::new(),
            stop_reason: Some("refusal".into()),
            stop_details: Some(crate::HistoryStopDetails {
                category: Some("category".into()),
                explanation: Some("explanation".into()),
            }),
            usage: ExecutionUsage::default(),
            cost: None,
            provider_metadata: Value::Null,
        };
        let expected = response.stop_details.clone();
        response.content.push(ContentBlock::Text {
            text: "refused".into(),
            cache_control: None,
            citations: None,
        });
        let result = accumulate_stream(boxed(response_to_stream_events(response)))
            .await
            .unwrap();
        assert_eq!(result.stop_details, expected);
    }

    #[tokio::test]
    async fn round_trip_empty_tool_input() {
        assert_round_trips(HistoryResponse {
            id: "m2".into(),
            model: "claude-mock".into(),
            content: vec![ContentBlock::ToolCall { input_projection: None,
                id: "tc-xyz".to_string(),
                name: "Now".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: ExecutionUsage::default(),
            cost: None,
            provider_metadata: Value::Null,
        })
        .await;
    }
}
