//! Drive a streaming [`LlmEvent`] sequence to a single [`LlmResponse`] — the
//! exact value the non-streaming round-trip returns — so a caller's downstream
//! logic is byte-identical regardless of which transport produced the turn.
//!
//! Byte-locked against `orchestrator::sse`'s `BlockAccumulator` +
//! `event_router` (source semantics: `claude.ts:1995-2300`). It lived in the
//! `agent` crate because that crate cannot depend on the orchestrator without
//! closing a dependency cycle; it now lives HERE, next to the `LlmEvent` and
//! `LlmResponse` it is defined in terms of, so every consumer of a stream gets
//! the same assembly instead of growing a second, weaker one. `agent`
//! re-exports it and is otherwise unchanged.
//!
//! [`response_to_stream_events`] is the inverse: it synthesizes a lossless
//! [`LlmEvent`] sequence from an `LlmResponse`, so a client that only
//! implements the non-streaming round-trip can still present a streaming seam.
//! The two round-trip exactly (see the `round_trip_*` tests).
#![forbid(unsafe_code)]

use crate::{
    ContentBlock, ContentDelta, LlmError, LlmEvent, LlmResponse, MessageDeltaPayload, Usage,
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::Value;
use std::collections::HashMap;

/// Tag identifying what kind of content block is being accumulated.
///
/// Mirrors `orchestrator::sse::accumulator::BlockKind`. `ToolUse` carries the
/// API-provided id + name verbatim; the input JSON is reassembled from
/// `input_json_delta` chunks.
#[derive(Debug, Clone)]
enum BlockKind {
    /// A `text` block — accumulates `text_delta` chunks.
    Text,
    /// A `tool_use` / `tool_call` block — accumulates `input_json_delta` chunks.
    ToolCall {
        /// Stable identifier echoed back in the matching `ToolResult`.
        id: String,
        /// Tool name (e.g. `"Read"`).
        name: String,
    },
    /// A `thinking` / `reasoning` block — accumulates `thinking_delta` chunks.
    Reasoning,
    /// A low-frequency server-side block (`redacted_thinking`,
    /// `server_tool_use`, `connector_text`, `advisor_tool_result`) captured in
    /// full from the `ContentBlockStart` event and preserved verbatim for
    /// resume/replay byte parity. (`server_tool_use` may also stream its input
    /// via `input_json_delta`; that is merged on `stop_block`.)
    Preserved(ContentBlock),
    /// Any other variant the accumulator cannot represent. Stores nothing;
    /// `stop_block` returns [`CompletedBlock::Skipped`].
    Other,
}

impl BlockKind {
    fn name(&self) -> &'static str {
        match self {
            BlockKind::Text => "text",
            BlockKind::ToolCall { .. } => "tool_call",
            BlockKind::Reasoning => "reasoning",
            BlockKind::Preserved(_) => "preserved",
            BlockKind::Other => "other",
        }
    }
}

/// One finished content block, ready to be appended to the assistant message.
#[derive(Debug, Clone)]
enum CompletedBlock {
    /// Plain text.
    Text {
        /// Concatenated body of all `text_delta` chunks.
        text: String,
    },
    /// Tool invocation, with reassembled JSON input.
    ToolCall {
        /// Tool call identifier (string id as returned by the provider).
        id: String,
        /// Tool name.
        name: String,
        /// Reassembled tool input.
        input: Value,
    },
    /// Extended thinking / reasoning.
    Reasoning {
        /// Concatenated reasoning body.
        text: String,
        /// Optional cryptographic signature.
        signature: Option<String>,
    },
    /// A low-frequency server-side block captured verbatim from
    /// `ContentBlockStart` (see [`BlockKind::Preserved`]). Replayed unchanged.
    Preserved(ContentBlock),
    /// A [`BlockKind::Other`] variant — caller drops it.
    Skipped,
}

