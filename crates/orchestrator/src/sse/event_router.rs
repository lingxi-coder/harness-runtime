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
use llm_runtime::{ContentBlock as LlmContentBlock, HistoryContentDelta, HistoryEvent, Usage};
use platform_api::OutputStream;
use protocol::{ContentBlock, ToolUseId};
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
        input: serde_json::Value,
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
    },
    /// `message_delta` arrived with usage but NO `stop_reason` (A3). The
    /// streaming loop records `output_tokens` and continues.
    RecordUsage {
        /// Cumulative output tokens from this delta's usage.
        output_tokens: u64,
        /// Full usage snapshot from this delta.
        usage: Option<Usage>,
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
) -> Result<RouterAction, StreamingError> {
    match event {
        // Hosted search consumers inspect this semantic event separately. Main
        // conversation attribution remains in the terminal metadata snapshot.
        HistoryEvent::WebSearch { .. } => Ok(RouterAction::Continue),
        HistoryEvent::MessageStart { response } => {
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
                    "input_tokens": response.usage.billable_tokens.input,
                    "cache_creation_input_tokens": response.usage.billable_tokens.cache_write,
                    "cache_read_input_tokens": response.usage.billable_tokens.cache_read,
                    "output_tokens": response.usage.billable_tokens.output,
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
            // stream-json P4: reconstruct SSE event for --include-partial-messages.
            if output.wants_partial_stream_events() {
                let cb_val = reconstruct_content_block_json(&content_block);
                let event_json = serde_json::to_string(&json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": cb_val
                }))
                .unwrap_or_default();
                output.emit_stream_event(&event_json, false).await;
            }
            let kind = match &content_block {
                LlmContentBlock::ProviderContent { protocol, value } => BlockKind::Preserved(ContentBlock::ProviderContent { protocol: protocol.clone(), value: value.clone() }),
                LlmContentBlock::Text { .. } | LlmContentBlock::TextJsUtf16 { .. } => {
                    BlockKind::Text
                }
                LlmContentBlock::ToolCall { id, name, .. } => BlockKind::ToolUse {
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
                        input: input.clone(),
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
                    content: content.clone(),
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
            Ok(RouterAction::Continue)
        }
        HistoryEvent::ContentBlockDelta { index, delta } => {
            // stream-json P4: reconstruct SSE event for --include-partial-messages.
            if output.wants_partial_stream_events() {
                let delta_val = reconstruct_delta_json(&delta);
                let event_json = serde_json::to_string(&json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": delta_val
                }))
                .unwrap_or_default();
                output.emit_stream_event(&event_json, false).await;
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
                        output.emit_text(&text).await;
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
                    output.emit_thinking(&thinking, None).await;
                }
                HistoryContentDelta::SignatureDelta { signature } => {
                    acc.set_signature(index, &signature)?;
                }
                HistoryContentDelta::CitationsDelta { .. }
                | HistoryContentDelta::ConnectorTextDelta { .. } => {
                    // Dropped at M5-04 boundary (parity with M5-02's
                    // `translate_response_blocks` which drops them).
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
            match completed {
                CompletedBlock::Text { text } => {
                    Ok(RouterAction::AppendAssistantBlock(ContentBlock::Text {
                        text,
                    }))
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
                        "output_tokens": u.billable_tokens.output
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
            let output_tokens = usage.as_ref().map_or(0, |u| u.billable_tokens.output);
            let usage_for_billing = usage.clone();
            if let Some(usage) = usage {
                emit_usage_if_present(output, &usage).await;
            }
            if let Some(sr) = delta.stop_reason {
                Ok(RouterAction::RecordStopReason {
                    stop_reason: sr,
                    output_tokens,
                    usage: usage_for_billing,
                    stop_details: delta.stop_details,
                })
            } else if output_tokens > 0 {
                Ok(RouterAction::RecordUsage {
                    output_tokens,
                    usage: usage_for_billing,
                })
            } else {
                Ok(RouterAction::Continue)
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
fn reconstruct_content_block_json(block: &LlmContentBlock) -> Value {
    match block {
        LlmContentBlock::ProviderContent { value, .. } => value.clone(),
        LlmContentBlock::Text { .. } | LlmContentBlock::TextJsUtf16 { .. } => {
            json!({"type": "text", "text": ""})
        }
        LlmContentBlock::ToolCall { id, name, .. } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": {}})
        }
        LlmContentBlock::Reasoning { .. } => json!({"type": "thinking", "thinking": ""}),
        LlmContentBlock::RedactedThinking { data } => {
            json!({"type": "redacted_thinking", "data": data})
        }
        LlmContentBlock::ServerToolUse { id, name, input } => {
            json!({"type": "server_tool_use", "id": id, "name": name, "input": input})
        }
        LlmContentBlock::ConnectorText {
            connector_text,
            signature,
        } => {
            json!({"type": "connector_text", "connector_text": connector_text, "signature": signature})
        }
        LlmContentBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
        }
        _ => json!({"type": "unknown"}),
    }
}

/// Reconstruct the JSON value for a `HistoryContentDelta`
/// (used in the `content_block_delta` SSE event for P4 partial-messages).
fn reconstruct_delta_json(delta: &HistoryContentDelta) -> Value {
    match delta {
        HistoryContentDelta::TextDelta { text } => json!({"type": "text_delta", "text": text}),
        HistoryContentDelta::InputJsonDelta { partial_json } => {
            json!({"type": "input_json_delta", "partial_json": partial_json})
        }
        HistoryContentDelta::ThinkingDelta { thinking } => {
            json!({"type": "thinking_delta", "thinking": thinking})
        }
        HistoryContentDelta::SignatureDelta { signature } => {
            json!({"type": "signature_delta", "signature": signature})
        }
        HistoryContentDelta::CitationsDelta { citation } => {
            json!({"type": "citations_delta", "citation": citation})
        }
        HistoryContentDelta::ConnectorTextDelta { connector_text } => {
            json!({"type": "connector_text_delta", "connector_text": connector_text})
        }
    }
}

/// Surface an SSE `usage` snapshot to the output sink as a live usage
/// update (§0.7 "light up thinking/usage").
///
/// Maps `llm_runtime::Usage` onto the four bare `u64` arguments of
/// [`OutputStream::emit_usage`] with the same field mapping the cost
/// pipeline uses: `input` ← `billable_tokens.input`, `output` ←
/// `billable_tokens.output`, `cache_read` ← `billable_tokens.cache_read`,
/// `cache_write` ← `billable_tokens.cache_write`.
async fn emit_usage_if_present(output: &Arc<dyn OutputStream>, usage: &Usage) {
    output
        .emit_usage(
            usage.billable_tokens.input,
            usage.billable_tokens.output,
            usage.billable_tokens.cache_read,
            usage.billable_tokens.cache_write,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockOutputStream;
    use llm_runtime::HistoryMessageDelta;

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
                },
            },
            &mut acc,
            &out,
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
        )
        .await
        .expect("delta");
        // OutputStream observed exactly one emit_text("hi")
        let events = mock.snapshot().await;
        assert_eq!(events.len(), 1);
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
                },
                usage: None,
            },
            &mut acc,
            &out,
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
        let action = dispatch_event(HistoryEvent::MessageStop, &mut acc, &out, false)
            .await
            .expect("ok");
        assert!(matches!(action, RouterAction::EndOfStream));
    }

    #[tokio::test]
    async fn completed_event_ends_stream() {
        use llm_runtime::{HistoryResponse, Usage};
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
            provider_metadata: serde_json::Value::Null,
        };
        let action = dispatch_event(
            HistoryEvent::Completed {
                response: Box::new(resp),
            },
            &mut acc,
            &out,
            false,
        )
        .await
        .expect("ok");
        assert!(matches!(action, RouterAction::EndOfStream));
    }
}
