//! Live-turn wrapper — `MessageComplete` synthesis + `OrchestratorError` →
//! `Error` mapping (plan F1-13).
//!
//! Two engine→DTO boundaries the streaming `OutputStream` (F1-12) cannot cover
//! on its own live here:
//!
//! 1. **Message-boundary synthesis.** The engine emits no message-boundary
//!    event — the assistant message is assembled from the
//!    [`PumpedTurn`](orchestrator::streaming_loop::PumpedTurn)
//!    (`streaming_loop.rs:40-50`) AFTER the stream ends. The adapter therefore
//!    SYNTHESIZES [`ClientEvent::MessageComplete`] from
//!    `PumpedTurn { assistant_blocks, stop_reason }`, reproducing the block set
//!    as a [`MessageDto`].
//! 2. **Turn-start synthesis.** The engine emits no turn-start event either; the
//!    adapter synthesizes [`ClientEvent::TurnStarted`] on `SendPrompt` receipt.
//! 3. **Turn-`Result` translation.** `run_turn`'s `Err(OrchestratorError)` is
//!    lowered to a single [`ClientEvent::Error`] carrying a coarse
//!    [`ErrorKindDto`] for client-side branching.
//!
//! ## `OrchestratorError` → `ErrorKindDto` mapping (plan F1-13)
//!
//! | `OrchestratorError` variant      | `ErrorKindDto` | rationale |
//! |----------------------------------|----------------|-----------|
//! | `Streaming(ApiError)`            | `Transport`    | mid-stream transport failure |
//! | `ApiCall(ApiError)`              | `Transport`    | batched transport failure (same class) |
//! | `RateLimitRejected`              | `Transport`    | terminal-429 limits copy (same class as the `ApiCall(RateLimited)` it replaces) |
//! | `StreamingProtocol(reason)` — server-emitted | `Server` | the mid-stream server-error event (`streaming_loop.rs:93`) |
//! | `StreamingProtocol(reason)` — other          | `Protocol` | wire-level per-block protocol violation |
//! | `StreamEndedWithoutStop`         | `Internal`     | stream cut before `message_stop` |
//! | `MaxTurnsReached`                | `MaxTurns`     | the `max_turns` budget was reached |
//! | `Internal`                       | `Internal`     | orchestrator invariant violation |
//! | `Compaction` / `CompactionCancelled` | `Internal` | compaction-layer failure |
//!
//! `StreamingProtocol` is split on the `streaming_loop.rs:93` prefix
//! (`"server-emitted error event: "`): the streaming loop folds a mid-stream
//! `error` event into a `StreamingProtocol` carrying that exact prefix, so the
//! adapter recovers the `Server` class structurally rather than inventing a new
//! engine signal (mirrors the `is_error` derivation in `output_stream.rs`).
//!
//! `OrchestratorError` is exhaustively matched (it is NOT `#[non_exhaustive]`),
//! so a future engine error variant surfaces here as a compile error — a
//! deliberate gate forcing an explicit `ErrorKindDto` decision rather than a
//! silent fall-through.

use std::sync::Arc;

use client_protocol::events::{ClientEvent, ErrorKindDto};
use client_protocol::message::{MessageBlockDto, MessageDto};
use orchestrator::streaming_loop::PumpedTurn;
use orchestrator::OrchestratorError;
use protocol::ContentBlock;

use crate::lowering::value_to_json_string;
use crate::sink::ClientEventSink;

/// The `streaming_loop.rs:93` prefix the mid-stream server-error event is folded
/// into a [`OrchestratorError::StreamingProtocol`] with. Used to recover the
/// `Server` error class structurally.
const SERVER_ERROR_PREFIX: &str = "server-emitted error event: ";

/// The role stamped on a synthesized assistant [`MessageDto`]. The pumped turn's
/// `assistant_blocks` are always the model's own output.
const ASSISTANT_ROLE: &str = "assistant";

