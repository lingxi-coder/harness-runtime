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
use lingxi_core::host::{OutputStream, ServerFallbackTombstoneMessage};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use llm_runtime::{ExecutionUsage as LlmUsage, HistoryEvent, LlmError};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[path = "streaming_partial_close.rs"]
mod partial_close;
use partial_close::PartialStreamClose;

/// Native `Au`/`iS`/`wa` completion state, independent of raw-frame forwarding.
#[derive(Debug, Default)]
struct StreamCompletionState {
    message_started: bool,
    open_block: Option<u32>,
    terminal_delta: bool,
}

impl StreamCompletionState {
    fn observe(&mut self, event: &HistoryEvent) {
        match event {
            HistoryEvent::MessageStart { .. } => self.message_started = true,
            HistoryEvent::ContentBlockStart { index, .. } => {
                self.open_block = Some(*index);
                self.terminal_delta = false;
            }
            HistoryEvent::ContentBlockDelta { .. } => self.terminal_delta = false,
            HistoryEvent::ContentBlockStop { .. } => {
                self.open_block = None;
                self.terminal_delta = false;
            }
            HistoryEvent::MessageDelta { delta, .. } if delta.stop_reason.is_some() => {
                self.terminal_delta = true;
            }
            _ => {}
        }
    }

    fn is_complete(&self, turn: &PumpedTurn) -> bool {
        self.terminal_delta && self.open_block.is_none() && turn.stop_reason.is_some()
    }

    fn is_complete_at_eof(&self, turn: &PumpedTurn) -> bool {
        self.message_started
            && (!turn.assistant_blocks.is_empty()
                || !turn.tool_uses.is_empty()
                || turn
                    .stop_reason
                    .as_ref()
                    .is_some_and(|reason| !reason.is_empty()))
            && self.is_complete(turn)
    }
}

fn complete_response_error(error: &LlmError, turn: &PumpedTurn) -> bool {
    // Native DUt treats connection loss / watchdog close as already complete.
    // A server error or overload additionally requires completed real output.
    matches!(
        error,
        LlmError::Transport { .. } | LlmError::TransportTimeout { .. }
    ) || llm_runtime::model::stream_watchdog::is_stream_idle_timeout(error)
        || llm_runtime::model::stream_watchdog::is_stream_suspended(error)
        || (matches!(
            error,
            LlmError::ProviderInternal
                | LlmError::ProviderTimeout { .. }
                | LlmError::Overloaded { .. }
        ) && partial_has_output(turn))
}

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

/// One completed assistant content-block row from the live stream. Native
/// server-fallback controls address these rows by API block index, while the
/// transcript writer needs the row UUID, provider message id, and model that
/// were current when the block completed.
#[derive(Debug, Clone)]
pub(crate) struct CompletedAssistantRow {
    pub(crate) per_turn_effort: Option<String>,
    /// Monotonic pump arrival order; provider block indexes may be reused by
    /// later accepted fallback responses in the same host turn.
    pub(crate) stream_order: u64,
    /// Host UUID for the eventual per-block transcript envelope.
    pub(crate) row_id: MessageId,
    /// Provider-owned `message_start.message.id` for the row's response.
    pub(crate) provider_message_id: String,
    /// Model identity associated with this completed row.
    pub(crate) model: String,
    /// Request-route profile captured for this row, before any later session
    /// change can rewrite the active profile.
    pub(crate) model_profile: Option<String>,
    /// Synthetic API-error rows are not candidates for native J stitching.
    pub(crate) is_api_error: bool,
    /// Terminal response fields copied onto every currently-live native row
    /// when MessageDelta arrives.
    pub(crate) stop_reason: Option<String>,
    pub(crate) stop_details: Option<llm_runtime::HistoryStopDetails>,
    pub(crate) usage: Option<LlmUsage>,
    /// Request header id associated with the streamed response, when known.
    pub(crate) request_id: Option<String>,
    /// Native outer-row creation time, captured at content_block_stop.
    pub(crate) timestamp: String,
    /// Link returned by the incremental transcript writer, if one is attached.
    pub(crate) persisted_link: Option<PersistedAssistantRowLink>,
    /// Canonical accepted blocks shared by query, W1, and persistence.
    /// Original ToolUse inputs are captured separately for dispatch.
    pub(crate) content: Vec<ContentBlock>,
    /// Mod append is an at-most-once row effect even if a writer call fails or
    /// a later MessageDelta revisits this completed row.
    pub(crate) session_append_dispatched: bool,
    /// Retained source rows replaced by a server-stitch text row, in native
    /// `retainedMessages` order.
    pub(crate) supersedes_row_ids: Vec<MessageId>,
}

/// Durable chain facts assigned by the incremental transcript writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedAssistantRowLink {
    pub(crate) uuid: String,
    pub(crate) parent_uuid: Option<String>,
}

/// Native `Je` is enabled for main-thread REPL and SDK query sources only.
/// Standalone pump users have no query-source context and therefore leave the
/// corresponding [`ExecutorPump`] flag disabled.
pub(crate) fn native_server_fallback_supersedes_enabled(query_source: &str) -> bool {
    let query_source = crate::config::sanitize_query_source(query_source);
    query_source.starts_with(crate::config::QUERY_SOURCE_REPL_MAIN_THREAD)
        || query_source == crate::config::QUERY_SOURCE_SDK
}

/// Outcome of consuming one stream.
#[derive(Debug, Default)]
pub struct PumpedTurn {
    pub per_turn_effort: Option<String>,
    /// Host-owned sweeps of exact live executor ids. These are runtime control
    /// facts and never become provider events or synthetic tool-result rows.
    pub tool_use_removals: Vec<lingxi_core::host::tool_use_lifecycle::ToolUseRemoval>,
    /// Canonical model identity used for visible session state after resolving
    /// native aliases. The exact provider-reported model remains in the SDK
    /// response metadata.
    pub served_model: Option<String>,
    /// Provider control observations. The query controller owns allowlist
    /// acceptance, notices and session selection.
    pub server_fallback_events: Vec<llm_runtime::history::HistoryServerFallback>,
    /// Prefix of `server_fallback_events` applied by the live conversation
    /// fallback controller. The completed-response adapter must not apply
    /// these observations a second time.
    pub handled_server_fallback_events: usize,
    /// Completed per-block assistant transcript rows. Streaming fallback
    /// stitches can replace these rows without flattening away their UUIDs.
    pub(crate) assistant_rows: Vec<CompletedAssistantRow>,
    /// Last stop-time row identity from this stream. The logical assistant turn
    /// identity remains separately owned by the driver.
    pub(crate) assistant_row_identity: Option<MessageId>,
    /// Tool-use row UUIDs returned by incremental JSONL appends.
    pub(crate) assistant_tool_parent_uuids: HashMap<ToolUseId, String>,
    /// Identity selected by native server-stitch when a retained prefix is
    /// merged into the next eligible assistant text row. The completed
    /// response still lives as one merged message in session history.
    pub(crate) replacement_message_id: Option<MessageId>,
    /// Text/thinking blocks accumulated during a live stream, or the complete
    /// content of a recovered non-streaming response. Live per-block rows
    /// retain the authoritative order separately in `assistant_rows`.
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
    /// At least one typed cost observation was delivered. When true, its
    /// native fallback marker is authoritative over nested stream metadata.
    pub cost_quote_observed: bool,
    /// A provider-native per-iteration quote was observed on the stream,
    /// including an explicit missing-price outcome. This is separate from
    /// aggregate usage so downstream settlement can avoid re-pricing it.
    pub native_server_fallback_quote: bool,
    /// Native usage-iteration identity for an unpriced fallback quote, when
    /// the provider exposed one independently of the host response model.
    pub native_cost_model: Option<String>,
    /// Refusal `stop_details` (`{category, explanation}`) from the final
    /// `message_delta` — drives the terminal refusal message's cyber/bio
    /// variant. `None` for non-refusal turns.
    pub stop_details: Option<llm_runtime::HistoryStopDetails>,
}

/// Query-local result collection for Native's post-event `Tn()` poll.
///
/// `query_rows` is the raw `je` sequence used to build the next model request;
/// `journal` is the event-order sequence used by the interactive transcript
/// writer. They intentionally remain separate because a stream may produce a
/// tool result between two assistant content-block rows while the next request
/// groups all assistant rows before all tool-result rows.
#[derive(Default)]
pub(crate) struct StreamToolSettlement {
    pub(crate) query_rows: Vec<(ConversationMessage, Option<ToolUseId>)>,
    pub(crate) journal: Vec<StreamEventJournalEntry>,
    pub(crate) journal_cursor: usize,
    pub(crate) prevent_continuation: bool,
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
    /// Generation that owns accepted result rows and the later batch hook.
    pub(crate) publication_guard:
        Option<std::sync::Arc<dyn hooks::attachment::HookPublicationGuard>>,
    pub(crate) context_modifiers: Vec<tool_api::ContextModifier>,
    pub(crate) tool_use_parent_uuids: HashMap<ToolUseId, String>,
}

#[derive(Debug)]
pub(crate) enum StreamEventJournalEntry {
    AssistantRow(MessageId),
    UserRow {
        stored: ConversationMessage,
        parent_uuid: Option<String>,
        persist: bool,
        /// Row creation time, retained for Native tombstones and durable
        /// serialization. It is captured when the user result is created,
        /// not when the deferred event journal is flushed.
        timestamp: String,
        /// Tool completion that owns this result/new-message row. `None`
        /// identifies a user row from outside the tool-completion `je` set.
        tool_completion_id: Option<ToolUseId>,
    },
    FlushHookAttachments(ToolUseId),
}

impl StreamToolSettlement {
    pub(crate) fn record_assistant_row(&mut self, row_id: MessageId) {
        self.journal
            .push(StreamEventJournalEntry::AssistantRow(row_id));
    }

