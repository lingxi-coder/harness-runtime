//! Streaming turn loop core helpers.
//!
//! ## `StreamingError` → `OrchestratorError` mapping
//!
//! Each [`crate::sse::StreamingError`] variant is converted to
//! [`crate::error::OrchestratorError::StreamingProtocol`] via its
//! `Display` impl. The Display strings (locked at Task 1 step 3) are
//! the public-facing reason carried in the orchestrator error.
#![forbid(unsafe_code)]

use crate::error::OrchestratorError;
use crate::sse::accumulator::BlockAccumulator;
use crate::sse::event_router::{dispatch_event, RouterAction};
use crate::streaming_executor::StreamingToolExecutor;
use futures::stream::{BoxStream, StreamExt};
use lingxi_core::host::OutputStream;
use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
use llm_runtime::{ExecutionUsage as LlmUsage, HistoryEvent, LlmError};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// One tool dispatch request observed during the stream. Carries the
/// id/name/input the orchestrator must invoke. The dispatch itself is
/// performed by the streaming-loop caller (so this module stays free of
/// `ToolRegistry` / `HookExecutor` / `PermissionGate` deps).
#[derive(Debug, Clone)]
pub struct ObservedToolUse {
    /// Stable identifier echoed back in the matching `ToolResult`.
    pub id: ToolUseId,
    /// Tool name.
    pub name: String,
    /// Reassembled tool input.
    pub input: Value,
    /// Verbatim provider-issued tool-call id, preserved for egress replay.
    pub provider_id: Option<String>,
}

/// Outcome of consuming one stream.
#[derive(Debug, Default)]
pub struct PumpedTurn {
    /// Content blocks (text + thinking) accumulated during the stream,
    /// in observation order. Used to construct the assistant message
    /// after the stream ends.
    pub assistant_blocks: Vec<ContentBlock>,
    /// Tool uses observed during the stream, in observation order
    /// (i.e. order of their `content_block_stop` events).
    pub tool_uses: Vec<ObservedToolUse>,
    /// Final `stop_reason` (from `message_delta`). `None` if the stream
    /// ended without a `message_delta` carrying one.
    pub stop_reason: Option<String>,
    /// A3: this turn's output-token count, taken from the final
    /// `message_delta` usage snapshot (the cumulative output tokens of the
    /// streamed message). `0` if the stream carried no usage. The token-budget
    /// continuation loop accumulates this into `global_turn_tokens`.
    pub output_tokens: u64,
    /// Full usage snapshot for billing. The `message_delta` usage is
    /// authoritative when present (it includes both input + output tokens
    /// as the final cumulative snapshot). Falls back to `message_start`
    /// usage when `message_delta` carried no usage. `None` only when the
    /// stream carried no usage at all (unusual; treated as zero-cost).
    ///
    /// Used by `try_run_turn_streaming` to record into `CostTracker`
    /// (mirrors the non-streaming path in `turn_loop.rs`).
    pub usage: Option<LlmUsage>,
    /// Client estimate frozen from the completed physical stream, when all
    /// billed buckets and execution facts were available.
    pub cost_quote: Option<llm_runtime::CostEstimate>,
    /// Refusal `stop_details` (`{category, explanation}`) from the final
    /// `message_delta` — drives the terminal refusal message's cyber/bio
    /// variant. `None` for non-refusal turns.
    pub stop_details: Option<llm_runtime::HistoryStopDetails>,
}

/// Merge host presentation snapshots while keeping complete SDK measurements
/// authoritative, including explicit zero counts.
fn merge_usage(seed: &LlmUsage, delta: &LlmUsage) -> LlmUsage {
    seed.merge_snapshot(delta)
}

