//! `StreamEvent` → router-action dispatch.
//!
//! The streaming loop ([`crate::streaming_loop::pump_stream`]) pumps one
//! `StreamEvent` at a time through [`dispatch_event`], which:
//!
//! - Mutates the [`BlockAccumulator`] for `content_block_*` events.
//! - Calls back into [`OutputStream::emit_text`] for `text_delta` events
//!   (true per-token streaming).
//! - Returns a [`RouterAction`] hint for actions the streaming loop must
//!   take ON its own — namely `DispatchToolUse` (when a tool block
//!   reaches stop) and `EndOfStream` (when `message_stop` arrives).
#![forbid(unsafe_code)]

use super::accumulator::{BlockAccumulator, BlockKind, CompletedBlock};
use super::StreamingError;
use lingxi_core::host::OutputStream;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ToolUseId};
use lingxi_llm_client::protocol::JsonPayload;
use llm_runtime::{
    ContentBlock as LlmContentBlock, ExecutionUsage as Usage, HistoryContentDelta, HistoryEvent,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Result of routing one `StreamEvent`. The streaming loop acts on each.
#[derive(Debug, Clone)]
pub enum RouterAction {
    /// No further action — event handled internally (state mutation or
    /// output emit only).
    Continue,
    /// A `tool_use` block just completed at `content_block_stop`. The
    /// streaming loop spawns a dispatch IMMEDIATELY.
    DispatchToolUse {
        /// Tool use identifier.
        id: ToolUseId,
        /// Tool name.
        name: String,
        /// Reassembled tool input.
        input: Utf16JsonProjection,
        /// Verbatim provider-issued tool-call id, preserved for egress replay.
        provider_id: Option<String>,
    },
    /// A text or thinking block completed — append to the in-flight
    /// assistant message and continue.
    AppendAssistantBlock(ContentBlock),
    /// `message_delta` arrived with a `stop_reason`. Streaming loop
    /// records this and continues until `message_stop` arrives.
    /// `output_tokens` carries the delta's usage snapshot (A3 budget
    /// accounting); `0` when the delta had no usage.
    RecordStopReason {
        /// The final `stop_reason`.
        stop_reason: String,
        /// Cumulative output tokens from this delta's usage (`0` if absent).
        output_tokens: u64,
        /// Full usage snapshot from the `message_delta` (authoritative for
        /// billing). `None` when the delta carried no usage.
        usage: Option<Usage>,
        /// Refusal `stop_details` (`{category, explanation}`) from the delta —
        /// drives the terminal refusal message's cyber/bio variant. `None` for
        /// non-refusal deltas.
        stop_details: Option<llm_runtime::HistoryStopDetails>,
        /// Exact provider-native continuation metadata, if present.
        resumable: Option<Utf16JsonProjection>,
    },
    /// The current `message_delta` has no string `stop_reason` (A3). The
    /// streaming loop records `output_tokens` and continues.
    RecordUsage {
        /// Cumulative output tokens from this delta's usage.
        output_tokens: u64,
        /// Full usage snapshot from this delta.
        usage: Option<Usage>,
        /// Apply or clear these details only while the attempt has no sticky
        /// stop reason. A reasonless delta preserves an existing terminal state.
        stop_details: Option<llm_runtime::HistoryStopDetails>,
        /// Exact provider-native continuation metadata, if present.
        resumable: Option<Utf16JsonProjection>,
    },
    /// `message_stop` arrived — terminate the per-turn loop.
    EndOfStream,
}

/// Route one event through the accumulator + output sink. Returns the
/// next action for the streaming loop.
///
/// # Errors
/// Propagates [`StreamingError`] from accumulator mutations.
pub async fn dispatch_event(
    event: HistoryEvent,
    acc: &mut BlockAccumulator,
    output: &Arc<dyn OutputStream>,
    // P2-04 (MessageDisplay `displayContent`): when a `MessageDisplay` hook is
    // registered, live per-token `text_delta` emission is SUPPRESSED here so the
    // completed-message pass can render the (possibly hook-substituted) full text
    // exactly once — mirroring claude-code, whose live path flows through the
    // display flush rather than raw deltas (`Qff`, BIN off 229876575). The text
    // is still accumulated into the block; only the on-screen echo is withheld.
    // `false` ⇒ byte-identical live streaming (the no-hook common case).
    suppress_live_text: bool,
    suppress_live_thinking: bool,
) -> Result<RouterAction, StreamingError> {
    match event {
        // Hosted search consumers inspect this semantic event separately. Main
        // conversation attribution remains in the terminal metadata snapshot.
        HistoryEvent::WebSearch { .. } => Ok(RouterAction::Continue),
        HistoryEvent::ServerFallback { .. }
        | HistoryEvent::ResponseObserved { .. }
        | HistoryEvent::CostQuoteObserved { .. } => Ok(RouterAction::Continue),
        HistoryEvent::MessageStart { response } => {
            output.note_first_request_input_tokens(response.usage.counts().input_tokens);
            output.note_response_timing(
                lingxi_core::host::orchestrator::ResponseTimingEvent::MessageStart,
                std::time::Instant::now(),
            );
            // No-op for state; the loop already knows the model + id from
            // the turn invocation. claude-code captures `partialMessage`
            // and `ttftMs` here; we don't need those at the M5-04 wire.
            //
            // §0.7 "light up thinking/usage": `message_start` carries the
            // initial usage snapshot (input + cache-read tokens). Surface
            // it to the output sink so consumers see an early token count;
            // the final `message_delta` usage supersedes it.
            //
            // stream-json P1: notify the sink of the message id + model so
            // it can record them before accumulating per-delta blocks.
            //
            // stream-json P4: emit the reconstructed SSE event for
            // --include-partial-messages BEFORE the normal handling.
            if output.wants_partial_stream_events() {
                let usage_val = json!({
                    "input_tokens": response.usage.counts().input_tokens,
                    "cache_creation_input_tokens": response.usage.counts().cache_write_tokens,
                    "cache_read_input_tokens": response.usage.counts().cache_read_tokens,
                    "output_tokens": response.usage.counts().output_tokens.saturating_sub(response.usage.counts().reasoning_tokens),
                    "service_tier": "standard"
                });
                let event_json = serde_json::to_string(&json!({
                    "type": "message_start",
                    "message": {
                        "id": response.id,
                        "type": "message",
                        "role": "assistant",
                        "model": response.model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": usage_val
                    }
                }))
                .unwrap_or_default();
                output.emit_stream_event(&event_json, true).await;
            }
            output
                .emit_message_start(&response.id, &response.model)
                .await;
            emit_usage_if_present(output, &response.usage).await;
            Ok(RouterAction::Continue)
        }
        HistoryEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            output.note_response_timing(
                lingxi_core::host::orchestrator::ResponseTimingEvent::ContentFrame,
                std::time::Instant::now(),
            );
            // stream-json P4: reconstruct SSE event for --include-partial-messages.
            if output.wants_partial_stream_events() {
                let event_json = reconstruct_content_block_event_json(index, &content_block);
                output.emit_stream_event(&event_json, false).await;
            }
            let kind = match &content_block {
                LlmContentBlock::ProviderContent { protocol, value }
                    if protocol == "anthropic_messages"
                        && value.string_at("/type").as_deref() == Some("text") =>
                {
                    BlockKind::PreservedText(ContentBlock::ProviderContent {
                        protocol: protocol.clone(),
                        value: core_projection_from_payload(value),
                    })
                }
                LlmContentBlock::ProviderContent { protocol, value } => {
                    BlockKind::Preserved(ContentBlock::ProviderContent {
                        protocol: protocol.clone(),
                        value: core_projection_from_payload(value),
                    })
                }
                LlmContentBlock::Text {
                    text, citations, ..
                } => BlockKind::Text {
                    citations: citations.clone(),
                    utf16_code_units: None,
                    initial_text: text.clone(),
                },
                LlmContentBlock::TextJsUtf16 {
                    text,
                    utf16_code_units,
                    citations,
                    ..
                } => BlockKind::Text {
                    citations: citations.clone(),
                    utf16_code_units: Some(utf16_code_units.clone()),
                    initial_text: text.clone(),
                },
                LlmContentBlock::ToolCall { id, name, input } => BlockKind::ToolUse {
                    initial_input: Some(core_projection_from_payload(input)),
                    // The provider-issued id IS the canonical ToolUseId (byte
                    // parity with claude-code). The provider_id sidecar is left
                    // None — the id already carries the canonical value.
                    id: ToolUseId::from(id.clone()),
                    name: name.clone(),
                    provider_id: None,
                },
                LlmContentBlock::Reasoning { .. } => BlockKind::Thinking,
                // Low-frequency server-side blocks: captured in full from the
                // start event and preserved verbatim for resume/replay byte parity.
                LlmContentBlock::RedactedThinking { data } => {
                    BlockKind::Preserved(ContentBlock::RedactedThinking { data: data.clone() })
                }
                LlmContentBlock::ServerToolUse { id, name, input } => {
                    BlockKind::Preserved(ContentBlock::ServerToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: core_projection_from_payload(input),
                    })
                }
                LlmContentBlock::ConnectorText {
                    connector_text,
                    signature,
                } => BlockKind::Preserved(ContentBlock::ConnectorText {
                    connector_text: connector_text.clone(),
                    signature: signature.clone(),
                }),
                LlmContentBlock::AdvisorToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => BlockKind::Preserved(ContentBlock::AdvisorToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: core_projection_from_payload(content),
                    is_error: *is_error,
                }),
                LlmContentBlock::Image { .. }
                | LlmContentBlock::ImageUrl { .. }
                | LlmContentBlock::Document { .. }
                | LlmContentBlock::ToolResult { .. }
                // cache_edits is a request-only directive — never streamed back.
                | LlmContentBlock::CacheEdits { .. } => BlockKind::Other,
            };
            acc.start_block(index, kind)?;
            if !suppress_live_text {
                match &content_block {
                    LlmContentBlock::Text { text, .. } if !text.is_empty() => {
                        output
                            .emit_text(&Utf16JsonProjection::from(text.as_str()))
                            .await;
                    }
                    LlmContentBlock::TextJsUtf16 {
                        text,
                        utf16_code_units,
                        ..
                    } if !text.is_empty() => {
                        output
                            .emit_text(
                                &Utf16JsonProjection::root_string(
                                    text.clone(),
                                    utf16_code_units.clone(),
                                )
                                .expect("validated exact start text"),
                            )
                            .await;
                    }
                    LlmContentBlock::ProviderContent { protocol, value }
                        if protocol == "anthropic_messages"
                            && value.string_at("/type").as_deref() == Some("text") =>
                    {
                        if let Some(units) = value
                            .payload_at("/text")
                            .and_then(|text| text.root_string_code_units())
                            .filter(|units| !units.is_empty())
                        {
                            let text = Utf16JsonProjection::root_string(
                                String::from_utf16_lossy(&units),
                                units,
                            )
                            .expect("validated provider start text");
                            output.emit_text(&text).await;
                        }
                    }
                    _ => {}
                }
            }
            Ok(RouterAction::Continue)
        }
        HistoryEvent::ContentBlockDelta { index, delta } => {
            output.note_response_timing(
                lingxi_core::host::orchestrator::ResponseTimingEvent::ContentFrame,
                std::time::Instant::now(),
            );
            // stream-json P4: reconstruct SSE event for --include-partial-messages.
            if output.wants_partial_stream_events() {
                if let Some(event_json) = reconstruct_delta_event_json(index, &delta) {
                    output.emit_stream_event(&event_json, false).await;
                }
            }
            match delta {
                HistoryContentDelta::TextDelta { text } => {
                    acc.append_text(index, &text)?;
                    // Stream the token to the output sink RIGHT NOW.
                    // This is the key M5-04 behavior: tokens are
                    // surfaced as they arrive, not buffered per-block.
                    // P2-04: withheld when a `MessageDisplay` hook is active — the
                    // completed-message pass renders the full (possibly
                    // substituted) text once (see the `suppress_live_text` doc).
                    if !suppress_live_text {
                        let text_projection = Utf16JsonProjection::root_string(
                            text.clone(),
                            text.encode_utf16().collect(),
                        )
                        .expect("UTF-8 text delta is a valid UTF-16 JSON string");
                        output.emit_text(&text_projection).await;
                    }
                }
                HistoryContentDelta::TextJsUtf16Delta {
                    text,
                    utf16_code_units,
                } => {
                    acc.append_text_utf16(index, &utf16_code_units)?;
                    if !suppress_live_text {
                        let text_projection = Utf16JsonProjection::root_string(
                            text.clone(),
                            utf16_code_units.clone(),
                        )
                        .expect("text delta display matches its exact UTF-16 units");
                        output.emit_text(&text_projection).await;
                    }
                }
                HistoryContentDelta::InputJsonDelta { partial_json } => {
                    acc.append_json(index, &partial_json)?;
                }
                HistoryContentDelta::ThinkingDelta { thinking } => {
                    acc.append_text(index, &thinking)?;
                    // §0.7 "light up thinking/usage": stream the reasoning
                    // delta to the output sink RIGHT NOW, mirroring the
                    // `TextDelta` arm above. `signature` is `None` on the
                    // live delta — the cryptographic signature only arrives
                    // on the completed thinking block (`SignatureDelta`).
                    if !suppress_live_thinking {
                        output.emit_thinking(&thinking, None).await;
                    }
                }
                HistoryContentDelta::SignatureDelta { signature } => {
                    acc.set_signature(index, &signature)?;
                }
                HistoryContentDelta::CitationsDelta { citation } => {
                    acc.append_citation(index, citation)?;
                }
                HistoryContentDelta::TextCitations { citations } => {
                    acc.set_text_citations(index, citations)?;
                }
                HistoryContentDelta::ProviderContentSnapshot { value } => {
                    acc.set_provider_content_snapshot(index, core_projection_from_payload(&value))?;
                }
                HistoryContentDelta::ConnectorTextDelta { .. } => {
                    // Connector text deltas are not represented in the local
                    // content-block accumulator.
                }
            }
            Ok(RouterAction::Continue)
        }
        HistoryEvent::ContentBlockStop { index } => {
            // stream-json P4: reconstruct SSE event for --include-partial-messages.
            if output.wants_partial_stream_events() {
                let event_json = serde_json::to_string(&json!({
                    "type": "content_block_stop",
                    "index": index
                }))
                .unwrap_or_default();
                output.emit_stream_event(&event_json, false).await;
            }
            let completed = acc.stop_block(index)?;
            if !matches!(&completed, CompletedBlock::Skipped) {
                output.note_response_timing(
                    lingxi_core::host::orchestrator::ResponseTimingEvent::AssistantMessage,
                    std::time::Instant::now(),
                );
            }
            match completed {
                CompletedBlock::Text {
                    text,
                    citations,
                    utf16_code_units,
                } => {
                    let block = match utf16_code_units {
                        Some(utf16_code_units) => ContentBlock::TextJsUtf16 {
                            text,
                            utf16_code_units,
                            citations: citations_to_core(citations),
                        },
                        None => ContentBlock::Text {
                            text,
                            citations: citations_to_core(citations),
                        },
                    };
                    Ok(RouterAction::AppendAssistantBlock(block))
                }
                CompletedBlock::Thinking {
                    thinking,
                    signature,
                } => Ok(RouterAction::AppendAssistantBlock(ContentBlock::Thinking {
                    thinking,
                    signature,
                })),
                CompletedBlock::ToolUse {
                    id,
                    name,
                    input,
                    provider_id,
                    ..
                } => {
                    // (cc 2.1.218 `jYd`) The STREAMING tool_use assembly must get
                    // the same literal-`\uXXXX` repair as the batched
                    // `translate_response_blocks` — the oracle's `Uun` is a single
                    // conversion shared by both paths. This is the PRIMARY
                    // interactive path (TUI / bridge / mobile all stream), and the
                    // repaired input must land here, BEFORE the tool executes and
                    // before the block is pushed into the assistant message/JSONL.
                    let (input, _stats) =
                        llm_runtime::unicode_repair::repair_tool_input(&name, &input);
                    Ok(RouterAction::DispatchToolUse {
                        id,
                        name,
                        input,
                        provider_id,
                    })
                }
                // Low-frequency server-side block preserved verbatim from the
                // start event — appended to the assistant message unchanged so
                // resume/replay JSONL bytes stay intact.
                CompletedBlock::Preserved(block) => Ok(RouterAction::AppendAssistantBlock(block)),
                CompletedBlock::Skipped => Ok(RouterAction::Continue),
            }
        }
        HistoryEvent::MessageDelta { delta, usage } => {
            // stream-json P4: reconstruct SSE event for --include-partial-messages.
            if output.wants_partial_stream_events() {
                let usage_val = usage.as_ref().map(|u| {
                    json!({
                        "output_tokens": u.counts().output_tokens.saturating_sub(u.counts().reasoning_tokens)
                    })
                });
                let mut delta_obj = serde_json::Map::new();
                if let Some(sr) = &delta.stop_reason {
                    delta_obj.insert("stop_reason".into(), json!(sr));
                } else {
                    delta_obj.insert("stop_reason".into(), Value::Null);
                }
                delta_obj.insert("stop_sequence".into(), Value::Null);
                let event_json = serde_json::to_string(&json!({
                    "type": "message_delta",
                    "delta": Value::Object(delta_obj),
                    "usage": usage_val.unwrap_or(Value::Null)
                }))
                .unwrap_or_default();
                output.emit_stream_event(&event_json, false).await;
            }
            // §0.7 "light up thinking/usage": `message_delta` carries the
            // final usage snapshot. Surface it to the output sink BEFORE
            // computing the router action — the stop-reason behavior below
            // is unchanged.
            // A3: capture the output-token count before `usage` is consumed
            // by the emit helper, so the budget loop can accumulate it.
            // BILLING: clone the full usage BEFORE emit consumes it so the
            // caller (pump_stream → try_run_turn_streaming) can record it in
            // CostTracker. The `message_delta` usage is the authoritative
            // final snapshot (includes both input and output tokens).
            let output_tokens = usage.as_ref().map_or(0, |u| {
                u.counts()
                    .output_tokens
                    .saturating_sub(u.counts().reasoning_tokens)
            });
            let usage_for_billing = usage.clone();
            let resumable = delta.resumable.map(|source| {
                Utf16JsonProjection::parse(source.as_json_source())
                    .expect("SDK JsonPayload source is valid JSON")
            });
            if let Some(usage) = usage {
                emit_usage_if_present(output, &usage).await;
            }
            if let Some(sr) = delta.stop_reason {
                Ok(RouterAction::RecordStopReason {
                    stop_reason: sr,
                    output_tokens,
                    usage: usage_for_billing,
                    stop_details: delta.stop_details,
                    resumable,
                })
            } else {
                Ok(RouterAction::RecordUsage {
                    output_tokens,
                    usage: usage_for_billing,
                    stop_details: delta.stop_details,
                    resumable,
                })
            }
        }
        // NOTE: HistoryEvent has no Ping or Error variants — errors surface as
        // Err(LlmError) from the stream, and keepalives are never forwarded
        // from the transport layer. The Completed short-circuit terminal is
        // treated as an end-of-stream signal (the full response is available
        // in the response field but we forward the already-accumulated blocks).
        HistoryEvent::MessageStop | HistoryEvent::Completed { .. } => {
            // stream-json P4: emit message_stop for --include-partial-messages.
            if output.wants_partial_stream_events() {
                let event_json = serde_json::to_string(&json!({
                    "type": "message_stop"
                }))
                .unwrap_or_default();
                output.emit_stream_event(&event_json, false).await;
            }
            Ok(RouterAction::EndOfStream)
        }
    }
}

