//! `AdapterOutputStream` — the live-turn feed (plan F1-12).
//!
//! This is the direct analog of the TUI's `BridgeOutputStream`
//! (`tui/src/events/orchestrator_bridge.rs`), but instead of pushing a TUI-local
//! `TurnEvent` onto an mpsc channel it lowers each callback into a
//! `crate::protocol::ClientEvent` DTO and forwards it through the
//! transport-agnostic [`ClientEventSink`]. The SAME stream therefore feeds both
//! transports (bridge-server WS and mobile `UniFFI`) — governing decision §0.1.
//!
//! It implements the [`lingxi_core::host::OutputStream`] callbacks
//! (`crates/core/src/host/orchestrator.rs:448-516`), including the two §0.7
//! "light up thinking/usage" follow-up callbacks (`emit_thinking`/`emit_usage`):
//!
//! | callback                    | emitted `ClientEvent`(s)            |
//! |-----------------------------|-------------------------------------|
//! | `emit_text`                 | `TextDelta`                         |
//! | `emit_system_notice`        | `SystemNotice`                     |
//! | `emit_tool_call`            | `ToolUseStarted`                    |
//! | `emit_tool_heartbeat`       | `ToolHeartbeat`                     |
//! | `emit_tool_result`          | `ToolUseResult`                     |
//! | `emit_end_turn`             | `CostUpdate` **then** `TurnEnded`   |
//! | `emit_compaction_completed` | `CompactionCompleted`               |
//! | `emit_thinking` (§0.7)      | `ThinkingDelta`                     |
//! | `emit_message_boundary`     | `MessageComplete`                   |
//! | `emit_usage` (§0.7)         | `UsageUpdate`                       |
//! | `emit_api_retry`            | `ApiRetry`                          |
//!
//! All `serde_json::Value` lowering goes through the pure F1-11 fns in
//! [`crate::adapter::lowering`] so the wire form is identical to every other surface and
//! `client::protocol` itself never sees a `Value`.
//!
//! ## `is_error` derivation
//!
//! [`lingxi_core::host::OutputStream::emit_tool_result`] carries `(id, tool, model_text,
//! &Value)` — it has NO separate `is_error` flag (verified
//! `crates/core/src/host/orchestrator.rs:436`). The `model_text` (the model-facing string)
//! is ignored by this adapter because the `client::protocol` DTO is wire-frozen;
//! `result_json` carries the full metadata `data`.
//! The orchestrator signals a failed tool by shaping the emitted payload as
//! `{ "error": "<message>" }` (verified `orchestrator/src/turn_loop.rs:319-333`).
//! The adapter therefore derives `is_error` structurally: a JSON object carrying
//! a top-level `"error"` key is an error result. This mirrors the engine's own
//! error-payload contract rather than inventing a new signal.
//!
//! ## `stop_reason` → `TurnOutcomeDto`
//!
//! `emit_end_turn` cannot observe cancellation (the cancel token is handled by
//! the F1-13 turn wrapper), so it maps only the model's stop reason — mirroring
//! `BridgeOutputStream` (`tui/src/events/orchestrator_bridge.rs:157-160`):
//! `"max_tokens"` ⇒ [`TurnOutcomeDto::MaxTurns`], anything else ⇒
//! [`TurnOutcomeDto::EndTurn`]. The raw `stop_reason` is preserved verbatim in
//! `TurnEnded.stop_reason` for clients that need the exact string.

use std::sync::Arc;

use crate::protocol::events::{ClientEvent, TurnOutcomeDto};
use crate::protocol::message::{MessageBlockDto, MessageDto};
use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OutputStream};

use crate::adapter::lowering::{lower_cost_snapshot, value_to_json_string};
use crate::adapter::sink::ClientEventSink;

/// An [`lingxi_core::host::OutputStream`] that lowers every live-turn callback into a
/// [`ClientEvent`] DTO and forwards it through an [`Arc<dyn ClientEventSink>`].
///
/// Connection-scoped: one stream per transport connection, holding the same
/// `Arc<dyn ClientEventSink>` as the permission gate and turn wrapper so all
/// three feed the one outbound channel (mirrors the single `BridgeOutputStream`
/// per TUI session).
#[derive(Clone)]
pub struct AdapterOutputStream {
    sink: Arc<dyn ClientEventSink>,
    /// Ordered blocks for the API response currently being streamed. The
    /// orchestrator calls `emit_message_boundary` after persistence and before
    /// any terminal `emit_end_turn`, making this the production message-level
    /// source of truth for mobile/web transcript reducers.
    message_blocks: Arc<tokio::sync::Mutex<Vec<MessageBlockDto>>>,
    /// `tool_use_id` → `(tool name, call input)` for calls awaiting a result,
    /// in INSERTION ORDER.
    ///
    /// The engine's `emit_tool_result` carries no input, but the result
    /// display needs it (the diff, and the edit headline's line counts).
    /// Mirrors the side-tables `ActiveTurn` (`tui-core/src/active_turn.rs`)
    /// and `ChatWidget` already keep for the same reason.
    ///
    /// A `VecDeque` rather than a `HashMap` so [`MAX_PENDING_TOOL_CALLS`] can
    /// evict the OLDEST entry; it is bounded at 256, so a lookup's linear scan
    /// is nothing beside what the cap used to cost (see [`Self::remember_call`]).
    ///
    /// A `std::sync::Mutex`, never held across an `.await`, so the struct
    /// stays `Send + Sync` without churning the async signatures.
    pending:
        Arc<std::sync::Mutex<std::collections::VecDeque<(String, (String, serde_json::Value))>>>,
}