/// Consume the given stream to completion, routing events through the
/// accumulator + output sink. Returns the per-block summary; the
/// streaming-loop caller is responsible for tool dispatch + appending
/// to session history.
///
/// # Errors
/// - [`OrchestratorError::Streaming`] wrapping an [`LlmError`] if the
///   underlying transport surfaces an error mid-stream.
/// - [`OrchestratorError::StreamingProtocol`] if the wire-level event
///   sequence violates the per-block protocol (out-of-order delta,
///   double stop, type mismatch, malformed `tool_use` input JSON).
/// - [`OrchestratorError::StreamEndedWithoutStop`] if the stream
///   produced no `MessageStop` event before terminating.
/// Outcome of a FAILED [`pump_stream_with_executor_tracked`] pump: the
/// [`OrchestratorError`] plus whether any *real* (non-thinking) content block
/// had STARTED before the failure.
///
/// `real_content_started` mirrors the 2.1.198 binary's `Hr` flag (set at a
/// non-thinking `content_block_start`, @219640711). The mid-stream transient
/// retry in `conversation.rs` fires ONLY when `!real_content_started` — which
/// subsumes the "never resubmit after a non-idempotent tool executed" guard:
/// a `tool_use` block starting is non-thinking, so it flips this true and
/// disqualifies the retry (a dispatched tool can never be re-run).
#[derive(Debug)]
pub(crate) struct PumpFailure {
    /// The terminal orchestrator error the pump surfaced.
    pub(crate) error: OrchestratorError,
    /// `true` once a non-thinking content block had started streaming.
    pub(crate) real_content_started: bool,
    /// The turn accumulated SO FAR before the failure: completed content blocks
    /// (text/thinking that reached `content_block_stop`) + dispatched `tool_use`s
    /// + a usage seed (`message_start` usage when no `message_delta` arrived).
    ///
    /// P1-04 (cc 2.1.199 partial-finalize, binary-verified): when a mid-stream
    /// server/overloaded/api error, watchdog stall, or connection close lands
    /// after useful output, the caller finalizes this partial in place
    /// (synthesized `stop_reason` + `usage`) instead of discarding it — see
    /// [`partial_has_output`] / [`partial_finalize_cause`]. For a transport close,
    /// this also includes non-empty text whose `content_block_stop` frame was lost,
    /// because those deltas were already emitted to the user.
    pub(crate) partial: PumpedTurn,
}

/// The finalize cause the caller stamps onto `tengu_streaming_partial_finalized`
/// and uses to pick the byte-exact incomplete-response notice. Mirrors cc 2.1.263
/// `tee=Yg?"watchdog":Hu?"server_error":u4?"stream_suspended":SR.has(code)?"network_down":"stale_connection"`
/// (`u4=rp?.code==="StreamSuspended"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartialFinalizeCause {
    /// Stream idle-timeout (watchdog) abort (`Yg`).
    Watchdog,
    /// Overloaded (529) / provider-internal (5xx) / api_error (`Hu`).
    ServerError,
    /// Watchdog abort because the machine slept (`u4`, `rp.code==="StreamSuspended"`).
    StreamSuspended,
    /// A recognized connection-drop error code (`SR.has(code)`).
    NetworkDown,
    /// Any other stale/closed connection.
    StaleConnection,
}

impl PartialFinalizeCause {
    /// The `cause` telemetry enum value (byte-exact cc strings).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Watchdog => "watchdog",
            Self::ServerError => "server_error",
            Self::StreamSuspended => "stream_suspended",
            Self::NetworkDown => "network_down",
            Self::StaleConnection => "stale_connection",
        }
    }

    /// The byte-exact incomplete-response notice for a HAS-OUTPUT partial finalize
    /// (cc 2.1.263 `Bl="API Error"` + the cause-specific tail at src_160988549.js
    /// @4784072). Only the has-output variants are ported here — the thinking-only
    /// "Try again" variants are handled by the retry/exhaustion path, not this
    /// finalize.
    pub(crate) fn incomplete_notice(self) -> &'static str {
        match self {
            Self::Watchdog => {
                "API Error: The response stopped arriving. The response above may be incomplete."
            }
            Self::ServerError => {
                "API Error: Server error mid-response. The response above may be incomplete."
            }
            Self::StreamSuspended => {
                "API Error: Your computer went to sleep mid-response. The response above may be incomplete."
            }
            Self::NetworkDown | Self::StaleConnection => {
                "API Error: Connection lost mid-response. The response above may be incomplete."
            }
        }
    }
}

/// Classify a terminal pump [`OrchestratorError`] into a partial-finalize cause,
/// or `None` when the error is NOT a finalize-class error (protocol violation,
/// auth, invalid-request, …). Ordering mirrors cc: watchdog first, then
/// server_error, then the connection-drop family.
pub(crate) fn partial_finalize_cause(error: &OrchestratorError) -> Option<PartialFinalizeCause> {
    match error {
        OrchestratorError::Streaming(e)
            if llm_runtime::model::stream_watchdog::is_stream_suspended(e) =>
        {
            Some(PartialFinalizeCause::StreamSuspended)
        }
        OrchestratorError::Streaming(e)
            if llm_runtime::model::stream_watchdog::is_stream_idle_timeout(e) =>
        {
            Some(PartialFinalizeCause::Watchdog)
        }
        OrchestratorError::Streaming(LlmError::Overloaded { .. } | LlmError::ProviderInternal) => {
            Some(PartialFinalizeCause::ServerError)
        }
        OrchestratorError::Streaming(LlmError::Transport { .. }) => {
            Some(PartialFinalizeCause::StaleConnection)
        }
        // A stream that closed before `message_stop` is a mid-response connection
        // close — cc's `network_down` bucket.
        OrchestratorError::StreamEndedWithoutStop => Some(PartialFinalizeCause::NetworkDown),
        _ => None,
    }
}

