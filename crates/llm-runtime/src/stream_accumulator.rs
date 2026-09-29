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

// The SDK owns text/signature/JSON assembly. This map only remembers the host
// presentation kind and lifecycle so invalid UI event sequences fail locally.
#[derive(Debug, Clone)]
enum BlockKind {
    Text,
    ToolCall {
        id: String,
        name: String,
        server: Option<Value>,
    },
    Reasoning,
    Preserved(ContentBlock),
    Other,
}

#[derive(Debug, Default)]
struct BlockAccumulator {
    sdk: lingxi_llm_client::stream_assembly::StreamAccumulator,
    kinds: HashMap<u32, BlockKind>,
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
            Some(BlockKind::Preserved(_)) => {}
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
            BlockKind::Preserved(block) => Some(block),
            BlockKind::Other => None,
            BlockKind::Text => match content {
                Some(SdkBlock::Text { text, .. }) => Some(ContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
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
            BlockKind::ToolCall { id, name, server } => match content {
                Some(SdkBlock::ToolUse { input, .. }) => Some(if let Some(initial) = server {
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
                    ContentBlock::ToolCall {
                        id,
                        name,
                        input: input.clone(),
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
        ContentBlock::ToolCall { id, name, .. } => BlockKind::ToolCall {
            id: id.clone(),
            name: name.clone(),
            server: None,
        },
        ContentBlock::ServerToolUse { id, name, input } => BlockKind::ToolCall {
            id: id.clone(),
            name: name.clone(),
            server: Some(input.clone()),
        },
        ContentBlock::Reasoning { .. } => BlockKind::Reasoning,
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
    mut stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
) -> Result<HistoryResponse, (Vec<ContentBlock>, LlmError)> {
    let mut acc = BlockAccumulator::new();
    let mut malformed_input: Option<LlmError> = None;
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut content_indices: Vec<(u32, bool)> = Vec::new();
    let mut id = String::new();
    let mut model = String::new();
    let mut usage = ExecutionUsage::default();
    let mut stop_reason: Option<String> = None;
    let mut stop_details = None;
    let mut cost = None;
    let mut provider_metadata = Value::Null;

    // Surface `$result`'s error paired with the blocks completed so far; the
    // `content` move only happens on the diverging error branch.
    macro_rules! salvage {
        ($result:expr) => {
            match $result {
                Ok(v) => v,
                Err(err) => return Err((content, err)),
            }
        };
    }

    while let Some(item) = stream.next().await {
        // Transport-level error: salvage the completed blocks + surface it.
        let event = match item {
            Ok(event) => event,
            Err(err) => return Err((content, malformed_input.unwrap_or(err))),
        };
        if let Some(error) = malformed_input.as_mut() {
            match event {
                HistoryEvent::ContentBlockStart { content_block, .. }
                    if matches!(
                        content_block,
                        ContentBlock::ToolCall { .. } | ContentBlock::ServerToolUse { .. }
                    ) =>
                {
                    acc.tool_calls_started += 1;
                }
                HistoryEvent::Completed { .. } => {
                    // A terminal snapshot may include calls without corresponding
                    // start events. Do not recover without complete event evidence.
                    return Err((content, malformed_input.expect("pending malformed input")));
                }
                HistoryEvent::MessageStop => {
                    if let LlmError::MalformedToolInput {
                        has_other_tool_calls,
                        ..
                    } = error
                    {
                        *has_other_tool_calls = acc.tool_calls_started > 1;
                    }
                    return Err((content, malformed_input.expect("pending malformed input")));
                }
                _ => {}
            }
            continue;
        }
        match event {
            HistoryEvent::WebSearch { .. } => {} // Metadata is retained by the terminal snapshot.
            HistoryEvent::MessageStart { response } => {
                // Capture id/model + the usage seed from the start snapshot.
                id = response.id;
                model = response.model;
                usage = response.usage;
                cost = response.cost;
                provider_metadata = response.provider_metadata;
                stop_details = response.stop_details;
            }
            HistoryEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                acc.start_block(index, block_kind_of(&content_block));
            }
            HistoryEvent::ContentBlockDelta { index, delta } => match delta {
                HistoryContentDelta::TextDelta { text } => salvage!(acc.append_text(index, &text)),
                HistoryContentDelta::InputJsonDelta { partial_json } => {
                    salvage!(acc.append_json(index, &partial_json));
                }
                HistoryContentDelta::ThinkingDelta { thinking } => {
                    // No-op on a non-thinking block (e.g. `redacted_thinking`),
                    // never a stream error — see [`append_thinking`].
                    acc.append_thinking(index, &thinking);
                }
                HistoryContentDelta::SignatureDelta { signature } => {
                    salvage!(acc.set_signature(index, &signature));
                }
                // Dropped at the `translate_response_blocks` boundary.
                HistoryContentDelta::CitationsDelta { .. }
                | HistoryContentDelta::ConnectorTextDelta { .. } => {}
            },
            HistoryEvent::ContentBlockStop { index } => {
                match acc.stop_block(index) {
                    Ok(block) => {
                        if let Some(block) = block {
                            if let ContentBlock::ProviderContent { value, .. } = &block {
                                if value["type"] == "lingxi_observation" {
                                    if let Some(metadata) = value.get("metadata") {
                                        provider_metadata = metadata.clone();
                                    }
                                    continue;
                                }
                            }
                            let key = crate::stream_content_order(index);
                            let position =
                                content_indices.partition_point(|existing| existing <= &key);
                            content_indices.insert(position, key);
                            content.insert(position, block);
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
                        malformed_input = Some(error);
                    }
                    Err(error) => return Err((content, error)),
                }
            }
            HistoryEvent::MessageDelta {
                delta,
                usage: delta_usage,
            } => {
                if let Some(sr) = delta.stop_reason {
                    stop_reason = Some(sr);
                }
                if delta.stop_details.is_some() {
                    stop_details = delta.stop_details;
                }
                if let Some(mut u) = delta_usage {
                    if let Some(metadata) = u.provider_metadata.get("stream") {
                        provider_metadata = metadata.clone();
                    }
                    if let Some(estimate) = u.cost_estimate.take() {
                        cost = Some(estimate);
                    }
                    usage = merge_usage(&usage, &u);
                }
            }
            HistoryEvent::MessageStop => {
                return Ok(HistoryResponse {
                    id,
                    model,
                    content,
                    stop_reason,
                    stop_details,
                    usage,
                    cost,
                    provider_metadata,
                });
            }
            // Short-circuit: the stream provider emits a fully-assembled
            // response in the `Completed` event — return it directly.
            // This is the canonical terminal for llm-runtime streams
            // (llm-runtime protocol.rs:302; drops Ping/Error from api-client).
            HistoryEvent::Completed { response } => {
                return Ok(*response);
            }
        }
    }
    // Stream ended without a `message_stop` or `completed` event.
    if let Some(error) = malformed_input {
        return Err((content, error));
    }
    Err((
        content,
        LlmError::StreamInterrupted {
            message: "stream ended without message_stop or completed event".to_string(),
        },
    ))
}

/// Synthesize a [`HistoryEvent`] sequence that reconstructs `resp` exactly when
/// fed back through [`accumulate_stream`].
///
/// Used by the default [`crate::api::SubagentApiClient::messages_create_stream`]
/// impl so a client that only implements the non-streaming `messages_create`
/// still presents a streaming seam. The round-trip is lossless: `text` /
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
                    },
                });
                events.push(HistoryEvent::ContentBlockDelta {
                    index,
                    delta: HistoryContentDelta::TextDelta { text },
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
            ContentBlock::ToolCall { id, name, input } => {
                events.push(HistoryEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolCall {
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
    async fn tool_call_stream_reassembles_input_json() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            HistoryEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolCall {
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
            ContentBlock::ToolCall { id, name, input } => {
                assert_eq!(id, "tc-1");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "foo.rs");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
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
                content_block: ContentBlock::ToolCall {
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
                        ContentBlock::ToolCall {
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
                        content_block: ContentBlock::ToolCall {
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
                        content_block: ContentBlock::ToolCall {
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
                        content_block: ContentBlock::ToolCall {
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
    async fn completed_event_short_circuits_response() {
        // A `Completed{response}` event immediately returns the contained
        // response without waiting for `MessageStop`.
        let resp = HistoryResponse {
            id: "cmp-1".into(),
            model: "claude-mock".into(),
            content: vec![ContentBlock::Text {
                text: "direct answer".into(),
                cache_control: None,
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
                },
                ContentBlock::Reasoning {
                    text: "reason".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::ToolCall {
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
            content: vec![ContentBlock::ToolCall {
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
