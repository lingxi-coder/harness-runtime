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
//! | `emit_mod_log`              | `UiLog`                            |
//! | `emit_mod_ui_client_frame`  | `UiClientFrame`                    |
//! | `emit_mod_ui_invalidate`    | `UiInvalidate`                     |
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

use crate::protocol::events::{
    ClientEvent, RefusalContinuationJoinDto, RefusalContinuationPhaseDto,
    ServerFallbackProviderMessageDto, ServerFallbackTombstoneMessageDto, TurnOutcomeDto,
};
use crate::protocol::message::{MessageBlockDto, MessageDto};
use crate::protocol::text::ClientTextDto;
use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OutputStream, ServerFallbackTombstoneMessage};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

use crate::adapter::lowering::lower_cost_snapshot;
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
    message_blocks: Arc<tokio::sync::Mutex<MessageBuffer>>,
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
        Arc<std::sync::Mutex<std::collections::VecDeque<(String, (String, Utf16JsonProjection))>>>,
}

#[derive(Default)]
struct MessageBuffer {
    blocks: Vec<BufferedMessageBlock>,
    active_block_key: Option<u64>,
    last_completed_row_id: Option<String>,
    pending_continuation: Option<PendingContinuation>,
    /// Splits the current text block into narration and visualization
    /// slots; the same splitter replay uses, so live and resumed blocks agree.
    references: visualization::ReferenceSplitter,
    /// Exact source units for text the splitter is holding back. The splitter
    /// classifies a display projection; these units keep ordinary text exact.
    pending_source_units: std::collections::VecDeque<u16>,
}

struct BufferedMessageBlock {
    block: MessageBlockDto,
    block_key: Option<u64>,
    row_id: Option<String>,
}

struct PendingContinuation {
    salvage_text: ClientTextDto,
    display_salvage_text: bool,
    replaces_uuids: std::collections::HashSet<String>,
}

impl MessageBuffer {
    fn association(&self) -> (Option<u64>, Option<String>) {
        match self.active_block_key {
            Some(block_key) => (Some(block_key), None),
            None => (None, self.last_completed_row_id.clone()),
        }
    }

    fn append(&mut self, block: MessageBlockDto) {
        let (block_key, row_id) = self.association();
        let same_association = self
            .blocks
            .last()
            .is_some_and(|previous| previous.block_key == block_key && previous.row_id == row_id);
        let mut next = Some(block);
        if same_association {
            if let Some(previous) = self.blocks.last_mut() {
                match (&mut previous.block, next.take().expect("pending block")) {
                    (MessageBlockDto::Text { text: current }, MessageBlockDto::Text { text }) => {
                        current.code_units.extend(text.code_units);
                    }
                    (
                        MessageBlockDto::Thinking {
                            thinking: current,
                            signature: current_signature,
                        },
                        MessageBlockDto::Thinking {
                            thinking,
                            signature,
                        },
                    ) => {
                        current.push_str(&thinking);
                        if signature.is_some() {
                            *current_signature = signature;
                        }
                    }
                    (_, other) => next = Some(other),
                }
            }
        }
        if let Some(block) = next {
            self.blocks.push(BufferedMessageBlock {
                block,
                block_key,
                row_id: row_id.clone(),
            });
        }
        self.apply_pending_if_eligible(row_id.as_deref());
    }

    fn bind_identity(&mut self, block_key: u64, row_id: &str) {
        for entry in &mut self.blocks {
            if entry.block_key == Some(block_key) {
                entry.row_id = Some(row_id.to_string());
            }
        }
        self.last_completed_row_id = Some(row_id.to_string());
        if self.active_block_key == Some(block_key) {
            self.active_block_key = None;
        }
        self.apply_pending_if_eligible(Some(row_id));
    }

    fn apply_pending_if_eligible(&mut self, row_id: Option<&str>) {
        let Some(row_id) = row_id else {
            return;
        };
        let Some(index) = self.blocks.iter().position(|entry| {
            entry.row_id.as_deref() == Some(row_id)
                && matches!(&entry.block, MessageBlockDto::Text { .. })
        }) else {
            return;
        };
        let MessageBlockDto::Text { text } = &mut self.blocks[index].block else {
            return;
        };
        if !has_non_whitespace_js(text) {
            return;
        }
        let Some(pending) = self.pending_continuation.take() else {
            return;
        };
        if pending.display_salvage_text {
            text.code_units
                .splice(0..0, pending.salvage_text.code_units);
        }
        self.blocks.retain(|entry| {
            !entry
                .row_id
                .as_ref()
                .is_some_and(|uuid| pending.replaces_uuids.contains(uuid))
        });
    }
}

impl MessageBuffer {
    /// Record splitter output in the message and return the live events, in
    /// order, for the caller to emit after releasing the buffer lock.
    fn absorb(&mut self, events: Vec<visualization::SplitEvent>) -> Vec<ClientEvent> {
        use crate::protocol::message::VisualizationBlockStatusDto as Status;
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            match event {
                visualization::SplitEvent::Text(display) => {
                    let count = display.encode_utf16().count();
                    let available = count.min(self.pending_source_units.len());
                    let source = self.pending_source_units.drain(..available).collect();
                    debug_assert_eq!(available, count);
                    let text = ClientTextDto::from_code_units(source);
                    self.append(MessageBlockDto::Text { text: text.clone() });
                    out.push(ClientEvent::TextDelta { text });
                }
                visualization::SplitEvent::Pending => out.push(ClientEvent::VisualizationBlock {
                    status: Status::Pending,
                    reference: None,
                }),
                visualization::SplitEvent::Ready(reference) => {
                    discard_reference_source_line(&mut self.pending_source_units);
                    let reference = crate::adapter::lowering::visualization_ref_dto(&reference);
                    self.append(MessageBlockDto::Visualization {
                        reference: Some(reference.clone()),
                    });
                    out.push(ClientEvent::VisualizationBlock {
                        status: Status::Ready,
                        reference: Some(reference),
                    });
                }
                visualization::SplitEvent::Unavailable => {
                    discard_reference_source_line(&mut self.pending_source_units);
                    self.append(MessageBlockDto::Visualization { reference: None });
                    out.push(ClientEvent::VisualizationBlock {
                        status: Status::Unavailable,
                        reference: None,
                    });
                }
                visualization::SplitEvent::Discarded => {
                    discard_reference_source_line(&mut self.pending_source_units);
                    out.push(ClientEvent::VisualizationBlock {
                        status: Status::Discarded,
                        reference: None,
                    });
                }
            }
        }
        out
    }

    /// The current text block ended: settle a held reference line.
    fn finish_text(&mut self) -> Vec<ClientEvent> {
        let events = self.references.finish();
        let emitted = self.absorb(events);
        debug_assert!(self.pending_source_units.is_empty());
        self.pending_source_units.clear();
        emitted
    }

    /// The stream was cut: drop a held partial line.
    fn abort_text(&mut self) -> Vec<ClientEvent> {
        let events = self.references.abort();
        let emitted = self.absorb(events);
        self.pending_source_units.clear();
        emitted
    }
}