impl CompletedBlock {
    /// Project a finished block back onto the wire [`ContentBlock`] shape the
    /// [`LlmResponse`] carries. [`CompletedBlock::Skipped`] yields `None`
    /// (the block is dropped, exactly as the non-streaming
    /// `translate_response_blocks` drops server-side variants).
    fn into_content_block(self) -> Option<ContentBlock> {
        match self {
            CompletedBlock::Text { text } => Some(ContentBlock::Text {
                text,
                cache_control: None,
            }),
            CompletedBlock::ToolCall { id, name, input } => {
                Some(ContentBlock::ToolCall { id, name, input })
            }
            CompletedBlock::Reasoning { text, signature } => {
                Some(ContentBlock::Reasoning { text, signature })
            }
            CompletedBlock::Preserved(block) => Some(block),
            CompletedBlock::Skipped => None,
        }
    }
}

/// Per-block state held during accumulation.
#[derive(Debug)]
struct BlockState {
    kind: BlockKind,
    /// Used for `Text` and `Reasoning`.
    text_buf: String,
    /// Used for `ToolCall` (raw `partial_json` concat).
    json_buf: String,
    /// Set by `signature_delta` on a `Reasoning` block.
    signature: Option<String>,
}

/// In-progress accumulator. One per active stream consumer.
#[derive(Debug, Default)]
struct BlockAccumulator {
    blocks: HashMap<u32, BlockState>,
    /// Counts started client and server tools, including completed blocks.
    tool_calls_started: usize,
}

impl BlockAccumulator {
    fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            tool_calls_started: 0,
        }
    }

    /// Register a new block at `index`. An existing entry is overwritten,
    /// mirroring `claude.ts:1996-2070` (`contentBlocks[part.index] = { ... }`).
    fn start_block(&mut self, index: u32, kind: BlockKind) {
        if matches!(
            kind,
            BlockKind::ToolCall { .. } | BlockKind::Preserved(ContentBlock::ServerToolUse { .. })
        ) {
            self.tool_calls_started += 1;
        }
        self.blocks.insert(
            index,
            BlockState {
                kind,
                text_buf: String::new(),
                json_buf: String::new(),
                signature: None,
            },
        );
    }

    /// Append `text` to the `Text` or `Reasoning` buffer of the block at
    /// `index`.
    fn append_text(&mut self, index: u32, text: &str) -> Result<(), LlmError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| block_not_found(index))?;
        match &state.kind {
            BlockKind::Text | BlockKind::Reasoning => {
                state.text_buf.push_str(text);
                Ok(())
            }
            other => Err(type_mismatch(index, other.name(), "text_delta")),
        }
    }

    /// Append a `partial_json` chunk to the `ToolCall` buffer of the block at
    /// `index`.
    fn append_json(&mut self, index: u32, partial: &str) -> Result<(), LlmError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| block_not_found(index))?;
        match &state.kind {
            // `server_tool_use` (Preserved) may stream its input via
            // `input_json_delta`; buffer it and merge on stop.
            BlockKind::ToolCall { .. } | BlockKind::Preserved(_) => {
                state.json_buf.push_str(partial);
                Ok(())
            }
            other => Err(type_mismatch(index, other.name(), "input_json_delta")),
        }
    }

    /// Append `thinking` from a `thinking_delta` to the block at `index` —
    /// applying ONLY to a `Reasoning` (thinking) block, and a SILENT NO-OP
    /// otherwise (claude-code `case"thinking_delta":{if(n?.type==="thinking")…;
    /// break}`). This must NOT error: the API streams `thinking_delta`
    /// (`estimated_tokens` pings) DURING the redacted-thinking phase, so a
    /// `thinking_delta` legitimately arrives on a `redacted_thinking`
    /// ([`BlockKind::Preserved`]) block — the previous shared `append_text` path
    /// raised a `type mismatch` and terminated the stream (a spurious failure
    /// where CC succeeds). A `thinking_delta` for a non-existent block is also a
    /// no-op, mirroring CC's `n?.type` optional-chaining.
    fn append_thinking(&mut self, index: u32, text: &str) {
        if let Some(state) = self.blocks.get_mut(&index) {
            if matches!(state.kind, BlockKind::Reasoning) {
                state.text_buf.push_str(text);
            }
        }
    }

    /// Set the signature on a `Reasoning` block (from a `signature_delta`).
    fn set_signature(&mut self, index: u32, sig: &str) -> Result<(), LlmError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| block_not_found(index))?;
        match &state.kind {
            BlockKind::Reasoning => {
                state.signature = Some(sig.to_string());
                Ok(())
            }
            other => Err(type_mismatch(index, other.name(), "signature_delta")),
        }
    }

    /// Finalize the block at `index`, removing it and returning a
    /// [`CompletedBlock`].
    fn stop_block(&mut self, index: u32) -> Result<CompletedBlock, LlmError> {
        let state = self
            .blocks
            .remove(&index)
            .ok_or_else(|| double_stop(index))?;
        let completed = match state.kind {
            BlockKind::Text => CompletedBlock::Text {
                text: state.text_buf,
            },
            BlockKind::Reasoning => CompletedBlock::Reasoning {
                text: state.text_buf,
                signature: state.signature,
            },
            BlockKind::ToolCall { id, name } => {
                let input = if state.json_buf.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str::<Value>(&state.json_buf).map_err(|e| {
                        LlmError::MalformedToolInput {
                            tool_name: name.clone(),
                            block_index: index,
                            reason: e.to_string(),
                            input_bytes: state.json_buf.len(),
                            has_other_tool_calls: self.tool_calls_started > 1,
                        }
                    })?
                };
                CompletedBlock::ToolCall { id, name, input }
            }
            BlockKind::Preserved(mut block) => {
                // Merge any `input_json_delta`-streamed input into a
                // `server_tool_use` block (the start event seeds an empty input).
                if !state.json_buf.is_empty() {
                    if let ContentBlock::ServerToolUse { input, .. } = &mut block {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&state.json_buf) {
                            *input = parsed;
                        }
                    }
                }
                CompletedBlock::Preserved(block)
            }
            BlockKind::Other => CompletedBlock::Skipped,
        };
        Ok(completed)
    }
}