/// Lower one engine [`ContentBlock`] to a [`MessageBlockDto`].
///
/// Returns `None` for blocks that have no `MessageBlockDto` analog
/// ([`ContentBlock::Image`], [`ContentBlock::Document`]): the `MessageBlockDto`
/// set equals the TUI scrollback block set (`Text | Thinking |
/// RedactedThinking | ToolUse | ToolResult`, plan F1-02), which omits image and
/// document input — so such a block in an assistant message is dropped from the
/// reproduced scrollback rather than mismodeled. A pumped `assistant_blocks` carries only text + thinking in
/// practice (`streaming_loop.rs:42-44`); the `ToolUse` / `ToolResult` arms keep
/// the lowering total for robustness. `ContentBlock` is exhaustively matched (it
/// is NOT `#[non_exhaustive]`), so adding an engine block kind surfaces here as a
/// compile error — a deliberate gate forcing a lowering decision.
#[must_use]
pub fn lower_content_block(block: &ContentBlock) -> Option<MessageBlockDto> {
    lower_content_block_with(block, &mut ToolUseIndex::default())
}

/// `tool_use_id` → `(tool name, call input)`, for pairing a `ToolResult` with
/// the `ToolUse` that produced it.
///
/// The engine's `ContentBlock::ToolResult` carries only `tool_use_id` and
/// `content` — no tool name, no input. Without this index a lowered result has
/// an EMPTY tool name and no diff, which is exactly the bug that made the iOS
/// client render `chat_tool_returned` as "工具  返回" and left its diff view
/// permanently unreachable.
///
/// The index must be threaded across a WHOLE transcript, never one message: a
/// `ToolUse` sits in assistant message N and its `ToolResult` in user message
/// N+1, never in the same message. `tui/src/replay.rs` keeps the same
/// side-table for the same reason.
#[derive(Debug, Default)]
pub struct ToolUseIndex(std::collections::HashMap<String, (String, serde_json::Value)>);

impl ToolUseIndex {
    /// Record a `ToolUse` so a later `ToolResult` can be paired with it.
    pub fn record(&mut self, id: &str, tool: &str, input: &serde_json::Value) {
        self.0
            .insert(id.to_string(), (tool.to_string(), input.clone()));
    }

    /// The `(tool, input)` recorded for `id`, if its call was seen.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<(&str, &serde_json::Value)> {
        self.0.get(id).map(|(tool, input)| (tool.as_str(), input))
    }
}

/// Lower one block, recording a `ToolUse` into `index` and consulting it to
/// enrich a `ToolResult`. See [`ToolUseIndex`] for the threading requirement.
#[must_use]
pub fn lower_content_block_with(
    block: &ContentBlock,
    index: &mut ToolUseIndex,
) -> Option<MessageBlockDto> {
    match block {
        ContentBlock::Text { text } | ContentBlock::TextJsUtf16 { text, .. } => {
            Some(MessageBlockDto::Text { text: text.clone() })
        }
        ContentBlock::Thinking {
            thinking,
            signature,
        } => Some(MessageBlockDto::Thinking {
            thinking: thinking.clone(),
            signature: signature.clone(),
        }),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => {
            index.record(&id.to_string(), name, input);
            Some(MessageBlockDto::ToolUse {
                id: id.to_string(),
                tool: name.clone(),
                input_json: value_to_json_string(input),
                header: Some(crate::tool_display::lower_tool_header(name, input)),
            })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            let id = tool_use_id.to_string();
            // The engine `ToolResult` echoes neither the tool name nor the
            // input; both come from the paired `ToolUse` via `index`. An
            // ORPHAN result (torn or compacted transcript window) keeps the
            // historical empty name — clients still correlate by `id`.
            let paired = index.get(&id);
            let tool = paired.map_or(String::new(), |(tool, _)| tool.to_string());
            let input = paired.map(|(_, input)| input);
            let (old_string, new_string, file_path) = input.map_or((None, None, None), |input| {
                client_presentation::tool_display::diff_inputs_for(&tool, input)
            });
            // `content` is the already-stringified tool output. Lower it through
            // the same JSON-String boundary as `ToolUseResult.result_json` so the
            // wire field is a JSON String (decision §0.4).
            let result = serde_json::Value::String(content.clone());
            Some(MessageBlockDto::ToolResult {
                id,
                tool: tool.clone(),
                result_json: value_to_json_string(&result),
                is_error: *is_error,
                old_string,
                new_string,
                file_path,
                display: Some(crate::tool_display::lower_tool_result_display(
                    &tool, input, &result, *is_error,
                )),
            })
        }
        // No `MessageBlockDto::Image`/`Document` — image and document input are
        // uniform inline wire DTOs elsewhere (decision §0.8), not scrollback
        // blocks. Drop them. The low-frequency server-side blocks
        // (`redacted_thinking`/`server_tool_use`/`connector_text`/
        // `advisor_tool_result`) likewise have no scrollback DTO analog — they
        // are preserved through the engine/JSONL for resume/replay byte parity
        // but render-skipped here.
        ContentBlock::Image { .. }
        | ContentBlock::Document { .. }
        | ContentBlock::RedactedThinking { .. }
        | ContentBlock::ServerToolUse { .. }
        | ContentBlock::ConnectorText { .. }
        | ContentBlock::AdvisorToolResult { .. }
        | ContentBlock::MediaAnalysis { .. }
        | ContentBlock::ProviderContent { .. } => None,
    }
}