// ── SSE reconstruction helpers ───────────────────────────────────────────────

/// Reconstruct the JSON value for an `LlmContentBlock` at stream start
/// (used in the `content_block_start` SSE event for P4 partial-messages).
fn reconstruct_content_block_event_json(index: u32, block: &LlmContentBlock) -> String {
    let block_source = match block {
        LlmContentBlock::ProviderContent { value, .. } => value.as_json_source().to_owned(),
        LlmContentBlock::Text { citations, .. }
        | LlmContentBlock::TextJsUtf16 { citations, .. } => {
            let mut payload = JsonPayload::parse(r#"{"type":"text","text":""}"#)
                .expect("static text block is valid JSON");
            if let Some(citations) = citations {
                let citations = citations
                    .clone()
                    .unwrap_or_else(|| JsonPayload::parse("null").expect("null is valid JSON"));
                payload = payload
                    .with_object_field("citations", &citations)
                    .expect("text block is an object payload");
            }
            payload.as_json_source().to_owned()
        }
        LlmContentBlock::ToolCall { id, name, .. } => generated_payload_source(
            &json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        ),
        LlmContentBlock::Reasoning { .. } => {
            generated_payload_source(&json!({"type": "thinking", "thinking": ""}))
        }
        LlmContentBlock::RedactedThinking { data } => {
            generated_payload_source(&json!({"type": "redacted_thinking", "data": data}))
        }
        LlmContentBlock::ServerToolUse { id, name, input } => {
            let mut payload = JsonPayload::from_generated_value(
                &json!({"type": "server_tool_use", "id": id, "name": name, "input": null}),
            )
            .expect("host-generated server tool block is valid JSON");
            payload = payload
                .with_object_field("input", input)
                .expect("server tool block is an object payload");
            payload.as_json_source().to_owned()
        }
        LlmContentBlock::ConnectorText {
            connector_text,
            signature,
        } => generated_payload_source(&json!({
            "type": "connector_text",
            "connector_text": connector_text,
            "signature": signature
        })),
        LlmContentBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut payload = JsonPayload::from_generated_value(&json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": null,
                "is_error": is_error
            }))
            .expect("host-generated advisor result is valid JSON");
            payload = payload
                .with_object_field("content", content)
                .expect("advisor result block is an object payload");
            payload.as_json_source().to_owned()
        }
        _ => generated_payload_source(&json!({"type": "unknown"})),
    };
    format!(
        "{{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{block_source}}}"
    )
}