    /// Clear all tool-completion query rows when Native resets its `je`
    /// accumulator. This intentionally removes the whole completion batch
    /// rather than selecting only the fallback event's discarded tool IDs.
    pub(crate) fn discard_tool_completion_rows(&mut self) -> Vec<ServerFallbackTombstoneMessage> {
        let mut removed_rows = HashSet::new();
        let mut removed_tools = HashSet::new();
        let mut tombstones = Vec::new();
        for entry in &self.journal {
            if let StreamEventJournalEntry::UserRow {
                stored,
                timestamp,
                tool_completion_id: Some(tool_use_id),
                ..
            } = entry
            {
                removed_rows.insert(stored.id());
                removed_tools.insert(tool_use_id.clone());
                let (message_type, content) = match stored {
                    ConversationMessage::User { content, .. } => ("user", content.clone()),
                    ConversationMessage::Assistant { content, .. } => {
                        ("assistant", content.clone())
                    }
                    ConversationMessage::System { content, .. } => (
                        "system",
                        vec![ContentBlock::Text {
                            text: content.clone(),
                            citations: None,
                        }],
                    ),
                };
                tombstones.push(ServerFallbackTombstoneMessage {
                    uuid: stored.id(),
                    message_type: message_type.into(),
                    timestamp: timestamp.clone(),
                    request_id: None,
                    request_ref: None,
                    provider_message_id: None,
                    model: None,
                    stop_reason: None,
                    stop_details: None,
                    usage: None,
                    content,
                    is_api_error_message: None,
                    supersedes_uuids: None,
                });
            }
        }

        self.query_rows
            .retain(|(message, _)| !removed_rows.contains(&message.id()));

        // Keep the cursor meaningful if this helper is ever used after a
        // prefix was flushed: it counts retained entries from that prefix.
        let old_cursor = self.journal_cursor;
        let mut retained_before_cursor = 0;
        let mut retained_journal = Vec::with_capacity(self.journal.len());
        for (index, entry) in std::mem::take(&mut self.journal).into_iter().enumerate() {
            let remove = match &entry {
                StreamEventJournalEntry::UserRow {
                    tool_completion_id: Some(_),
                    ..
                } => true,
                StreamEventJournalEntry::FlushHookAttachments(tool_use_id) => {
                    removed_tools.contains(tool_use_id)
                }
                _ => false,
            };
            if !remove {
                if index < old_cursor {
                    retained_before_cursor += 1;
                }
                retained_journal.push(entry);
            }
        }
        self.journal = retained_journal;
        self.journal_cursor = retained_before_cursor;
        self.prevent_continuation = false;
        self.post_tool_batch_calls.clear();
        tombstones
    }

    /// Drop the entire attempt-local K/je journal for a model-chain advance.
    pub(crate) fn discard_attempt_rows(&mut self) -> Vec<ServerFallbackTombstoneMessage> {
        let tombstones = self.discard_tool_completion_rows();
        self.query_rows.clear();
        self.journal.clear();
        self.journal_cursor = 0;
        self.prevent_continuation = false;
        self.post_tool_batch_calls.clear();
        self.context_modifiers.clear();
        self.tool_use_parent_uuids.clear();
        tombstones
    }
}

/// Native accepted-midstream `J` state. Source rows remain untouched until a
/// non-error assistant text block with visible text arrives; if the stream
/// terminates first, those original rows remain the only durable content.
#[derive(Debug, Clone)]
struct PendingServerStitch {
    retained_text: String,
    retained_row_ids: Vec<MessageId>,
    retained_rows: Vec<CompletedAssistantRow>,
    after_row_order: u64,
}

fn rows_for_api_indices(
    current_row_by_api_index: &HashMap<u32, MessageId>,
    api_indices: &[usize],
) -> Vec<MessageId> {
    api_indices
        .iter()
        .filter_map(|index| u32::try_from(*index).ok())
        .filter_map(|index| current_row_by_api_index.get(&index).copied())
        .collect()
}

fn tool_ids_for_row_ids(turn: &PumpedTurn, row_ids: &[MessageId]) -> Vec<ToolUseId> {
    row_ids
        .iter()
        .flat_map(|row_id| {
            turn.assistant_rows
                .iter()
                .find(|row| row.row_id == *row_id)
                .into_iter()
                .flat_map(|row| row.content.iter())
        })
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn persisted_links_for_row_ids(
    turn: &PumpedTurn,
    row_ids: &[MessageId],
) -> Vec<PersistedAssistantRowLink> {
    let row_ids: HashSet<MessageId> = row_ids.iter().copied().collect();
    turn.assistant_rows
        .iter()
        .filter(|row| row_ids.contains(&row.row_id))
        .filter_map(|row| row.persisted_link.clone())
        .collect()
}

pub(crate) fn tombstone_message_for_row(
    row: &CompletedAssistantRow,
) -> ServerFallbackTombstoneMessage {
    ServerFallbackTombstoneMessage {
        uuid: row.row_id,
        message_type: "assistant".into(),
        timestamp: row.timestamp.clone(),
        request_id: row.request_id.clone(),
        request_ref: None,
        provider_message_id: (!row.provider_message_id.is_empty())
            .then(|| row.provider_message_id.clone()),
        model: (!row.model.is_empty()).then(|| row.model.clone()),
        stop_reason: row.stop_reason.clone(),
        stop_details: row
            .stop_details
            .as_ref()
            .map(|details| serde_json::to_value(details).expect("stop details serialize")),
        usage: row.usage.as_ref().and_then(assistant_usage_value),
        content: row.content.clone(),
        is_api_error_message: row.is_api_error.then_some(true),
        supersedes_uuids: (!row.supersedes_row_ids.is_empty())
            .then(|| row.supersedes_row_ids.clone()),
    }
}

pub(crate) async fn clear_tombstoned_tool_result_metadata(
    orch: &crate::conversation::ConversationOrchestrator,
    tombstones: &[ServerFallbackTombstoneMessage],
) {
    let mut tool_use_ids = HashSet::new();
    for tombstone in tombstones {
        for block in &tombstone.content {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                tool_use_ids.insert(tool_use_id.clone());
            }
        }
    }
    for tool_use_id in tool_use_ids {
        orch.clear_discarded_tool_result_metadata(&tool_use_id)
            .await;
    }
}

fn assistant_usage_value(usage: &LlmUsage) -> Option<Value> {
    const NATIVE_USAGE_FIELDS: &[&str] = &[
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "cache_creation",
        "server_tool_use",
    ];
    let source = usage.provider_metadata.as_object()?;
    let native = NATIVE_USAGE_FIELDS
        .iter()
        .filter_map(|key| source.get(*key).map(|value| ((*key).into(), value.clone())))
        .collect::<serde_json::Map<String, Value>>();
    (!native.is_empty()).then_some(Value::Object(native))
}

pub(crate) fn assistant_row_timestamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn rows_for_ids(turn: &PumpedTurn, row_ids: &[MessageId]) -> Vec<CompletedAssistantRow> {
    row_ids
        .iter()
        .filter_map(|row_id| turn.assistant_rows.iter().find(|row| row.row_id == *row_id))
        .cloned()
        .collect()
}

fn supersedes_ids_for_retained_rows(rows: &[CompletedAssistantRow]) -> Vec<MessageId> {
    rows.iter()
        .filter(|row| !row.is_api_error)
        .filter(|row| {
            row.content.iter().any(|block| {
                block
                    .visible_text()
                    .map_or(true, |text| !text.trim().is_empty())
            })
        })
        .map(|row| row.row_id)
        .collect()
}

fn replace_stream_blocks_for_row(
    turn: &mut PumpedTurn,
    assistant_block_indices: &mut Vec<(u32, bool)>,
    assistant_block_row_ids: &mut Vec<MessageId>,
    row_id: MessageId,
    content: &[ContentBlock],
) {
    update_accepted_stream_blocks_for_row(
        turn,
        assistant_block_indices,
        assistant_block_row_ids,
        row_id,
        content,
        None,
    );
}

fn update_accepted_stream_blocks_for_row(
    turn: &mut PumpedTurn,
    assistant_block_indices: &mut Vec<(u32, bool)>,
    assistant_block_row_ids: &mut Vec<MessageId>,
    row_id: MessageId,
    content: &[ContentBlock],
    new_row_order: Option<(u32, bool)>,
) {
    let mut blocks = Vec::new();
    let mut indices = Vec::new();
    let mut row_ids = Vec::new();
    let mut source_order = None;
    for ((block, index), current_row_id) in std::mem::take(&mut turn.assistant_blocks)
        .into_iter()
        .zip(std::mem::take(assistant_block_indices))
        .zip(std::mem::take(assistant_block_row_ids))
    {
        if current_row_id == row_id {
            source_order.get_or_insert(index);
        } else {
            blocks.push(block);
            indices.push(index);
            row_ids.push(current_row_id);
        }
    }
    if let Some(index) = source_order.or(new_row_order) {
        let position = indices.partition_point(|existing| existing <= &index);
        // ToolUse blocks remain in the canonical row and the source dispatch
        // index. This list tracks the non-tool blocks, including Text added by
        // session.append to an originally tool-only row.
        for (offset, block) in content
            .iter()
            .filter(|block| !matches!(block, ContentBlock::ToolUse { .. }))
            .enumerate()
        {
            blocks.insert(position + offset, block.clone());
            indices.insert(position + offset, index);
            row_ids.insert(position + offset, row_id);
        }
    }
    turn.assistant_blocks = blocks;
    *assistant_block_indices = indices;
    *assistant_block_row_ids = row_ids;
}