// ----- Error constructors (mirror `orchestrator::sse::StreamingError` Display,
// wrapped in `LlmError::StreamInterrupted` so the subagent loop's single
// `Result<LlmResponse, LlmError>` seam carries them) --------------------

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

/// Map a wire `content_block` payload to its accumulator [`BlockKind`].
/// Mirrors `orchestrator::sse::event_router`'s `ContentBlockStart` arm.
fn block_kind_of(content_block: &ContentBlock) -> BlockKind {
    match content_block {
        ContentBlock::Text { .. } | ContentBlock::TextJsUtf16 { .. } => BlockKind::Text,
        ContentBlock::ToolCall { id, name, .. } => BlockKind::ToolCall {
            id: id.clone(),
            name: name.clone(),
        },
        ContentBlock::Reasoning { .. } => BlockKind::Reasoning,
        // Low-frequency server-side blocks: captured verbatim from the start
        // event and preserved for resume/replay byte parity.
        ContentBlock::RedactedThinking { .. }
        | ContentBlock::ServerToolUse { .. }
        | ContentBlock::ConnectorText { .. }
        | ContentBlock::ProviderContent { .. }
        | ContentBlock::AdvisorToolResult { .. } => BlockKind::Preserved(content_block.clone()),
        ContentBlock::Image { .. }
        | ContentBlock::ImageUrl { .. }
        | ContentBlock::Document { .. }
        | ContentBlock::ToolResult { .. }
        // cache_edits is a request-only directive — never streamed back as a
        // content_block_start, so it never reaches the accumulator.
        | ContentBlock::CacheEdits { .. } => BlockKind::Other,
    }
}