fn generated_payload_source(value: &Value) -> String {
    JsonPayload::from_generated_value(value)
        .expect("host-generated stream event block is valid JSON")
        .as_json_source()
        .to_owned()
}

fn reconstruct_delta_event_json(index: u32, delta: &HistoryContentDelta) -> Option<String> {
    if let HistoryContentDelta::TextJsUtf16Delta {
        utf16_code_units, ..
    } = delta
    {
        let value = json!({
            "type": "content_block_delta",
            "index": index,
            "delta": reconstruct_delta_json(delta)?
        });
        let mut projection = Utf16JsonProjection::from_serde_value(&value).ok()?;
        projection
            .set_string_units("/delta/text", utf16_code_units)
            .ok()?;
        return Some(projection.to_json_string().unwrap_or_default());
    }
    let exact_delta_source = match delta {
        HistoryContentDelta::InputJsonDelta { partial_json } => Some(format!(
            "{{\"type\":\"input_json_delta\",\"partial_json\":{}}}",
            JsonPayload::string_from_utf16_code_units(partial_json.utf16_code_units())
                .as_json_source()
        )),
        HistoryContentDelta::CitationsDelta { citation } => Some(format!(
            "{{\"type\":\"citations_delta\",\"citation\":{}}}",
            citation.as_json_source()
        )),
        HistoryContentDelta::ConnectorTextDelta { connector_text } => Some(format!(
            "{{\"type\":\"connector_text_delta\",\"connector_text\":{}}}",
            JsonPayload::string_from_utf16_code_units(connector_text.utf16_code_units())
                .as_json_source()
        )),
        _ => None,
    };
    if let Some(delta_source) = exact_delta_source {
        return Some(format!(
            "{{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{delta_source}}}"
        ));
    }
    reconstruct_delta_json(delta).map(|delta| {
        serde_json::to_string(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": delta
        }))
        .unwrap_or_default()
    })
}