/// Apply native J as soon as the next eligible completed assistant text row is
/// available. The per-row envelope is already known not to be a synthetic API
/// error row; later transport failures do not undo this completed seam.
/// `J.text` is concatenated literally, unlike retry `H$r`.
async fn finish_pending_server_stitch(
    turn: &mut PumpedTurn,
    output: &Arc<dyn OutputStream>,
    assistant_block_indices: &mut Vec<(u32, bool)>,
    assistant_block_row_ids: &mut Vec<MessageId>,
    current_row_by_api_index: &mut HashMap<u32, MessageId>,
    pending: &mut Option<PendingServerStitch>,
    pump: &mut Option<ExecutorPump<'_, '_>>,
) {
    let Some(stitch) = pending.take() else {
        return;
    };
    let candidate_position = turn.assistant_rows.iter().position(|row| {
        row.stream_order >= stitch.after_row_order
            && !row.is_api_error
            && row
                .content
                .iter()
                .find_map(ContentBlock::visible_text)
                .is_some_and(|text| !text.trim().is_empty())
    });
    let Some(candidate_position) = candidate_position else {
        // A blank/error response leaves J pending for a later completed row.
        *pending = Some(stitch);
        return;
    };

    let candidate_id = turn.assistant_rows[candidate_position].row_id;
    let should_record_supersedes = pump.as_ref().is_some_and(|pump| pump.record_supersedes);
    let supersedes_row_ids = supersedes_ids_for_retained_rows(&stitch.retained_rows);
    let candidate = &mut turn.assistant_rows[candidate_position];
    if let Some(block) = candidate
        .content
        .iter_mut()
        .find(|block| block.visible_text().is_some())
    {
        match block {
            ContentBlock::Text { text, .. } => {
                text.insert_str(0, &stitch.retained_text);
            }
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                ..
            } => {
                text.insert_str(0, &stitch.retained_text);
                let mut merged: Vec<u16> = stitch.retained_text.encode_utf16().collect();
                merged.append(utf16_code_units);
                *utf16_code_units = merged;
            }
            ContentBlock::ProviderContent { value, .. } => {
                if let Some(text) = value.get_mut("text") {
                    let mut merged = stitch.retained_text.clone();
                    merged.push_str(text.as_str().expect("visible native text"));
                    *text = serde_json::Value::String(merged);
                }
            }
            _ => unreachable!("visible_text only exposes text blocks"),
        }
        if should_record_supersedes {
            candidate.supersedes_row_ids = supersedes_row_ids;
        }
    }
    let candidate_content = turn.assistant_rows[candidate_position].content.clone();
    replace_stream_blocks_for_row(
        turn,
        assistant_block_indices,
        assistant_block_row_ids,
        candidate_id,
        &candidate_content,
    );

    let retained_ids: HashSet<MessageId> = stitch.retained_row_ids.iter().copied().collect();
    let mut kept_blocks = Vec::new();
    let mut kept_indices = Vec::new();
    let mut kept_row_ids = Vec::new();
    for ((block, index), row_id) in std::mem::take(&mut turn.assistant_blocks)
        .into_iter()
        .zip(std::mem::take(assistant_block_indices))
        .zip(std::mem::take(assistant_block_row_ids))
    {
        if !retained_ids.contains(&row_id) {
            kept_blocks.push(block);
            kept_indices.push(index);
            kept_row_ids.push(row_id);
        }
    }
    turn.assistant_blocks = kept_blocks;
    *assistant_block_indices = kept_indices;
    *assistant_block_row_ids = kept_row_ids;
    let retained_tombstones: Vec<ServerFallbackTombstoneMessage> = stitch
        .retained_rows
        .iter()
        .map(tombstone_message_for_row)
        .collect();
    for tombstone in &retained_tombstones {
        output.emit_server_fallback_tombstone(tombstone, true).await;
    }
    let retained_links: Vec<PersistedAssistantRowLink> = stitch
        .retained_rows
        .iter()
        .filter_map(|row| row.persisted_link.clone())
        .collect();
    if let Some(p) = pump.as_mut() {
        p.executor
            .remove_assistant_stream_rows(&retained_links)
            .await;
    }
    turn.assistant_rows
        .retain(|row| !retained_ids.contains(&row.row_id));
    current_row_by_api_index.retain(|_, row_id| !retained_ids.contains(row_id));

    if let Some(p) = pump.as_mut() {
        p.assistant_id = candidate_id;
    }
    turn.assistant_row_identity = Some(candidate_id);
    turn.replacement_message_id = Some(candidate_id);
}

/// Merge host presentation snapshots while keeping complete SDK measurements
/// authoritative, including explicit zero counts.
fn merge_usage(seed: &LlmUsage, delta: &LlmUsage) -> LlmUsage {
    seed.merge_snapshot(delta)
}

fn sync_pending_stitch_rows(turn: &PumpedTurn, pending: &mut Option<PendingServerStitch>) {
    let Some(stitch) = pending.as_mut() else {
        return;
    };
    for snapshot in &mut stitch.retained_rows {
        if let Some(row) = turn
            .assistant_rows
            .iter()
            .find(|row| row.row_id == snapshot.row_id)
        {
            snapshot.model.clone_from(&row.model);
            snapshot.model_profile.clone_from(&row.model_profile);
            snapshot.stop_reason.clone_from(&row.stop_reason);
            snapshot.stop_details.clone_from(&row.stop_details);
            snapshot.usage.clone_from(&row.usage);
            snapshot.request_id.clone_from(&row.request_id);
            snapshot.content.clone_from(&row.content);
            snapshot.persisted_link.clone_from(&row.persisted_link);
            snapshot.session_append_dispatched = row.session_append_dispatched;
        }
    }
}

async fn sync_message_delta_to_rows(
    turn: &mut PumpedTurn,
    assistant_block_indices: &mut Vec<(u32, bool)>,
    assistant_block_row_ids: &mut Vec<MessageId>,
    pump: &mut Option<ExecutorPump<'_, '_>>,
    pending: &mut Option<PendingServerStitch>,
    update_stop_details: bool,
    persist_unlinked_rows: bool,
) {
    let mut tool_parents = Vec::new();
    let mut rewritten_rows = Vec::new();
    for row in &mut turn.assistant_rows {
        row.stop_reason.clone_from(&turn.stop_reason);
        if update_stop_details {
            row.stop_details.clone_from(&turn.stop_details);
        }
        row.usage.clone_from(&turn.usage);
    }
    sync_pending_stitch_rows(turn, pending);

    if !persist_unlinked_rows {
        return;
    }
    let Some(pump) = pump.as_mut() else {
        return;
    };
    for row in &mut turn.assistant_rows {
        if row.persisted_link.is_some() || row.session_append_dispatched {
            continue;
        }
        if let Some(link) = pump.executor.persist_completed_assistant_row(row).await {
            for block in &row.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    tool_parents.push((id.clone(), link.uuid.clone()));
                }
            }
            row.persisted_link = Some(link);
        }
        row.session_append_dispatched = true;
        rewritten_rows.push((row.row_id, row.content.clone()));
    }
    for (row_id, content) in rewritten_rows {
        replace_stream_blocks_for_row(
            turn,
            assistant_block_indices,
            assistant_block_row_ids,
            row_id,
            &content,
        );
    }
    turn.assistant_tool_parent_uuids.extend(tool_parents);
    sync_pending_stitch_rows(turn, pending);
}

/// Remove blocks tombstoned by a provider fallback event while preserving
/// each completed tool call's original content-block index for exact matching.
fn tombstone_server_fallback_blocks(
    turn: &mut PumpedTurn,
    assistant_block_indices: &mut Vec<(u32, bool)>,
    assistant_block_row_ids: &mut Vec<MessageId>,
    tool_use_row_ids: &mut Vec<MessageId>,
    current_row_by_api_index: &mut HashMap<u32, MessageId>,
    discarded_row_ids: &[MessageId],
) -> bool {
    let discarded_ids: HashSet<MessageId> = discarded_row_ids.iter().copied().collect();
    let discarded_tools = tool_ids_for_row_ids(turn, discarded_row_ids);
    let discarded_tool_ids: HashSet<ToolUseId> = discarded_tools.iter().cloned().collect();

    let mut kept_blocks = Vec::new();
    let mut kept_indices = Vec::new();
    let mut kept_row_ids = Vec::new();
    for ((block, index), row_id) in std::mem::take(&mut turn.assistant_blocks)
        .into_iter()
        .zip(std::mem::take(assistant_block_indices))
        .zip(std::mem::take(assistant_block_row_ids))
    {
        if !discarded_ids.contains(&row_id) {
            kept_blocks.push(block);
            kept_indices.push(index);
            kept_row_ids.push(row_id);
        }
    }
    turn.assistant_blocks = kept_blocks;
    *assistant_block_indices = kept_indices;
    *assistant_block_row_ids = kept_row_ids;
    turn.assistant_rows
        .retain(|row| !discarded_ids.contains(&row.row_id));

    let mut kept_tools = Vec::new();
    let mut kept_tool_row_ids = Vec::new();
    for (tool, row_id) in std::mem::take(&mut turn.tool_uses)
        .into_iter()
        .zip(std::mem::take(tool_use_row_ids))
    {
        if !discarded_ids.contains(&row_id) {
            kept_tools.push(tool);
            kept_tool_row_ids.push(row_id);
        }
    }
    turn.tool_uses = kept_tools;
    *tool_use_row_ids = kept_tool_row_ids;
    turn.assistant_tool_parent_uuids
        .retain(|tool_id, _| !discarded_tool_ids.contains(tool_id));
    current_row_by_api_index.retain(|_, row_id| !discarded_ids.contains(row_id));
    !discarded_tools.is_empty()
}

fn projected_response_model(metadata: &Value) -> Option<&str> {
    metadata.get("llm_client")?.get("response_model")?.as_str()
}

/// Apply the host projector's terminal response-model observation after an
/// earlier admitted fallback event. The reserved metadata key is cleared at
/// the llm-runtime boundary, and requiring a preceding typed fallback event
/// keeps ordinary turns and provider-supplied metadata from changing session
/// identity.
fn update_served_model_from_projection(turn: &mut PumpedTurn, metadata: &Value) {
    let Some(previous_fallback) = turn.server_fallback_events.last() else {
        return;
    };
    let Some(received_model) = projected_response_model(metadata) else {
        return;
    };
    turn.served_model = Some(
        lingxi_core::host::refusal_server_control::resolve_received_model(
            Some(&previous_fallback.lane.model),
            received_model,
        ),
    );
}