/// Belt-and-braces bound on [`AdapterOutputStream::pending`] so a turn that
/// never ends cannot grow it without limit.
const MAX_PENDING_TOOL_CALLS: usize = 256;

impl AdapterOutputStream {
    /// Wrap a sink. The sink is shared with the rest of the connection-scoped
    /// adapter (permission gate, turn wrapper).
    #[must_use]
    pub fn new(sink: Arc<dyn ClientEventSink>) -> Self {
        Self {
            sink,
            message_blocks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Clear an unfinished response before a host starts a new turn or after a
    /// hard failure that did not reach an engine message boundary.
    pub async fn reset_message_buffer(&self) {
        self.message_blocks.lock().await.clear();
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }

    /// Record a dispatched call's input for the eventual result.
    ///
    /// At [`MAX_PENDING_TOOL_CALLS`] this evicts the OLDEST entry. It used to
    /// `clear()`, which cost EVERY in-flight call its structured diff and
    /// headline — one overflow blanked the whole batch instead of the single
    /// longest-waiting call.
    fn remember_call(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        input: &serde_json::Value,
    ) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let key = id.to_string();
        // A re-dispatch under the same id replaces its entry rather than
        // stacking a second one behind it.
        pending.retain(|(pending_id, _)| pending_id != &key);
        while pending.len() >= MAX_PENDING_TOOL_CALLS {
            pending.pop_front();
        }
        pending.push_back((key, (tool.to_string(), input.clone())));
    }

    /// Take back a dispatched call's input, if it is still pending.
    fn take_call(&self, id: &lingxi_core::types::ToolUseId) -> Option<(String, serde_json::Value)> {
        let mut pending = self.pending.lock().ok()?;
        let key = id.to_string();
        let at = pending
            .iter()
            .position(|(pending_id, _)| pending_id == &key)?;
        pending.remove(at).map(|(_, call)| call)
    }

    /// Derive the `is_error` flag from a tool-result payload.
    ///
    /// The orchestrator emits a failed tool result as a JSON object with a
    /// top-level `"error"` key (`orchestrator/src/turn_loop.rs:327`); any other
    /// shape is a success payload. This keeps the adapter aligned with the
    /// engine's existing error-payload contract.
    fn result_is_error(result: &serde_json::Value) -> bool {
        result.get("error").is_some()
    }

    async fn emit_tool_result_event(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        result: &serde_json::Value,
    ) {
        let is_error = Self::result_is_error(result);
        let call_input = self.take_call(id).map(|(_, input)| input);
        self.sink
            .emit(ClientEvent::ToolUseResult {
                id: id.to_string(),
                tool: tool.to_string(),
                result_json: value_to_json_string(result),
                is_error,
                display: Some(crate::adapter::tool_display::lower_tool_result_display(
                    tool,
                    call_input.as_ref(),
                    result,
                    is_error,
                )),
            })
            .await;
    }

    /// Map a model `stop_reason` to a [`TurnOutcomeDto`].
    ///
    /// Mirrors `BridgeOutputStream` (`orchestrator_bridge.rs:157-160`):
    /// `"max_tokens"` ⇒ `MaxTurns`, everything else ⇒ `EndTurn`. Cancellation
    /// is NOT observable here — it is surfaced by the F1-13 turn wrapper.
    fn outcome_for(stop_reason: &str) -> TurnOutcomeDto {
        match stop_reason {
            "max_tokens" => TurnOutcomeDto::MaxTurns,
            _ => TurnOutcomeDto::EndTurn,
        }
    }

    async fn emit_buffered_message(&self, stop_reason: Option<&str>, include_empty: bool) {
        let blocks = std::mem::take(&mut *self.message_blocks.lock().await);
        if blocks.is_empty() && !include_empty {
            return;
        }
        self.sink
            .emit(ClientEvent::MessageComplete {
                stop_reason: stop_reason.map(str::to_string),
                message: Some(MessageDto {
                    loop_wakeup: None,
                    role: "assistant".to_string(),
                    blocks,
                    images: Vec::new(),
                }),
            })
            .await;
    }
}

#[async_trait]
impl OutputStream for AdapterOutputStream {
    async fn emit_task_lifecycle(&self, event: &serde_json::Value) {
        self.sink
            .emit(ClientEvent::TaskLifecycle {
                event_json: event.to_string(),
            })
            .await;
    }