fn core_projection_from_payload(payload: &JsonPayload) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(payload.as_json_source())
        .expect("SDK JSON payload is valid JSON source")
}

fn citations_to_core(
    citations: Option<Option<JsonPayload>>,
) -> Option<Option<Utf16JsonProjection>> {
    citations.map(|citation| citation.map(|payload| core_projection_from_payload(&payload)))
}

/// Reconstruct the JSON value for a `HistoryContentDelta`
/// (used in the `content_block_delta` SSE event for P4 partial-messages).
fn reconstruct_delta_json(delta: &HistoryContentDelta) -> Option<Value> {
    Some(match delta {
        HistoryContentDelta::TextDelta { text } => json!({"type": "text_delta", "text": text}),
        HistoryContentDelta::TextJsUtf16Delta { text, .. } => {
            json!({"type": "text_delta", "text": text})
        }
        HistoryContentDelta::InputJsonDelta { .. } => return None,
        HistoryContentDelta::ThinkingDelta { thinking } => {
            json!({"type": "thinking_delta", "thinking": thinking})
        }
        HistoryContentDelta::SignatureDelta { signature } => {
            json!({"type": "signature_delta", "signature": signature})
        }
        HistoryContentDelta::CitationsDelta { .. }
        | HistoryContentDelta::ConnectorTextDelta { .. } => return None,
        HistoryContentDelta::TextCitations { .. }
        | HistoryContentDelta::ProviderContentSnapshot { .. } => return None,
    })
}