fn observation_block(event: &HistoryEvent) -> Option<(u32, &Value)> {
    match event {
        HistoryEvent::ContentBlockStart {
            index,
            content_block: llm_runtime::ContentBlock::ProviderContent { value, .. },
        } if (*index & 0x8000_0000) != 0
            && value["type"] == "lingxi_observation"
            && value["metadata"]["llm_client"]["response_model"]
                .as_str()
                .is_some() =>
        {
            Some((*index, value))
        }
        _ => None,
    }
}

fn creates_assistant_row(content_block: &llm_runtime::ContentBlock) -> bool {
    !matches!(
        content_block,
        llm_runtime::ContentBlock::Image { .. }
            | llm_runtime::ContentBlock::ImageUrl { .. }
            | llm_runtime::ContentBlock::Document { .. }
            | llm_runtime::ContentBlock::ToolResult { .. }
            | llm_runtime::ContentBlock::CacheEdits { .. }
    )
}

/// Consume the given stream to completion, routing events through the
/// accumulator + output sink. Returns the per-block summary; the
/// streaming-loop caller is responsible for tool dispatch + appending
/// to session history.
/// This wrapper drops a tail after MessageStop; Mod-aware callers use the
/// remaining-stream variant to resume later records.
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
    pub(crate) progress: llm_runtime::model::stream_recovery::Progress,
    pub(crate) partial_close: PartialStreamClose,
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
    /// Distinguishes a controller-declined server hop from a stream failure so
    /// callers can skip retries and partial-response salvage.
    pub(crate) disposition: PumpFailureDisposition,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PumpFailureDisposition {
    #[default]
    Stream,
    ServerFallbackDeclined,
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
    pub(crate) fn notice(self, has_output: bool) -> &'static str {
        if has_output {
            return self.incomplete_notice();
        }
        match self {
            Self::Watchdog => {
                "API Error: The response stalled before a response was produced. Try again."
            }
            Self::StreamSuspended => {
                "API Error: Your computer went to sleep before a response was produced. Try again."
            }
            _ => "API Error: Connection lost before a response was produced. Try again.",
        }
    }
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
        OrchestratorError::Streaming(
            LlmError::Overloaded { .. }
            | LlmError::ProviderInternal
            | LlmError::ProviderTimeout { .. },
        ) => Some(PartialFinalizeCause::ServerError),
        OrchestratorError::Streaming(
            LlmError::Transport { .. } | LlmError::TransportTimeout { .. },
        ) => Some(PartialFinalizeCause::StaleConnection),
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

/// Native `Ji && !UR`: a completed local tool call owns its result sequence.
pub(crate) fn partial_is_text_continuable(turn: &PumpedTurn) -> bool {
    partial_has_output(turn) && turn.tool_uses.is_empty()
}

pub(crate) fn stream_failure_cause(
    error: &OrchestratorError,
) -> Option<llm_runtime::model::stream_recovery::Cause> {
    use llm_runtime::model::stream_recovery::Cause;
    match error {
        OrchestratorError::Streaming(LlmError::Overloaded { .. }) => Some(Cause::Overloaded),
        OrchestratorError::Streaming(LlmError::ProviderTimeout { .. }) => Some(Cause::TimedOut),
        OrchestratorError::Streaming(LlmError::ProviderInternal) => Some(Cause::ServerError),
        OrchestratorError::Streaming(
            LlmError::Transport { .. } | LlmError::TransportTimeout { .. },
        ) => Some(Cause::ConnectionLost),
        OrchestratorError::Streaming(e)
            if llm_runtime::model::stream_watchdog::is_stream_idle_timeout(e) =>
        {
            Some(Cause::Stalled)
        }
        OrchestratorError::Streaming(e)
            if llm_runtime::model::stream_watchdog::is_stream_suspended(e) =>
        {
            Some(Cause::ConnectionLost)
        }
        OrchestratorError::StreamEndedWithoutStop => Some(Cause::Truncated),
        _ => None,
    }
}

/// Attach the accumulated partial + a usage seed to a terminal pump error.
fn build_failure(
    mut partial: PumpedTurn,
    accumulator: &BlockAccumulator,
    message_start_usage: &Option<LlmUsage>,
    error: OrchestratorError,
    real_content_started: bool,
    partial_close: PartialStreamClose,
    any_event: bool,
) -> PumpFailure {
    use llm_runtime::model::stream_recovery::Progress;
    let completed = !partial.assistant_blocks.is_empty() || !partial.tool_uses.is_empty();
    let progress = match (completed, real_content_started, any_event) {
        (true, true, _) => Progress::Output,
        (true, false, _) => Progress::ThinkingOnly,
        (false, true, _) => Progress::PartialOutput,
        (false, false, true) => Progress::Started,
        _ => Progress::Nothing,
    };
    // A network close can land between the final text delta and the block-stop
    // frame. Those text bytes have already reached every live output sink, so
    // preserve them in history and let the existing partial-finalize path render
    // its stable interruption notice. Provider errors (not transport closes)
    // retain their existing retry/fallback semantics for incomplete blocks.
    if matches!(
        error,
        OrchestratorError::Streaming(LlmError::Transport { .. })
            | OrchestratorError::Streaming(LlmError::TransportTimeout { .. })
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
        progress,
        partial_close,
        partial,
        disposition: PumpFailureDisposition::Stream,
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
        .map(|(turn, _remaining)| turn)
        .map_err(|f| f.error)
}

/// Context handed to [`pump_stream_with_executor_tracked`] so that, as each
/// `tool_use` block's `content_block_stop` arrives mid-stream, its tool is
/// registered with (and dispatched into) the [`StreamingToolExecutor`] —
/// faithful to claude-code `query.ts:837-844`, where `addTool` runs INSIDE
/// the live stream loop.
///
/// The main-query driver supplies a settlement state. With it, each ready
/// tool result is accepted and entered into the query/event buffers after a
/// stream event; JSONL flushing remains a separate cursor operation so an
/// incomplete assistant row can hold the writer tail.
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
    /// Frozen request messages from before the current assistant row. Late
    /// queued tool starts must use this same query snapshot.
    pub(crate) query_history: Vec<ConversationMessage>,
    /// Profile captured with the API request route for per-row persistence.
    pub(crate) model_profile: Option<String>,
    /// Native `Je` source gate for persisted `supersedesUuids`. The bare pump
    /// has no query-source authority; the conversation driver derives this
    /// from the captured host query source.
    pub(crate) record_supersedes: bool,
    /// The turn's USER-interrupt token (ESC / new message). The owned actor
    /// applies interrupt behavior per resolved tool: Cancel calls become
    /// synthetics, while Block calls keep running.
    pub(crate) user_cancel: Option<&'a CancellationToken>,
    /// P2-04 (MessageDisplay `displayContent`): `true` when a `MessageDisplay`
    /// hook is registered for this turn, in which case live per-token
    /// `text_delta` emission is suppressed in [`dispatch_event`] so the
    /// orchestrator's completed-message pass renders the full (possibly
    /// hook-substituted) text exactly once. `false` ⇒ byte-identical live
    /// streaming (the no-hook common case).
    pub(crate) suppress_live_text: bool,
    /// `turn.step` presents hook-yielded thinking separately while the pump
    /// retains the provider-signed original in history.
    pub(crate) suppress_live_thinking: bool,
    /// Live main-query path only. Ready tool results are accepted into the
    /// query result sequence and event journal after each provider event; a
    /// plain pump leaves this absent and continues to buffer until its caller's
    /// final drain.
    pub(crate) settlement: Option<&'a mut StreamToolSettlement>,
}

async fn poll_ready_stream_tool_results(pump: &mut Option<ExecutorPump<'_, '_>>) {
    let Some(pump) = pump.as_mut() else {
        return;
    };
    if pump.settlement.is_none() {
        // The plain/test pump retains the historical buffer-until-final-drain
        // contract; only the main query wires Native Tn settlement.
        return;
    }

    loop {
        let ready_count = pump.executor.drain_ready().await;
        let results = pump.executor.take_newly_completed();
        let had_results = !results.is_empty();
        if !results.is_empty() {
            let settlement = pump
                .settlement
                .as_deref_mut()
                .expect("settlement was checked above");
            let empty_parents = HashMap::new();
            pump.executor
                .orchestrator()
                .settle_stream_tool_results(settlement, results, &empty_parents, &None)
                .await;
        }
        if ready_count == 0 && !had_results {
            break;
        }
    }
}

/// Like [`pump_stream`], but registers a [`StreamingToolExecutor`] DURING the
/// stream: each arriving `tool_use` block is admitted at its
/// `content_block_stop`, and the owned actor starts eligible W1 tasks without
/// waiting for the next provider event or Tn poll. Mirrors claude-code
/// `query.ts:659/837-844`.
///
/// With a main-query settlement state, ready results are emitted and added to
/// query-local `je` after each stream event. The event-order writer journal is
/// flushed separately after assistant terminal fields are known; the final
/// driver drain waits for only the remaining tools.
/// Return the stream tail after one assistant response. A Mod may yield more
/// than one response from one `turn.step` dispatch; the driver can resume the
/// same worker stream after it finishes this response's tools and hooks.
pub(crate) async fn pump_stream_with_executor_tracked_remaining(
    stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
    output: &Arc<dyn OutputStream>,
    pump: ExecutorPump<'_, '_>,
) -> Result<
    (
        PumpedTurn,
        BoxStream<'static, Result<HistoryEvent, LlmError>>,
    ),
    PumpFailure,
> {
    pump_stream_inner(stream, output, Some(pump)).await
}