/// Merge a `message_delta` usage snapshot into the `message_start` seed.
///
/// On the wire, `message_start.usage` carries `input_tokens` + cache counts
/// with `output_tokens` still `0`, and `message_delta.usage` carries the final
/// `output_tokens` (Anthropic omits the input/cache counts there). So
/// `output_tokens` always takes the delta value, while the input/cache counts
/// keep the seed unless the delta reports a non-zero override. This makes the
/// [`response_to_stream_events`] round-trip exact (the seed already equals the
/// final usage) and stays correct against a real provider stream.
///
/// Mapping from the old `UsageApi` flat fields:
///   - `input_tokens` → `billable_tokens.input`
///   - `output_tokens` → `billable_tokens.output`
///   - `cache_creation_input_tokens` → `billable_tokens.cache_write`
///   - `cache_read_input_tokens` → `billable_tokens.cache_read`
///   - `server_tool_use` → `server_tool_use`
///   - `speed` → `speed`
pub(crate) fn merge_usage(seed: &Usage, delta: &Usage) -> Usage {
    let bt_seed = &seed.billable_tokens;
    let bt_delta = &delta.billable_tokens;
    Usage {
        billable_tokens: crate::TokenUsage {
            input: if bt_delta.input > 0 {
                bt_delta.input
            } else {
                bt_seed.input
            },
            output: bt_delta.output,
            cache_write: if bt_delta.cache_write > 0 {
                bt_delta.cache_write
            } else {
                bt_seed.cache_write
            },
            cache_read: if bt_delta.cache_read > 0 {
                bt_delta.cache_read
            } else {
                bt_seed.cache_read
            },
            reasoning_output: if bt_delta.reasoning_output > 0 {
                bt_delta.reasoning_output
            } else {
                bt_seed.reasoning_output
            },
        },
        server_tool_use: delta.server_tool_use.or(seed.server_tool_use),
        speed: delta.speed.clone().or_else(|| seed.speed.clone()),
        // Preserve context_tokens / provider_reported_total / provider_metadata
        // from the delta if present, else keep the seed.
        context_tokens: delta.context_tokens.or(seed.context_tokens),
        provider_reported_total_tokens: delta
            .provider_reported_total_tokens
            .or(seed.provider_reported_total_tokens),
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
/// [`LlmResponse`] — the streaming analog of one non-streaming
/// `messages_create` round-trip. `id` / `model` / the usage seed come from
/// `message_start`; the final `stop_reason` + usage come from `message_delta`.
///
/// If the stream yields a [`LlmEvent::Completed`] event, the contained
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
    stream: BoxStream<'static, Result<LlmEvent, LlmError>>,
) -> Result<LlmResponse, LlmError> {
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
    mut stream: BoxStream<'static, Result<LlmEvent, LlmError>>,
) -> Result<LlmResponse, (Vec<ContentBlock>, LlmError)> {
    let mut acc = BlockAccumulator::new();
    let mut malformed_input: Option<LlmError> = None;
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut content_indices: Vec<(u32, bool)> = Vec::new();
    let mut id = String::new();
    let mut model = String::new();
    let mut usage = Usage::default();
    let mut stop_reason: Option<String> = None;
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
                LlmEvent::ContentBlockStart { content_block, .. }
                    if matches!(
                        content_block,
                        ContentBlock::ToolCall { .. } | ContentBlock::ServerToolUse { .. }
                    ) =>
                {
                    acc.tool_calls_started += 1;
                }
                LlmEvent::Completed { .. } => {
                    // A terminal snapshot may include calls without corresponding
                    // start events. Do not recover without complete event evidence.
                    return Err((content, malformed_input.expect("pending malformed input")));
                }
                LlmEvent::MessageStop => {
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
            LlmEvent::MessageStart { response } => {
                // Capture id/model + the usage seed from the start snapshot.
                id = response.id;
                model = response.model;
                usage = response.usage;
                cost = response.cost;
                provider_metadata = response.provider_metadata;
            }
            LlmEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                acc.start_block(index, block_kind_of(&content_block));
            }
            LlmEvent::ContentBlockDelta { index, delta } => match delta {
                ContentDelta::TextDelta { text } => salvage!(acc.append_text(index, &text)),
                ContentDelta::InputJsonDelta { partial_json } => {
                    salvage!(acc.append_json(index, &partial_json));
                }
                ContentDelta::ThinkingDelta { thinking } => {
                    // No-op on a non-thinking block (e.g. `redacted_thinking`),
                    // never a stream error — see [`append_thinking`].
                    acc.append_thinking(index, &thinking);
                }
                ContentDelta::SignatureDelta { signature } => {
                    salvage!(acc.set_signature(index, &signature));
                }
                // Dropped at the `translate_response_blocks` boundary.
                ContentDelta::CitationsDelta { .. } | ContentDelta::ConnectorTextDelta { .. } => {}
            },
            LlmEvent::ContentBlockStop { index } => {
                match acc.stop_block(index) {
                    Ok(block) => {
                        if let Some(block) = block.into_content_block() {
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
            LlmEvent::MessageDelta {
                delta,
                usage: delta_usage,
            } => {
                if let Some(sr) = delta.stop_reason {
                    stop_reason = Some(sr);
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
            LlmEvent::MessageStop => {
                return Ok(LlmResponse {
                    id,
                    model,
                    content,
                    stop_reason,
                    stop_details: None,
                    usage,
                    cost,
                    provider_metadata,
                });
            }
            // Short-circuit: the stream provider emits a fully-assembled
            // response in the `Completed` event — return it directly.
            // This is the canonical terminal for llm-runtime streams
            // (llm-runtime protocol.rs:302; drops Ping/Error from api-client).
            LlmEvent::Completed { response } => {
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

/// Synthesize a [`LlmEvent`] sequence that reconstructs `resp` exactly when
/// fed back through [`accumulate_stream`].
///
/// Used by the default [`crate::api::SubagentApiClient::messages_create_stream`]
/// impl so a client that only implements the non-streaming `messages_create`
/// still presents a streaming seam. The round-trip is lossless: `text` /
/// `reasoning` bodies ride a single delta (the `content_block_start` payload is
/// empty, exactly as on the wire), `tool_call` input rides one `input_json_delta`
/// (re-parsed on stop), and the full usage is seeded on `message_start` so the
/// [`merge_usage`] reconstruction reproduces `resp.usage`.
pub fn response_to_stream_events(resp: LlmResponse) -> Vec<LlmEvent> {
    let mut events = Vec::with_capacity(resp.content.len() * 3 + 3);
    // `message_start` carries id/model + a usage seed. On the wire the seed
    // holds input/cache with `output_tokens == 0`; here we seed the FULL usage
    // so the round-trip is exact regardless of the field split.
    events.push(LlmEvent::MessageStart {
        response: Box::new(LlmResponse {
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
                events.push(LlmEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::Text {
                        text: String::new(),
                        cache_control: None,
                    },
                });
                events.push(LlmEvent::ContentBlockDelta {
                    index,
                    delta: ContentDelta::TextDelta { text },
                });
            }
            ContentBlock::Reasoning { text, signature } => {
                events.push(LlmEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::Reasoning {
                        text: String::new(),
                        signature: None,
                    },
                });
                events.push(LlmEvent::ContentBlockDelta {
                    index,
                    delta: ContentDelta::ThinkingDelta { thinking: text },
                });
                if let Some(sig) = signature {
                    events.push(LlmEvent::ContentBlockDelta {
                        index,
                        delta: ContentDelta::SignatureDelta { signature: sig },
                    });
                }
            }
            ContentBlock::ToolCall { id, name, input } => {
                events.push(LlmEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: Value::Null,
                    },
                });
                // The accumulator reassembles + parses this back to `input`.
                events.push(LlmEvent::ContentBlockDelta {
                    index,
                    delta: ContentDelta::InputJsonDelta {
                        partial_json: input.to_string(),
                    },
                });
            }
            // Low-frequency server-side variants: emit only a start so the index
            // is consumed. The accumulator captures the full block from this
            // start event and yields `Preserved` on stop, so it round-trips
            // verbatim (matching `translate_response_blocks` preservation).
            other => {
                events.push(LlmEvent::ContentBlockStart {
                    index,
                    content_block: other,
                });
            }
        }
        events.push(LlmEvent::ContentBlockStop { index });
    }
    events.push(LlmEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: resp.stop_reason,
            stop_details: None,
        },
        usage: Some(resp.usage),
    });
    events.push(LlmEvent::MessageStop);
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenUsage;
    use futures::stream;

    fn boxed(events: Vec<LlmEvent>) -> BoxStream<'static, Result<LlmEvent, LlmError>> {
        stream::iter(events.into_iter().map(Ok)).boxed()
    }

    fn message_start(id: &str, model: &str) -> LlmEvent {
        LlmEvent::MessageStart {
            response: Box::new(LlmResponse {
                id: id.to_string(),
                model: model.to_string(),
                content: Vec::new(),
                stop_reason: None,
                stop_details: None,
                usage: Usage::default(),
                cost: None,
                provider_metadata: Value::Null,
            }),
        }
    }

    #[tokio::test]
    async fn host_observations_update_response_metadata_without_token_usage() {
        let metadata = serde_json::json!({"llm_client":{"web_search":[{"citations":[{"url":"https://example.com"}]}]}});
        let response = accumulate_stream(boxed(vec![
            message_start("m", "model"),
            LlmEvent::ContentBlockStart {
                index: u32::MAX,
                content_block: ContentBlock::ProviderContent {
                    protocol: "open_ai_responses".into(),
                    value: serde_json::json!({"type":"lingxi_observation","metadata":metadata}),
                },
            },
            LlmEvent::ContentBlockStop { index: u32::MAX },
            LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            LlmEvent::MessageStop,
        ]))
        .await
        .unwrap();
        assert_eq!(response.provider_metadata, metadata);
        assert_eq!(response.usage, Usage::default());
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
        let mut usage = Usage::default();
        usage.cost_estimate = Some(quote);
        let response = accumulate_stream(boxed(vec![
            message_start("m", "deepseek-flash"),
            LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: Some(usage),
            },
            LlmEvent::MessageStop,
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
            LlmEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta { text: "he".into() },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta { text: "llo".into() },
            },
            LlmEvent::ContentBlockStop { index: 0 },
            LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            LlmEvent::MessageStop,
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
            LlmEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolCall {
                    id: "tc-1".to_string(),
                    name: "Read".into(),
                    input: Value::Null,
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 1,
                delta: ContentDelta::InputJsonDelta {
                    partial_json: "{\"file".into(),
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 1,
                delta: ContentDelta::InputJsonDelta {
                    partial_json: "_path\":\"foo.rs\"}".into(),
                },
            },
            LlmEvent::ContentBlockStop { index: 1 },
            LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("tool_use".into()),
                    stop_details: None,
                },
                usage: None,
            },
            LlmEvent::MessageStop,
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
            LlmEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Reasoning {
                    text: String::new(),
                    signature: None,
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::ThinkingDelta {
                    thinking: "ponder".into(),
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::SignatureDelta {
                    signature: "sig-1".into(),
                },
            },
            LlmEvent::ContentBlockStop { index: 0 },
            LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            },
            LlmEvent::MessageStop,
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
    async fn final_usage_merges_start_seed_and_delta() {
        let evs = vec![
            LlmEvent::MessageStart {
                response: Box::new(LlmResponse {
                    id: "m1".into(),
                    model: "claude-mock".into(),
                    content: Vec::new(),
                    stop_reason: None,
                    stop_details: None,
                    usage: Usage {
                        billable_tokens: TokenUsage {
                            input: 42,
                            output: 0,
                            cache_write: 7,
                            cache_read: 3,
                            reasoning_output: 0,
                        },
                        ..Usage::default()
                    },
                    cost: None,
                    provider_metadata: Value::Null,
                }),
            },
            LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: Some(Usage {
                    billable_tokens: TokenUsage {
                        input: 0,
                        output: 99,
                        cache_write: 0,
                        cache_read: 0,
                        reasoning_output: 0,
                    },
                    ..Usage::default()
                }),
            },
            LlmEvent::MessageStop,
        ];
        let resp = accumulate_stream(boxed(evs)).await.expect("accumulate");
        // output from the delta; input/cache kept from the start seed.
        assert_eq!(resp.usage.billable_tokens.input, 42);
        assert_eq!(resp.usage.billable_tokens.output, 99);
        assert_eq!(resp.usage.billable_tokens.cache_write, 7);
        assert_eq!(resp.usage.billable_tokens.cache_read, 3);
    }

    #[tokio::test]
    async fn stream_without_message_stop_errors() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            LlmEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
            LlmEvent::ContentBlockStop { index: 0 },
            // no message_stop
        ];
        let err = accumulate_stream(boxed(evs)).await.expect_err("no stop");
        assert!(matches!(err, LlmError::StreamInterrupted { .. }));
    }

    #[tokio::test]
    async fn delta_before_start_is_malformed() {
        let evs = vec![
            message_start("m1", "claude-mock"),
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: "oops".into(),
                },
            },
            LlmEvent::MessageStop,
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
            LlmEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::InputJsonDelta {
                    partial_json: "{}".into(),
                },
            },
            LlmEvent::MessageStop,
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
            LlmEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::ThinkingDelta {
                    thinking: "leak".into(),
                },
            },
            LlmEvent::ContentBlockStop { index: 0 },
            LlmEvent::MessageStop,
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
            LlmEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolCall {
                    id: "tc-1".to_string(),
                    name: "Read".into(),
                    input: Value::Null,
                },
            },
            LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::InputJsonDelta {
                    partial_json: "{not json".into(),
                },
            },
            LlmEvent::ContentBlockStop { index: 0 },
            LlmEvent::MessageStop,
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
                    evs.push(LlmEvent::ContentBlockStart {
                        index: 0,
                        content_block,
                    });
                    if completed {
                        evs.push(LlmEvent::ContentBlockStop { index: 0 });
                    }
                }
                let input = r#"{"secret":"never-print-this""#;
                evs.extend([
                    LlmEvent::ContentBlockStart {
                        index: 1,
                        content_block: ContentBlock::ToolCall {
                            id: "output".into(),
                            name: "StructuredOutput".into(),
                            input: Value::Null,
                        },
                    },
                    LlmEvent::ContentBlockDelta {
                        index: 1,
                        delta: ContentDelta::InputJsonDelta {
                            partial_json: input.into(),
                        },
                    },
                    LlmEvent::ContentBlockStop { index: 1 },
                    LlmEvent::MessageStop,
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
                    LlmEvent::ContentBlockStart {
                        index: 0,
                        content_block: ContentBlock::ToolCall {
                            id: "output".into(),
                            name: "StructuredOutput".into(),
                            input: Value::Null,
                        },
                    },
                    LlmEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentDelta::InputJsonDelta {
                            partial_json: "{".into(),
                        },
                    },
                    LlmEvent::ContentBlockStop { index: 0 },
                ];
                if later_tool {
                    evs.push(LlmEvent::ContentBlockStart {
                        index: 1,
                        content_block: ContentBlock::ToolCall {
                            id: "later".into(),
                            name: "Write".into(),
                            input: Value::Null,
                        },
                    });
                }
                if terminal {
                    evs.push(LlmEvent::MessageStop);
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
        let s: BoxStream<'static, Result<LlmEvent, LlmError>> = stream::iter(vec![
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
        let resp = LlmResponse {
            id: "cmp-1".into(),
            model: "claude-mock".into(),
            content: vec![ContentBlock::Text {
                text: "direct answer".into(),
                cache_control: None,
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: Usage::default(),
            cost: None,
            provider_metadata: Value::Null,
        };
        let events = vec![LlmEvent::Completed {
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
        let resp = LlmResponse {
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
            usage: Usage::default(),
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

    async fn assert_round_trips(resp: LlmResponse) {
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
            round.usage.billable_tokens.input,
            resp.usage.billable_tokens.input
        );
        assert_eq!(
            round.usage.billable_tokens.output,
            resp.usage.billable_tokens.output
        );
        assert_eq!(
            round.usage.billable_tokens.cache_write,
            resp.usage.billable_tokens.cache_write
        );
        assert_eq!(
            round.usage.billable_tokens.cache_read,
            resp.usage.billable_tokens.cache_read
        );
    }

    #[tokio::test]
    async fn round_trip_text_and_tool_call_and_reasoning() {
        assert_round_trips(LlmResponse {
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
            usage: Usage {
                billable_tokens: TokenUsage {
                    input: 11,
                    output: 22,
                    cache_write: 1,
                    cache_read: 2,
                    reasoning_output: 0,
                },
                ..Usage::default()
            },
            cost: None,
            provider_metadata: Value::Null,
        })
        .await;
    }

    #[tokio::test]
    async fn round_trip_empty_tool_input() {
        assert_round_trips(LlmResponse {
            id: "m2".into(),
            model: "claude-mock".into(),
            content: vec![ContentBlock::ToolCall {
                id: "tc-xyz".to_string(),
                name: "Now".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: Usage::default(),
            cost: None,
            provider_metadata: Value::Null,
        })
        .await;
    }
}