/// Surface an SSE `usage` snapshot to the output sink as a live usage
/// update (§0.7 "light up thinking/usage").
///
/// Maps `llm_runtime::ExecutionUsage` onto the four bare `u64` arguments of
/// [`OutputStream::emit_usage`] with the same field mapping the cost
/// pipeline uses, with visible output excluding the SDK reasoning subset.
async fn emit_usage_if_present(output: &Arc<dyn OutputStream>, usage: &Usage) {
    output
        .emit_usage(
            usage.counts().input_tokens,
            usage
                .counts()
                .output_tokens
                .saturating_sub(usage.counts().reasoning_tokens),
            usage.counts().cache_read_tokens,
            usage.counts().cache_write_tokens,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockOutputStream;
    use llm_runtime::HistoryMessageDelta;

    #[derive(Default)]
    struct ExactTextOutput {
        text: std::sync::Mutex<Option<Utf16JsonProjection>>,
        chunks: tokio::sync::Mutex<Vec<Utf16JsonProjection>>,
    }

    #[async_trait::async_trait]
    impl OutputStream for ExactTextOutput {
        async fn emit_text(&self, text: &Utf16JsonProjection) {
            self.chunks.lock().await.push(text.clone());
            *self.text.lock().expect("captured output text") = Some(text.clone());
        }

        async fn emit_tool_call(
            &self,
            _: &lingxi_core::types::ToolUseId,
            _: &str,
            _: &Utf16JsonProjection,
        ) {
        }

        async fn emit_tool_result(
            &self,
            _: &lingxi_core::types::ToolUseId,
            _: &str,
            _: &Utf16JsonProjection,
            _: &Utf16JsonProjection,
            _mcp_meta: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        ) {
        }

        async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
    }

    #[tokio::test]
    async fn utf16_start_and_delta_keep_exact_units_without_duplicate_start_text() {
        let mut acc = BlockAccumulator::new();
        let capture = Arc::new(ExactTextOutput::default());
        let output: Arc<dyn OutputStream> = capture.clone();
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: LlmContentBlock::TextJsUtf16 {
                    text: "�".into(),
                    utf16_code_units: vec![0xd800],
                    cache_control: None,
                    citations: None,
                },
            },
            &mut acc,
            &output,
            false,
            false,
        )
        .await
        .unwrap();
        dispatch_event(
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextJsUtf16Delta {
                    text: "�".into(),
                    utf16_code_units: vec![0xdc00],
                },
            },
            &mut acc,
            &output,
            false,
            false,
        )
        .await
        .unwrap();
        let action = dispatch_event(
            HistoryEvent::ContentBlockStop { index: 0 },
            &mut acc,
            &output,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            *capture.chunks.lock().await,
            vec![
                Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap(),
                Utf16JsonProjection::root_string("�".into(), vec![0xdc00]).unwrap()
            ]
        );
        assert!(
            matches!(action, RouterAction::AppendAssistantBlock(ContentBlock::TextJsUtf16 { utf16_code_units, .. }) if utf16_code_units == [0xd800, 0xdc00])
        );
    }

    #[tokio::test]
    async fn nonempty_plain_start_emits_once_and_display_hook_suppresses_rich_start() {
        let mut acc = BlockAccumulator::new();
        let capture = Arc::new(ExactTextOutput::default());
        let output: Arc<dyn OutputStream> = capture.clone();
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: LlmContentBlock::Text {
                    text: "seed".into(),
                    cache_control: None,
                    citations: None,
                },
            },
            &mut acc,
            &output,
            false,
            false,
        )
        .await
        .unwrap();
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 1,
                content_block: LlmContentBlock::TextJsUtf16 {
                    text: "�".into(),
                    utf16_code_units: vec![0xdfff],
                    cache_control: None,
                    citations: None,
                },
            },
            &mut acc,
            &output,
            true,
            false,
        )
        .await
        .unwrap();
        let action = dispatch_event(
            HistoryEvent::ContentBlockStop { index: 1 },
            &mut acc,
            &output,
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            *capture.chunks.lock().await,
            vec![Utf16JsonProjection::from("seed")]
        );
        assert!(
            matches!(action, RouterAction::AppendAssistantBlock(ContentBlock::TextJsUtf16 { utf16_code_units, .. }) if utf16_code_units == [0xdfff])
        );
    }

    #[test]
    fn utf16_partial_delta_keeps_native_json_text_without_private_fields() {
        let encoded = reconstruct_delta_event_json(
            7,
            &HistoryContentDelta::TextJsUtf16Delta {
                text: "�".into(),
                utf16_code_units: vec![0xd800],
            },
        )
        .unwrap();
        assert!(encoded.contains("\\ud800"), "{encoded}");
        assert!(encoded.contains("text_delta"), "{encoded}");
        assert!(!encoded.contains("utf16_code_units"), "{encoded}");
    }

    #[tokio::test]
    async fn text_delta_emits_to_output_and_accumulates() {
        let mut acc = BlockAccumulator::new();
        let mock = Arc::new(MockOutputStream::new());
        let out: Arc<dyn OutputStream> = mock.clone();
        // start a text block
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: LlmContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                    citations: None,
                },
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("start");
        // delta
        dispatch_event(
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta { text: "hi".into() },
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("delta");
        // OutputStream observed exactly one emit_text("hi")
        let events = mock.snapshot().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn text_js_utf16_delta_emits_exact_units_to_output() {
        let mut acc = BlockAccumulator::new();
        let out = Arc::new(ExactTextOutput::default());
        let output: Arc<dyn OutputStream> = out.clone();
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 2,
                content_block: LlmContentBlock::TextJsUtf16 {
                    text: String::new(),
                    utf16_code_units: Vec::new(),
                    citations: None,
                    cache_control: None,
                },
            },
            &mut acc,
            &output,
            false,
            false,
        )
        .await
        .expect("text block start");
        dispatch_event(
            HistoryEvent::ContentBlockDelta {
                index: 2,
                delta: HistoryContentDelta::TextJsUtf16Delta {
                    text: "�".into(),
                    utf16_code_units: vec![0xd800],
                },
            },
            &mut acc,
            &output,
            false,
            false,
        )
        .await
        .expect("text delta");

        assert_eq!(
            out.text
                .lock()
                .expect("captured output text")
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some(vec![0xd800])
        );
    }

    #[tokio::test]
    async fn opaque_anthropic_text_deltas_are_visible_and_finish_as_one_raw_block() {
        let mut acc = BlockAccumulator::new();
        let mock = Arc::new(MockOutputStream::new());
        let out: Arc<dyn OutputStream> = mock.clone();
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 3,
                content_block: LlmContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: JsonPayload::from_generated_value(&json!({
                        "type":"text",
                        "text":"",
                        "future_annotation":{"keep":true}
                    }))
                    .unwrap(),
                },
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("opaque text start");
        for text in ["visible ", "answer"] {
            dispatch_event(
                HistoryEvent::ContentBlockDelta {
                    index: 3,
                    delta: HistoryContentDelta::TextDelta { text: text.into() },
                },
                &mut acc,
                &out,
                false,
                false,
            )
            .await
            .expect("opaque text delta");
        }
        dispatch_event(
            HistoryEvent::ContentBlockDelta {
                index: 3,
                delta: HistoryContentDelta::ProviderContentSnapshot {
                    value: JsonPayload::from_generated_value(&json!({
                        "type":"text",
                        "text":"visible answer",
                        "citations":null,
                        "future_annotation":{"keep":true}
                    }))
                    .unwrap(),
                },
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("opaque text final snapshot");

        assert_eq!(
            mock.text_events().await,
            vec!["visible ".to_owned(), "answer".to_owned()]
        );
        assert!(matches!(
            dispatch_event(
                HistoryEvent::ContentBlockStop { index: 3 },
                &mut acc,
                &out,
                false,
                false,
            )
            .await
            .expect("opaque text stop"),
            RouterAction::AppendAssistantBlock(ContentBlock::ProviderContent {
                protocol,
                value,
            }) if protocol == "anthropic_messages"
                && value.string_at("/text").as_deref() == Some("visible answer")
                && value.at_pointer("/citations").is_some_and(|value| value.is_null())
                && value
                    .at_pointer("/future_annotation/keep")
                    .and_then(|value| value.as_bool())
                    == Some(true)
        ));
    }

    #[tokio::test]
    async fn tool_use_json_delta_preserves_lone_utf16_units_through_dispatch() {
        let mut acc = BlockAccumulator::new();
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        dispatch_event(
            HistoryEvent::ContentBlockStart {
                index: 4,
                content_block: LlmContentBlock::ToolCall {
                    id: "toolu_utf16".into(),
                    name: "Read".into(),
                    input: JsonPayload::parse("{}").unwrap(),
                },
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("tool start");

        let mut units = "{\"text\":\"".encode_utf16().collect::<Vec<_>>();
        units.push(0xd800);
        units.extend("\"}".encode_utf16());
        dispatch_event(
            HistoryEvent::ContentBlockDelta {
                index: 4,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json:
                        lingxi_llm_client::protocol::JsonPayloadFragment::from_utf16_code_units(
                            units,
                        ),
                },
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("tool input delta");

        let action = dispatch_event(
            HistoryEvent::ContentBlockStop { index: 4 },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("tool stop");
        assert!(matches!(
            action,
            RouterAction::DispatchToolUse { input, .. }
                if input.string_units("/text") == Some(vec![0xd800])
        ));
    }

    #[tokio::test]
    async fn message_delta_records_stop_reason() {
        let mut acc = BlockAccumulator::new();
        let mock = Arc::new(MockOutputStream::new());
        let out: Arc<dyn OutputStream> = mock.clone();
        let action = dispatch_event(
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                    resumable: None,
                },
                usage: None,
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("ok");
        match action {
            RouterAction::RecordStopReason { stop_reason, .. } => {
                assert_eq!(stop_reason, "end_turn");
            }
            other => panic!("expected RecordStopReason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn message_stop_ends_stream() {
        let mut acc = BlockAccumulator::new();
        let mock = Arc::new(MockOutputStream::new());
        let out: Arc<dyn OutputStream> = mock.clone();
        let action = dispatch_event(HistoryEvent::MessageStop, &mut acc, &out, false, false)
            .await
            .expect("ok");
        assert!(matches!(action, RouterAction::EndOfStream));
    }

    #[tokio::test]
    async fn completed_event_ends_stream() {
        use llm_runtime::{ExecutionUsage as Usage, HistoryResponse};
        let mut acc = BlockAccumulator::new();
        let mock = Arc::new(MockOutputStream::new());
        let out: Arc<dyn OutputStream> = mock.clone();
        let resp = HistoryResponse {
            id: "msg_1".into(),
            model: "claude-opus-4-7".into(),
            content: vec![],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: Usage::default(),
            cost: None,
            provider_metadata: JsonPayload::parse("null").expect("null is valid JSON"),

            resumable: None,
        };
        let action = dispatch_event(
            HistoryEvent::Completed {
                response: Box::new(resp),
            },
            &mut acc,
            &out,
            false,
            false,
        )
        .await
        .expect("ok");
        assert!(matches!(action, RouterAction::EndOfStream));
    }
}