async fn pump_stream_inner(
    mut stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
    output: &Arc<dyn OutputStream>,
    mut pump: Option<ExecutorPump<'_, '_>>,
) -> Result<
    (
        PumpedTurn,
        BoxStream<'static, Result<HistoryEvent, LlmError>>,
    ),
    PumpFailure,
> {
    let mut acc = BlockAccumulator::new();
    // P2-04: suppress live per-token text emission while a `MessageDisplay` hook
    // is registered (only the executor-driven pump carries the flag; the plain
    // `pump_stream` test helper defaults to `false` = live streaming).
    let mut suppress_live_text = pump.as_ref().map(|p| p.suppress_live_text).unwrap_or(false);
    let suppress_live_thinking = pump
        .as_ref()
        .map(|p| p.suppress_live_thinking)
        .unwrap_or(false);
    let mut turn = PumpedTurn::default();
    let mut assistant_block_indices: Vec<(u32, bool)> = Vec::new();
    let mut assistant_block_row_ids: Vec<MessageId> = Vec::new();
    let mut tool_use_row_ids: Vec<MessageId> = Vec::new();
    let mut current_row_by_api_index: HashMap<u32, MessageId> = HashMap::new();
    let mut open_block_keys: HashMap<u32, u64> = HashMap::new();
    let mut next_block_key = 0u64;
    let mut observation_block_indices = HashSet::new();
    let mut provider_message_id = String::new();
    let mut assistant_row_model = String::new();
    let mut assistant_row_profile = pump.as_ref().and_then(|p| p.model_profile.clone());
    let mut next_row_order = 0u64;
    let mut pending_server_stitch: Option<PendingServerStitch> = None;
    // Mirrors the binary's `Hr`: set true the moment a non-thinking content
    // block STARTS (text / tool_use / etc.). Gates the caller's mid-stream
    // transient retry — see [`PumpFailure`].
    let mut real_content_started = false;
    // Capture the MessageStart usage as the fallback billing source for
    // input tokens, in case MessageDelta carries no usage (rare). The
    // MessageDelta usage supersedes this when present.
    let mut message_start_usage: Option<LlmUsage> = None;
    let mut partial_close = PartialStreamClose::default();
    let mut completion = StreamCompletionState::default();
    let mut any_event = false;

    loop {
        // Owned dispatches continue independently while the provider waits.
        // Completion advances the scheduler immediately; results become
        // query-visible only at the post-event Tn poll below.
        let Some(item) = stream.next().await else {
            poll_ready_stream_tool_results(&mut pump).await;
            break;
        };
        let mut event = match item {
            Ok(ev) => ev,
            Err(e) => {
                poll_ready_stream_tool_results(&mut pump).await;
                if completion.message_started
                    && completion.is_complete(&turn)
                    && complete_response_error(&e, &turn)
                    && !pump
                        .as_ref()
                        .is_some_and(|p| p.user_cancel.is_some_and(CancellationToken::is_cancelled))
                {
                    finish_pending_server_stitch(
                        &mut turn,
                        output,
                        &mut assistant_block_indices,
                        &mut assistant_block_row_ids,
                        &mut current_row_by_api_index,
                        &mut pending_server_stitch,
                        &mut pump,
                    )
                    .await;
                    partial_close.close(output.as_ref()).await;
                    return Ok((turn, stream));
                }
                return Err(build_failure(
                    turn,
                    &acc,
                    &message_start_usage,
                    OrchestratorError::Streaming(e),
                    real_content_started,
                    partial_close,
                    any_event,
                ));
            }
        };
        any_event = true;
        if let Some((index, value)) = observation_block(&event) {
            observation_block_indices.insert(index);
            if let Some(metadata) = value.get("metadata") {
                update_served_model_from_projection(&mut turn, metadata);
                if let Some(model) = turn.served_model.as_ref() {
                    assistant_row_model.clone_from(model);
                }
            }
            // `lingxi_observation` carries host metadata through the existing
            // content-block seam when a stream has no usage report. It is not
            // assistant content and must not be emitted or persisted.
            poll_ready_stream_tool_results(&mut pump).await;
            continue;
        }
        if let HistoryEvent::ContentBlockStop { index } = &event {
            if observation_block_indices.remove(index) {
                poll_ready_stream_tool_results(&mut pump).await;
                continue;
            }
        }
        if let HistoryEvent::ResponseObserved { model, response_id } = &event {
            // This is the fallback controller's model/id observation, not a
            // fresh physical API-attempt boundary. Keep the current attempt's
            // sticky stop facts; each outer pump already starts with fresh
            // stop state, while SDK-internal retry boundaries are not exposed
            // as a distinct history event here.
            assistant_row_model.clone_from(model);
            if let Some(response_id) = response_id {
                provider_message_id.clone_from(response_id);
            }
            turn.served_model = Some(model.clone());
            poll_ready_stream_tool_results(&mut pump).await;
            continue;
        }
        if let HistoryEvent::CostQuoteObserved {
            estimate,
            native_server_fallback,
            summary_model,
        } = &event
        {
            turn.cost_quote_observed = true;
            turn.native_server_fallback_quote = *native_server_fallback;
            turn.cost_quote.clone_from(estimate);
            if *native_server_fallback {
                turn.native_cost_model.clone_from(summary_model);
            } else {
                turn.native_cost_model = None;
            }
            poll_ready_stream_tool_results(&mut pump).await;
            continue;
        }
        if let HistoryEvent::ServerFallback {
            event,
            profile,
            lane,
            ..
        } = &event
        {
            let observation = llm_runtime::history::HistoryServerFallback {
                event: (**event).clone(),
                profile: profile.clone(),
                lane: lane.clone(),
            };
            let user_visible = matches!(event.reason.as_str(), "refusal" | "sticky");
            if !user_visible {
                turn.server_fallback_events.push(observation);
                poll_ready_stream_tool_results(&mut pump).await;
                continue;
            }
            let discarded_row_ids =
                rows_for_api_indices(&current_row_by_api_index, &event.discarded_blocks);
            let discarded_tool_use_ids = tool_ids_for_row_ids(&turn, &discarded_row_ids);
            let discarded_tool = !discarded_tool_use_ids.is_empty();
            let admission = match pump.as_mut() {
                Some(p) => Some(
                    p.executor
                        .observe_server_fallback(&observation, discarded_tool)
                        .await,
                ),
                None => None,
            };
            match admission {
                Some(Ok(crate::server_fallback::ServerFallbackAdmission::Applied)) => {
                    if user_visible {
                        // Native aborts discarded in-flight tool work before
                        // awaiting any client output event. Keep the completed
                        // rows available below for their full tombstone DTOs.
                        if discarded_tool {
                            if let Some(p) = pump.as_mut() {
                                let removal = match p.executor.reset_after_server_fallback_owned(Some(
                                    lingxi_core::host::tool_use_lifecycle::ToolUseRemovalReason::FallbackSweep,
                                )).await {
                                    Ok(removal) => removal,
                                    Err(error) => return Err(build_failure(
                                        turn,
                                        &acc,
                                        &message_start_usage,
                                        error,
                                        real_content_started,
                                        partial_close,
                                        any_event,
                                    )),
                                };
                                if let Some(settlement) = p.settlement.as_deref_mut() {
                                    settlement.publication_guard =
                                        Some(Arc::new(p.executor.publication_fence()));
                                }
                                if !removal.ids.is_empty() {
                                    turn.tool_use_removals.push(removal);
                                }
                            }
                        }
                        assistant_row_profile = Some(profile.clone());
                        let visible_model =
                            lingxi_core::host::refusal_server_control::resolve_received_model(
                                Some(&lane.model),
                                &event.to_model,
                            );
                        assistant_row_model.clone_from(&visible_model);
                        turn.served_model = Some(visible_model.clone());
                        let active_row_ids: HashSet<MessageId> =
                            current_row_by_api_index.values().copied().collect();
                        for row in &mut turn.assistant_rows {
                            if active_row_ids.contains(&row.row_id) {
                                row.model.clone_from(&visible_model);
                                row.model_profile = Some(profile.clone());
                            }
                        }
                        if let Some(stitch) = pending_server_stitch.as_mut() {
                            for row in &mut stitch.retained_rows {
                                if active_row_ids.contains(&row.row_id) {
                                    row.model.clone_from(&visible_model);
                                    row.model_profile = Some(profile.clone());
                                }
                            }
                        }

                        output
                            .emit_server_fallback_query_model_change(&visible_model)
                            .await;
                        for row_id in &discarded_row_ids {
                            if let Some(row) =
                                turn.assistant_rows.iter().find(|row| row.row_id == *row_id)
                            {
                                let tombstone = tombstone_message_for_row(row);
                                output
                                    .emit_server_fallback_tombstone(&tombstone, true)
                                    .await;
                            }
                        }
                        let discarded_links =
                            persisted_links_for_row_ids(&turn, &discarded_row_ids);
                        if let Some(p) = pump.as_mut() {
                            p.executor
                                .remove_assistant_stream_rows(&discarded_links)
                                .await;
                        }

                        let retained_row_ids =
                            rows_for_api_indices(&current_row_by_api_index, &event.retained_blocks);

                        let tombstoned_tool = tombstone_server_fallback_blocks(
                            &mut turn,
                            &mut assistant_block_indices,
                            &mut assistant_block_row_ids,
                            &mut tool_use_row_ids,
                            &mut current_row_by_api_index,
                            &discarded_row_ids,
                        );
                        debug_assert_eq!(discarded_tool, tombstoned_tool);

                        // Native `X.tombstonedToolUse` resets the complete
                        // query-local `je` accumulator and tombstones every
                        // previously yielded result row. Without that flag,
                        // completed results remain part of this query.
                        if tombstoned_tool {
                            let result_tombstones = pump
                                .as_mut()
                                .and_then(|p| p.settlement.as_deref_mut())
                                .map(StreamToolSettlement::discard_tool_completion_rows)
                                .unwrap_or_default();
                            if let Some(p) = pump.as_ref() {
                                clear_tombstoned_tool_result_metadata(
                                    p.executor.orchestrator(),
                                    &result_tombstones,
                                )
                                .await;
                            }
                            for tombstone in &result_tombstones {
                                output.emit_server_fallback_tombstone(tombstone, true).await;
                            }
                        }

                        // A non-empty mid-stream seed replaces pending J.
                        let can_start_stitch = event.mid_stream && !event.retained_text.is_empty();
                        if can_start_stitch {
                            pending_server_stitch = Some(PendingServerStitch {
                                retained_text: event.retained_text.clone(),
                                retained_rows: rows_for_ids(&turn, &retained_row_ids),
                                retained_row_ids,
                                after_row_order: next_row_order,
                            });
                        }
                        // Native emits begin for the current pending J after
                        // every accepted visible hop, including a hop that has
                        // no new seed and therefore reuses the prior J.
                        if let Some(stitch) = pending_server_stitch.as_ref() {
                            output
                                .emit_refusal_continuation_begin(
                                    &stitch.retained_text,
                                    &stitch.retained_row_ids,
                                    !pump.as_ref().is_some_and(|p| p.suppress_live_text),
                                )
                                .await;
                        }
                    }
                    turn.handled_server_fallback_events += 1;
                }
                Some(Ok(crate::server_fallback::ServerFallbackAdmission::Declined)) => {
                    let mut declined = PumpedTurn::default();
                    let declined_model =
                        lingxi_core::host::refusal_server_control::resolve_received_model(
                            Some(&lane.model),
                            &event.to_model,
                        );
                    let mut declined_row_ids =
                        rows_for_api_indices(&current_row_by_api_index, &event.discarded_blocks);
                    declined_row_ids.extend(
                        turn.assistant_rows
                            .iter()
                            .filter(|row| row.model == declined_model)
                            .map(|row| row.row_id),
                    );
                    let mut seen_declined_rows = HashSet::new();
                    declined_row_ids.retain(|row_id| seen_declined_rows.insert(*row_id));
                    let declined_rows = rows_for_ids(&turn, &declined_row_ids);
                    let declined_links = persisted_links_for_row_ids(&turn, &declined_row_ids);
                    let _ = tombstone_server_fallback_blocks(
                        &mut turn,
                        &mut assistant_block_indices,
                        &mut assistant_block_row_ids,
                        &mut tool_use_row_ids,
                        &mut current_row_by_api_index,
                        &declined_row_ids,
                    );
                    let result_tombstones = if let Some(p) = pump.as_mut() {
                        // Cancel/drop active work and clear je before any
                        // asynchronous tombstone/output work can yield.
                        let removal = match p.executor.reset_after_server_fallback_owned(None).await
                        {
                            Ok(removal) => removal,
                            Err(error) => {
                                return Err(build_failure(
                                    turn,
                                    &acc,
                                    &message_start_usage,
                                    error,
                                    real_content_started,
                                    partial_close,
                                    any_event,
                                ));
                            }
                        };
                        if let Some(settlement) = p.settlement.as_deref_mut() {
                            settlement.publication_guard =
                                Some(Arc::new(p.executor.publication_fence()));
                        }
                        if !removal.ids.is_empty() {
                            declined.tool_use_removals.push(removal);
                        }
                        let tombstones = p
                            .settlement
                            .as_deref_mut()
                            .map(StreamToolSettlement::discard_tool_completion_rows)
                            .unwrap_or_default();
                        p.executor
                            .remove_assistant_stream_rows(&declined_links)
                            .await;
                        Some(tombstones)
                    } else {
                        None
                    };
                    if let (Some(p), Some(tombstones)) = (pump.as_ref(), result_tombstones.as_ref())
                    {
                        clear_tombstoned_tool_result_metadata(
                            p.executor.orchestrator(),
                            tombstones,
                        )
                        .await;
                    }
                    for row in &declined_rows {
                        output
                            .emit_server_fallback_tombstone(&tombstone_message_for_row(row), false)
                            .await;
                    }
                    if let Some(tombstones) = result_tombstones {
                        for tombstone in &tombstones {
                            output
                                .emit_server_fallback_tombstone(tombstone, false)
                                .await;
                        }
                    }
                    if let Some(p) = pump.as_mut() {
                        partial_close.close(output.as_ref()).await;
                        output.emit_message_retracted(&p.assistant_id).await;
                    }
                    declined.server_fallback_events.push(observation);
                    declined.usage = turn.usage.clone().or_else(|| message_start_usage.clone());
                    declined.output_tokens = turn.output_tokens;
                    declined.cost_quote = turn.cost_quote.clone();
                    declined.cost_quote_observed = turn.cost_quote_observed;
                    declined.native_server_fallback_quote = turn.native_server_fallback_quote;
                    declined
                        .native_cost_model
                        .clone_from(&turn.native_cost_model);
                    return Err(PumpFailure {
                        error: OrchestratorError::Internal(
                            "server fallback was not admitted by the conversation controller"
                                .into(),
                        ),
                        real_content_started,
                        progress: llm_runtime::model::stream_recovery::Progress::Nothing,
                        partial_close,
                        partial: declined,
                        disposition: PumpFailureDisposition::ServerFallbackDeclined,
                    });
                }
                Some(Err(error)) => {
                    poll_ready_stream_tool_results(&mut pump).await;
                    return Err(build_failure(
                        turn,
                        &acc,
                        &message_start_usage,
                        error,
                        real_content_started,
                        partial_close,
                        any_event,
                    ));
                }
                None => {}
            }
            if user_visible {
                turn.served_model = Some(
                    lingxi_core::host::refusal_server_control::resolve_received_model(
                        Some(&lane.model),
                        &event.to_model,
                    ),
                );
                if let Some(model) = turn.served_model.as_ref() {
                    assistant_row_model.clone_from(model);
                }
            }
            turn.server_fallback_events.push(observation);
        }
        if let HistoryEvent::MessageStart { response } = &event {
            provider_message_id.clone_from(&response.id);
            assistant_row_model.clone_from(&response.model);
        }
        if let HistoryEvent::MessageDelta {
            usage: Some(usage), ..
        } = &mut event
        {
            if let Some(metadata) = usage.provider_metadata.get("stream") {
                update_served_model_from_projection(&mut turn, metadata);
                if let Some(model) = turn.served_model.as_ref() {
                    assistant_row_model.clone_from(model);
                }
            }
            if let Some(quote) = usage.cost_estimate.take() {
                turn.cost_quote = Some(quote);
            }
        }
        // Capture MessageStart usage before dispatching (dispatch consumes the event).
        if let HistoryEvent::MessageStart { ref response } = event {
            message_start_usage = Some(response.usage.clone());
            turn.per_turn_effort = response.per_turn_effort().map(str::to_owned);
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
        if let HistoryEvent::ContentBlockStart {
            index,
            content_block,
        } = &event
        {
            if creates_assistant_row(content_block) {
                let block_key = next_block_key;
                next_block_key = next_block_key.saturating_add(1);
                open_block_keys.insert(*index, block_key);
                output.emit_assistant_block_start(block_key).await;
            }
        }
        if output.wants_partial_stream_events() {
            partial_close.observe(&event);
        }
        completion.observe(&event);
        let action = match dispatch_event(
            event,
            &mut acc,
            output,
            suppress_live_text,
            suppress_live_thinking,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                poll_ready_stream_tool_results(&mut pump).await;
                return Err(build_failure(
                    turn,
                    &acc,
                    &message_start_usage,
                    OrchestratorError::StreamingProtocol(e.to_string()),
                    real_content_started,
                    partial_close,
                    any_event,
                ));
            }
        };
        let completed_block_key =
            completed_block_index.and_then(|index| open_block_keys.remove(&index));
        match action {
            RouterAction::Continue => {}
            RouterAction::AppendAssistantBlock(block) => {
                if let ContentBlock::RedactedThinking { data } = &block {
                    output.emit_redacted_thinking(data).await;
                }
                let api_block_index =
                    completed_block_index.expect("completed block action has an index");
                let key = llm_runtime::stream_content_order(api_block_index);
                let position = assistant_block_indices.partition_point(|existing| existing <= &key);
                assistant_block_indices.insert(position, key);
                let row_id = MessageId::new();
                if let Some(block_key) = completed_block_key {
                    output
                        .emit_assistant_block_identity(block_key, &row_id)
                        .await;
                }
                turn.assistant_blocks.insert(position, block.clone());
                assistant_block_row_ids.insert(position, row_id);
                let row = CompletedAssistantRow {
                    per_turn_effort: turn.per_turn_effort.clone(),
                    stream_order: next_row_order,
                    row_id,
                    provider_message_id: provider_message_id.clone(),
                    model: assistant_row_model.clone(),
                    model_profile: assistant_row_profile.clone(),
                    is_api_error: false,
                    stop_reason: None,
                    stop_details: None,
                    usage: None,
                    request_id: pump
                        .as_ref()
                        .and_then(|pump| pump.executor.last_request_id()),
                    timestamp: assistant_row_timestamp(),
                    persisted_link: None,
                    content: vec![block],
                    session_append_dispatched: false,
                    supersedes_row_ids: Vec::new(),
                };
                turn.assistant_rows.push(row);
                turn.assistant_row_identity = Some(row_id);
                current_row_by_api_index.insert(api_block_index, row_id);
                next_row_order = next_row_order.saturating_add(1);
                if turn.assistant_rows.last().is_some_and(|row| {
                    row.content.iter().any(|block| {
                        block
                            .visible_text()
                            .is_some_and(|text| !text.trim().is_empty())
                    })
                }) {
                    finish_pending_server_stitch(
                        &mut turn,
                        output,
                        &mut assistant_block_indices,
                        &mut assistant_block_row_ids,
                        &mut current_row_by_api_index,
                        &mut pending_server_stitch,
                        &mut pump,
                    )
                    .await;
                    if pending_server_stitch.is_none()
                        && !pump.as_ref().is_some_and(|p| p.suppress_live_text)
                    {
                        suppress_live_text = false;
                    }
                }
                if let Some(p) = pump.as_mut() {
                    let accepted_content = if let Some(row) = turn
                        .assistant_rows
                        .iter_mut()
                        .find(|row| row.row_id == row_id)
                    {
                        p.executor.append_assistant_row(row).await;
                        Some(row.content.clone())
                    } else {
                        None
                    };
                    if let Some(content) = accepted_content {
                        update_accepted_stream_blocks_for_row(
                            &mut turn,
                            &mut assistant_block_indices,
                            &mut assistant_block_row_ids,
                            row_id,
                            &content,
                            Some(key),
                        );
                    }
                    if let Some(settlement) = p.settlement.as_deref_mut() {
                        settlement.record_assistant_row(row_id);
                    }
                }
            }
            RouterAction::DispatchToolUse {
                id,
                name,
                input,
                input_projection,
                provider_id,
            } => {
                let api_block_index =
                    completed_block_index.expect("completed tool action has an index");
                let block = ContentBlock::ToolUse {
                    input_projection,
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                };
                let row_id = MessageId::new();
                let mut row = CompletedAssistantRow {
                    per_turn_effort: turn.per_turn_effort.clone(),
                    stream_order: next_row_order,
                    row_id,
                    provider_message_id: provider_message_id.clone(),
                    model: assistant_row_model.clone(),
                    model_profile: assistant_row_profile.clone(),
                    is_api_error: false,
                    stop_reason: None,
                    stop_details: None,
                    usage: None,
                    request_id: pump
                        .as_ref()
                        .and_then(|pump| pump.executor.last_request_id()),
                    timestamp: assistant_row_timestamp(),
                    persisted_link: None,
                    content: vec![block],
                    session_append_dispatched: false,
                    supersedes_row_ids: Vec::new(),
                };
                // Native awaits append-through before yielding the accepted
                // row. Its source ToolUse still supplies immutable dispatch
                // identity/input while accepted Text is shared with query K.
                if let Some(p) = pump.as_mut() {
                    p.executor.append_assistant_row(&mut row).await;
                    update_accepted_stream_blocks_for_row(
                        &mut turn,
                        &mut assistant_block_indices,
                        &mut assistant_block_row_ids,
                        row_id,
                        &row.content,
                        Some(llm_runtime::stream_content_order(api_block_index)),
                    );
                    if let Some(settlement) = p.settlement.as_deref_mut() {
                        settlement.record_assistant_row(row_id);
                    }
                }
                if let Some(block_key) = completed_block_key {
                    output
                        .emit_assistant_block_identity(block_key, &row_id)
                        .await;
                }
                // Register the completed source row at content_block_stop.
                // Known tools are eagerly dispatched by the owned scheduler;
                // query-visible results still wait for the event-boundary Tn.
                if let Some(p) = pump.as_mut() {
                    let dispatch_facts = crate::turn_loop::ToolUseDispatchFacts {
                        query_history: p.query_history.clone(),
                        assistant_message: ConversationMessage::Assistant {
                            per_turn_effort: turn.per_turn_effort.clone(),
                            id: row.row_id,
                            content: row.content.clone(),
                            stop_reason: None,
                        },
                        // Each live ContentBlockStop is accepted as its own
                        // assistant row and therefore its own assistantMessage
                        // object. Native only supplies prior siblings from the
                        // same assistantMessage; a recovered/full response row
                        // builds that list in the driver's full-row path.
                        same_turn_tool_uses: Vec::new(),
                    };
                    if let Err(error) = p
                        .executor
                        .add_tool_with_context_owned(
                            id.clone(),
                            name.clone(),
                            input.clone(),
                            provider_id.clone(),
                            row_id,
                            dispatch_facts,
                        )
                        .await
                    {
                        poll_ready_stream_tool_results(&mut pump).await;
                        return Err(build_failure(
                            turn,
                            &acc,
                            &message_start_usage,
                            error,
                            real_content_started,
                            partial_close,
                            any_event,
                        ));
                    }
                    // The owned actor begins W1 and promotes its queue without
                    // waiting for another provider event. Query/journal results
                    // remain hidden until the ordinary post-event Tn below.
                }
                turn.tool_uses.push(ObservedToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                });
                tool_use_row_ids.push(row_id);
                turn.assistant_rows.push(row);
                turn.assistant_row_identity = Some(row_id);
                current_row_by_api_index.insert(api_block_index, row_id);
                next_row_order = next_row_order.saturating_add(1);
            }
            RouterAction::RecordStopReason {
                stop_reason,
                output_tokens,
                usage,
                stop_details,
            } => {
                turn.stop_reason = Some(stop_reason);
                turn.stop_details = stop_details;
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
                // The main query's event journal writes in source order after
                // the response terminalizes. A plain executor pump retains the
                // preexisting direct row-writer behavior.
                let persist_unlinked_rows =
                    pump.as_ref().is_some_and(|pump| pump.settlement.is_none());
                sync_message_delta_to_rows(
                    &mut turn,
                    &mut assistant_block_indices,
                    &mut assistant_block_row_ids,
                    &mut pump,
                    &mut pending_server_stitch,
                    true,
                    persist_unlinked_rows,
                )
                .await;
            }
            RouterAction::RecordUsage {
                output_tokens,
                usage,
                stop_details,
            } => {
                // Usage-only delta (no stop_reason yet): keep the latest count.
                let update_stop_details = turn.stop_reason.is_none();
                if update_stop_details {
                    turn.stop_details = stop_details;
                }
                turn.output_tokens = output_tokens;
                turn.usage = match (usage.as_ref(), message_start_usage.as_ref()) {
                    (Some(delta), Some(seed)) => Some(merge_usage(seed, delta)),
                    (Some(delta), None) => Some(delta.clone()),
                    (None, seed) => seed.cloned(),
                };
                sync_message_delta_to_rows(
                    &mut turn,
                    &mut assistant_block_indices,
                    &mut assistant_block_row_ids,
                    &mut pump,
                    &mut pending_server_stitch,
                    update_stop_details,
                    false,
                )
                .await;
            }
            RouterAction::EndOfStream => {
                finish_pending_server_stitch(
                    &mut turn,
                    output,
                    &mut assistant_block_indices,
                    &mut assistant_block_row_ids,
                    &mut current_row_by_api_index,
                    &mut pending_server_stitch,
                    &mut pump,
                )
                .await;
                poll_ready_stream_tool_results(&mut pump).await;
                return Ok((turn, stream));
            }
        }
        poll_ready_stream_tool_results(&mut pump).await;
    }
    // A terminal message_delta completes the response even when the final
    // message_stop is lost. Later block events invalidate that terminal state.
    if completion.is_complete_at_eof(&turn)
        && !pump
            .as_ref()
            .is_some_and(|p| p.user_cancel.is_some_and(CancellationToken::is_cancelled))
    {
        finish_pending_server_stitch(
            &mut turn,
            output,
            &mut assistant_block_indices,
            &mut assistant_block_row_ids,
            &mut current_row_by_api_index,
            &mut pending_server_stitch,
            &mut pump,
        )
        .await;
        partial_close.close(output.as_ref()).await;
        return Ok((turn, stream));
    }
    // Stream ended without either a real stop or a complete terminal delta.
    Err(build_failure(
        turn,
        &acc,
        &message_start_usage,
        OrchestratorError::StreamEndedWithoutStop,
        real_content_started,
        partial_close,
        any_event,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_native_partial_notice_and_tool_truncation_flag() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../../llm-runtime/tests/fixtures/stream_recovery_2_1_288.json"
        ))
        .unwrap();
        let cases = oracle["notices"].as_array().unwrap();
        assert_eq!(cases.len(), 128);
        for row in cases {
            let input = &row["input"];
            let mut turn = PumpedTurn::default();
            if input["Ji"] == true {
                turn.assistant_blocks.push(ContentBlock::Text {
                    text: "visible".into(),
                    citations: None,
                });
            }
            if input["UR"] == true {
                turn.tool_uses.push(ObservedToolUse {
                    id: ToolUseId::from("tool-1".to_owned()),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                    provider_id: None,
                });
            }
            let has_output = input["Ji"] == true;
            if !has_output && input["UR"] == true {
                continue;
            } // Supplied producer facts would contradict completed tool output.
            assert_eq!(
                partial_is_text_continuable(&turn),
                row["expected"]["truncatedAfterOutput"] == true
            );
            if input["Bu"] == true {
                continue;
            } // Malformed copy awaits complete stream classification.
            let cause = if input["Wg"] == true {
                PartialFinalizeCause::Watchdog
            } else if has_output && input["Tc"] == true {
                PartialFinalizeCause::ServerError
            } else if input["xSe"] == true {
                PartialFinalizeCause::StreamSuspended
            } else {
                PartialFinalizeCause::StaleConnection
            };
            assert_eq!(
                cause.notice(has_output),
                row["expected"]["content"].as_str().unwrap()
            );
        }
    }
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

    #[test]
    fn completion_state_matches_native_287_terminal_and_eof_predicates() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/partial_stream_close_2_1_287.json"
        ))
        .unwrap();
        let cases = oracle["completionCases"].as_array().unwrap();
        assert_eq!(cases.len(), 24);
        for case in cases {
            let mut state = StreamCompletionState::default();
            let mut turn = PumpedTurn::default();
            let mut real_stop = false;
            for frame in case["events"].as_array().unwrap() {
                let index = frame["index"].as_u64().unwrap_or(0) as u32;
                let event = match frame["type"].as_str().unwrap() {
                    "message_start" => message_start("fixture", "fixture-model"),
                    "content_block_start" => match frame["content_block"]["type"].as_str().unwrap()
                    {
                        "text" => content_block_start_text(index),
                        "thinking" => content_block_start_thinking(index),
                        "tool_use" => content_block_start_tool_use(index, ToolUseId::new(), "Read"),
                        "server_tool_use" => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: llm_runtime::ContentBlock::ServerToolUse {
                                id: "server-1".into(),
                                name: "web_search".into(),
                                input: serde_json::json!({}),
                            },
                        },
                        "mcp_tool_use" => HistoryEvent::ContentBlockStart {
                            index,
                            content_block: llm_runtime::ContentBlock::ProviderContent {
                                protocol: "anthropic_messages".into(),
                                value: frame["content_block"].clone(),
                            },
                        },
                        unknown => panic!("uncovered native completion block {unknown}"),
                    },
                    "content_block_delta" => text_delta(index, "fixture"),
                    "content_block_stop" => {
                        // The bounded oracle counts completed messages without
                        // executing native payload conversion or tool dispatch.
                        turn.assistant_blocks.push(ContentBlock::Text {
                            text: "completed".into(),
                            citations: None,
                        });
                        content_block_stop(index)
                    }
                    "message_delta" => {
                        let stop_reason =
                            frame["delta"]["stop_reason"].as_str().map(str::to_string);
                        if stop_reason.is_some() {
                            turn.stop_reason = stop_reason.clone();
                        }
                        HistoryEvent::MessageDelta {
                            delta: llm_runtime::HistoryMessageDelta {
                                stop_reason,
                                stop_details: None,
                            },
                            usage: None,
                        }
                    }
                    "message_stop" => {
                        real_stop = true;
                        message_stop()
                    }
                    unknown => panic!("uncovered native completion event {unknown}"),
                };
                state.observe(&event);
            }
            let expected = &case["expected"];
            assert_eq!(
                state.message_started,
                expected["messageStarted"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                state.terminal_delta,
                expected["terminalDelta"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::json!(state.open_block),
                expected["openBlock"],
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::json!(turn.stop_reason),
                expected["stopReason"],
                "{}",
                case["name"]
            );
            assert_eq!(
                turn.assistant_blocks.len() as u64,
                expected["completedBlocks"].as_u64().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                state.is_complete(&turn),
                expected["errorAccepted"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                real_stop || state.is_complete_at_eof(&turn),
                expected["cleanAccepted"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn completion_error_eligibility_matches_native_287_classification() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/partial_stream_close_2_1_287.json"
        ))
        .unwrap();
        let cases = oracle["errorEligibilityCases"].as_array().unwrap();
        assert_eq!(cases.len(), 16);
        for case in cases {
            let input = &case["input"];
            let flag = |name: &str| input[name].as_bool().unwrap();
            let error = if flag("denied") {
                LlmError::PermissionDenied {
                    message: "denied".into(),
                }
            } else if flag("isConnectionError") {
                LlmError::Transport {
                    message: "connection closed".into(),
                }
            } else if flag("stalled") {
                llm_runtime::model::stream_watchdog::idle_timeout_error(
                    std::time::Duration::from_millis(1),
                )
            } else if flag("isServerError") {
                LlmError::ProviderInternal
            } else {
                match input["errorClass"].as_str().unwrap() {
                    "overloaded" => LlmError::Overloaded { repeated: false },
                    "xt" => LlmError::InvalidRequest {
                        message: "bad request".into(),
                    },
                    _ => LlmError::StreamInterrupted {
                        message: "malformed or unrelated terminal error".into(),
                    },
                }
            };
            let mut turn = PumpedTurn::default();
            if flag("anyBlockFinished") {
                turn.assistant_blocks.push(if flag("anyOutputShown") {
                    ContentBlock::Text {
                        text: "completed output".into(),
                        citations: None,
                    }
                } else {
                    ContentBlock::Thinking {
                        thinking: "completed reasoning".into(),
                        signature: None,
                    }
                });
            }
            assert_eq!(
                flag("complete") && complete_response_error(&error, &turn),
                case["expected"]["bypassesStreamFailurePolicy"]
                    .as_bool()
                    .unwrap(),
                "{}",
                case["name"],
            );
        }
    }

    #[tokio::test]
    async fn terminal_delta_completion_preserves_payload_and_rejects_unrelated_errors() {
        let errors = [
            (None, true),
            (
                Some(LlmError::Transport {
                    message: "connection closed".into(),
                }),
                true,
            ),
            (Some(LlmError::ProviderInternal), true),
            (Some(LlmError::Overloaded { repeated: false }), true),
            (
                Some(llm_runtime::model::stream_watchdog::idle_timeout_error(
                    std::time::Duration::from_millis(1),
                )),
                true,
            ),
            (
                Some(llm_runtime::model::stream_watchdog::watchdog_abort_error(
                    std::time::Duration::from_millis(1),
                    std::time::Duration::from_secs(3),
                )),
                true,
            ),
            (
                Some(LlmError::Authentication {
                    message: "revoked".into(),
                }),
                false,
            ),
            (
                Some(LlmError::InvalidRequest {
                    message: "invalid request".into(),
                }),
                false,
            ),
            (
                Some(LlmError::StreamInterrupted {
                    message: "malformed stream".into(),
                }),
                false,
            ),
            (
                Some(LlmError::StreamInterrupted {
                    message: "aborted".into(),
                }),
                false,
            ),
        ];
        for (error, accepted) in errors {
            let sink = Arc::new(MockOutputStream::new().with_partial_stream_events());
            let output: Arc<dyn OutputStream> = sink.clone();
            let details = llm_runtime::HistoryStopDetails {
                category: Some("category".into()),
                explanation: Some("explanation".into()),
            };
            let mut usage = LlmUsage::default();
            usage.counts_mut().input_tokens = 100;
            usage.counts_mut().output_tokens = 17;
            let mut events = vec![
                Ok(message_start("complete", "claude-sonnet-4-6")),
                Ok(content_block_start_text(0)),
                Ok(text_delta(0, "complete answer")),
                Ok(content_block_stop(0)),
                Ok(HistoryEvent::MessageDelta {
                    delta: llm_runtime::HistoryMessageDelta {
                        stop_reason: Some("max_tokens".into()),
                        stop_details: Some(details.clone()),
                    },
                    usage: Some(usage),
                }),
            ];
            if let Some(error) = &error {
                events.push(Err(error.clone()));
            }
            let result = pump_stream(stream::iter(events).boxed(), &output).await;
            if accepted {
                let turn = result.unwrap();
                assert_eq!(turn.stop_reason.as_deref(), Some("max_tokens"));
                assert_eq!(turn.stop_details, Some(details));
                let usage = turn.usage.unwrap();
                assert_eq!(usage.counts().input_tokens, 100);
                assert_eq!(usage.counts().output_tokens, 17);
                assert!(
                    matches!(turn.assistant_blocks.as_slice(), [ContentBlock::Text { text, .. }] if text == "complete answer")
                );
                assert_eq!(
                    sink.partial_stream_event_snapshot().await.last().unwrap(),
                    "{\"type\":\"message_stop\"}"
                );
            } else {
                assert!(
                    matches!(result.unwrap_err(), OrchestratorError::Streaming(actual) if Some(&actual) == error.as_ref())
                );
                assert!(!sink
                    .partial_stream_event_snapshot()
                    .await
                    .iter()
                    .any(|frame| frame == "{\"type\":\"message_stop\"}"));
            }
        }
    }

    #[tokio::test]
    async fn terminal_delta_cannot_complete_an_open_tool_or_a_protocol_error() {
        for events in [
            vec![
                message_start("open-tool", "claude-sonnet-4-6"),
                content_block_start_tool_use(0, ToolUseId::new(), "Read"),
                input_json_delta(0, "{\"file_path\":\"unfinished"),
                message_delta_stop("tool_use"),
            ],
            vec![
                message_start("protocol-error", "claude-sonnet-4-6"),
                message_delta_stop("end_turn"),
                content_block_stop(9),
            ],
            vec![
                message_start("empty-stop", "claude-sonnet-4-6"),
                message_delta_stop(""),
            ],
            vec![message_delta_stop("end_turn")],
        ] {
            let sink = Arc::new(MockOutputStream::new().with_partial_stream_events());
            let output: Arc<dyn OutputStream> = sink.clone();
            let result = pump_stream(boxed(events), &output).await;
            assert!(matches!(
                result.unwrap_err(),
                OrchestratorError::StreamEndedWithoutStop | OrchestratorError::StreamingProtocol(_)
            ));
            assert!(sink.tool_calls().await.is_empty());
            assert!(!sink
                .partial_stream_event_snapshot()
                .await
                .iter()
                .any(|frame| frame == "{\"type\":\"message_stop\"}"));
        }
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
            matches!(&turn.assistant_blocks[1],ContentBlock::Text { text, .. } if text == "answer")
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
        if let ContentBlock::Text { text, .. } = &turn.assistant_blocks[0] {
            assert_eq!(text, "hello");
        } else {
            panic!("expected Text block, got {:?}", turn.assistant_blocks[0]);
        }
        assert!(turn.tool_uses.is_empty());
    }

    #[tokio::test]
    async fn pump_remainder_preserves_later_assistant_responses() {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let events = vec![
            message_start("m1", "model"),
            content_block_start_text(0),
            text_delta(0, "first"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
            message_start("m2", "model"),
            content_block_start_text(0),
            text_delta(0, "second"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ];
        let (first, rest) = pump_stream_inner(boxed(events), &out, None)
            .await
            .expect("first response");
        assert!(
            matches!(&first.assistant_blocks[0], ContentBlock::Text { text, .. } if text == "first")
        );
        let (second, mut rest) = pump_stream_inner(rest, &out, None)
            .await
            .expect("second response");
        assert!(
            matches!(&second.assistant_blocks[0], ContentBlock::Text { text, .. } if text == "second")
        );
        assert!(rest.next().await.is_none());
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

#[cfg(test)]
#[path = "streaming_loop_server_fallback_tests.rs"]
mod server_fallback_tests;