    /// Announce a turn the CLIENT did not submit — claude-code enqueues a
    /// background-task completion onto the SAME command queue as typed input
    /// (`enqueuePendingNotification` pushes onto the array `enqueue` pushes
    /// onto) and the main loop then runs it as an ordinary turn: same spinner,
    /// same transcript, same permission prompts. There is no "turn the client
    /// did not start" anywhere in the oracle, so a host must be able to tell a
    /// rewake apart from an idle connection.
    ///
    /// This impl was MISSING, so the trait's no-op default ran and
    /// [`ClientEvent::TurnStarted`] had NO producer on the bridge path at all.
    /// A desktop host that mirrors turn liveness (Electron's `activeTurn`, set
    /// only when it itself sends a prompt and cleared by every `TurnEnded`)
    /// therefore sat at "idle" for the whole of an engine-initiated turn and
    /// dropped its events — and its permission requests, which then died at the
    /// gate's 300s timeout with no prompt ever shown. `turn_id` is left `None`:
    /// a rewake has no client correlator, and the bridge's `FrameEventSink`
    /// stamps the owning turn's id on the way out.
    async fn emit_turn_started(&self) {
        self.sink
            .emit(crate::adapter::turn::turn_started_event(None))
            .await;
    }

    async fn emit_text(&self, text: &str) {
        let mut blocks = self.message_blocks.lock().await;
        if let Some(MessageBlockDto::Text { text: current }) = blocks.last_mut() {
            current.push_str(text);
        } else {
            blocks.push(MessageBlockDto::Text {
                text: text.to_string(),
            });
        }
        drop(blocks);
        self.sink
            .emit(ClientEvent::TextDelta {
                text: text.to_string(),
            })
            .await;
    }

    async fn emit_assistant_message_identity(&self, message_id: &lingxi_core::types::MessageId) {
        self.sink
            .emit(ClientEvent::MessageIdentity {
                message_id: message_id.as_uuid().to_string(),
            })
            .await;
    }

    async fn emit_message_retracted(&self, message_id: &lingxi_core::types::MessageId) {
        self.message_blocks.lock().await.clear();
        self.sink
            .emit(ClientEvent::MessageRetracted {
                message_id: message_id.as_uuid().to_string(),
            })
            .await;
    }

    async fn emit_system_notice(&self, message: &str, is_error: bool) {
        self.sink
            .emit(ClientEvent::SystemNotice {
                message: message.to_string(),
                is_error,
            })
            .await;
    }

    async fn emit_tool_call(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        input: &serde_json::Value,
    ) {
        self.remember_call(id, tool, input);
        self.message_blocks
            .lock()
            .await
            .push(MessageBlockDto::ToolUse {
                id: id.to_string(),
                tool: tool.to_string(),
                input_json: value_to_json_string(input),
                header: Some(crate::adapter::tool_display::lower_tool_header(tool, input)),
            });
        self.sink
            .emit(ClientEvent::ToolUseStarted {
                id: id.to_string(),
                tool: tool.to_string(),
                input_json: value_to_json_string(input),
                header: Some(crate::adapter::tool_display::lower_tool_header(tool, input)),
            })
            .await;
        // TodoWrite rewrites the whole plan. Emitted on the CALL, not the
        // result, matching how the terminal updates its pinned block.
        if let Some(tasks) = crate::adapter::tool_display::plan_from_tool_call(tool, input) {
            self.sink.emit(ClientEvent::PlanUpdated { tasks }).await;
        }
    }

    async fn emit_tool_heartbeat(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        elapsed_ms: u64,
    ) {
        self.sink
            .emit(ClientEvent::ToolHeartbeat {
                id: id.to_string(),
                tool: tool.to_string(),
                elapsed_ms,
            })
            .await;
    }

    async fn emit_tool_result(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        _model_text: &str,
        result: &serde_json::Value,
    ) {
        // The `client::protocol` `ToolUseResult` DTO is wire-frozen, so we do NOT
        // add a `model_text` field yet — the adapter ignores it and keeps
        // lowering the full metadata `data` into `result_json`. `is_error` still
        // derives structurally from the `{ "error": … }` payload shape. KNOWN
        // RESIDUAL: a migrated tool's model text is not carried on this DTO; if a
        // consumer needs it, a follow-up DTO field is required (out of scope).
        self.emit_tool_result_event(id, tool, result).await;
    }

    async fn emit_tool_result_denied(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        _model_text: &str,
        result: &serde_json::Value,
        denial_kind: &str,
    ) {
        // Keep the frozen `ClientEvent` shape while carrying the engine's
        // structured interruption provenance inside the already-extensible JSON
        // payload. Existing clients ignore the additive key; newer clients can
        // distinguish cancellation from a real tool failure without matching a
        // localized error string.
        let mut tagged = result.clone();
        if let serde_json::Value::Object(fields) = &mut tagged {
            fields.insert(
                "tool_denial_kind".to_string(),
                serde_json::Value::String(denial_kind.to_string()),
            );
        }
        self.emit_tool_result_event(id, tool, &tagged).await;
    }