/// Reproduce the assistant [`MessageDto`] from a [`PumpedTurn`].
///
/// Lowers `assistant_blocks` (in observation order) through
/// [`lower_content_block`], dropping any block with no DTO analog. The role is
/// always `"assistant"` — `assistant_blocks` are the model's own output.
#[must_use]
pub fn synthesize_message(turn: &PumpedTurn) -> MessageDto {
    MessageDto {
        loop_wakeup: None,
        role: ASSISTANT_ROLE.to_string(),
        blocks: turn
            .assistant_blocks
            .iter()
            .filter_map(lower_content_block)
            .collect(),
        images: Vec::new(),
    }
}

/// Synthesize the [`ClientEvent::MessageComplete`] for a finished turn.
///
/// Carries the reproduced [`MessageDto`] and the model `stop_reason` (both are
/// optional on the wire; the message is always present here, the `stop_reason`
/// is `Some` iff the stream carried a `message_delta`).
#[must_use]
pub fn message_complete_event(turn: &PumpedTurn) -> ClientEvent {
    ClientEvent::MessageComplete {
        stop_reason: turn.stop_reason.clone(),
        message: Some(synthesize_message(turn)),
    }
}

/// Synthesize the [`ClientEvent::TurnStarted`] emitted on `SendPrompt` receipt.
///
/// The engine emits no turn-start event; the adapter mints one so a client can
/// open a turn UI immediately. `turn_id` is an optional client-supplied
/// correlator carried verbatim.
#[must_use]
pub fn turn_started_event(turn_id: Option<u64>) -> ClientEvent {
    ClientEvent::TurnStarted { turn_id }
}

/// Classify an [`OrchestratorError`] into a coarse [`ErrorKindDto`].
///
/// See the module table. The `_ => Internal` arm fail-safes any future
/// `#[non_exhaustive]` variant.
#[must_use]
pub fn error_kind_for(err: &OrchestratorError) -> ErrorKindDto {
    match err {
        // Task 6 (llm-runtime future-work batch 5): the terminal-429 limits
        // copy ("You've hit your … limit · resets …") — an API-side failure,
        // same coarse class as the `ApiCall` it replaces.
        OrchestratorError::Streaming(_)
        | OrchestratorError::ApiCall(_)
        | OrchestratorError::RateLimitRejected { .. } => ErrorKindDto::Transport,
        OrchestratorError::StreamingProtocol(reason) => {
            if reason.starts_with(SERVER_ERROR_PREFIX) {
                ErrorKindDto::Server
            } else {
                ErrorKindDto::Protocol
            }
        }
        OrchestratorError::MaxTurnsReached { .. } => ErrorKindDto::MaxTurns,
        OrchestratorError::StreamEndedWithoutStop
        | OrchestratorError::Internal(_)
        | OrchestratorError::PermissionAbort { .. }
        | OrchestratorError::Compaction(_)
        | OrchestratorError::CompactionCancelled
        | OrchestratorError::VisionDelegationCancelled
        // Task 7: all consecutive-overloaded retries exhausted with no fallback model.
        // Byte-locked message: "Repeated 529 Overloaded errors" (errors.ts:166).
        // Treated as a fatal API-side condition — same class as a hard API failure.
        | OrchestratorError::RepeatedOverloaded
        // MaxBudget is a terminal cost-ceiling stop (claude-code error_max_budget_usd);
        // no dedicated DTO kind, so it maps to Internal — the "Reached maximum budget
        // ($X)" Display is preserved in the event's message. (client-adapter is not the
        // headless `--max-budget` path; this just keeps the match exhaustive.)
        | OrchestratorError::MaxBudgetReached { .. } => ErrorKindDto::Internal,
    }
}