fn discard_reference_source_line(source_units: &mut std::collections::VecDeque<u16>) {
    while let Some(unit) = source_units.pop_front() {
        if unit == u16::from(b'\n') {
            break;
        }
    }
}

fn has_non_whitespace_js(text: &ClientTextDto) -> bool {
    text.display()
        .chars()
        .any(|character| !character.is_whitespace() && character != '\u{feff}')
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
            message_blocks: Arc::new(tokio::sync::Mutex::new(MessageBuffer::default())),
            pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        }
    }

    async fn emit_all(&self, events: Vec<ClientEvent>) {
        for event in events {
            self.sink.emit(event).await;
        }
    }

    /// End the current assistant text block before a non-text event.
    async fn finish_text_block(&self) {
        let events = self.message_blocks.lock().await.finish_text();
        self.emit_all(events).await;
    }

    /// Drop a held partial reference line after a retraction or refusal.
    async fn abort_text_block(&self) {
        let events = self.message_blocks.lock().await.abort_text();
        self.emit_all(events).await;
    }

    /// Clear an unfinished response before a host starts a new turn or after a
    /// hard failure that did not reach an engine message boundary.
    pub async fn reset_message_buffer(&self) {
        *self.message_blocks.lock().await = MessageBuffer::default();
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
        input: &Utf16JsonProjection,
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
    fn take_call(
        &self,
        id: &lingxi_core::types::ToolUseId,
    ) -> Option<(String, Utf16JsonProjection)> {
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
    fn result_is_error(result: &Utf16JsonProjection) -> bool {
        result.contains_property("error")
    }

    /// Materialize a display-only view for the existing tool-card renderer.
    /// Machine-facing event JSON continues to serialize the source projection.
    fn display_value(value: &Utf16JsonProjection) -> serde_json::Value {
        value
            .to_json_string()
            .ok()
            .and_then(|source| serde_json::from_str(&source).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    async fn emit_tool_result_event(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        result: &Utf16JsonProjection,
    ) {
        let is_error = Self::result_is_error(result);
        let call_input = self.take_call(id).map(|(_, input)| input);
        let display_input = call_input.as_ref().map(Self::display_value);
        let display_result = Self::display_value(result);
        self.sink
            .emit(ClientEvent::ToolUseResult {
                id: id.to_string(),
                tool: tool.to_string(),
                result_json: result
                    .to_json_string()
                    .expect("tool result source is valid JSON"),
                is_error,
                display: crate::adapter::tool_display::lower_tool_result_display(
                    tool,
                    display_input.as_ref(),
                    &display_result,
                    is_error,
                ),
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
        self.finish_text_block().await;
        let blocks = {
            let mut buffer = self.message_blocks.lock().await;
            buffer.active_block_key = None;
            buffer.last_completed_row_id = None;
            buffer.pending_continuation = None;
            std::mem::take(&mut buffer.blocks)
                .into_iter()
                .map(|entry| entry.block)
                .collect::<Vec<_>>()
        };
        if blocks.is_empty() && !include_empty {
            return;
        }
        self.sink
            .emit(ClientEvent::MessageComplete {
                stop_reason: stop_reason.map(str::to_string),
                message: Some(MessageDto {
                    sent_at_ms: None,
                    loop_wakeup: None,
                    visualization_context: None,
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
    async fn emit_task_lifecycle(&self, event: &Utf16JsonProjection) {
        self.sink
            .emit(ClientEvent::TaskLifecycle {
                event_json: event
                    .to_json_string()
                    .expect("task lifecycle source is valid JSON"),
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

    async fn emit_text(&self, text: &Utf16JsonProjection) {
        let source = ClientTextDto::from_projection(text)
            .expect("OutputStream text is a root-string source projection");
        let events = {
            let mut buffer = self.message_blocks.lock().await;
            buffer
                .pending_source_units
                .extend(source.code_units.iter().copied());
            if source.code_units.is_empty() {
                if buffer.references.is_holding() {
                    Vec::new()
                } else {
                    buffer.append(MessageBlockDto::Text {
                        text: source.clone(),
                    });
                    vec![ClientEvent::TextDelta { text: source }]
                }
            } else {
                let split = buffer.references.push(&source.display());
                buffer.absorb(split)
            }
        };
        self.emit_all(events).await;
    }

    async fn emit_assistant_message_identity(&self, message_id: &lingxi_core::types::MessageId) {
        self.sink
            .emit(ClientEvent::MessageIdentity {
                message_id: message_id.as_uuid().to_string(),
            })
            .await;
    }

    async fn emit_user_transcript_row_identity(&self, row_token: &str, uuid: &str) {
        self.sink
            .emit(ClientEvent::UserTranscriptRowIdentity {
                row_token: row_token.to_string(),
                uuid: uuid.to_string(),
            })
            .await;
    }

    async fn emit_assistant_transcript_row_uuids(
        &self,
        message_id: &lingxi_core::types::MessageId,
        uuids: &[Option<String>],
    ) {
        self.sink
            .emit(ClientEvent::AssistantTranscriptRowUuids {
                message_id: message_id.as_uuid().to_string(),
                uuids: uuids.to_vec(),
            })
            .await;
    }

    async fn emit_message_retracted(&self, message_id: &lingxi_core::types::MessageId) {
        self.abort_text_block().await;
        *self.message_blocks.lock().await = MessageBuffer::default();
        self.sink
            .emit(ClientEvent::MessageRetracted {
                message_id: message_id.as_uuid().to_string(),
            })
            .await;
    }

    async fn emit_server_fallback_query_model_change(&self, to_model: &str) {
        self.sink
            .emit(ClientEvent::QueryModelChange {
                to_model: to_model.to_string(),
            })
            .await;
    }

    async fn emit_assistant_block_start(&self, block_key: u64) {
        self.finish_text_block().await;
        self.message_blocks.lock().await.active_block_key = Some(block_key);
        self.sink
            .emit(ClientEvent::AssistantBlockStart { block_key })
            .await;
    }

    async fn emit_assistant_block_identity(
        &self,
        block_key: u64,
        row_id: &lingxi_core::types::MessageId,
    ) {
        let message_uuid = row_id.as_uuid().to_string();
        self.finish_text_block().await;
        self.message_blocks
            .lock()
            .await
            .bind_identity(block_key, &message_uuid);
        self.sink
            .emit(ClientEvent::AssistantBlockIdentity {
                block_key,
                message_uuid,
            })
            .await;
    }

    async fn emit_server_fallback_tombstone(
        &self,
        message: &ServerFallbackTombstoneMessage,
        display_only: bool,
    ) {
        self.abort_text_block().await;
        let row_id = message.uuid.as_uuid().to_string();
        let discarded_tool_ids: std::collections::HashSet<String> = message
            .content
            .iter()
            .filter_map(|block| match block {
                lingxi_core::types::ContentBlock::ToolUse { id, .. } => Some(id.to_string()),
                lingxi_core::types::ContentBlock::ToolResult { tool_use_id, .. } => {
                    Some(tool_use_id.to_string())
                }
                lingxi_core::types::ContentBlock::ServerToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        {
            let mut buffer = self.message_blocks.lock().await;
            buffer.blocks.retain(|entry| {
                if entry.row_id.as_deref() == Some(row_id.as_str()) {
                    return false;
                }
                match &entry.block {
                    MessageBlockDto::ToolUse { id, .. }
                    | MessageBlockDto::ToolResult { id, .. } => !discarded_tool_ids.contains(id),
                    _ => true,
                }
            });
            if buffer.last_completed_row_id.as_deref() == Some(row_id.as_str()) {
                buffer.last_completed_row_id = None;
            }
        }
        if !discarded_tool_ids.is_empty() {
            if let Ok(mut pending) = self.pending.lock() {
                pending.retain(|(pending_id, _)| !discarded_tool_ids.contains(pending_id));
            }
        }

        let dto = ServerFallbackTombstoneMessageDto {
            uuid: row_id,
            message_type: message.message_type.clone(),
            timestamp: message.timestamp.clone(),
            request_id: message.request_id.clone(),
            request_ref_json: message.request_ref.as_ref().map(|request_ref| {
                request_ref
                    .to_json_string()
                    .expect("request reference is a valid source JSON projection")
            }),
            message: ServerFallbackProviderMessageDto {
                id: message.provider_message_id.clone(),
                model: message.model.clone(),
                stop_reason: message.stop_reason.clone(),
                stop_details_json: message.stop_details.as_ref().map(|details| {
                    details
                        .to_json_string()
                        .expect("stop details are a valid source JSON projection")
                }),
                usage_json: message.usage.as_ref().map(|usage| {
                    usage
                        .to_json_string()
                        .expect("usage is a valid source JSON projection")
                }),
                content_json: serde_json::to_string(&message.content)
                    .expect("core content blocks serialize to JSON"),
            },
            is_api_error_message: message.is_api_error_message,
            supersedes_uuids: message.supersedes_uuids.as_ref().map(|uuids| {
                uuids
                    .iter()
                    .map(|uuid| uuid.as_uuid().to_string())
                    .collect()
            }),
        };
        self.sink
            .emit(ClientEvent::Tombstone {
                message: dto,
                display_only,
            })
            .await;
    }

    async fn emit_refusal_continuation_begin(
        &self,
        salvage_text: &str,
        replaces_uuids: &[lingxi_core::types::MessageId],
        display_salvage_text: bool,
    ) {
        self.abort_text_block().await;
        self.message_blocks.lock().await.pending_continuation = Some(PendingContinuation {
            salvage_text: ClientTextDto::from_utf8(salvage_text),
            display_salvage_text,
            replaces_uuids: replaces_uuids
                .iter()
                .map(|uuid| uuid.as_uuid().to_string())
                .collect(),
        });
        self.sink
            .emit(ClientEvent::RefusalContinuation {
                phase: RefusalContinuationPhaseDto::Begin,
                salvage_text: salvage_text.to_string(),
                join: RefusalContinuationJoinDto::Exact,
                replaces_uuids: replaces_uuids
                    .iter()
                    .map(|uuid| uuid.as_uuid().to_string())
                    .collect(),
                display_salvage_text,
            })
            .await;
    }

    async fn emit_system_notice(&self, message: &Utf16JsonProjection, is_error: bool) {
        self.sink
            .emit(ClientEvent::SystemNotice {
                message: ClientTextDto::from_projection(message)
                    .expect("system notice is a root string projection"),
                is_error,
            })
            .await;
    }

    async fn emit_mod_log(&self, plugin: &str, text: &Utf16JsonProjection) {
        self.sink
            .emit(ClientEvent::UiLog {
                plugin: plugin.to_string(),
                text: ClientTextDto::from_projection(text)
                    .expect("Mod log text is a root-string source projection"),
            })
            .await;
    }

    async fn emit_mod_toast(&self, plugin: &str, text: &Utf16JsonProjection, timeout_ms: u64) {
        self.sink
            .emit(ClientEvent::UiToast {
                plugin: plugin.to_string(),
                text: ClientTextDto::from_projection(text)
                    .expect("Mod toast text is a root-string source projection"),
                timeout_ms,
            })
            .await;
    }

    async fn emit_mod_notify(&self, plugin: &str, text: &Utf16JsonProjection) {
        self.sink
            .emit(ClientEvent::UiNotify {
                plugin: plugin.to_string(),
                text: ClientTextDto::from_projection(text)
                    .expect("Mod notify text is a root-string source projection"),
            })
            .await;
    }

    async fn emit_mod_status(&self, plugin: &str, text: Option<&Utf16JsonProjection>) {
        self.sink
            .emit(ClientEvent::UiStatus {
                plugin: plugin.to_string(),
                text: text.map(|text| {
                    ClientTextDto::from_projection(text)
                        .expect("Mod status text is a root-string source projection")
                }),
            })
            .await;
    }

    async fn emit_mod_ui_client_frame(&self, runtime_id: &str, frame_json: &str) {
        self.sink
            .emit(ClientEvent::UiClientFrame {
                runtime_id: runtime_id.to_string(),
                frame_json: frame_json.to_string(),
            })
            .await;
    }

    async fn emit_mod_ui_invalidate(
        &self,
        instances_json: Option<&str>,
        uuid: &str,
        session_id: &str,
    ) {
        self.sink
            .emit(ClientEvent::UiInvalidate {
                instances_json: instances_json.map(str::to_string),
                uuid: uuid.to_string(),
                session_id: session_id.to_string(),
            })
            .await;
    }

    async fn emit_tool_call(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        input: &Utf16JsonProjection,
    ) {
        self.finish_text_block().await;
        self.remember_call(id, tool, input);
        let display_input = Self::display_value(input);
        let input_json = input
            .to_json_string()
            .expect("tool input source is valid JSON");
        self.message_blocks
            .lock()
            .await
            .append(MessageBlockDto::ToolUse {
                id: id.to_string(),
                tool: tool.to_string(),
                input_json: input_json.clone(),
                header: crate::adapter::tool_display::lower_tool_header(tool, &display_input),
            });
        self.sink
            .emit(ClientEvent::ToolUseStarted {
                id: id.to_string(),
                tool: tool.to_string(),
                input_json,
                header: crate::adapter::tool_display::lower_tool_header(tool, &display_input),
            })
            .await;
        // TodoWrite rewrites the whole plan. Emitted on the CALL, not the
        // result, matching how the terminal updates its pinned block.
        if let Some(tasks) = crate::adapter::tool_display::plan_from_tool_call(tool, &display_input)
        {
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
        _model_text: &Utf16JsonProjection,
        result: &Utf16JsonProjection,
        _mcp_meta: Option<&Utf16JsonProjection>,
    ) {
        // MCP metadata belongs to the native stream-json envelope. This
        // adapter emits the separate, frozen client UI event shape.
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
        _model_text: &Utf16JsonProjection,
        result: &Utf16JsonProjection,
        denial_kind: &str,
        _projection: Option<&lingxi_core::host::ToolResultProjection>,
    ) {
        // Keep the frozen `ClientEvent` shape while carrying the engine's
        // structured interruption provenance inside the already-extensible JSON
        // payload. Existing clients ignore the additive key; newer clients can
        // distinguish cancellation from a real tool failure without matching a
        // localized error string.
        let mut tagged = result.clone();
        if tagged.object_entries().is_some() {
            tagged
                .set_field("tool_denial_kind", denial_kind.into())
                .expect("tool result root is an object");
        }
        self.emit_tool_result_event(id, tool, &tagged).await;
    }

    fn wants_prompt_suggestions(&self) -> bool {
        true
    }

    async fn emit_prompt_suggestion_state(&self, enabled: bool, notice: Option<&str>) {
        self.sink
            .emit(ClientEvent::PromptSuggestionState {
                enabled,
                notice: notice.map(str::to_owned),
            })
            .await;
    }

    async fn emit_prompt_suggestion(
        &self,
        session_id: &str,
        uuid: &str,
        suggestion: &Utf16JsonProjection,
    ) {
        let suggestion = ClientTextDto::from_projection(suggestion)
            .expect("prompt suggestion must be a root-string projection");
        self.sink
            .emit(ClientEvent::PromptSuggestion {
                suggestion,
                session_id: session_id.into(),
                uuid: uuid.into(),
            })
            .await;
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
        summary: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) {
        self.sink
            .emit(ClientEvent::CompactionCompleted {
                messages_before,
                messages_after,
                bytes_saved,
                summary: ClientTextDto::from_projection(summary)
                    .expect("compaction summary is text"),
            })
            .await;
    }

    /// §0.7 "light up thinking/usage": lower each live reasoning delta into a
    /// [`ClientEvent::ThinkingDelta`]. `signature` is `None` for live deltas
    /// (the cryptographic signature only arrives on the completed thinking
    /// block, not per-delta) — see `lingxi_core::host::OutputStream::emit_thinking`.
    async fn emit_thinking(&self, thinking: &str, signature: Option<&str>) {
        self.finish_text_block().await;
        self.message_blocks
            .lock()
            .await
            .append(MessageBlockDto::Thinking {
                thinking: thinking.to_string(),
                signature: signature.map(str::to_string),
            });
        self.sink
            .emit(ClientEvent::ThinkingDelta {
                thinking: thinking.to_string(),
                signature: signature.map(str::to_string),
            })
            .await;
    }

    async fn emit_redacted_thinking(&self, data: &str) {
        self.finish_text_block().await;
        self.message_blocks
            .lock()
            .await
            .append(MessageBlockDto::RedactedThinking {
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

    async fn emit_terminal_api_error(
        &self,
        api_error_status: Option<u16>,
        terminal_reason: &str,
        stop_reason: &str,
    ) {
        self.sink
            .emit(ClientEvent::TerminalApiError {
                api_error_status,
                terminal_reason: terminal_reason.to_owned(),
                stop_reason: stop_reason.to_owned(),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adapter::test_support::MockSink;

    fn projection(text: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(text.to_string(), text.encode_utf16().collect())
            .expect("scalar text is a valid root string")
    }

    fn json_projection(value: &serde_json::Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("generated JSON is a valid projection")
    }

    /// `emit_text` → exactly one `TextDelta` carrying the payload verbatim.
    #[tokio::test]
    async fn task_lifecycle_reaches_client_event_sink() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let event = Utf16JsonProjection::parse(
            r#"{"type":"system","subtype":"task_updated","task_id":"b12345678","patch":{"status":"completed","description":"\ud800"}}"#,
        )
        .unwrap();
        stream.emit_task_lifecycle(&event).await;
        assert_eq!(
            sink.events().await,
            vec![ClientEvent::TaskLifecycle {
                event_json: event.to_json_string().unwrap()
            }]
        );
    }

    #[tokio::test]
    async fn terminal_api_error_forwards_all_terminal_facts_without_text_inference() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream
            .emit_terminal_api_error(None, "api_error", "stop_sequence")
            .await;
        stream
            .emit_terminal_api_error(Some(413), "image_error", "stop_sequence")
            .await;
        stream
            .emit_terminal_api_error(None, "api_error", "refusal")
            .await;

        assert_eq!(
            sink.events().await,
            vec![
                ClientEvent::TerminalApiError {
                    api_error_status: None,
                    terminal_reason: "api_error".into(),
                    stop_reason: "stop_sequence".into(),
                },
                ClientEvent::TerminalApiError {
                    api_error_status: Some(413),
                    terminal_reason: "image_error".into(),
                    stop_reason: "stop_sequence".into(),
                },
                ClientEvent::TerminalApiError {
                    api_error_status: None,
                    terminal_reason: "api_error".into(),
                    stop_reason: "refusal".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn transcript_row_identity_events_preserve_stop_time_jsonl_uuids() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let message_id = lingxi_core::types::MessageId::new();
        let uuids = vec![Some("persisted-text-row".into()), None];

        stream
            .emit_user_transcript_row_identity("pending-user-row", "persisted-user-row")
            .await;
        stream
            .emit_assistant_transcript_row_uuids(&message_id, &uuids)
            .await;

        assert_eq!(
            sink.events().await,
            vec![
                ClientEvent::UserTranscriptRowIdentity {
                    row_token: "pending-user-row".into(),
                    uuid: "persisted-user-row".into(),
                },
                ClientEvent::AssistantTranscriptRowUuids {
                    message_id: message_id.as_uuid().to_string(),
                    uuids,
                },
            ]
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

        stream.emit_text(&projection("hello world")).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::TextDelta {
                text: "hello world".into()
            }
        );
    }

    #[tokio::test]
    async fn emit_text_preserves_lone_surrogate_in_live_and_completed_dtos() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let units = vec![b'a' as u16, 0xd800, b'b' as u16];
        let source =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone())
                .expect("root string display matches exact units");

        stream.emit_text(&source).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;

        let events = sink.events().await;
        let ClientEvent::TextDelta { text } = &events[0] else {
            panic!("expected live text delta")
        };
        assert_eq!(text.code_units, units);
        let ClientEvent::MessageComplete {
            message: Some(message),
            ..
        } = events.last().expect("completed message")
        else {
            panic!("expected completed message")
        };
        let MessageBlockDto::Text { text } = &message.blocks[0] else {
            panic!("expected completed text block")
        };
        assert_eq!(text.code_units, units);
    }

    #[tokio::test]
    async fn retry_retraction_carries_identity_and_clears_partial_blocks() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let id = lingxi_core::types::MessageId::new();
        stream.emit_text(&projection("rejected")).await;
        stream.emit_assistant_message_identity(&id).await;
        stream.emit_message_retracted(&id).await;
        stream.emit_text(&projection("clean")).await;
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

    #[tokio::test]
    async fn server_fallback_forwards_model_change_and_complete_tombstone_row() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let old_id = lingxi_core::types::MessageId::new();
        let new_id = lingxi_core::types::MessageId::new();
        let discarded_tool = lingxi_core::types::ToolUseId::from("toolu_discarded");
        let discarded_input = json_projection(&serde_json::json!({"file_path": "/tmp/discarded"}));

        stream
            .emit_server_fallback_query_model_change("claude-sonnet-4")
            .await;
        stream.emit_assistant_block_start(41).await;
        stream.emit_text(&projection("old assistant output")).await;
        stream.emit_assistant_block_identity(41, &old_id).await;
        stream
            .emit_tool_call(&discarded_tool, "Read", &discarded_input)
            .await;
        stream
            .emit_server_fallback_tombstone(
                &ServerFallbackTombstoneMessage {
                    uuid: old_id,
                    message_type: "assistant".into(),
                    timestamp: "2026-10-03T12:00:00.000Z".into(),
                    request_id: Some("request-1".into()),
                    request_ref: Some(json_projection(&serde_json::json!({"lane": "main"}))),
                    provider_message_id: Some("provider-message-1".into()),
                    model: Some("claude-opus-4".into()),
                    stop_reason: Some("tool_use".into()),
                    stop_details: Some(json_projection(&serde_json::json!({"category": "cyber"}))),
                    usage: Some(json_projection(
                        &serde_json::json!({"input_tokens": 11, "output_tokens": 3}),
                    )),
                    content: vec![
                        lingxi_core::types::ContentBlock::Text {
                            text: "old assistant output".into(),
                            citations: None,
                        },
                        lingxi_core::types::ContentBlock::ToolUse {
                            id: discarded_tool.clone(),
                            name: "Read".into(),
                            input: discarded_input.clone(),
                            provider_id: Some("toolu_discarded".into()),
                        },
                    ],
                    is_api_error_message: Some(false),
                    supersedes_uuids: None,
                },
                true,
            )
            .await;
        stream.emit_assistant_block_start(42).await;
        stream.emit_text(&projection("new response")).await;
        stream.emit_assistant_block_identity(42, &new_id).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;

        let events = sink.events().await;
        assert_eq!(
            events[0],
            ClientEvent::QueryModelChange {
                to_model: "claude-sonnet-4".into(),
            }
        );
        assert_eq!(
            events[1],
            ClientEvent::AssistantBlockStart { block_key: 41 }
        );
        assert!(matches!(
            &events[3],
            ClientEvent::AssistantBlockIdentity { block_key: 41, message_uuid }
                if message_uuid == &old_id.as_uuid().to_string()
        ));
        let ClientEvent::Tombstone {
            message,
            display_only,
        } = &events[5]
        else {
            panic!("missing row tombstone")
        };
        assert!(*display_only);
        assert_eq!(message.uuid, old_id.as_uuid().to_string());
        assert_eq!(message.message_type, "assistant");
        assert_eq!(message.timestamp, "2026-10-03T12:00:00.000Z");
        assert_eq!(message.request_id.as_deref(), Some("request-1"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(message.request_ref_json.as_deref().unwrap())
                .unwrap(),
            serde_json::json!({"lane": "main"})
        );
        assert_eq!(message.message.id.as_deref(), Some("provider-message-1"));
        assert_eq!(message.message.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                message.message.stop_details_json.as_deref().unwrap()
            )
            .unwrap(),
            serde_json::json!({"category": "cyber"})
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                message.message.usage_json.as_deref().unwrap()
            )
            .unwrap(),
            serde_json::json!({"input_tokens": 11, "output_tokens": 3})
        );
        let content: serde_json::Value =
            serde_json::from_str(&message.message.content_json).expect("content array JSON");
        assert_eq!(content.as_array().map(Vec::len), Some(2));
        assert_eq!(
            content[0],
            serde_json::json!({"type": "text", "text": "old assistant output"})
        );
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(
            content[1]["input"],
            serde_json::json!({"file_path": "/tmp/discarded"})
        );
        assert_eq!(content[1]["provider_id"], "toolu_discarded");
        assert_eq!(message.message.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(message.is_api_error_message, Some(false));
        assert_eq!(message.supersedes_uuids, None);
        let ClientEvent::MessageComplete {
            message: Some(message),
            ..
        } = events.last().expect("message boundary event")
        else {
            panic!("missing completed message")
        };
        assert_eq!(
            message.blocks,
            vec![MessageBlockDto::Text {
                text: "new response".into()
            }]
        );
    }

    #[tokio::test]
    async fn refusal_continuation_begin_seeds_exact_prefix_on_stop_time_row() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let first_replaced = lingxi_core::types::MessageId::new();
        let second_replaced = lingxi_core::types::MessageId::new();
        let replaced_ids = vec![first_replaced, second_replaced];
        let incoming_id = lingxi_core::types::MessageId::new();

        stream.emit_assistant_block_start(7).await;
        stream.emit_text(&projection("old first row")).await;
        stream
            .emit_assistant_block_identity(7, &first_replaced)
            .await;
        stream.emit_assistant_block_start(8).await;
        stream.emit_text(&projection("old second row")).await;
        stream
            .emit_assistant_block_identity(8, &second_replaced)
            .await;
        stream
            .emit_refusal_continuation_begin("retained🙂", &replaced_ids, true)
            .await;
        stream.emit_assistant_block_start(9).await;
        stream.emit_text(&projection("fresh text")).await;
        stream.emit_assistant_block_identity(9, &incoming_id).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;

        let events = sink.events().await;
        let begin_index = events
            .iter()
            .position(|event| matches!(event, ClientEvent::RefusalContinuation { .. }))
            .expect("continuation begin event");
        assert_eq!(
            events[begin_index],
            ClientEvent::RefusalContinuation {
                phase: RefusalContinuationPhaseDto::Begin,
                salvage_text: "retained🙂".into(),
                join: RefusalContinuationJoinDto::Exact,
                replaces_uuids: replaced_ids
                    .iter()
                    .map(|uuid| uuid.as_uuid().to_string())
                    .collect(),
                display_salvage_text: true,
            }
        );
        assert!(matches!(
            &events[begin_index + 2],
            ClientEvent::TextDelta { text } if text.display() == "fresh text"
        ));
        assert!(matches!(
            &events[begin_index + 3],
            ClientEvent::AssistantBlockIdentity { block_key: 9, message_uuid }
                if message_uuid == &incoming_id.as_uuid().to_string()
        ));
        let ClientEvent::MessageComplete {
            message: Some(message),
            ..
        } = events.last().expect("message boundary event")
        else {
            panic!("missing completed message")
        };
        assert_eq!(
            message.blocks,
            vec![MessageBlockDto::Text {
                text: "retained🙂fresh text".into()
            }]
        );
    }

    #[tokio::test]
    async fn refusal_continuation_hook_and_ineligible_text_do_not_get_seeded() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let row_id = lingxi_core::types::MessageId::new();

        stream
            .emit_refusal_continuation_begin("native retained", &[], false)
            .await;
        stream.emit_assistant_block_start(1).await;
        stream.emit_assistant_block_identity(1, &row_id).await;
        stream.emit_text(&projection("hook override")).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;

        let events = sink.events().await;
        let ClientEvent::MessageComplete {
            message: Some(message),
            ..
        } = events.last().expect("message boundary event")
        else {
            panic!("missing completed hook message")
        };
        assert_eq!(
            message.blocks,
            vec![MessageBlockDto::Text {
                text: "hook override".into()
            }]
        );

        let second_sink = MockSink::arc();
        let second = AdapterOutputStream::new(second_sink.clone());
        let whitespace_row = lingxi_core::types::MessageId::new();
        second
            .emit_refusal_continuation_begin("must remain pending", &[], true)
            .await;
        second.emit_assistant_block_start(2).await;
        second.emit_text(&projection(" \u{feff} \n")).await;
        second
            .emit_assistant_block_identity(2, &whitespace_row)
            .await;
        second.emit_message_boundary(Some("end_turn"), None).await;
        let events = second_sink.events().await;
        let ClientEvent::MessageComplete {
            message: Some(message),
            ..
        } = events.last().expect("whitespace message boundary")
        else {
            panic!("missing whitespace message")
        };
        assert_eq!(
            message.blocks,
            vec![MessageBlockDto::Text {
                text: " \u{feff} \n".into()
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
            .emit_system_notice(&"Conversation changes could not be saved.".into(), true)
            .await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::SystemNotice {
                message: "Conversation changes could not be saved.".into(),
                is_error: true,
            }]
        );
    }

    #[tokio::test]
    async fn emit_mod_log_keeps_plugin_and_plain_text_separate() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_mod_log("review", &projection("Found a mismatch"))
            .await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::UiLog {
                plugin: "review".to_string(),
                text: "Found a mismatch".into(),
            }]
        );
    }

    #[tokio::test]
    async fn emit_mod_log_preserves_unpaired_surrogate_on_client_wire() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let source = Utf16JsonProjection::parse(r#""log-\ud800""#).unwrap();

        stream.emit_mod_log("native", &source).await;

        let events = sink.events().await;
        let ClientEvent::UiLog { text, .. } = &events[0] else {
            panic!("expected UI log event")
        };
        assert_eq!(
            text.code_units,
            vec![b'l' as u16, b'o' as u16, b'g' as u16, b'-' as u16, 0xD800]
        );
        assert_eq!(
            serde_json::to_string(&events[0]).unwrap(),
            r#"{"type":"ui_log","plugin":"native","text":"log-\ud800"}"#
        );
    }

    #[tokio::test]
    async fn emit_mod_toast_preserves_plugin_text_and_timeout() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_mod_toast("review", &projection("Done"), 4000)
            .await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::UiToast {
                plugin: "review".to_string(),
                text: "Done".into(),
                timeout_ms: 4000,
            }]
        );
    }

    #[tokio::test]
    async fn emit_mod_notify_preserves_lone_surrogate_on_client_wire() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let source = Utf16JsonProjection::parse(r#""notice-\ud800""#).unwrap();

        stream.emit_mod_notify("review", &source).await;

        let events = sink.events().await;
        let ClientEvent::UiNotify { plugin, text } = &events[0] else {
            panic!("expected UI notify event")
        };
        assert_eq!(plugin, "review");
        assert_eq!(
            text.code_units,
            vec![110, 111, 116, 105, 99, 101, 45, 0xD800]
        );
        assert_eq!(
            serde_json::to_string(&events[0]).unwrap(),
            r#"{"type":"ui_notify","plugin":"review","text":"notice-\ud800"}"#
        );
    }

    #[tokio::test]
    async fn emit_mod_status_uses_nullable_clear() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_mod_status("review", Some(&projection("Working")))
            .await;
        stream.emit_mod_status("review", None).await;

        assert_eq!(
            sink.events().await,
            vec![
                ClientEvent::UiStatus {
                    plugin: "review".to_string(),
                    text: Some("Working".into()),
                },
                ClientEvent::UiStatus {
                    plugin: "review".to_string(),
                    text: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn emit_mod_ui_client_frame_forwards_runtime_and_frame_json() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_mod_ui_client_frame(
                "runtime-1",
                r#"{"type":"ui.render","tree":{"type":"text"}}"#,
            )
            .await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::UiClientFrame {
                runtime_id: "runtime-1".into(),
                frame_json: r#"{"type":"ui.render","tree":{"type":"text"}}"#.into(),
            }]
        );
    }

    #[tokio::test]
    async fn emit_mod_ui_invalidate_preserves_targeted_instances_and_identity() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        stream
            .emit_mod_ui_invalidate(
                Some(r#"[{"surface":"desktop","component":"Status","instance_id":"one"}]"#),
                "event-1",
                "session-1",
            )
            .await;

        assert_eq!(
            sink.events().await,
            vec![ClientEvent::UiInvalidate {
                instances_json: Some(
                    r#"[{"surface":"desktop","component":"Status","instance_id":"one"}]"#.into(),
                ),
                uuid: "event-1".into(),
                session_id: "session-1".into(),
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
        let input_source = json_projection(&input);
        stream.emit_tool_call(&id, "Read", &input_source).await;

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

    #[tokio::test]
    async fn emit_tool_call_preserves_exact_json_source_for_wire_payload() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let id = lingxi_core::types::ToolUseId::new();
        let input = Utf16JsonProjection::parse(r#"{"path":"\ud800"}"#).unwrap();
        let expected = input.to_json_string().unwrap();

        stream.emit_tool_call(&id, "Read", &input).await;

        let events = sink.events().await;
        let ClientEvent::ToolUseStarted { input_json, .. } = &events[0] else {
            panic!("expected tool-use event")
        };
        assert_eq!(input_json, &expected);
    }

    /// A success tool result (no top-level `"error"` key) lowers to
    /// `ToolUseResult { is_error: false }`.
    #[tokio::test]
    async fn emit_tool_result_success_is_not_error() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());

        let id = lingxi_core::types::ToolUseId::new();
        let result = serde_json::json!({"content": "ok", "lines": 3});
        let result_source = json_projection(&result);
        stream
            .emit_tool_result(&id, "Read", &projection("ok"), &result_source, None)
            .await;

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
        let result_source = json_projection(&result);
        stream
            .emit_tool_result(
                &id,
                "Read",
                &projection("file not found"),
                &result_source,
                None,
            )
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
        let result_source = json_projection(&result);
        stream
            .emit_tool_result_denied(
                &id,
                "Bash",
                &projection("interrupted"),
                &result_source,
                "interrupted",
                None,
            )
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
            .emit_compaction_completed(42, 8, 1_024, &"Summary:\nkept context".into())
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            ClientEvent::CompactionCompleted {
                messages_before: 42,
                messages_after: 8,
                bytes_saved: 1_024,
                summary: "Summary:\nkept context".into(),
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

        stream.emit_text(&projection("via trait object")).await;

        let events = sink.events().await;
        assert_eq!(
            events[0],
            ClientEvent::TextDelta {
                text: "via trait object".into()
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

        stream.emit_text(&projection("A")).await;
        stream.emit_thinking("reason", Some("sig")).await;
        let input = json_projection(&serde_json::json!({"path": "a"}));
        stream.emit_tool_call(&id, "Read", &input).await;
        stream.emit_text(&projection("B")).await;
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
                    matches!(&message.blocks[0], MessageBlockDto::Text { text } if text.display() == "A")
                );
                assert!(
                    matches!(&message.blocks[1], MessageBlockDto::Thinking { thinking, .. } if thinking == "reason")
                );
                assert!(
                    matches!(&message.blocks[2], MessageBlockDto::ToolUse { id: got, .. } if got == &id.to_string())
                );
                assert!(
                    matches!(&message.blocks[3], MessageBlockDto::Text { text } if text.display() == "B")
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
        let input_source = json_projection(&input);
        let result_source = json_projection(&result);
        // What the transcript actually persists for that same call: the
        // model-facing string (`ToolCallResult.model_content`).
        let content = "The file /tmp/x.rs has been updated.";

        // ── live ──────────────────────────────────────────────────────────
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream.emit_tool_call(&id, tool, &input_source).await;
        stream
            .emit_tool_result(&id, tool, &projection(""), &result_source, None)
            .await;
        let live = sink
            .events()
            .await
            .into_iter()
            .find_map(|e| match e {
                ClientEvent::ToolUseResult { display, .. } => Some(display),
                _ => None,
            })
            .expect("a live display");

        // ── resumed ───────────────────────────────────────────────────────
        let history = vec![
            ConversationMessage::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: tool.to_string(),
                    input: input_source.clone(),
                    provider_id: None,
                }],
                stop_reason: Some("tool_use".to_string()),
                resumable: None,
            },
            ConversationMessage::User {
                api_message_override: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: content.to_string().into(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
            },
        ];
        let resumed = crate::adapter::lowering::lower_transcript(&history)
            .into_iter()
            .flat_map(|m| m.blocks)
            .find_map(|b| match b {
                MessageBlockDto::ToolResult { display, .. } => Some(display),
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
            let input = json_projection(&input);
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

#[cfg(test)]
mod visualization_stream_tests {
    use super::*;
    use crate::adapter::test_support::MockSink;
    use crate::protocol::message::{VisualizationBlockStatusDto, VisualizationRefDto};
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

    const LINE: &str = "::lingxi-visualization{id=\"chart\" rev=\"2\"}";

    fn projection(text: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(text.to_string(), text.encode_utf16().collect())
            .expect("scalar text is a valid root string")
    }

    fn exact_projection(code_units: Vec<u16>) -> Utf16JsonProjection {
        let display = String::from_utf16_lossy(&code_units);
        Utf16JsonProjection::root_string(display, code_units)
            .expect("display is derived from the exact UTF-16 units")
    }

    fn chart() -> VisualizationRefDto {
        VisualizationRefDto {
            id: "chart".into(),
            revision: 2,
        }
    }

    fn completed_blocks(events: &[ClientEvent]) -> Vec<MessageBlockDto> {
        events
            .iter()
            .find_map(|event| match event {
                ClientEvent::MessageComplete {
                    message: Some(message),
                    ..
                } => Some(message.blocks.clone()),
                _ => None,
            })
            .expect("message complete")
    }

    fn replayed(text: &str) -> Vec<MessageBlockDto> {
        crate::adapter::lowering::lower_conversation_message(&ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            resumable: None,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
        })
        .blocks
    }

    fn replayed_exact(code_units: Vec<u16>) -> Vec<MessageBlockDto> {
        let display = String::from_utf16_lossy(&code_units);
        crate::adapter::lowering::lower_conversation_message(&ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            resumable: None,
            content: vec![ContentBlock::TextJsUtf16 {
                text: display,
                utf16_code_units: code_units,
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
        })
        .blocks
    }

    #[tokio::test]
    async fn reference_lines_stream_as_slots_and_complete_like_replay() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let text = format!("Here it is:\n{LINE}\nDrag the slider.");
        for chunk in [
            "Here it is:\n::lingxi-vis",
            "ualization{id=\"chart\" ",
            "rev=\"2\"}\nDrag the slider.",
        ] {
            stream.emit_text(&projection(chunk)).await;
        }
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let events = sink.events().await;
        let live: Vec<_> = events
            .iter()
            .filter(|event| !matches!(event, ClientEvent::MessageComplete { .. }))
            .cloned()
            .collect();
        assert_eq!(
            live,
            vec![
                ClientEvent::TextDelta {
                    text: "Here it is:\n".into()
                },
                ClientEvent::VisualizationBlock {
                    status: VisualizationBlockStatusDto::Pending,
                    reference: None
                },
                ClientEvent::VisualizationBlock {
                    status: VisualizationBlockStatusDto::Ready,
                    reference: Some(chart())
                },
                ClientEvent::TextDelta {
                    text: "Drag the slider.".into()
                },
            ]
        );
        let blocks = completed_blocks(&events);
        assert_eq!(
            blocks,
            vec![
                MessageBlockDto::Text {
                    text: "Here it is:\n".into()
                },
                MessageBlockDto::Visualization {
                    reference: Some(chart())
                },
                MessageBlockDto::Text {
                    text: "Drag the slider.".into()
                },
            ]
        );
        assert_eq!(
            blocks,
            replayed(&text),
            "live and resumed block sequences agree"
        );
    }

    #[tokio::test]
    async fn a_reference_only_message_keeps_its_slot_and_settles_at_the_boundary() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream.emit_text(&projection(LINE)).await;
        let before = sink.events().await;
        assert_eq!(
            before,
            vec![ClientEvent::VisualizationBlock {
                status: VisualizationBlockStatusDto::Pending,
                reference: None
            }],
            "an unterminated line is held, not shown as text"
        );
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let blocks = completed_blocks(&sink.events().await);
        assert_eq!(
            blocks,
            vec![MessageBlockDto::Visualization {
                reference: Some(chart())
            }]
        );
        assert_eq!(blocks, replayed(LINE));
    }

    #[tokio::test]
    async fn standalone_empty_delta_remains_visible_to_native_clients() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream.emit_text(&projection("")).await;
        assert_eq!(
            sink.events().await,
            vec![ClientEvent::TextDelta {
                text: ClientTextDto::default(),
            }]
        );
    }

    #[tokio::test]
    async fn empty_delta_while_holding_split_reference_preserves_exact_live_replay_blocks() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let prefix = vec![u16::from(b'A'), 0xd800, u16::from(b'\n')];
        let mut first = prefix.clone();
        first.extend("::lingxi-vis".encode_utf16());
        stream.emit_text(&exact_projection(first)).await;

        // The reference is still a possible directive, so this empty native
        // delta must not create a text block ahead of the eventual slot.
        stream.emit_text(&projection("")).await;
        let remainder = "ualization{id=\"chart\" rev=\"2\"}"
            .encode_utf16()
            .collect::<Vec<_>>();
        stream.emit_text(&exact_projection(remainder)).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;

        let events = sink.events().await;
        let live: Vec<_> = events
            .iter()
            .filter(|event| !matches!(event, ClientEvent::MessageComplete { .. }))
            .cloned()
            .collect();
        assert_eq!(
            live,
            vec![
                ClientEvent::TextDelta {
                    text: ClientTextDto::from_code_units(prefix.clone()),
                },
                ClientEvent::VisualizationBlock {
                    status: VisualizationBlockStatusDto::Pending,
                    reference: None,
                },
                ClientEvent::VisualizationBlock {
                    status: VisualizationBlockStatusDto::Ready,
                    reference: Some(chart()),
                },
            ]
        );

        let mut complete_source = prefix.clone();
        complete_source.extend(LINE.encode_utf16());
        assert_eq!(completed_blocks(&events), replayed_exact(complete_source));
        assert_eq!(
            completed_blocks(&events),
            vec![
                MessageBlockDto::Text {
                    text: ClientTextDto::from_code_units(prefix),
                },
                MessageBlockDto::Visualization {
                    reference: Some(chart()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_tool_call_ends_the_text_block_and_settles_the_held_line() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream
            .emit_text(&projection("::lingxi-visualization{id=\"chart\""))
            .await;
        let input = Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path": "/x"}))
            .expect("tool input is a valid JSON projection");
        stream
            .emit_tool_call(&lingxi_core::types::ToolUseId::new(), "Read", &input)
            .await;
        let events = sink.events().await;
        assert!(matches!(
            events[1],
            ClientEvent::VisualizationBlock {
                status: VisualizationBlockStatusDto::Discarded,
                ..
            }
        ));
        assert!(matches!(events[2], ClientEvent::ToolUseStarted { .. }));
    }

    #[tokio::test]
    async fn retraction_drops_the_held_line() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        stream
            .emit_text(&projection("::lingxi-visualization{"))
            .await;
        stream.emit_message_retracted(&MessageId::new()).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let events = sink.events().await;
        assert!(events.iter().any(|event| matches!(
            event,
            ClientEvent::VisualizationBlock {
                status: VisualizationBlockStatusDto::Discarded,
                ..
            }
        )));
        assert!(completed_blocks(&events).is_empty());
    }

    #[tokio::test]
    async fn code_fences_and_ordinary_text_pass_through_unchanged() {
        let sink = MockSink::arc();
        let stream = AdapterOutputStream::new(sink.clone());
        let text = format!("```\n{LINE}\n```\nplain");
        stream.emit_text(&projection(&text)).await;
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let blocks = completed_blocks(&sink.events().await);
        assert_eq!(
            blocks,
            vec![MessageBlockDto::Text {
                text: text.clone().into(),
            }]
        );
        assert_eq!(blocks, replayed(&text));
    }
}