    async fn emit_end_turn(&self, stop_reason: &str, cost: &CostSnapshot) {
        // Any call still awaiting a result at turn end never gets one; drop
        // the side-table so it cannot leak across turns. Mirrors
        // `ActiveTurn`'s `tool_inputs.clear()` on `TurnEvent::TurnEnded`.
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        // Guarded terminal paths can emit assistant text and end without the
        // normal persisted-message callback. Flush that residual response here,
        // still before the terminal marker.
        self.emit_buffered_message(Some(stop_reason), false).await;
        // Emit the cumulative cost update BEFORE the turn-end marker so a client
        // can refresh its cost line in the same render pass it ends the turn —
        // exactly the ordering `BridgeOutputStream::emit_end_turn` uses
        // (`orchestrator_bridge.rs:148-162`).
        let cost_dto = lower_cost_snapshot(cost);
        self.sink
            .emit(ClientEvent::CostUpdate {
                total_usd: cost_dto.total_usd,
                input_tokens: cost_dto.input_tokens,
                output_tokens: cost_dto.output_tokens,
                api_calls: cost_dto.api_calls,
                session_duration_secs: cost_dto.session_duration_secs,
                formatted: cost_dto.formatted.clone(),
            })
            .await;

        self.sink
            .emit(ClientEvent::TurnEnded {
                outcome: Self::outcome_for(stop_reason),
                stop_reason: Some(stop_reason.to_string()),
                cost: cost_dto,
            })
            .await;
    }

    async fn emit_compaction_started(&self) {
        self.emit_compaction_phase("preparing").await;
    }

    async fn emit_compaction_skipped(&self) {
        self.emit_compaction_phase("skipped").await;
    }

    async fn emit_compaction_phase(&self, phase: &str) {
        self.sink
            .emit(ClientEvent::CompactionStatus {
                phase: phase.to_string(),
                error: None,
            })
            .await;
    }

    async fn emit_compaction_finished(&self, error: Option<&str>) {
        let phase = match error {
            None => "complete",
            Some("Compaction canceled.") => "cancelled",
            Some(_) => "error",
        };
        self.sink
            .emit(ClientEvent::CompactionStatus {
                phase: phase.to_string(),
                error: error.map(str::to_string),
            })
            .await;
    }

    async fn emit_compaction_completed(
        &self,
        messages_before: u32,
        messages_after: u32,
        bytes_saved: u64,
        summary: &str,
    ) {
        self.sink
            .emit(ClientEvent::CompactionCompleted {
                messages_before,
                messages_after,
                bytes_saved,
                summary: summary.to_string(),
            })
            .await;
    }

    /// §0.7 "light up thinking/usage": lower each live reasoning delta into a
    /// [`ClientEvent::ThinkingDelta`]. `signature` is `None` for live deltas
    /// (the cryptographic signature only arrives on the completed thinking
    /// block, not per-delta) — see `lingxi_core::host::OutputStream::emit_thinking`.
    async fn emit_thinking(&self, thinking: &str, signature: Option<&str>) {
        let mut blocks = self.message_blocks.lock().await;
        if let Some(MessageBlockDto::Thinking {
            thinking: current,
            signature: current_signature,
        }) = blocks.last_mut()
        {
            current.push_str(thinking);
            if signature.is_some() {
                *current_signature = signature.map(str::to_string);
            }
        } else {
            blocks.push(MessageBlockDto::Thinking {
                thinking: thinking.to_string(),
                signature: signature.map(str::to_string),
            });
        }
        drop(blocks);
        self.sink
            .emit(ClientEvent::ThinkingDelta {
                thinking: thinking.to_string(),
                signature: signature.map(str::to_string),
            })
            .await;
    }

    async fn emit_redacted_thinking(&self, data: &str) {
        self.message_blocks
            .lock()
            .await
            .push(MessageBlockDto::RedactedThinking {
                data: data.to_string(),
            });
    }

    async fn emit_message_boundary(&self, stop_reason: Option<&str>, _request_id: Option<&str>) {
        self.emit_buffered_message(stop_reason, true).await;
    }

    /// §0.7 "light up thinking/usage": lower each incremental token-usage
    /// update into a [`ClientEvent::UsageUpdate`]. The four counters map
    /// field-for-field from `lingxi_core::host::OutputStream::emit_usage` (which itself
    /// mirrors `cost::TokenUsage` on the orchestrator side).
    async fn emit_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) {
        self.sink
            .emit(ClientEvent::UsageUpdate {
                is_snapshot: None,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            })
            .await;
    }

    /// Coordinator-activation T09 (§0.9 reserved→live): push the live
    /// active-worker scalar as a [`ClientEvent::CoordinatorStatus`]. Fired from
    /// the `CoordinatorStatusSink` on every status transition that changes the
    /// active count. Mirrors `emit_thinking`/`emit_usage`: the single
    /// `Arc<dyn ClientEventSink>` already fans out to bridge WS + mobile UniFFI,
    /// so there is NO transport change — only the trait override lights up the
    /// previously-no-op (T08) default. `team` maps `Option<&str>` →
    /// `Option<String>` 1:1 (no placeholder substitution).
    async fn emit_coordinator_worker(&self, worker: &lingxi_core::host::team_registry::WorkerInfo) {
        self.sink
            .emit(ClientEvent::CoordinatorWorker {
                worker: crate::adapter::lowering::lower_worker_agent(worker),
            })
            .await;
    }

    async fn emit_coordinator_status(&self, active_workers: u32, team: Option<&str>) {
        self.sink
            .emit(ClientEvent::CoordinatorStatus {
                active_workers,
                team: team.map(str::to_string),
            })
            .await;
    }