/// Lower an [`OrchestratorError`] to a [`ClientEvent::Error`].
///
/// The `kind` is the coarse class from [`error_kind_for`]; the `message` is the
/// error's `Display` string (the byte-locked reasons — e.g.
/// `"Reached maximum number of turns (30)"` — preserved verbatim for the client).
#[must_use]
pub fn map_orchestrator_error(err: &OrchestratorError) -> ClientEvent {
    ClientEvent::Error {
        kind: error_kind_for(err),
        message: err.to_string(),
    }
}

/// The connection-scoped live-turn wrapper.
///
/// Holds the same `Arc<dyn ClientEventSink>` as the [`AdapterOutputStream`]
/// (F1-12) and the permission gate (F1-14), so the synthesized boundary events
/// interleave with the streamed `TextDelta` / `ToolUse*` events on the one
/// outbound channel. One wrapper per transport connection.
///
/// [`AdapterOutputStream`]: crate::output_stream::AdapterOutputStream
pub struct TurnWrapper {
    sink: Arc<dyn ClientEventSink>,
}

impl TurnWrapper {
    /// Wrap a sink shared with the rest of the connection-scoped adapter.
    #[must_use]
    pub fn new(sink: Arc<dyn ClientEventSink>) -> Self {
        Self { sink }
    }

    /// Emit the synthesized [`ClientEvent::TurnStarted`] — call on `SendPrompt`
    /// receipt, BEFORE driving the turn future.
    pub async fn emit_turn_started(&self, turn_id: Option<u64>) {
        self.sink.emit(turn_started_event(turn_id)).await;
    }