/// Whether the accumulated partial carries REAL (non-thinking) output worth
/// preserving: a completed non-thinking content block, a dispatched `tool_use`,
/// or visible in-flight text recovered by [`build_failure`] after a transport
/// close.
///
/// Completed blocks follow cc's `_r.some(...)` finalize guard. The transport
/// recovery is a mobile reliability extension: text deltas are rendered before
/// `content_block_stop`, so dropping an unterminated text block makes persisted
/// history disagree with the transcript the user already saw.
pub(crate) fn partial_has_output(turn: &PumpedTurn) -> bool {
    !turn.tool_uses.is_empty()
        || turn.assistant_blocks.iter().any(|b| {
            !matches!(
                b,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        })
}

/// Attach the accumulated partial + a usage seed to a terminal pump error.
fn build_failure(
    mut partial: PumpedTurn,
    accumulator: &BlockAccumulator,
    message_start_usage: &Option<LlmUsage>,
    error: OrchestratorError,
    real_content_started: bool,
) -> PumpFailure {
    // A network close can land between the final text delta and the block-stop
    // frame. Those text bytes have already reached every live output sink, so
    // preserve them in history and let the existing partial-finalize path render
    // its stable interruption notice. Provider errors (not transport closes)
    // retain their existing retry/fallback semantics for incomplete blocks.
    if matches!(
        error,
        OrchestratorError::Streaming(LlmError::Transport { .. })
            | OrchestratorError::StreamEndedWithoutStop
    ) {
        partial
            .assistant_blocks
            .extend(accumulator.incomplete_text_blocks());
    }
    // Seed billing from `message_start` when no `message_delta` usage arrived, so
    // a finalized partial still records input tokens (cc patches `message.usage`
    // onto every yielded message from the same `pn` snapshot).
    if partial.usage.is_none() {
        partial.usage = message_start_usage.clone();
    }
    PumpFailure {
        error,
        real_content_started,
        partial,
    }
}

/// Whether a mid-stream [`LlmError`] is a TRANSIENT network failure eligible
/// for the streaming-request retry (cc 2.1.198 mid-response transient retry):
/// a transport-layer drop (ECONNRESET / "connection closed" / reset / EPIPE /
/// timeout — surfaced as [`LlmError::Transport`]) or a watchdog idle-timeout
/// abort ([`llm_runtime::model::stream_watchdog::is_stream_idle_timeout`]).
///
/// `ProviderInternal` / `Overloaded` are deliberately EXCLUDED here — those
/// keep their dedicated non-streaming fallback arm.
pub(crate) fn is_transient_mid_stream(error: &OrchestratorError) -> bool {
    match error {
        OrchestratorError::Streaming(e) | OrchestratorError::ApiCall(e) => {
            matches!(e, LlmError::Transport { .. })
                || llm_runtime::model::stream_watchdog::is_stream_idle_timeout(e)
        }
        _ => false,
    }
}

/// Max streaming-request retries for a stale-connection drop. Binary `An=2`
/// (`Kn<An`, @219649648) — the stale-connection retry budget.
pub(crate) const MID_STREAM_STALE_CONNECTION_MAX_RETRIES: u32 = 2;

/// Max streaming-request retries for a watchdog idle-timeout. Binary `ao=1`
/// (`Mn<ao`, @219649648) — the idle-timeout retry budget.
pub(crate) const MID_STREAM_IDLE_TIMEOUT_MAX_RETRIES: u32 = 1;

/// Cause-aware retry cap for a mid-stream transient error, matching the
/// binary's split `ac?Mn<ao:Kn<An` (idle-timeout `ao=1` vs stale-connection
/// `An=2`). Only meaningful when [`is_transient_mid_stream`] is `true`.
pub(crate) fn mid_stream_retry_cap(error: &OrchestratorError) -> u32 {
    let is_idle = matches!(
        error,
        OrchestratorError::Streaming(e) | OrchestratorError::ApiCall(e)
            if llm_runtime::model::stream_watchdog::is_stream_idle_timeout(e)
    );
    if is_idle {
        MID_STREAM_IDLE_TIMEOUT_MAX_RETRIES
    } else {
        MID_STREAM_STALE_CONNECTION_MAX_RETRIES
    }
}

pub async fn pump_stream(
    stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
    output: &Arc<dyn OutputStream>,
) -> Result<PumpedTurn, OrchestratorError> {
    pump_stream_inner(stream, output, None)
        .await
        .map_err(|f| f.error)
}

/// Context handed to [`pump_stream_with_executor_tracked`] so that, as each
/// `tool_use` block's `content_block_stop` arrives mid-stream, its tool is
/// registered with (and dispatched into) the [`StreamingToolExecutor`] —
/// faithful to claude-code `query.ts:837-844`, where `addTool` runs INSIDE
/// the live stream loop.
///
/// ## Byte-equivalence contract
/// This struct only changes *when tools START* (during the stream vs. after
/// it). It does NOT drain / persist / yield any `tool_result` mid-stream:
/// `take_newly_completed` + per-result persistence stay in the post-stream
/// caller loop (`conversation.rs`), preserving the exact JSONL / history /
/// request byte ordering. The executor buffers each `Completed` tool until
/// that post-stream drain.
pub(crate) struct ExecutorPump<'a, 'e> {
    /// The executor created BEFORE the stream (claude-code `query.ts:562`),
    /// borrowing `&orch` for the whole turn.
    pub(crate) executor: &'a mut StreamingToolExecutor<'e>,
    /// The pre-allocated id of THIS turn's assistant message (claude-code
    /// passes the already-yielded assistant `message` to `addTool`). It only
    /// populates `TrackedTool.assistant_id`; the post-stream drain parents
    /// results via the per-block JSONL uuid map, so this value does not affect
    /// output bytes.
    pub(crate) assistant_id: MessageId,
    /// The turn's USER-interrupt token (ESC / new message), mirroring
    /// claude-code's `!toolUseContext.abortController.signal.aborted` guard at
    /// `query.ts:839`. When already fired, mid-stream tools are still
    /// REGISTERED (so the post-stream drain can substitute their synthetic
    /// cancel results) but NOT started — identical to the pre-change behavior
    /// where the post-stream loop's `apply_abort_to_pending` produces the
    /// synthetics. `None` ⇒ never aborted ⇒ always dispatch.
    pub(crate) user_cancel: Option<&'a CancellationToken>,
    /// P2-04 (MessageDisplay `displayContent`): `true` when a `MessageDisplay`
    /// hook is registered for this turn, in which case live per-token
    /// `text_delta` emission is suppressed in [`dispatch_event`] so the
    /// orchestrator's completed-message pass renders the full (possibly
    /// hook-substituted) text exactly once. `false` ⇒ byte-identical live
    /// streaming (the no-hook common case).
    pub(crate) suppress_live_text: bool,
}

/// Like [`pump_stream`], but drives a [`StreamingToolExecutor`] DURING the
/// stream: each arriving `tool_use` block is `add_tool`'d and `process_queue`'d
/// the moment its `content_block_stop` is observed, and in-flight tool futures
/// are polled CONCURRENTLY with the stream so a tool genuinely begins executing
/// before `EndOfStream`. Mirrors claude-code `query.ts:659/837-844`.
///
/// The drain of completed results into history is intentionally NOT performed
/// here — see [`ExecutorPump`]'s byte-equivalence contract. The caller drains
/// the (possibly already-`Completed`) tools post-stream in received order.
/// Like [`pump_stream_with_executor_tracked`], but surfaces the richer
/// [`PumpFailure`] (error + `real_content_started`) so the caller can decide
/// whether to retry the streaming request (cc 2.1.198 mid-response transient
/// retry). On success the outcome is byte-identical to
/// [`pump_stream_with_executor_tracked`].
pub(crate) async fn pump_stream_with_executor_tracked(
    stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
    output: &Arc<dyn OutputStream>,
    pump: ExecutorPump<'_, '_>,
) -> Result<PumpedTurn, PumpFailure> {
    pump_stream_inner(stream, output, Some(pump)).await
}

async fn pump_stream_inner(
    mut stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
    output: &Arc<dyn OutputStream>,
    mut pump: Option<ExecutorPump<'_, '_>>,
) -> Result<PumpedTurn, PumpFailure> {
    let mut acc = BlockAccumulator::new();
    // P2-04: suppress live per-token text emission while a `MessageDisplay` hook
    // is registered (only the executor-driven pump carries the flag; the plain
    // `pump_stream` test helper defaults to `false` = live streaming).
    let suppress_live_text = pump.as_ref().map(|p| p.suppress_live_text).unwrap_or(false);
    let mut turn = PumpedTurn::default();
    let mut assistant_block_indices: Vec<(u32, bool)> = Vec::new();
    // Mirrors the binary's `Hr`: set true the moment a non-thinking content
    // block STARTS (text / tool_use / etc.). Gates the caller's mid-stream
    // transient retry — see [`PumpFailure`].
    let mut real_content_started = false;
    // Capture the MessageStart usage as the fallback billing source for
    // input tokens, in case MessageDelta carries no usage (rare). The
    // MessageDelta usage supersedes this when present.
    let mut message_start_usage: Option<LlmUsage> = None;

    loop {
        // Race the next stream event against the completion of any in-flight
        // tool future. `drain_one` only RECORDS the completed tool's result
        // into the executor (and runs the Bash sibling-error cascade) — it
        // does NOT persist / emit / drain into history, so polling it
        // mid-stream advances the tool's work without changing output bytes.
        // The `if` guard keeps us off an empty `FuturesUnordered`, whose
        // `next()` resolves to `None` immediately and would busy-loop.
        let item = if let Some(p) = pump.as_mut() {
            tokio::select! {
                biased;
                _ = p.executor.drain_one(), if !p.executor.inflight_is_empty() => {
                    // A tool finished mid-stream; loop to keep reading the
                    // stream / polling remaining in-flight tools.
                    continue;
                }
                item = stream.next() => item,
            }
        } else {
            stream.next().await
        };
        let Some(item) = item else { break };
        let mut event = match item {
            Ok(ev) => ev,
            Err(e) => {
                return Err(build_failure(
                    turn,
                    &acc,
                    &message_start_usage,
                    OrchestratorError::Streaming(e),
                    real_content_started,
                ));
            }
        };
        if let HistoryEvent::MessageDelta {
            usage: Some(usage), ..
        } = &mut event
        {
            if let Some(quote) = usage.cost_estimate.take() {
                turn.cost_quote = Some(quote);
            }
        }
        // Capture MessageStart usage before dispatching (dispatch consumes the event).
        if let HistoryEvent::MessageStart { ref response } = event {
            message_start_usage = Some(response.usage.clone());
        }
        // `Hr` (binary @219640711): a non-thinking `content_block_start` flips
        // `real_content_started`, disqualifying the mid-stream transient retry.
        if let HistoryEvent::ContentBlockStart {
            ref content_block, ..
        } = event
        {
            if !matches!(
                content_block,
                llm_runtime::ContentBlock::Reasoning { .. }
                    | llm_runtime::ContentBlock::RedactedThinking { .. }
            ) {
                real_content_started = true;
            }
        }
        let completed_block_index = match &event {
            HistoryEvent::ContentBlockStop { index } => Some(*index),
            _ => None,
        };
        let action = match dispatch_event(event, &mut acc, output, suppress_live_text).await {
            Ok(a) => a,
            Err(e) => {
                return Err(build_failure(
                    turn,
                    &acc,
                    &message_start_usage,
                    OrchestratorError::StreamingProtocol(e.to_string()),
                    real_content_started,
                ));
            }
        };
        match action {
            RouterAction::Continue => {}
            RouterAction::AppendAssistantBlock(block) => {
                if let ContentBlock::RedactedThinking { data } = &block {
                    output.emit_redacted_thinking(data).await;
                }
                let key = llm_runtime::stream_content_order(
                    completed_block_index.expect("completed block action has an index"),
                );
                let position = assistant_block_indices.partition_point(|existing| existing <= &key);
                assistant_block_indices.insert(position, key);
                turn.assistant_blocks.insert(position, block);
            }
            RouterAction::DispatchToolUse {
                id,
                name,
                input,
                provider_id,
            } => {
                // claude-code `query.ts:837-844`: register THIS tool with the
                // executor the moment its block completes. Registration always
                // happens (so the post-stream drain has a TrackedTool to emit a
                // result for); execution (`process_queue`) is gated on the
                // user-interrupt token, mirroring the `!signal.aborted` guard.
                if let Some(p) = pump.as_mut() {
                    p.executor.add_tool(
                        id.clone(),
                        name.clone(),
                        input.clone(),
                        provider_id.clone(),
                        p.assistant_id,
                    );
                    let aborted = p.user_cancel.is_some_and(CancellationToken::is_cancelled);
                    if !aborted {
                        p.executor.process_queue();
                    }
                }
                turn.tool_uses.push(ObservedToolUse {
                    id,
                    name,
                    input,
                    provider_id,
                });
            }
            RouterAction::RecordStopReason {
                stop_reason,
                output_tokens,
                usage,
                stop_details,
            } => {
                turn.stop_reason = Some(stop_reason);
                if stop_details.is_some() {
                    turn.stop_details = stop_details;
                }
                // The final delta's usage supersedes any earlier snapshot.
                if output_tokens > 0 {
                    turn.output_tokens = output_tokens;
                }
                // BILLING: per-field merge — MessageStart is the seed
                // (input + cache tokens); MessageDelta overlays output.
                // A whole-delta `or_else` would zero input/cache when the
                // delta is present but carries `0` for those fields (real
                // Anthropic wire shape). Mirrors `agent::accumulator::merge_usage`.
                turn.usage = match (usage.as_ref(), message_start_usage.as_ref()) {
                    (Some(delta), Some(seed)) => Some(merge_usage(seed, delta)),
                    (Some(delta), None) => Some(delta.clone()),
                    (None, seed) => seed.cloned(),
                };
            }
            RouterAction::RecordUsage {
                output_tokens,
                usage,
            } => {
                // Usage-only delta (no stop_reason yet): keep the latest count.
                turn.output_tokens = output_tokens;
                turn.usage = match (usage.as_ref(), message_start_usage.as_ref()) {
                    (Some(delta), Some(seed)) => Some(merge_usage(seed, delta)),
                    (Some(delta), None) => Some(delta.clone()),
                    (None, seed) => seed.cloned(),
                };
            }
            RouterAction::EndOfStream => {
                return Ok(turn);
            }
        }
    }
    // Stream ended without a MessageStop.
    Err(build_failure(
        turn,
        &acc,
        &message_start_usage,
        OrchestratorError::StreamEndedWithoutStop,
        real_content_started,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockOutputStream;
    use crate::test_support_stream::{
        content_block_start_text, content_block_start_thinking, content_block_start_tool_use,
        content_block_stop, input_json_delta, message_delta_stop, message_delta_stop_with_usage,
        message_start, message_start_with_usage, message_stop, text_delta, thinking_delta,
    };
    use futures::stream;
    use lingxi_core::types::ToolUseId;

    fn boxed(events: Vec<HistoryEvent>) -> BoxStream<'static, Result<HistoryEvent, LlmError>> {
        stream::iter(events.into_iter().map(Ok)).boxed()
    }

    #[tokio::test]
    async fn stream_pump_extracts_frozen_quote_without_serializing_it() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let mut estimate = llm_runtime::CostEstimate::unestimated(llm_runtime::PricingModelRef {
            pricing_provider_id: llm_runtime::ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            billing_model: "deepseek-flash".into(),
            request_model: "deepseek-flash".into(),
            display_model: "deepseek-flash".into(),
        });
        estimate.estimated = true;
        estimate.total_cost_usd = Some(0.00075);
        let mut usage = llm_runtime::ExecutionUsage::default();
        usage.counts_mut().input_tokens = 1_000;
        usage.cost_estimate = Some(estimate);
        let turn = pump_stream(
            boxed(vec![
                message_start("m", "deepseek-flash"),
                message_delta_stop_with_usage("end_turn", usage),
                message_stop(),
            ]),
            &out,
        )
        .await
        .unwrap();
        assert_eq!(turn.cost_quote.unwrap().total_cost_usd, Some(0.00075));
        let usage = turn.usage.unwrap();
        assert!(usage.cost_estimate.is_none());
        assert!(serde_json::to_value(usage)
            .unwrap()
            .get("cost_estimate")
            .is_none());
    }

    /// 2.1.263 src_160988549.js @4784072 has-output notices + `tee` cause names.
    #[test]
    fn incomplete_notice_matches_2_1_263_has_output_copy() {
        assert_eq!(
            PartialFinalizeCause::Watchdog.incomplete_notice(),
            "API Error: The response stopped arriving. The response above may be incomplete."
        );
        assert_eq!(
            PartialFinalizeCause::ServerError.incomplete_notice(),
            "API Error: Server error mid-response. The response above may be incomplete."
        );
        assert_eq!(
            PartialFinalizeCause::StreamSuspended.incomplete_notice(),
            "API Error: Your computer went to sleep mid-response. The response above may be incomplete."
        );
        assert_eq!(
            PartialFinalizeCause::NetworkDown.incomplete_notice(),
            "API Error: Connection lost mid-response. The response above may be incomplete."
        );
        assert_eq!(
            PartialFinalizeCause::StaleConnection.incomplete_notice(),
            "API Error: Connection lost mid-response. The response above may be incomplete."
        );
        assert_eq!(
            PartialFinalizeCause::StreamSuspended.as_str(),
            "stream_suspended"
        );
    }

    #[test]
    fn partial_finalize_cause_classifies_suspend_before_idle() {
        let suspend = llm_runtime::model::stream_watchdog::watchdog_abort_error(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(5),
        );
        assert_eq!(
            partial_finalize_cause(&OrchestratorError::Streaming(suspend)),
            Some(PartialFinalizeCause::StreamSuspended)
        );
        let idle = llm_runtime::model::stream_watchdog::idle_timeout_error(
            std::time::Duration::from_secs(1),
        );
        assert_eq!(
            partial_finalize_cause(&OrchestratorError::Streaming(idle)),
            Some(PartialFinalizeCause::Watchdog)
        );
    }

    #[tokio::test]
    async fn late_native_content_keeps_provider_order_in_the_transcript() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let native = serde_json::json!({"type":"reasoning","encrypted_content":"opaque"});
        let events = vec![
            message_start("m", "model"),
            content_block_start_text(1),
            text_delta(1, "answer"),
            content_block_stop(1),
            HistoryEvent::ContentBlockStart {
                index: 0x8000_0000,
                content_block: llm_runtime::ContentBlock::ProviderContent {
                    protocol: "open_ai_responses".into(),
                    value: native.clone(),
                },
            },
            content_block_stop(0x8000_0000),
            message_delta_stop("end_turn"),
            message_stop(),
        ];
        let turn = pump_stream(boxed(events), &out).await.unwrap();
        assert!(
            matches!(&turn.assistant_blocks[0],ContentBlock::ProviderContent { value, .. } if value == &native)
        );
        assert!(
            matches!(&turn.assistant_blocks[1],ContentBlock::Text { text } if text == "answer")
        );
        assert!(turn.tool_uses.is_empty());
    }

    #[tokio::test]
    async fn text_only_pump_assembles_one_text_block() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let evs = vec![
            message_start("m1", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "he"),
            text_delta(0, "llo"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ];
        let turn = pump_stream(boxed(evs), &out).await.expect("pump");
        assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(turn.assistant_blocks.len(), 1);
        if let ContentBlock::Text { text } = &turn.assistant_blocks[0] {
            assert_eq!(text, "hello");
        } else {
            panic!("expected Text block, got {:?}", turn.assistant_blocks[0]);
        }
        assert!(turn.tool_uses.is_empty());
    }

    #[tokio::test]
    async fn tool_use_pump_collects_dispatch_request() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let tu = ToolUseId::new();
        let evs = vec![
            message_start("m1", "claude-opus-4-7"),
            content_block_start_tool_use(1, tu.clone(), "Read"),
            input_json_delta(1, "{\"file"),
            input_json_delta(1, "_path\":\"foo.rs\"}"),
            content_block_stop(1),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let turn = pump_stream(boxed(evs), &out).await.expect("pump");
        assert_eq!(turn.tool_uses.len(), 1);
        assert_eq!(turn.tool_uses[0].name, "Read");
        assert_eq!(turn.tool_uses[0].input["file_path"], "foo.rs");
        assert_eq!(turn.stop_reason.as_deref(), Some("tool_use"));
    }

    #[tokio::test]
    async fn stream_without_message_stop_errors() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let evs = vec![
            message_start("m1", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "partial"),
            content_block_stop(0),
            // no message_stop
        ];
        let err = pump_stream(boxed(evs), &out).await.expect_err("no stop");
        assert!(matches!(err, OrchestratorError::StreamEndedWithoutStop));
    }

    #[tokio::test]
    async fn streaming_protocol_error_propagates() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        // delta before start → BlockNotFound
        let evs = vec![
            message_start("m1", "claude-opus-4-7"),
            text_delta(0, "oops"),
            message_stop(),
        ];
        let err = pump_stream(boxed(evs), &out).await.expect_err("proto");
        match err {
            OrchestratorError::StreamingProtocol(reason) => {
                assert!(reason.contains("block index 0"), "{reason}");
            }
            other => panic!("expected StreamingProtocol, got {other:?}"),
        }
    }

    /// §0.7 "light up thinking/usage": a stream carrying a `ThinkingDelta`
    /// and a `MessageDelta` with a final `usage` snapshot must surface both
    /// to the `OutputStream` via `emit_thinking` + `emit_usage`, while the
    /// stop-reason / assembled-turn behavior stays exactly as before.
    #[tokio::test]
    async fn thinking_and_usage_deltas_emit_to_output() {
        use crate::test_support::MockOutputStream;
        use lingxi_core::host::OutputEvent;
        use llm_runtime::ExecutionUsage as Usage;

        let mock = Arc::new(MockOutputStream::new());
        let out: Arc<dyn OutputStream> = mock.clone();
        let evs = vec![
            message_start("m1", "claude-opus-4-7"),
            // a thinking block streamed as a delta
            content_block_start_thinking(0),
            thinking_delta(0, "let me reason"),
            content_block_stop(0),
            // a text block so the assembled turn is non-trivial
            content_block_start_text(1),
            text_delta(1, "answer"),
            content_block_stop(1),
            // final message_delta with stop_reason AND usage
            message_delta_stop_with_usage(
                "end_turn",
                Usage {
                    report: llm_runtime::UsageReport::measured(
                        llm_runtime::Usage {
                            input_tokens: 120,
                            output_tokens: 35,
                            cache_write_tokens: 10,
                            cache_read_tokens: 5,
                            reasoning_tokens: 0,
                            ..Default::default()
                        },
                        llm_runtime::services::sdk::protocol::UsageState::Complete,
                    ),
                    ..Usage::default()
                },
            ),
            message_stop(),
        ];
        let turn = pump_stream(boxed(evs), &out).await.expect("pump");

        // Existing behavior is unchanged: stop reason + assembled blocks.
        assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(turn.assistant_blocks.len(), 2);
        assert!(matches!(
            &turn.assistant_blocks[0],
            ContentBlock::Thinking { thinking, signature }
                if thinking == "let me reason" && signature.is_none()
        ));

        let events = mock.snapshot().await;

        // emit_thinking fired exactly once with the live delta + None sig.
        let thinking: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                OutputEvent::Thinking {
                    thinking,
                    signature,
                } => Some((thinking.clone(), signature.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec![("let me reason".to_string(), None)]);

        // emit_usage fired with the message_delta usage mapped field-for-field.
        // (message_start carried a default all-zero usage, emitted first.)
        let usages: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                OutputEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                } => Some((
                    *input_tokens,
                    *output_tokens,
                    *cache_read_tokens,
                    *cache_creation_tokens,
                )),
                _ => None,
            })
            .collect();
        assert!(
            usages.contains(&(120, 35, 5, 10)),
            "expected final usage (120,35,5,10), got {usages:?}"
        );
        // message_start's default-zero usage was surfaced too.
        assert_eq!(usages.first(), Some(&(0, 0, 0, 0)));
    }

    #[tokio::test]
    async fn underlying_stream_error_surfaces_as_streaming_variant() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let s: BoxStream<'static, Result<HistoryEvent, LlmError>> = stream::iter(vec![
            Ok(message_start("m1", "claude-opus-4-7")),
            Err(LlmError::Transport {
                message: "dropped".into(),
            }),
        ])
        .boxed();
        let err = pump_stream(s, &out).await.expect_err("network");
        assert!(matches!(err, OrchestratorError::Streaming(_)));
    }

    /// Final SDK measurements include the accumulated input and cache counts.
    #[tokio::test]
    async fn canonical_final_usage_preserves_sdk_input_and_cache_measurement() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());

        // Real wire: MessageStart carries input=1000, cache_read=200, output=0.
        let start_usage = LlmUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: 1_000,
                    output_tokens: 0,
                    cache_write_tokens: 0,
                    cache_read_tokens: 200,
                    reasoning_tokens: 0,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..LlmUsage::default()
        };
        // SDK final measurements already include input/cache from earlier frames.
        let delta_usage = LlmUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: 1_000,
                    output_tokens: 500,
                    cache_write_tokens: 0,
                    cache_read_tokens: 200,
                    reasoning_tokens: 0,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..LlmUsage::default()
        };

        let evs = vec![
            message_start_with_usage("m1", "claude-opus-4-7", start_usage),
            content_block_start_text(0),
            text_delta(0, "hi"),
            content_block_stop(0),
            message_delta_stop_with_usage("end_turn", delta_usage),
            message_stop(),
        ];

        let turn = pump_stream(boxed(evs), &out).await.expect("pump");

        let usage = turn.usage.expect("usage must be recorded");
        let bt = usage.counts();
        assert_eq!(
            bt.input_tokens, 1_000,
            "input tokens must come from MessageStart; got {}",
            bt.input_tokens
        );
        assert_eq!(
            bt.cache_read_tokens, 200,
            "cache_read tokens must come from MessageStart; got {}",
            bt.cache_read_tokens
        );
        assert_eq!(
            bt.output_tokens, 500,
            "output tokens must come from MessageDelta; got {}",
            bt.output_tokens
        );
    }
}