    async fn emit_attachment(&self, attachment: lingxi_core::host::AttachmentKind) {
        let dto = match attachment {
            lingxi_core::host::AttachmentKind::NestedMemory { display_path } => {
                crate::protocol::events::AttachmentDto::NestedMemory { display_path }
            }
            // `AttachmentKind` is `#[non_exhaustive]`: a kind added upstream
            // without a DTO here must not be silently swallowed into a wrong
            // variant. Dropping it renders nothing, which is visible; guessing
            // would render something false.
            _ => return,
        };
        self.sink
            .emit(ClientEvent::Attachment { attachment: dto })
            .await;
    }

    async fn emit_api_retry(&self, message: &str, attempt: u32, max_retries: u32, delay_ms: u64) {
        self.sink
            .emit(ClientEvent::ApiRetry {
                message: message.to_string(),
                attempt,
                max_retries,
                delay_ms,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adapter::test_support::MockSink;

    /// `emit_text` → exactly one `TextDelta` carrying the payload verbatim.
    #[tokio::test]
    async fn task_lifecycle_reaches_client_event_sink() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let event = serde_json::json!({"type":"system", "subtype":"task_updated", "task_id":"b12345678", "patch":{"status":"completed"}});
        stream.emit_task_lifecycle(&event).await;
        assert_eq!(
            sink.events().await,
            vec![ClientEvent::TaskLifecycle {
                event_json: event.to_string()
            }]
        );
    }

    /// `emit_turn_started` must reach the sink as a real `TurnStarted`.
    ///
    /// This impl did not exist, so the `OutputStream` trait's no-op default ran
    /// and the ONLY producer of `ClientEvent::TurnStarted` on the bridge path
    /// was nothing at all. Every turn the engine started by itself — a
    /// background-task rewake, a queue drain — reached the desktop as a stream
    /// of events for a turn the client had never been told about, and the
    /// Electron host, whose `activeTurn` is armed only by its own `sendPrompt`
    /// and cleared by every `TurnEnded`, dropped all of them.
    ///
    /// Asserting on the SINK (not on the call returning) is the point: a
    /// default-implemented trait method returns `()` just as happily as a wired
    /// one, so only the emitted event distinguishes the two.
    #[tokio::test]
    async fn emit_turn_started_reaches_client_event_sink() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        lingxi_core::host::OutputStream::emit_turn_started(&stream).await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::TurnStarted { turn_id: None }],
            "an engine-initiated turn must announce itself; `turn_id` stays None \
             because a rewake has no client correlator and the bridge's \
             FrameEventSink stamps the owning turn's id on the way out",
        );
    }

    #[tokio::test]
    async fn emit_text_produces_text_delta() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream.emit_text("hello world").await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::TextDelta {
                text: "hello world".to_string()
            }
        );
    }

    #[tokio::test]
    async fn retry_retraction_carries_identity_and_clears_partial_blocks() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let id = lingxi_core::types::MessageId::new();
        stream.emit_text("rejected").await;
        stream.emit_assistant_message_identity(&id).await;
        stream.emit_message_retracted(&id).await;
        stream.emit_text("clean").await;
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let events = sink.events().await;
        assert_eq!(
            events[1],
            ClientEvent::MessageIdentity {
                message_id: id.as_uuid().to_string()
            }
        );
        assert_eq!(
            events[2],
            ClientEvent::MessageRetracted {
                message_id: id.as_uuid().to_string()
            }
        );
        let ClientEvent::MessageComplete {
            message: Some(message),
            ..
        } = events.last().unwrap()
        else {
            panic!("missing completed message")
        };
        assert_eq!(
            message.blocks,
            vec![MessageBlockDto::Text {
                text: "clean".into()
            }]
        );
    }

    /// Non-terminal persistence diagnostics must cross the adapter boundary;
    /// silently accepting the trait default would hide them from clients.
    #[tokio::test]
    async fn emit_system_notice_produces_system_notice() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_system_notice("Conversation changes could not be saved.", true)
            .await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::SystemNotice {
                message: "Conversation changes could not be saved.".to_string(),
                is_error: true,
            }]
        );
    }

    /// `emit_tool_call` → one `ToolUseStarted`; the `Value` input is lowered to
    /// the `input_json` JSON String (F1-11) and the id to its `tu:` string form.
    #[tokio::test]
    async fn emit_tool_call_produces_tool_use_started() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let id = lingxi_core::types::ToolUseId::new();
        let input = serde_json::json!({"file_path": "/tmp/x"});
        stream.emit_tool_call(&id, "Read", &input).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::ToolUseStarted {
                id: gid,
                tool,
                input_json,
                ..
            } => {
                assert_eq!(*gid, id.to_string());
                assert_eq!(tool, "Read");
                // The lowered string round-trips back to the original Value.
                let back: serde_json::Value = serde_json::from_str(input_json).unwrap();
                assert_eq!(back, input);
            }
            other => panic!("expected ToolUseStarted, got {other:?}"),
        }
    }

    /// A success tool result (no top-level `"error"` key) lowers to
    /// `ToolUseResult { is_error: false }`.
    #[tokio::test]
    async fn emit_tool_result_success_is_not_error() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let id = lingxi_core::types::ToolUseId::new();
        let result = serde_json::json!({"content": "ok", "lines": 3});
        stream.emit_tool_result(&id, "Read", "ok", &result).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::ToolUseResult {
                id: gid,
                tool,
                result_json,
                is_error,
                ..
            } => {
                assert_eq!(*gid, id.to_string());
                assert_eq!(tool, "Read");
                let back: serde_json::Value = serde_json::from_str(result_json).unwrap();
                assert_eq!(back, result);
                assert!(!is_error);
            }
            other => panic!("expected ToolUseResult, got {other:?}"),
        }
    }

    /// A failed tool result — the orchestrator's `{ "error": "<msg>" }` payload
    /// shape (`turn_loop.rs:327`) — lowers to `ToolUseResult { is_error: true }`.
    #[tokio::test]
    async fn emit_tool_result_error_payload_sets_is_error() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let id = lingxi_core::types::ToolUseId::new();
        let result = serde_json::json!({"error": "file not found"});
        stream
            .emit_tool_result(&id, "Read", "file not found", &result)
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::ToolUseResult { is_error, .. } => assert!(*is_error),
            other => panic!("expected ToolUseResult, got {other:?}"),
        }
    }

    /// An interrupted tool result preserves the ordinary error contract while
    /// adding machine-readable cancellation provenance for newer clients.
    #[tokio::test]
    async fn emit_tool_result_denied_tags_interruption_kind() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let id = lingxi_core::types::ToolUseId::new();
        let result = serde_json::json!({"error": "interrupted"});
        stream
            .emit_tool_result_denied(&id, "Bash", "interrupted", &result, "interrupted")
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::ToolUseResult {
                result_json,
                is_error,
                ..
            } => {
                assert!(*is_error);
                let payload: serde_json::Value = serde_json::from_str(result_json).unwrap();
                assert_eq!(payload["error"], "interrupted");
                assert_eq!(payload["tool_denial_kind"], "interrupted");
            }
            other => panic!("expected ToolUseResult, got {other:?}"),
        }
    }

    /// `emit_end_turn` produces BOTH a `CostUpdate` and a `TurnEnded`, in that
    /// order (the named F1-12 assertion). The cost lowers via `lower_cost_snapshot`.
    #[tokio::test]
    async fn emit_end_turn_produces_cost_then_turn_ended() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let cost = CostSnapshot {
            total_usd: 0.0123,
            input_tokens: 100,
            output_tokens: 50,
            api_calls: 3,
            session_duration: Duration::from_secs(125),
            ..Default::default()
        };
        stream.emit_end_turn("end_turn", &cost).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 2, "expected CostUpdate then TurnEnded");

        // First: the cumulative cost update.
        match &events[0] {
            ClientEvent::CostUpdate {
                total_usd,
                input_tokens,
                output_tokens,
                api_calls,
                session_duration_secs,
                formatted,
            } => {
                #[allow(clippy::float_cmp)]
                {
                    assert_eq!(*total_usd, 0.0123);
                }
                assert_eq!(*input_tokens, 100);
                assert_eq!(*output_tokens, 50);
                assert_eq!(*api_calls, 3);
                assert_eq!(*session_duration_secs, 125);
                assert_eq!(formatted, "$0.0123");
            }
            other => panic!("expected CostUpdate first, got {other:?}"),
        }

        // Second: the turn-end marker, carrying the same lowered cost.
        match &events[1] {
            ClientEvent::TurnEnded {
                outcome,
                stop_reason,
                cost: cost_dto,
            } => {
                assert_eq!(*outcome, TurnOutcomeDto::EndTurn);
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(cost_dto.session_duration_secs, 125);
                assert_eq!(cost_dto.formatted, "$0.0123");
            }
            other => panic!("expected TurnEnded second, got {other:?}"),
        }
    }

    /// `"max_tokens"` stop reason ends the turn with the `MaxTurns` outcome,
    /// while the raw reason is preserved on `TurnEnded.stop_reason`.
    #[tokio::test]
    async fn emit_end_turn_max_tokens_maps_to_max_turns_outcome() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_end_turn("max_tokens", &CostSnapshot::default())
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 2);
        match &events[1] {
            ClientEvent::TurnEnded {
                outcome,
                stop_reason,
                ..
            } => {
                assert_eq!(*outcome, TurnOutcomeDto::MaxTurns);
                assert_eq!(stop_reason.as_deref(), Some("max_tokens"));
            }
            other => panic!("expected TurnEnded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compaction_status_reports_actual_phases_and_terminal_outcomes() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream.emit_compaction_started().await;
        stream.emit_compaction_phase("summarizing").await;
        stream.emit_compaction_phase("restoring").await;
        stream.emit_compaction_finished(None).await;
        stream
            .emit_compaction_finished(Some("summary failed"))
            .await;
        stream
            .emit_compaction_finished(Some("Compaction canceled."))
            .await;
        stream.emit_compaction_skipped().await;
        let events = sink.events().await;
        let expected = [
            ("preparing", None),
            ("summarizing", None),
            ("restoring", None),
            ("complete", None),
            ("error", Some("summary failed")),
            ("cancelled", Some("Compaction canceled.")),
            ("skipped", None),
        ];
        assert_eq!(events.len(), expected.len());
        for (event, (phase, error)) in events.iter().zip(expected) {
            assert_eq!(
                event,
                &ClientEvent::CompactionStatus {
                    phase: phase.into(),
                    error: error.map(str::to_string),
                }
            );
        }
    }

    /// `emit_compaction_completed` → one `CompactionCompleted` carrying the
    /// counters and summary verbatim.
    #[tokio::test]
    async fn emit_compaction_completed_produces_compaction_completed() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_compaction_completed(42, 8, 1_024, "Summary:\nkept context")
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::CompactionCompleted {
                messages_before: 42,
                messages_after: 8,
                bytes_saved: 1_024,
                summary: "Summary:\nkept context".to_string(),
            }
        );
    }

    /// §0.7: `emit_thinking` → exactly one `ThinkingDelta` carrying the reasoning
    /// text verbatim. Live deltas carry `signature: None` (the trait passes `None`
    /// per-delta — the signature only lands on the completed block).
    #[tokio::test]
    async fn emit_thinking_produces_thinking_delta() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream.emit_thinking("let me reason about this", None).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::ThinkingDelta {
                thinking: "let me reason about this".to_string(),
                signature: None,
            }
        );
    }

    /// §0.7: a `Some(signature)` is forwarded onto `ThinkingDelta.signature`
    /// (proves the adapter does not hard-code `None` — it maps whatever the
    /// engine passes, future-proofing the completed-block signature path).
    #[tokio::test]
    async fn emit_thinking_forwards_signature_when_present() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_thinking("done reasoning", Some("sig-abc"))
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::ThinkingDelta {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "done reasoning");
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("expected ThinkingDelta, got {other:?}"),
        }
    }

    /// §0.7: `emit_usage` → exactly one `UsageUpdate` with the four token
    /// counters mapped field-for-field.
    #[tokio::test]
    async fn emit_usage_produces_usage_update() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream.emit_usage(120, 48, 30, 90).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::UsageUpdate {
                is_snapshot: None,
                input_tokens: 120,
                output_tokens: 48,
                cache_read_tokens: 30,
                cache_creation_tokens: 90,
            }
        );
    }

    /// Coordinator-activation T09: `emit_coordinator_status` → exactly one
    /// `CoordinatorStatus` carrying the active-worker scalar and the (mapped)
    /// team name. Mirrors `emit_thinking_produces_thinking_delta`.
    #[tokio::test]
    async fn emit_coordinator_status_produces_one_event() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream.emit_coordinator_status(3, Some("alpha")).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::CoordinatorStatus {
                active_workers: 3,
                team: Some("alpha".to_string()),
            }
        );
    }

    /// Coordinator-activation T09: a `None` team round-trips as `team: None`
    /// (the adapter maps `Option<&str>` → `Option<String>` rather than
    /// substituting a placeholder), proving the absent-team path.
    #[tokio::test]
    async fn emit_coordinator_status_none_team() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream.emit_coordinator_status(0, None).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::CoordinatorStatus {
                active_workers: 0,
                team: None,
            }
        );
    }

    /// The default-trait `emit_compaction_completed` is overridden — exercising
    /// it through an `Arc<dyn OutputStream>` proves the stream is object-safe and
    /// usable in the form the orchestrator binds (`Arc<dyn OutputStream>`).
    #[tokio::test]
    async fn usable_as_dyn_output_stream() {
        let sink = MockSink::arc();
        let stream: Arc<dyn OutputStream> = Arc::new(AdapterOutputStream::new(sink.clone()));

        stream.emit_text("via trait object").await;

        let events = sink.events().await;
        assert_eq!(
            events[0],
            ClientEvent::TextDelta {
                text: "via trait object".to_string()
            }
        );
    }

    /// One completed API response is emitted before the turn terminal, with
    /// its text/reasoning/tool blocks in the same order the live callbacks
    /// arrived. This is the ordering contract consumed by transcript UIs.
    #[tokio::test]
    async fn message_boundary_emits_ordered_complete_before_turn_ended() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let id = lingxi_core::types::ToolUseId::new();

        stream.emit_text("A").await;
        stream.emit_thinking("reason", Some("sig")).await;
        stream
            .emit_tool_call(&id, "Read", &serde_json::json!({"path": "a"}))
            .await;
        stream.emit_text("B").await;
        stream.emit_message_boundary(Some("end_turn"), None).await;
        stream
            .emit_end_turn("end_turn", &CostSnapshot::default())
            .await;

        let events = sink.events().await;
        let complete = events
            .iter()
            .position(|event| matches!(event, ClientEvent::MessageComplete { .. }))
            .expect("message complete");
        let ended = events
            .iter()
            .position(|event| matches!(event, ClientEvent::TurnEnded { .. }))
            .expect("turn ended");
        assert!(
            complete < ended,
            "MessageComplete must precede TurnEnded: {events:?}"
        );
        match &events[complete] {
            ClientEvent::MessageComplete {
                message: Some(message),
                ..
            } => {
                assert!(
                    matches!(&message.blocks[0], MessageBlockDto::Text { text } if text == "A")
                );
                assert!(
                    matches!(&message.blocks[1], MessageBlockDto::Thinking { thinking, .. } if thinking == "reason")
                );
                assert!(
                    matches!(&message.blocks[2], MessageBlockDto::ToolUse { id: got, .. } if got == &id.to_string())
                );
                assert!(
                    matches!(&message.blocks[3], MessageBlockDto::Text { text } if text == "B")
                );
            }
            other => panic!("expected completed message, got {other:?}"),
        }
    }

    /// The live turn and a resumed transcript must produce the SAME
    /// `ToolResultDisplayDto` for the same `(tool, input, result)`.
    ///
    /// This is the invariant that makes a session look identical before and
    /// after a restart. They reach the DTO by different routes — the live path
    /// pairs the call through `AdapterOutputStream`'s pending map, the resume
    /// path through `turn::ToolUseIndex` across two messages — so nothing but
    /// a test keeps them from drifting.
    ///
    /// The two routes are fed DIFFERENT payloads on purpose, because that is
    /// what the engine feeds them: the live path gets `ToolCallResult.data`
    /// (the object), the resumed path the tool's model-facing STRING. Handing
    /// both the same `Value::String` — as this test used to — never crossed
    /// the seam it exists to guard, and it stayed green through the whole
    /// window in which resumed Bash/Read results rendered "(No content)".
    #[tokio::test]
    async fn live_and_resumed_paths_produce_identical_tool_result_displays() {
        use crate::protocol::message::MessageBlockDto;
        use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

        let id = lingxi_core::types::ToolUseId::new();
        let tool = "Edit";
        let input = serde_json::json!({
            "file_path": "/tmp/x.rs",
            "old_string": "fn a() {}\n",
            "new_string": "fn b() {}\nfn c() {}\n",
        });
        // The LIVE payload: the literal `data` shape from
        // `tools/file/src/edit.rs`.
        let result = serde_json::json!({
            "filePath": "/tmp/x.rs",
            "oldString": "fn a() {}\n",
            "newString": "fn b() {}\nfn c() {}\n",
            "originalFile": "fn a() {}\n",
            "structuredPatch": "-fn a() {}\n+fn b() {}\n+fn c() {}\n",
            "userModified": false,
            "replaceAll": false,
        });
        // What the transcript actually persists for that same call: the
        // model-facing string (`ToolCallResult.model_content`).
        let content = "The file /tmp/x.rs has been updated.";

        // ── live ──────────────────────────────────────────────────────────
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream.emit_tool_call(&id, tool, &input).await;
        stream.emit_tool_result(&id, tool, "", &result).await;
        let live = sink
            .events()
            .await
            .into_iter()
            .find_map(|e| match e {
                ClientEvent::ToolUseResult { display, .. } => display,
                _ => None,
            })
            .expect("a live display");

        // ── resumed ───────────────────────────────────────────────────────
        let history = vec![
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: tool.to_string(),
                    input: input.clone(),
                    provider_id: None,
                }],
                stop_reason: Some("tool_use".to_string()),
            },
            ConversationMessage::User {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: content.to_string(),
                    is_error: false,
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            },
        ];
        let resumed = crate::adapter::lowering::lower_transcript(&history)
            .into_iter()
            .flat_map(|m| m.blocks)
            .find_map(|b| match b {
                MessageBlockDto::ToolResult { display, .. } => display,
                _ => None,
            })
            .expect("a resumed display");

        assert_eq!(live, resumed, "live and resumed displays must be identical");
        // And it is a real display, not two matching empties.
        assert_eq!(
            live.headline.as_deref(),
            Some("Added 2 lines, removed 1 line")
        );
        assert!(live.diff.is_some_and(|d| d.rows.len() == 3));
        // The diff IS the body for an edit; the pre-edit file never ships as
        // user-visible text.
        assert_eq!(live.body, None);
    }

    /// Overflowing the pending-call cap must cost ONE call, not all of them.
    ///
    /// `remember_call` used to `clear()` the whole side-table at the cap, so
    /// the 257th dispatched call wiped every other in-flight call's input —
    /// and with it every one of their structured diffs and edit headlines.
    #[tokio::test]
    async fn overflowing_the_pending_cap_evicts_only_the_oldest_call() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let ids: Vec<lingxi_core::types::ToolUseId> = (0..=MAX_PENDING_TOOL_CALLS)
            .map(|_| lingxi_core::types::ToolUseId::new())
            .collect();
        for id in &ids {
            let input = serde_json::json!({
                "file_path": "/tmp/x.rs",
                "old_string": "a\n",
                "new_string": "b\n",
            });
            stream.emit_tool_call(id, "Edit", &input).await;
        }

        // The OLDEST call is the one that fell out.
        assert!(stream.take_call(&ids[0]).is_none(), "oldest is evicted");
        // Every other call — including the newest — still has its input.
        for id in &ids[1..] {
            assert!(
                stream.take_call(id).is_some(),
                "an overflow must not wipe the other in-flight calls"
            );
        }
    }
}