    /// Translate a finished turn `Result` into its boundary [`ClientEvent`].
    ///
    /// On `Ok(PumpedTurn)`: emit the synthesized [`ClientEvent::MessageComplete`]
    /// (the `OutputStream` already streamed the `TurnEnded` / `CostUpdate`; this
    /// is the message-boundary marker on top).
    ///
    /// On `Err(OrchestratorError)`: emit the lowered [`ClientEvent::Error`].
    pub async fn complete(&self, result: Result<PumpedTurn, OrchestratorError>) {
        match result {
            Ok(turn) => {
                self.sink.emit(message_complete_event(&turn)).await;
            }
            Err(err) => {
                self.sink.emit(map_orchestrator_error(&err)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockSink;
    use protocol::{ContentBlock, ToolUseId};

    /// Construct an `LlmError` for the transport-class table rows.
    ///
    /// `OrchestratorError::Streaming` and `::ApiCall` both wrap `llm_runtime::LlmError`
    /// (retyped in 3a Task 5, commit 5f8b3e35).
    fn api_error() -> llm_runtime::LlmError {
        llm_runtime::LlmError::Transport {
            message: "nope".into(),
        }
    }

    /// Every `OrchestratorError` variant maps to the correct coarse
    /// `ErrorKindDto`. Table-driven over the variants (the named F1-13 test).
    #[test]
    fn each_orchestrator_error_maps_to_error_kind() {
        let cases: Vec<(OrchestratorError, ErrorKindDto)> = vec![
            // Transport: both the streaming and batched API-failure variants.
            (
                OrchestratorError::Streaming(api_error()),
                ErrorKindDto::Transport,
            ),
            (
                OrchestratorError::ApiCall(api_error()),
                ErrorKindDto::Transport,
            ),
            // Task 6 (batch 5): terminal-429 limits copy — same coarse class
            // as the ApiCall(RateLimited) it replaces.
            (
                OrchestratorError::RateLimitRejected {
                    message: "You've hit your weekly limit · resets 3pm".into(),
                },
                ErrorKindDto::Transport,
            ),
            // Server: a `StreamingProtocol` carrying the `streaming_loop.rs:93`
            // server-emitted-error prefix.
            (
                OrchestratorError::StreamingProtocol(format!("{SERVER_ERROR_PREFIX}overloaded")),
                ErrorKindDto::Server,
            ),
            // Protocol: any other `StreamingProtocol` (a per-block wire violation).
            (
                OrchestratorError::StreamingProtocol("block 3 has no start".into()),
                ErrorKindDto::Protocol,
            ),
            // MaxTurns: the budget-exhausted variant.
            (
                OrchestratorError::MaxTurnsReached { max_turns: 30 },
                ErrorKindDto::MaxTurns,
            ),
            // Internal: stream cut, orchestrator invariant, compaction.
            (
                OrchestratorError::StreamEndedWithoutStop,
                ErrorKindDto::Internal,
            ),
            (
                OrchestratorError::Internal("boom".into()),
                ErrorKindDto::Internal,
            ),
            (
                OrchestratorError::PermissionAbort {
                    message: "Agent aborted: too many classifier denials in headless mode".into(),
                },
                ErrorKindDto::Internal,
            ),
            (
                OrchestratorError::CompactionCancelled,
                ErrorKindDto::Internal,
            ),
            // Task 7: exhausted 529 retries with no fallback model configured.
            (
                OrchestratorError::RepeatedOverloaded,
                ErrorKindDto::Internal,
            ),
        ];

        for (err, expected) in &cases {
            assert_eq!(
                error_kind_for(err),
                *expected,
                "variant {err:?} should map to {expected:?}"
            );
        }
    }

    /// `map_orchestrator_error` emits a `ClientEvent::Error` carrying the coarse
    /// kind AND the error's verbatim `Display` message (the byte-locked reason).
    #[test]
    fn map_orchestrator_error_carries_kind_and_display_message() {
        let ev = map_orchestrator_error(&OrchestratorError::MaxTurnsReached { max_turns: 30 });
        match ev {
            ClientEvent::Error { kind, message } => {
                assert_eq!(kind, ErrorKindDto::MaxTurns);
                // The byte-locked Display literal is preserved verbatim.
                assert_eq!(message, "Reached maximum number of turns (30)");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// The server-emitted-error fold (`streaming_loop.rs:93`) is recovered as the
    /// `Server` class, while a bare protocol violation stays `Protocol`.
    #[test]
    fn streaming_protocol_splits_server_from_protocol() {
        let server =
            OrchestratorError::StreamingProtocol(format!("{SERVER_ERROR_PREFIX}rate limited"));
        let protocol = OrchestratorError::StreamingProtocol("double stop on block 1".into());
        assert_eq!(error_kind_for(&server), ErrorKindDto::Server);
        assert_eq!(error_kind_for(&protocol), ErrorKindDto::Protocol);
    }

    /// `MessageComplete` is synthesized from a `PumpedTurn`: the `assistant_blocks`
    /// become a `MessageDto` (role `"assistant"`) reproducing the block set, and
    /// the `stop_reason` rides on the event (the named F1-13 test).
    #[test]
    fn message_complete_synthesized_from_pumped_turn() {
        let turn = PumpedTurn {
            stop_details: None,
            output_tokens: 0,
            assistant_blocks: vec![
                ContentBlock::Thinking {
                    thinking: "let me think".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "the answer is 42".into(),
                },
            ],
            tool_uses: Vec::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            cost_quote: None,
        };

        match message_complete_event(&turn) {
            ClientEvent::MessageComplete {
                stop_reason,
                message,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                let msg = message.expect("message present");
                assert_eq!(msg.role, "assistant");
                assert_eq!(
                    msg.blocks,
                    vec![
                        MessageBlockDto::Thinking {
                            thinking: "let me think".into(),
                            signature: Some("sig".into()),
                        },
                        MessageBlockDto::Text {
                            text: "the answer is 42".into(),
                        },
                    ]
                );
            }
            other => panic!("expected MessageComplete, got {other:?}"),
        }
    }

    /// An image block in `assistant_blocks` (no `MessageBlockDto` analog) is
    /// dropped from the reproduced scrollback rather than mismodeled.
    #[test]
    fn image_block_is_dropped_from_synthesized_message() {
        let turn = PumpedTurn {
            stop_details: None,
            output_tokens: 0,
            assistant_blocks: vec![
                ContentBlock::Image {
                    source: protocol::ImageSource::Url {
                        url: "https://example.test/x.png".into(),
                    },
                },
                ContentBlock::Text {
                    text: "caption".into(),
                },
            ],
            tool_uses: Vec::new(),
            stop_reason: None,
            usage: None,
            cost_quote: None,
        };
        let msg = synthesize_message(&turn);
        assert_eq!(
            msg.blocks,
            vec![MessageBlockDto::Text {
                text: "caption".into()
            }]
        );
    }

    /// A `ToolUse` block lowers with its input as a JSON String (decision §0.4)
    /// and its id stringified.
    #[test]
    fn tool_use_block_lowers_input_to_json_string() {
        let id = ToolUseId::new();
        let block = ContentBlock::ToolUse {
            id: id.clone(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/tmp/x"}),
            provider_id: None,
        };
        match lower_content_block(&block).expect("lowered") {
            MessageBlockDto::ToolUse {
                id: gid,
                tool,
                input_json,
                ..
            } => {
                assert_eq!(gid, id.to_string());
                assert_eq!(tool, "Read");
                let back: serde_json::Value = serde_json::from_str(&input_json).unwrap();
                assert_eq!(back, serde_json::json!({"file_path": "/tmp/x"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// `TurnStarted` is synthesized on `SendPrompt` receipt, carrying any
    /// client-supplied correlator verbatim (the named F1-13 test).
    #[tokio::test]
    async fn turn_started_emitted_on_send() {
        let sink = MockSink::arc();
        let wrapper = TurnWrapper::new(sink.clone());

        wrapper.emit_turn_started(Some(7)).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ClientEvent::TurnStarted { turn_id: Some(7) });
    }

    /// `TurnStarted` with no correlator carries `turn_id: None`.
    #[tokio::test]
    async fn turn_started_without_correlator() {
        let sink = MockSink::arc();
        let wrapper = TurnWrapper::new(sink.clone());

        wrapper.emit_turn_started(None).await;

        assert_eq!(
            sink.events().await[0],
            ClientEvent::TurnStarted { turn_id: None }
        );
    }

    /// The wrapper's `complete(Ok)` path emits the synthesized `MessageComplete`
    /// on the sink.
    #[tokio::test]
    async fn complete_ok_emits_message_complete() {
        let sink = MockSink::arc();
        let wrapper = TurnWrapper::new(sink.clone());

        let turn = PumpedTurn {
            stop_details: None,
            output_tokens: 0,
            assistant_blocks: vec![ContentBlock::Text { text: "hi".into() }],
            tool_uses: Vec::new(),
            stop_reason: Some("end_turn".into()),
            usage: None,
            cost_quote: None,
        };
        wrapper.complete(Ok(turn)).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::MessageComplete {
                stop_reason,
                message,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(message.as_ref().unwrap().blocks.len(), 1);
            }
            other => panic!("expected MessageComplete, got {other:?}"),
        }
    }

    /// The wrapper's `complete(Err)` path emits the lowered `Error` on the sink.
    #[tokio::test]
    async fn complete_err_emits_error_event() {
        let sink = MockSink::arc();
        let wrapper = TurnWrapper::new(sink.clone());

        wrapper
            .complete(Err(OrchestratorError::Streaming(api_error())))
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClientEvent::Error { kind, message } => {
                assert_eq!(*kind, ErrorKindDto::Transport);
                assert!(
                    message.starts_with("streaming transport error: "),
                    "got: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
