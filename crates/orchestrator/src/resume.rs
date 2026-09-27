//! Resume entrypoint — load a prior session, validate the chain, replay its
//! messages into a fresh [`SessionState`], and continue the turn loop.
//!
//! Spec §3 M5-08 row + §4.x resume completeness checks.
//!
//! ## SES-4: this layer is a pure transcript replay, deliberately
//!
//! Verified 2026-09-14: nothing in this file mentions tasks or background
//! workers, so a resume through it does not re-attach to a background session
//! that is still running. That is the gap the backlog names — but the
//! capability is not missing from the product, only from this layer: shell
//! handoff/adoption lives at the CLI composition root
//! (`apps/cli/src/shell_handoff.rs` driving
//! `TaskRegistryHandle::{export,prepare,adopt}_shell_handoff`), and a live
//! attach is an explicit `lingxi-cli attach` (`apps/cli/src/commands/attach.rs`).
//!
//! ⇒ The question to settle before building anything here is which HOSTS lose
//! live workers on resume — desktop and mobile resume through this path and
//! have no `attach` command — not whether `replay_session_state` "should" know
//! about tasks. Moving the adoption down into the orchestrator would also move
//! it away from the only layer that currently owns process handles.
//!
//! Plan adaptation: the M5-07 `load_session` surface takes
//! `(lingxi_home, cwd, session_id, fs)` (rather than the plan-doc's
//! `(session_id, cwd)`), so [`replay_session_state`] mirrors that signature.
//! The CLI/REPL callers already have a `lingxi_home: PathBuf` and an
//! `Arc<dyn FileSystem>` from M3-01 + M4-01, so threading them through is
//! cheap and avoids hard-coding `dirs::home_dir()` inside the orchestrator.

use crate::config::OrchestratorConfig;
use crate::conversation::{ConversationOrchestrator, NoStreamingApiClient, OrchestratorApiClient};
use crate::test_support::{HookExecutor, PermissionGate};
use lingxi_core::session::{ActiveGoalState, GoalOrigin};
use lingxi_core::SessionState;
use platform_api::{FileSystem, OutputStream};
use protocol::{ContentBlock, ConversationMessage, MessageId, SessionId, ToolUseId};
use serde_json::Value;
use session::jsonl::{
    load_session_across_worktrees, load_session_entries_across_worktrees, JsonlMessage,
    JsonlWriter, LoaderError,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use telemetry::tengu::session::RESUMED;
use tokio::sync::Mutex;
use tool_api::registry::ToolRegistry;
use uuid::Uuid;

/// Errors raised by the resume path. Forwards loader errors verbatim.
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    /// Bubbled up from [`load_session`].
    #[error(transparent)]
    Loader(#[from] LoaderError),
    /// Deferred-tool replay failed after the transcript loaded.
    #[error("deferred replay: {0}")]
    DeferredReplay(String),
}

/// Result of [`replay_session_state`]: the rebuilt session, the UUID of the
/// last replayed message (`None` only if zero messages were replayed — i.e.
/// the loader returned an empty `Vec`, which is structurally rejected
/// upstream but kept as `Option` for type safety), and the raw replayed
/// messages (callers may want to inspect them, e.g. interop tests).
#[derive(Debug)]
pub struct ReplayedSession {
    /// Replayed session state with `history` populated from the JSONL.
    pub state: SessionState,
    /// Complete main-thread history for UI transcript replay. This may include
    /// messages before the latest compact boundary; callers must keep it out
    /// of the model context and use [`Self::state`] for engine resume.
    pub display_history: Vec<ConversationMessage>,
    /// UUID of the last replayed message — used to seed the orchestrator's
    /// `last_jsonl_uuid` so the next append chains via `parent_uuid`.
    pub last_message_uuid: Option<Uuid>,
    /// Raw replayed messages (file-order), for callers that need them.
    pub messages: Vec<JsonlMessage>,
    /// Runtime-only state reconstructed from transcript envelope metadata.
    /// This is not representable in `SessionState.history`, but must survive a
    /// cold resume for effort and compaction behavior to remain continuous.
    pub runtime_metadata: ResumeRuntimeMetadata,
    /// The [`CLIENT_STATE_TOOLS`] results in [`Self::display_history`], keyed by
    /// `tool_use_id` — see [`client_state_tool_results_from_messages`]. A host
    /// lowering that transcript passes this to
    /// `client_adapter::lowering::lower_transcript_with_tool_results` so a
    /// replayed subagent card still knows which call created it, and a replayed
    /// plan card still has its document and its approval.
    pub client_state_tool_results: std::collections::HashMap<String, Value>,
}

impl ReplayedSession {
    /// Project the JSONL-only state into the leaf trait used by bridge/mobile
    /// hot-resume. Keeping this conversion beside replay prevents individual
    /// hosts from restoring only a subset of compaction state.
    #[must_use]
    pub fn handle_runtime_snapshot(&self) -> platform_api::ResumeRuntimeSnapshot {
        let tracking = &self.runtime_metadata.compaction_tracking;
        // Only present a model in the snapshot when a REAL assistant model row
        // was recovered from the transcript (same filter the replay uses:
        // non-empty, not a `<synthetic>`-style placeholder). Otherwise emit an
        // EMPTY model so the hot-resume consumer's `!model.is_empty()` guard
        // keeps the live session model instead of adopting the `DEFAULT_MODEL`
        // seed that `build_state_from_jsonl` left behind.
        let model_recovered = self.messages.iter().any(|m| {
            carries_human_turn_settings(m)
                && m.message
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| !s.is_empty() && !(s.starts_with('<') && s.ends_with('>')))
        });
        platform_api::ResumeRuntimeSnapshot {
            current_usage: self.runtime_metadata.current_usage,
            model: if model_recovered {
                self.state.model.clone()
            } else {
                String::new()
            },
            model_profile: self.state.model_profile.clone(),
            effort: self.runtime_metadata.effort.clone(),
            reasoning_selection: self.runtime_metadata.reasoning_selection.clone(),
            main_thread_agent_type: self.runtime_metadata.main_thread_agent_type.clone(),
            main_thread_agent_definition: self
                .runtime_metadata
                .main_thread_agent_definition
                .clone(),
            transcript_only_message_ids: self
                .state
                .transcript_only_messages
                .iter()
                .copied()
                .collect(),
            compact_summary_message_ids: self
                .state
                .compact_summary_messages
                .iter()
                .copied()
                .collect(),
            model_context_excluded_message_ids: self
                .state
                .model_context_excluded_messages
                .iter()
                .copied()
                .collect(),
            loaded_tool_names: session::jsonl::discovered_tool_names(&self.messages),
            post_compact_skill_attachments: post_compact_skill_attachments_from_messages(
                &self.messages,
            )
            .into_iter()
            .collect(),
            cumulative_dropped_tokens: self.runtime_metadata.cumulative_dropped_tokens,
            compacted: tracking.compacted,
            turn_counter: tracking.turn_counter,
            turn_id: tracking.turn_id.clone(),
            consecutive_failures: tracking.consecutive_failures,
            consecutive_rapid_refills: tracking.consecutive_rapid_refills,
            deferred_tools: self.runtime_metadata.deferred_tools.clone(),
            prompt_snapshot: self.runtime_metadata.prompt_snapshot.clone(),
        }
    }
}

/// Runtime state recoverable from a persisted JSONL transcript.
#[derive(Debug, Clone)]
pub struct ResumeRuntimeMetadata {
    /// Latest real assistant usage, never cumulative session billing.
    pub current_usage: Option<platform_api::CurrentUsageSnapshot>,
    /// Last real assistant response's top-level `effort` value.
    pub effort: Option<String>,
    /// Structured reasoning selection persisted by newer runtimes.
    pub reasoning_selection: Option<platform_api::ReasoningSelection>,
    /// Persisted main-thread agent name, when the session selected one.
    pub main_thread_agent_type: Option<String>,
    /// Integrity-checked immutable resolved agent definition, when available.
    pub main_thread_agent_definition: Option<serde_json::Value>,
    /// Latest compact boundary's `cumulativeDroppedTokens` value.
    pub cumulative_dropped_tokens: u64,
    /// Reconstructed rapid-refill/autocompact tracking state.
    pub compaction_tracking: compaction::AutoCompactTrackingState,
    /// Deferred hook tools that were persisted but never produced a result.
    pub deferred_tools: Vec<platform_api::DeferredToolReplay>,
    /// Last valid static prompt snapshot recovered from the transcript.
    pub prompt_snapshot: Option<platform_api::PromptSnapshot>,
}

/// Load + replay a session by UUID. Emits a single [`RESUMED`]
/// (`tengu_session_resumed`) after a successful replay — matching claude's
/// single resume event (the started/completed pair is not in the binary).
///
/// Errors: any [`LoaderError`] from `load_session` is wrapped in
/// [`ResumeError::Loader`].
pub async fn replay_session_state(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<ReplayedSession, ResumeError> {
    let sid_str = session_id.to_string();
    let transcript_path =
        session::jsonl::resolve_session_path_across_worktrees(lingxi_home, cwd, session_id).await?;
    let messages = load_session_across_worktrees(lingxi_home, cwd, session_id, fs.clone()).await?;
    let transcript_entries =
        load_session_entries_across_worktrees(lingxi_home, cwd, session_id, fs.clone()).await?;
    let (state, last_uuid, mut runtime_metadata) = build_state_from_jsonl(session_id, &messages);
    let display_entries = transcript_entries
        .iter()
        .filter(|message| !message.is_sidechain)
        .cloned()
        .collect::<Vec<_>>();
    let (display_state, _, _) = build_state_from_jsonl(session_id, &display_entries);
    let (agent_type, agent_definition) =
        session::jsonl::read_agent_resume_state(&transcript_path, fs, &sid_str).await;
    runtime_metadata.main_thread_agent_type = agent_type;
    runtime_metadata.main_thread_agent_definition = agent_definition;
    runtime_metadata.deferred_tools = deferred_tool_replays_from_messages(&transcript_entries);
    // `load_session` returns only the resumable chain; prompt snapshots are
    // generic attachments and may be off-chain when a transcript ended before
    // the assistant response. Recover from the full routed entry stream.
    runtime_metadata.prompt_snapshot = prompt_snapshot_from_messages(&transcript_entries);
    // claude emits a SINGLE `tengu_session_resumed` on resume (no started/
    // completed pair — those names have 0 hits in the 2.1.195 binary).
    tracing::info!(
        event = RESUMED,
        session_id = %sid_str,
        message_count = messages.len() as u64,
    );
    Ok(ReplayedSession {
        state,
        display_history: display_state.history,
        last_message_uuid: last_uuid,
        messages,
        runtime_metadata,
        // Built from the DISPLAY entries, which is exactly the history the
        // hosts lower for the client — and, being non-sidechain, is what makes
        // this map main-chain-only.
        client_state_tool_results: client_state_tool_results_from_messages(&display_entries),
    })
}

/// Rebuild a replayed [`SessionState`] from transcript messages ALREADY loaded
/// from disk (`session::jsonl::load_session` → `Vec<JsonlMessage>`).
///
/// (M5-13) The CLI's `--resume <uuid>` mount loads the transcript once (for the
/// existence check + the TUI scrollback seed) and must NOT re-read it to seed the
/// engine session: a second `load_session` is both wasteful and a TOCTOU window
/// against a concurrent delete. This thin public wrapper exposes the SAME
/// per-line mapping [`replay_session_state`] uses ([`build_state_from_jsonl`]) so
/// the caller seeds the orchestrator's session directly from the in-hand
/// messages. Returns only the [`SessionState`] (callers seeding an already-built
/// orchestrator through its public `session()` accessor cannot set the
/// `pub(crate)` `last_jsonl_uuid` chain pointer anyway; the desktop/CLI build
/// wires no JSONL writer, so that pointer is inert on this path).
#[must_use]
pub fn state_from_messages(session_id: Uuid, messages: &[JsonlMessage]) -> SessionState {
    build_state_from_jsonl(session_id, messages).0
}

/// Recover the runtime-only resume metadata without rebuilding the session
/// history. CLI remount paths use this before constructing a replacement
/// runtime so the saved effort can seed the provider adapter.
#[must_use]
pub fn runtime_metadata_from_messages(messages: &[JsonlMessage]) -> ResumeRuntimeMetadata {
    resume_runtime_metadata(messages)
}

/// Recover exact post-compact skill attachment bodies from version-tolerant
/// JSONL envelope metadata. The contents are intentionally opaque: Markdown
/// may contain any renderer separator, so resume must never reverse-parse the
/// model-visible message body.
#[must_use]
pub fn post_compact_skill_attachments_from_messages(
    messages: &[JsonlMessage],
) -> std::collections::HashMap<MessageId, Vec<String>> {
    messages
        .iter()
        .filter_map(|message| {
            let uuid = Uuid::parse_str(&message.uuid).ok()?;
            let contents = message
                .extra
                .get("invokedSkillContents")?
                .as_array()?
                .iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            (!contents.is_empty()).then_some((MessageId::from_uuid(uuid), contents))
        })
        .collect()
}

/// Recover the last valid carved-slate prompt snapshot from generic JSONL
/// attachment rows. Invalid or incomplete rows are ignored so a damaged tail
/// falls back to live prompt/tool assembly; a later valid row still wins.
#[must_use]
pub fn prompt_snapshot_from_messages(
    messages: &[JsonlMessage],
) -> Option<platform_api::PromptSnapshot> {
    messages.iter().rev().find_map(|message| {
        if message.message_type != "attachment" {
            return None;
        }
        let attachment = message.extra.get("attachment")?;
        if attachment.get("type").and_then(Value::as_str) != Some("prompt_snapshot") {
            return None;
        }
        let snapshot =
            serde_json::from_value::<platform_api::PromptSnapshot>(attachment.clone()).ok()?;
        (!snapshot.system_prompt.is_empty()
            && snapshot.system_prompt.iter().all(|part| !part.is_empty())
            && snapshot.tools.iter().all(|tool| !tool.name.is_empty()))
        .then_some(snapshot)
    })
}

/// An in-progress run of consecutive per-block "assistant" JSONL rows that
/// share one originating turn (see [`flush_pending_assistant`]).
struct PendingAssistant {
    /// Grouping key: the inner `message.id` shared by every row of the
    /// turn (or, for legacy/malformed rows with no inner id, that row's own
    /// top-level `uuid` — which degrades to "one row, one group", matching
    /// pre-grouping behavior).
    inner_key: String,
    hook_grouping: (bool, bool),
    /// The turn's logical id, restored from `inner_key` when it parses as a
    /// UUID (the common case); falls back to the first row's own uuid.
    message_id: Uuid,
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    model_context_excluded: bool,
}

/// Flush an accumulated per-block assistant run into `state.history` as ONE
/// merged [`ConversationMessage::Assistant`].
///
/// Write-side, one assistant turn `[reasoning, tool_use A, tool_use B]` is
/// persisted as three single-block "assistant" JSONL rows sharing one inner
/// `message.id` (`ConversationOrchestrator::persist_assistant_per_block`).
/// Without this merge, [`build_state_from_jsonl`] would push three SEPARATE
/// single-block `ConversationMessage::Assistant` turns on resume — which
/// then encode as three separate provider-wire messages instead of one. For
/// DeepSeek's thinking mode that splits a turn's `reasoning_content` off
/// from the `tool_calls` message it belongs to, and the provider 400s with
/// "The reasoning_content in the thinking mode must be passed back to the
/// API." Restoring the single-turn shape here fixes it at the source
/// instead of papering over it in the OpenAI-chat encoder.
fn flush_pending_assistant(state: &mut SessionState, pending: Option<PendingAssistant>) {
    if let Some(pending) = pending {
        let message_id = MessageId::from_uuid(pending.message_id);
        if pending.hook_grouping != (false, false) {
            state
                .hook_message_grouping
                .insert(message_id, pending.hook_grouping);
        }
        if pending.model_context_excluded {
            state.model_context_excluded_messages.insert(message_id);
        }
        state.history.push(ConversationMessage::Assistant {
            id: message_id,
            content: pending.content,
            stop_reason: pending.stop_reason,
        });
    }
}

/// Convert the replayed JSONL into a fresh [`SessionState`] + the UUID of
/// the tail message. `type: "user" | "assistant"` lines are appended to
/// `history`; compact-boundary system lines are rebuilt as typed protocol
/// messages, while other system / sidechain / agent-internal entries are
/// reconstructed from settings + memory and are not replayed.
fn build_state_from_jsonl(
    session_id: Uuid,
    messages: &[JsonlMessage],
) -> (SessionState, Option<Uuid>, ResumeRuntimeMetadata) {
    let mut state = SessionState::empty(
        SessionId::from_uuid(session_id),
        crate::config::DEFAULT_MODEL.to_string(),
    );
    let mut last_uuid: Option<Uuid> = None;
    let mut pending_assistant: Option<PendingAssistant> = None;
    let mut ultracode_state = tool_workflow::UltracodeState::default();
    for m in messages {
        let Ok(msg_uuid) = Uuid::parse_str(&m.uuid) else {
            tracing::warn!(
                message_type = %m.message_type,
                "skipping transcript row with malformed UUID"
            );
            continue;
        };
        match m.message_type.as_str() {
            "user" => {
                flush_pending_assistant(&mut state, pending_assistant.take());
                let content_blocks = extract_content_blocks(&m.message);
                // Restore the `isMeta` outer-envelope flag (claude-code persists
                // it as a top-level field; we read it back from `extra`) so a
                // resumed Stop-hook-feedback message stays meta/hidden.
                let is_meta = m
                    .extra
                    .get("isMeta")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let replay_text = content_blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ultracode_state.observe_persisted_user_message(&replay_text, is_meta);
                state.ultracode_active = ultracode_state.active;
                state.ultracode_non_meta_turns_since_reminder =
                    ultracode_state.non_meta_turns_since_reminder;
                let is_visible_in_transcript_only = m
                    .extra
                    .get("isVisibleInTranscriptOnly")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if is_visible_in_transcript_only {
                    state
                        .transcript_only_messages
                        .insert(MessageId::from_uuid(msg_uuid));
                }
                let is_compact_summary = m
                    .extra
                    .get("isCompactSummary")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if is_compact_summary {
                    state
                        .compact_summary_messages
                        .insert(MessageId::from_uuid(msg_uuid));
                }
                let is_model_context_excluded = m
                    .extra
                    .get("isModelContextExcluded")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if is_model_context_excluded {
                    state
                        .model_context_excluded_messages
                        .insert(MessageId::from_uuid(msg_uuid));
                }
                state.history.push(ConversationMessage::User {
                    id: MessageId::from_uuid(msg_uuid),
                    content: content_blocks,
                    is_meta,
                    is_compact_summary,
                    is_visible_in_transcript_only,
                });
                last_uuid = Some(msg_uuid);
            }
            "assistant" => {
                let content_blocks = extract_content_blocks(&m.message);
                // Recover the session's active model: each assistant line records
                // the model that produced it, so the LAST one is the model the
                // session was on at save time. Restoring it (over the
                // `DEFAULT_MODEL` seed) lets a resumed session continue on its
                // saved model instead of the launch default — otherwise a
                // resumed non-default session reported/showed the wrong model.
                //
                // SKIP placeholder markers like `<synthetic>` (error/system
                // messages that carry no real model): a real model id never
                // starts with `<`. Otherwise a session whose LAST assistant line
                // was a synthetic error (e.g. a request-rejection notice) would
                // resume onto model `<synthetic>` → `resolve_in` → ModelUnavailable
                // on the first turn.
                if let Some(model) = m
                    .message
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty() && !(s.starts_with('<') && s.ends_with('>')))
                    .filter(|_| carries_human_turn_settings(m))
                {
                    state.model = model.to_string();
                }
                // Newer real-assistant rows record the provider profile beside
                // the model. Presence is significant: JSON null intentionally
                // clears a profile, while legacy rows omit the field and retain
                // the most recently reconstructed value.
                if let Some(profile) = m
                    .extra
                    .get("modelProfile")
                    .filter(|_| carries_human_turn_settings(m))
                {
                    state.model_profile = profile.as_str().map(str::to_owned);
                }
                // Per-block persistence (write-side) splits one assistant turn
                // into several single-block rows sharing one inner `message.id`
                // — merge consecutive rows with the same inner id back into one
                // turn instead of pushing each block as its own turn (see
                // `flush_pending_assistant`).
                let inner_key = m
                    .message
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| m.uuid.clone());
                match &mut pending_assistant {
                    Some(pending) if pending.inner_key == inner_key => {
                        pending.content.extend(content_blocks);
                        if let Some(reason) = m.message.get("stop_reason").and_then(Value::as_str) {
                            pending.stop_reason = Some(reason.to_string());
                        }
                        pending.model_context_excluded |= m
                            .extra
                            .get("isModelContextExcluded")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                    }
                    _ => {
                        flush_pending_assistant(&mut state, pending_assistant.take());
                        let message_id = Uuid::parse_str(&inner_key).unwrap_or(msg_uuid);
                        pending_assistant = Some(PendingAssistant {
                            hook_grouping: (
                                m.extra
                                    .get("isVirtual")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false),
                                m.extra
                                    .get("resumedFromIncompleteThinking")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false),
                            ),
                            inner_key,
                            message_id,
                            content: content_blocks,
                            stop_reason: m
                                .message
                                .get("stop_reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            model_context_excluded: m
                                .extra
                                .get("isModelContextExcluded")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                        });
                    }
                }
                // Message timing is a session sidecar, not part of the frozen
                // ConversationMessage wire shape. Legacy or malformed rows
                // leave it absent, making time-based microcompact a safe no-op.
                if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&m.timestamp) {
                    let committed_at: std::time::SystemTime =
                        parsed.with_timezone(&chrono::Utc).into();
                    if match state.message_timing.last_assistant_at {
                        Some(current) => committed_at > current,
                        None => true,
                    } {
                        state.message_timing.last_assistant_at = Some(committed_at);
                    }
                }
                last_uuid = Some(msg_uuid);
            }
            _ => {
                flush_pending_assistant(&mut state, pending_assistant.take());
                if let Some(message) = hook_attachment_message_for_api(m, msg_uuid) {
                    state.history.push(message);
                }
                if is_compact_boundary(m) {
                    let content = m
                        .extra
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(compaction::BOUNDARY_CONTENT)
                        .to_string();
                    let compact_metadata = m
                        .extra
                        .get("compactMetadata")
                        .cloned()
                        .and_then(|value| {
                            serde_json::from_value::<protocol::CompactBoundaryMetadata>(value).ok()
                        })
                        .map(|mut metadata| {
                            metadata.logical_parent_uuid = m.logical_parent_uuid.clone();
                            metadata
                        });
                    state.history.push(ConversationMessage::System {
                        id: MessageId::from_uuid(msg_uuid),
                        content,
                        subtype: Some("compact_boundary".to_string()),
                        compact_metadata,
                        refusal_fallback: None,
                    });
                }
                if is_scheduled_task_fire(m) {
                    // The live path (`transcript.rs`) pushes the fire into
                    // history AND excludes it from model context; resume did
                    // neither, so a `/loop` or fixed fire was written to the
                    // transcript and then dropped on the way back in. The
                    // companion `user_meta` row is NOT excluded — it is the
                    // line the model is meant to read.
                    state.history.push(ConversationMessage::System {
                        id: MessageId::from_uuid(msg_uuid),
                        content: m
                            .extra
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        subtype: Some("scheduled_task_fire".to_string()),
                        compact_metadata: None,
                        refusal_fallback: None,
                    });
                    state
                        .model_context_excluded_messages
                        .insert(MessageId::from_uuid(msg_uuid));
                }
                if let Some(active_goal) = goal_state_from_message(m) {
                    state.active_goal = active_goal;
                }
                // Other system / sidechain / agent-internal entries are
                // skipped for history replay. Every persisted line still
                // advances the chain pointer so the next append is anchored to
                // the file tail (matching claude-code's chain semantics).
                if !m.uuid.is_empty() {
                    last_uuid = Some(msg_uuid);
                }
            }
        }
    }
    flush_pending_assistant(&mut state, pending_assistant.take());
    restore_thinking_stripped_ranges(&mut state, messages);
    let runtime_metadata = resume_runtime_metadata(messages);
    if transcript_has_open_plan_segment(messages) {
        state.plan_mode = true;
        state.plan_reminder_shown = false;
    }
    (state, last_uuid, runtime_metadata)
}

/// Claude 2.1.263 mce/uhr/wys: markers only affect preceding assistant
/// blocks, with partial offsets counted across per-block rows sharing an ID.
fn restore_thinking_stripped_ranges(state: &mut SessionState, messages: &[JsonlMessage]) {
    struct Row {
        key: String,
        id: MessageId,
        thinking_before: usize,
        thinking_count: usize,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut totals = std::collections::HashMap::<String, usize>::new();
    let mut pending: Option<(String, MessageId)> = None;
    let mut group_offsets = std::collections::HashMap::<MessageId, usize>::new();
    for message in messages {
        let Ok(uuid) = Uuid::parse_str(&message.uuid) else {
            continue;
        };
        if message.message_type == "assistant" {
            let key = message
                .message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&message.uuid)
                .to_string();
            let id = match &pending {
                Some((previous, id)) if previous == &key => *id,
                _ => MessageId::from_uuid(Uuid::parse_str(&key).unwrap_or(uuid)),
            };
            pending = Some((key.clone(), id));
            let thinking_count = extract_content_blocks(&message.message)
                .iter()
                .filter(|block| {
                    matches!(
                        block,
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                    )
                })
                .count();
            let total = totals.entry(key.clone()).or_default();
            group_offsets.entry(id).or_insert(*total);
            rows.push(Row {
                key,
                id,
                thinking_before: *total,
                thinking_count,
            });
            *total += thinking_count;
            continue;
        }
        pending = None;
        let Some(attachment) = message
            .extra
            .get("attachment")
            .filter(|_| message.message_type == "attachment")
        else {
            continue;
        };
        if attachment.get("type").and_then(Value::as_str) != Some("thinking_stripped") {
            continue;
        }
        state.thinking_signature_stripped = true;
        let partial = if attachment.get("scope").and_then(Value::as_str) == Some("partial") {
            attachment.get("from").and_then(|from| {
                Some((
                    from.get("messageId")?.as_str()?,
                    usize::try_from(from.get("thinkingIndex")?.as_u64()?).ok()?,
                ))
            })
        } else {
            None
        };
        let start = partial.and_then(|(key, from)| {
            rows.iter()
                .position(|row| row.key == key && row.thinking_before + row.thinking_count > from)
        });
        for row in &rows[start.unwrap_or(0)..] {
            let from = if let (Some(_), Some((key, from))) = (start, partial) {
                if row.key == key {
                    from.saturating_sub(row.thinking_before)
                } else {
                    0
                }
            } else {
                0
            };
            // Several JSONL rows can coalesce into the same history message.
            // Preserve the group's earlier thinking when the marker starts
            // inside a later row belonging to that same group.
            let group_start = group_offsets.get(&row.id).copied().unwrap_or(0);
            let from = if let (Some(_), Some((key, index))) = (start, partial) {
                if row.key == key {
                    index.saturating_sub(group_start)
                } else {
                    from
                }
            } else {
                from
            };
            state
                .thinking_stripped_messages
                .entry(row.id)
                .and_modify(|current| *current = (*current).min(from))
                .or_insert(from);
        }
    }
}

fn hook_attachment_message_for_api(
    message: &JsonlMessage,
    message_uuid: Uuid,
) -> Option<ConversationMessage> {
    if message.message_type != "attachment" {
        return None;
    }
    let attachment = message.extra.get("attachment")?;
    match attachment.get("type").and_then(Value::as_str)? {
        "hook_stopped_continuation" => {
            let hook_name = attachment.get("hookName")?.as_str()?;
            let reason = attachment.get("message")?.as_str()?;
            Some(ConversationMessage::user_meta(
                MessageId::from_uuid(message_uuid),
                format!(
                    "<system-reminder>\n{hook_name} hook stopped continuation: {reason}\n</system-reminder>"
                ),
            ))
        }
        "hook_additional_context" => {
            let hook_name = attachment.get("hookName")?.as_str()?;
            let content = attachment.get("content")?.as_array()?;
            if content.is_empty() {
                return None;
            }
            // Claude 2.1.263 JDo/Eht rejects the entire malformed attachment;
            // filtering individual values would replay a different hook body.
            let body = content
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?
                .join("\n");
            Some(ConversationMessage::user_meta(
                MessageId::from_uuid(message_uuid),
                format!(
                    "<system-reminder>\n{hook_name} hook additional context: {body}\n</system-reminder>"
                ),
            ))
        }
        "hook_blocking_error" => {
            let hook_name = attachment.get("hookName")?.as_str()?;
            let blocking = attachment.get("blockingError")?;
            let command = blocking.get("command")?.as_str()?;
            let error = blocking.get("blockingError")?.as_str()?;
            Some(ConversationMessage::user_meta(
                MessageId::from_uuid(message_uuid),
                format!(
                    "<system-reminder>\n{hook_name} hook blocking error from command: \"{command}\": {error}\n</system-reminder>"
                ),
            ))
        }
        _ => None,
    }
}

fn tool_result_ids(message: &JsonlMessage) -> impl Iterator<Item = &str> {
    message
        .message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|blocks| blocks.iter())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
}

/// The tools whose STRUCTURED result a client rebuilds UI state from, and which
/// therefore has to survive a replay.
///
/// Deliberately an allowlist and not a shape test. Every other tool's `data` is
/// the uncapped raw result — `Read` ships an image's base64, `Edit` ships the
/// whole pre-edit file — which no client renders and which would replace the
/// capped model-facing text; carrying all of them measured 2.0x-34.5x growth on
/// real transcripts for a payload that crosses as ONE `SessionResumed` frame.
///
/// * `Agent` / `Task` / `Skill` — the payload's `agentId` is the only structural
///   link from a subagent card back to the call that spawned it.
/// * `ExitPlanMode` — the payload's `plan` is the plan document and its
///   `model_content` is the approval wording. Without them a resumed plan card
///   has no body and falls back to its `submitted` placeholder, so a plan the
///   user actually approved reads as still being prepared.
pub const CLIENT_STATE_TOOLS: &[&str] = &["Agent", "Task", "Skill", "ExitPlanMode"];

/// Recover the [`CLIENT_STATE_TOOLS`] results from the persisted MAIN-CHAIN
/// transcript, keyed by the `tool_use_id` each one answers.
///
/// The turn loop stamps a tool's raw structured result on the persisted
/// `tool_result` user line as `toolUseResult` (`conversation/transcript.rs`)
/// precisely because `ContentBlock::ToolResult` cannot hold it: that block keeps
/// only the model-facing TEXT. Replay used to drop it, so every client that
/// rebuilds state from `result_json` saw nothing after a restart.
///
/// Scope, all three parts load-bearing:
///
/// * **Allowlisted tools only** — see [`CLIENT_STATE_TOOLS`]. The pairing index
///   below is what makes this a TOOL test rather than a guess at the payload's
///   shape: the `tool_result` line names only the id it answers, so the name has
///   to come from the `tool_use` in an earlier message.
/// * **Main chain only.** Callers pass the non-sidechain display entries, so
///   this can never describe a SUBAGENT's own transcript. Those are read from a
///   separate per-agent file (`engine-desktop::session_agents`), which drops the
///   sibling before lowering and needs its own recovery — do not reach for this
///   map there; it would compile, stay green, and be empty.
/// * **Single-block lines only.** Mirrors the writer: `take_tool_use_result`
///   stamps the field only when the line holds exactly one `tool_result`
///   (`sole_tool_result_id`). Fanning one payload across a multi-block line
///   would hand tool B tool A's payload and anchor a card to the WRONG call —
///   a wrong answer in place of the old missing one.
#[must_use]
pub fn client_state_tool_results_from_messages(
    messages: &[JsonlMessage],
) -> std::collections::HashMap<String, Value> {
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let mut results = std::collections::HashMap::new();
    for message in messages {
        for block in message
            .message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) {
                names.insert(id, name);
            }
        }
        let Some(result) = message.extra.get("toolUseResult") else {
            continue;
        };
        let mut ids = tool_result_ids(message);
        let (Some(only), None) = (ids.next(), ids.next()) else {
            continue;
        };
        if !names
            .get(only)
            .is_some_and(|name| CLIENT_STATE_TOOLS.contains(name))
        {
            continue;
        }
        results.insert(only.to_string(), result.clone());
    }
    results
}

/// Extract all persisted deferred hook tools that have no later tool result.
#[must_use]
pub fn deferred_tool_replays_from_messages(
    messages: &[JsonlMessage],
) -> Vec<platform_api::DeferredToolReplay> {
    let mut resolved: HashSet<&str> = HashSet::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut deferred = Vec::new();
    for message in messages.iter().rev() {
        resolved.extend(tool_result_ids(message));
        if message.message_type != "attachment" {
            continue;
        }
        let Some(attachment) = message.extra.get("attachment") else {
            continue;
        };
        if attachment.get("type").and_then(Value::as_str) != Some("hook_deferred_tool") {
            continue;
        }
        let Some(tool_use_id) = attachment.get("toolUseID").and_then(Value::as_str) else {
            continue;
        };
        if resolved.contains(tool_use_id) {
            continue;
        }
        // One replay per tool_use_id, not one per attachment. A replay whose
        // hook defers the tool AGAIN writes a fresh `hook_deferred_tool`
        // attachment while nothing resolving is persisted, so a transcript can
        // hold several unresolved attachments for the same id. Without this the
        // next resume dispatches that tool once per attachment — the side
        // effects (a Bash command, a Write) run twice, and the duplicate
        // `tool_result` is then silently discarded downstream, hiding it. The
        // walk is newest-first, so the first attachment seen is the live one.
        if !seen.insert(tool_use_id) {
            continue;
        }
        let Some(tool_name) = attachment.get("toolName").and_then(Value::as_str) else {
            continue;
        };
        let Some(tool_input) = attachment.get("toolInput") else {
            continue;
        };
        deferred.push(platform_api::DeferredToolReplay {
            tool_use_id: tool_use_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            permission_mode: attachment
                .get("permissionMode")
                .and_then(Value::as_str)
                .map(str::to_string),
            traceparent: attachment
                .get("traceparent")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    deferred.reverse();
    deferred
}

/// What one replayed tool contributes to the single batched result message.
struct DeferredReplayOutcome {
    tool_use_id: ToolUseId,
    tool_results: Vec<ContentBlock>,
    injected_messages: Vec<(ConversationMessage, ToolUseId)>,
}

async fn replay_deferred_tool_after_resume(
    orch: &ConversationOrchestrator,
    deferred: platform_api::DeferredToolReplay,
) -> Result<DeferredReplayOutcome, crate::OrchestratorError> {
    let tool_use_id = ToolUseId::from(deferred.tool_use_id);
    let trace_context = deferred.traceparent.as_deref().map(|traceparent| {
        telemetry::otel::SerializedTraceContext {
            traceparent: traceparent.to_string(),
            tracestate: None,
        }
    });
    let live_permission_mode = {
        let plan_mode = orch.session.lock().await.plan_mode;
        if plan_mode {
            "plan".to_string()
        } else {
            orch.permission_mode()
                .unwrap_or_else(|| "default".to_string())
        }
    };
    if deferred
        .permission_mode
        .as_deref()
        .is_some_and(|stored| stored != live_permission_mode.as_str())
    {
        tracing::warn!(
            tool_use_id = %tool_use_id,
            stored_permission_mode = deferred.permission_mode.as_deref().unwrap_or("default"),
            live_permission_mode = %live_permission_mode,
            "resuming deferred tool under a different permission mode; replaying with the live mode"
        );
    }

    let tool_uses = vec![(
        tool_use_id.clone(),
        deferred.tool_name,
        deferred.tool_input,
        None,
    )];
    let (tool_results, _prevent, injected_messages, context_modifiers) =
        telemetry::otel::with_trace_context_future(
            trace_context.as_ref(),
            crate::turn_loop::dispatch_tool_uses_tracked(orch, &tool_uses, None),
        )
        .await?;

    // Applied per tool, before the next one is dispatched: a context modifier
    // is about the model's context, not about message shape, and the following
    // replay has to see it.
    crate::turn_loop::apply_model_context_modifiers(orch, context_modifiers).await;
    Ok(DeferredReplayOutcome {
        tool_use_id,
        tool_results,
        injected_messages,
    })
}

/// Replay all persisted deferred hook tools after a cold or in-place resume.
pub async fn replay_deferred_tools_after_resume(
    orch: &ConversationOrchestrator,
    deferred_tools: Vec<platform_api::DeferredToolReplay>,
) -> Result<(), crate::OrchestratorError> {
    // ONE user message carrying EVERY replayed result, exactly as the normal
    // dispatch path batches a turn's results. Emitting one message per tool
    // would be a wire-shape bug, not a cosmetic one: `ensure_tool_result_pairing`
    // only inspects the single message that FOLLOWS an assistant turn, so with
    // two deferred tools in one assistant message it sees only the first
    // result, synthesizes `[Tool result missing due to internal error]` for the
    // second, and then strips the trailing user message as an orphan — the
    // second tool's real output never reaches the model.
    let mut tool_use_ids: Vec<ToolUseId> = Vec::new();
    let mut tool_results: Vec<ContentBlock> = Vec::new();
    let mut injected_messages: Vec<(ConversationMessage, ToolUseId)> = Vec::new();
    for deferred in deferred_tools {
        let outcome = replay_deferred_tool_after_resume(orch, deferred).await?;
        if outcome.tool_results.is_empty() {
            continue;
        }
        tool_use_ids.push(outcome.tool_use_id);
        tool_results.extend(outcome.tool_results);
        injected_messages.extend(outcome.injected_messages);
    }
    if tool_results.is_empty() {
        return Ok(());
    }

    let tool_results_msg = ConversationMessage::User {
        id: MessageId::new(),
        content: tool_results,
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    {
        let mut session = orch.session.lock().await;
        session.history.push(tool_results_msg.clone());
        for (message, source_id) in &injected_messages {
            session.history.push(message.clone());
            session
                .injected_message_sources
                .insert(message.id(), source_id.clone());
        }
    }
    orch.persist_message_to_jsonl(&tool_results_msg).await;
    for tool_use_id in &tool_use_ids {
        orch.flush_hook_attachments(tool_use_id).await;
    }
    for (message, _source_id) in &injected_messages {
        if message.is_meta() {
            continue;
        }
        orch.persist_message_to_jsonl(message).await;
    }
    Ok(())
}

fn goal_state_from_message(message: &JsonlMessage) -> Option<Option<ActiveGoalState>> {
    if message.message_type == "attachment" {
        let attachment = message.extra.get("attachment")?;
        if attachment.get("type").and_then(serde_json::Value::as_str) == Some("goal_status") {
            let status: platform_api::GoalStatusAttachment =
                serde_json::from_value(attachment.clone()).ok()?;
            // 2.1.266 shape: a SENTINEL announces set (`met:false`) or clear
            // (`met:true`); a non-sentinel record is an evaluation, terminal
            // when it is `met` (achieved) or `failed` (impossible), and
            // otherwise a not-met turn that leaves the goal running.
            let terminal = if status.sentinel == Some(true) {
                status.met
            } else {
                status.met || status.failed == Some(true)
            };
            if terminal {
                return Some(None);
            }
            return status.goal_state.map(|goal| {
                Some(ActiveGoalState {
                    condition: goal.condition,
                    set_at: goal.set_at,
                    last_reason: goal.last_reason,
                    iterations: goal.iterations,
                    tokens_at_start: goal.tokens_at_start,
                    origin: GoalOrigin::Restored,
                })
            });
        }
    }
    if message.message_type != "system" {
        return None;
    }
    if message
        .extra
        .get("subtype")
        .and_then(serde_json::Value::as_str)
        == Some("thread_goal_updated")
    {
        let goal_state = message.extra.get("goalState")?;
        return serde_json::from_value::<Option<ActiveGoalState>>(goal_state.clone())
            .ok()
            .map(restored);
    }
    let compact_metadata = message.extra.get("compactMetadata")?;
    let goal_state = compact_metadata.get("activeGoal")?;
    serde_json::from_value::<Option<ActiveGoalState>>(goal_state.clone())
        .ok()
        .map(restored)
}

/// `mon` (`src_182607998.js`) stamps `origin:"restored"` on the goal it hands
/// back, whichever transcript record it recovered the goal from. `origin` is not
/// part of the persisted shape, so the fold re-derives it here rather than
/// trusting whatever a decode defaulted to.
fn restored(goal: Option<ActiveGoalState>) -> Option<ActiveGoalState> {
    goal.map(|goal| ActiveGoalState {
        origin: GoalOrigin::Restored,
        ..goal
    })
}

/// A scheduled-task fire row — `/loop` wakeups and fixed-schedule fires alike.
/// Written by `conversation::transcript`, and until now read by nothing.
/// Whether this row's model / profile / effort / reasoning describe the
/// session's HUMAN defaults.
///
/// An automatic (scheduled) turn persists the settings IT ran under and tags the
/// row `perTurnSettings`. `docs/cron-task-center.md`: "Automatic assistant
/// transcript rows carry `perTurnSettings` and retain their actual
/// model/provider/reasoning. Session replay keeps these messages in history but
/// excludes their settings when recovering human defaults." Every settings scan
/// below goes through this, because each of them scanned independently and none
/// of them excluded scheduled rows — so a nightly task's model, effort and
/// reasoning became the session's own on the next resume.
fn carries_human_turn_settings(message: &JsonlMessage) -> bool {
    message.message_type == "assistant"
        && !message
            .extra
            .get("perTurnSettings")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn is_scheduled_task_fire(message: &JsonlMessage) -> bool {
    message.message_type == "system"
        && message
            .extra
            .get("subtype")
            .and_then(serde_json::Value::as_str)
            == Some("scheduled_task_fire")
}

fn is_compact_boundary(message: &JsonlMessage) -> bool {
    message.message_type == "system"
        && message
            .extra
            .get("subtype")
            .and_then(serde_json::Value::as_str)
            == Some("compact_boundary")
}

/// Claude `getCurrentUsage` / `getTokenUsage`: scan the effective chain in
/// reverse, skip synthetic responses, and default missing cache counts to zero.
/// Never cross a compact boundary; preserved pre-compact usage is zeroed by
/// the session loader before this projection.
fn current_usage_from_messages(
    messages: &[JsonlMessage],
) -> Option<platform_api::CurrentUsageSnapshot> {
    for message in messages.iter().rev() {
        if is_compact_boundary(message) {
            break;
        }
        if message.is_sidechain
            || message.message_type != "assistant"
            || message.message.get("model").and_then(Value::as_str) == Some("<synthetic>")
        {
            continue;
        }
        let first_text = message
            .message
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str);
        if matches!(first_text, Some(
            "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
            | "No response requested."
            | "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed."
            | "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed."
        )) { continue; }
        if let Some(usage) = message.message.get("usage").filter(|v| v.is_object()) {
            let mut usage = usage.clone();
            // Claude uses `?? 0`, including providers that explicitly send null.
            for field in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
                if usage.get(field).is_none_or(Value::is_null) {
                    usage[field] = Value::from(0);
                }
            }
            if let Ok(usage) = serde_json::from_value(usage) {
                return Some(usage);
            }
        }
    }
    None
}

/// Reconstruct state Claude keeps adjacent to the message array. Boundaries
/// persist the cumulative counter directly; the rapid-refill window is derived
/// from the number of completed assistant iterations between adjacent
/// boundaries and after the latest boundary.
fn resume_runtime_metadata(messages: &[JsonlMessage]) -> ResumeRuntimeMetadata {
    let reasoning_selection = messages.iter().rev().find_map(|m| {
        (carries_human_turn_settings(m)
            && !m
                .extra
                .get("isApiErrorMessage")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false))
        .then(|| {
            m.extra
                .get("reasoningSelection")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
        })
        .flatten()
    });
    let effort = messages.iter().rev().find_map(|m| {
        if !carries_human_turn_settings(m)
            || m.extra
                .get("isApiErrorMessage")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            return None;
        }
        m.extra
            .get("effort")
            .and_then(serde_json::Value::as_str)
            .filter(|value| matches!(*value, "low" | "medium" | "high" | "xhigh" | "max"))
            .map(str::to_owned)
    });

    let boundary_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| is_compact_boundary(message).then_some(index))
        .collect();

    let mut cumulative_dropped_tokens = 0;
    let mut tracking = compaction::AutoCompactTrackingState::default();
    if let Some(&last_boundary) = boundary_indices.last() {
        let compact_metadata = messages[last_boundary].extra.get("compactMetadata");
        cumulative_dropped_tokens = compact_metadata
            .and_then(|metadata| metadata.get("cumulativeDroppedTokens"))
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                let metadata = compact_metadata?;
                let pre = metadata.get("preTokens")?.as_u64()?;
                let post = metadata.get("postTokens")?.as_u64()?;
                Some(pre.saturating_sub(post))
            })
            .unwrap_or(0);

        tracking.compacted = true;
        tracking.turn_counter = u32::try_from(
            messages[last_boundary + 1..]
                .iter()
                .filter(|message| message.message_type == "assistant")
                .count(),
        )
        .unwrap_or(u32::MAX);
        tracking.turn_id = messages[last_boundary + 1..]
            .iter()
            .rev()
            .find(|message| message.message_type == "assistant")
            .map_or_else(
                || messages[last_boundary].uuid.clone(),
                |message| message.uuid.clone(),
            );

        // A first compact stores zero rapid refills. Each later compact whose
        // predecessor is fewer than the configured turn window away increments
        // the consecutive count; a wider interval resets it.
        let mut consecutive = 0_u32;
        for pair in boundary_indices.windows(2) {
            let turns = messages[pair[0] + 1..pair[1]]
                .iter()
                .filter(|message| message.message_type == "assistant")
                .count();
            if turns < compaction::RAPID_REFILL_TURN_WINDOW as usize {
                consecutive = consecutive.saturating_add(1);
            } else {
                consecutive = 0;
            }
        }
        tracking.consecutive_rapid_refills = consecutive;
    }

    ResumeRuntimeMetadata {
        current_usage: current_usage_from_messages(messages),
        effort,
        reasoning_selection,
        main_thread_agent_type: None,
        main_thread_agent_definition: None,
        cumulative_dropped_tokens,
        compaction_tracking: tracking,
        deferred_tools: Vec::new(),
        prompt_snapshot: prompt_snapshot_from_messages(messages),
    }
}

/// Claude Code 2.1.246 `uy`: recover whether the transcript ended inside an
/// open plan-mode segment. The reverse walk gives newer explicit non-plan user
/// modes precedence over older plan markers and treats only successful
/// Enter/ExitPlanMode tool results as state transitions.
fn transcript_has_open_plan_segment(messages: &[JsonlMessage]) -> bool {
    let mut duplicate_tool_uses = HashSet::new();
    let mut seen_tool_uses = HashSet::new();
    for message in messages {
        if message.message_type != "assistant" {
            continue;
        }
        let Some(content) = message.message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                if !seen_tool_uses.insert(id.to_string()) {
                    duplicate_tool_uses.insert(id.to_string());
                }
            }
        }
    }

    let mut successful_tool_results = HashSet::new();
    let mut failed_tool_results = HashSet::new();
    let mut newer_non_plan_mode = false;
    for message in messages.iter().rev() {
        if message.message_type == "attachment" {
            match message
                .extra
                .get("attachment")
                .and_then(|attachment| attachment.get("type"))
                .and_then(Value::as_str)
            {
                Some("plan_mode" | "plan_mode_reentry") => return !newer_non_plan_mode,
                Some("plan_mode_exit") => return false,
                _ => {}
            }
            continue;
        }

        if message.message_type == "assistant" {
            let Some(content) = message.message.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in content.iter().rev() {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let Some(id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = block.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if name == "ExitPlanMode"
                    && successful_tool_results.contains(id)
                    && !failed_tool_results.contains(id)
                    && !duplicate_tool_uses.contains(id)
                {
                    return false;
                }
                if name == "EnterPlanMode"
                    && successful_tool_results.contains(id)
                    && (!failed_tool_results.contains(id) || duplicate_tool_uses.contains(id))
                {
                    return !newer_non_plan_mode;
                }
            }
            continue;
        }

        if message.message_type != "user" {
            continue;
        }
        if let Some(content) = message.message.get("content").and_then(Value::as_array) {
            let mut had_tool_result = false;
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                had_tool_result = true;
                let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                if block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    failed_tool_results.insert(id.to_string());
                } else {
                    successful_tool_results.insert(id.to_string());
                }
            }
            if had_tool_result {
                continue;
            }
        }

        let text = message
            .message
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| {
                message
                    .message
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|content| {
                        content.iter().find_map(|block| {
                            (block.get("type").and_then(Value::as_str) == Some("text"))
                                .then(|| block.get("text").and_then(Value::as_str))
                                .flatten()
                        })
                    })
            });
        if text.is_some_and(|text| {
            text.trim_start()
                .starts_with("<command-name>/plan</command-name>")
        }) {
            return !newer_non_plan_mode;
        }
        match message.extra.get("permissionMode").and_then(Value::as_str) {
            Some("plan") => return !newer_non_plan_mode,
            Some(_)
                if !message
                    .extra
                    .get("isMeta")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
                newer_non_plan_mode = true;
            }
            _ => {}
        }
    }
    false
}

/// Best-effort extraction of `content` from a JSONL `message` payload.
///
/// claude-code stores `message.content` as either:
/// - a string (for simple text-only turns), or
/// - an array of content blocks.
///
/// We accept both: a string becomes a single `ContentBlock::Text`; an array
/// is deserialized after removing malformed text blocks. If neither shape
/// matches, the result is an empty `Vec` — the message is still appended
/// so the chain is preserved, but the inner content is empty.
fn extract_content_blocks(message: &serde_json::Value) -> Vec<ContentBlock> {
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    if let Some(s) = content.as_str() {
        return vec![ContentBlock::Text {
            text: s.to_string(),
        }];
    }
    if let Some(content) = content.as_array() {
        // Claude 2.1.263 nNo drops malformed text blocks on resume before
        // normalizing history. One damaged text block must not erase its
        // otherwise valid siblings (including tool calls and tool results).
        let content = content
            .iter()
            .filter(|block| {
                block.get("type").and_then(Value::as_str) != Some("text")
                    || block.get("text").is_some_and(Value::is_string)
            })
            .cloned()
            .collect();
        if let Ok(blocks) = serde_json::from_value::<Vec<ContentBlock>>(Value::Array(content)) {
            return blocks;
        }
    }
    Vec::new()
}

impl ConversationOrchestrator {
    pub(crate) fn restore_response_usage(&self, usage: Option<platform_api::CurrentUsageSnapshot>) {
        let usage = usage.unwrap_or_default();
        self.compaction_runtime.last_response_input_tokens.store(
            usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.compaction_runtime
            .last_response_output_tokens
            .store(usage.output_tokens, std::sync::atomic::Ordering::Relaxed);
    }

    /// Restore transcript-derived compaction state on an already-built runtime.
    /// Used by the CLI's remount path, which intentionally constructs the
    /// writer for the target session before adopting its history.
    pub async fn restore_resume_runtime_metadata(&self, messages: &[JsonlMessage]) {
        let metadata = resume_runtime_metadata(messages);
        self.restore_response_usage(metadata.current_usage);
        // A resumed transcript is never allowed to manufacture a missing
        // prompt snapshot on its first subsequent request. Adopt the last
        // valid attachment (if any), otherwise leave prompt assembly live.
        self.prompt_runtime
            .prompt_snapshot_resume
            .store(true, std::sync::atomic::Ordering::Release);
        *self.prompt_runtime.prompt_snapshot.lock().await = metadata.prompt_snapshot;
        self.compaction_runtime
            .compaction_cumulative_dropped_tokens
            .store(
                metadata.cumulative_dropped_tokens,
                std::sync::atomic::Ordering::Relaxed,
            );
        *self.compaction_runtime.compaction_tracking.lock().await = metadata.compaction_tracking;
        *self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            post_compact_skill_attachments_from_messages(messages);
    }

    /// Restore only metadata that may live off the resumable model chain.
    ///
    /// CLI resume callers first restore runtime counters from the normalized
    /// chain returned by `load_session`, then read the full routed transcript
    /// again to recover prompt snapshots and skill attachments. Re-running the
    /// full runtime projection on that raw stream would let preserved
    /// pre-compaction assistant rows overwrite the normalized current usage.
    pub async fn restore_resume_prompt_metadata(&self, messages: &[JsonlMessage]) {
        if let Some(snapshot) = prompt_snapshot_from_messages(messages) {
            *self.prompt_runtime.prompt_snapshot.lock().await = Some(snapshot);
        }
        *self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            post_compact_skill_attachments_from_messages(messages);
    }

    /// Construct an orchestrator pre-populated with a replayed session.
    ///
    /// 1. Calls [`replay_session_state`] to load + validate the JSONL.
    /// 2. Constructs the orchestrator (via the existing
    ///    [`Self::new_with_streaming`]) and overrides its internal session +
    ///    `last_jsonl_uuid` with the replayed values.
    /// 3. Sets `config.resume_session_id = Some(session_id)` so downstream
    ///    code can introspect the resume status.
    ///
    /// The new appends emitted by the M5-07 writer chain off
    /// `last_jsonl_uuid` so the parent-uuid chain continues unbroken.
    #[allow(clippy::too_many_arguments)]
    pub async fn with_resume(
        mut config: OrchestratorConfig,
        session_id: Uuid,
        lingxi_home: PathBuf,
        cwd_str: String,
        fs: Arc<dyn FileSystem>,
        api: Arc<dyn OrchestratorApiClient>,
        tools: Arc<ToolRegistry>,
        hooks: Arc<HookExecutor>,
        perms: Arc<dyn PermissionGate>,
        output: Arc<dyn OutputStream>,
        memory: Arc<dyn crate::prompt::MemoryHierarchyProvider>,
        cwd: std::path::PathBuf,
        jsonl_writer: Option<Arc<JsonlWriter>>,
    ) -> Result<Self, ResumeError> {
        config.resume_session_id = Some(session_id);
        let replayed = replay_session_state(&lingxi_home, &cwd_str, session_id, fs.clone()).await?;
        let effort_was_explicit = config.effort.is_some();
        if config.effort.is_none() {
            config.effort.clone_from(&replayed.runtime_metadata.effort);
        }

        // Re-seed dynamic-tool discovery from both surviving tool_reference
        // blocks and compact-boundary carry metadata. Scanning only boundaries
        // loses every tool discovered in a session that has not compacted yet.
        let discovered = session::jsonl::discovered_tool_names(&replayed.messages);
        tools.deferral().replace_loaded(discovered);

        let mut orch = Self::new_with_streaming(
            config,
            api,
            Arc::new(NoStreamingApiClient),
            tools,
            hooks,
            perms,
            output,
            memory,
            cwd,
        );
        // A transcript-inherited effort must remain inheritable if this
        // runtime later hot-resumes another session. Only the caller's
        // pre-resume launch choice pins the value.
        orch.model_runtime
            .current_effort_explicit
            .store(effort_was_explicit, std::sync::atomic::Ordering::Release);
        // Override the auto-generated session + chain pointer with the
        // replayed values. Both fields are `pub(crate)` so this is allowed
        // from a sibling module in the same crate.
        orch.session = Arc::new(Mutex::new(replayed.state));
        orch.restore_response_usage(replayed.runtime_metadata.current_usage);
        orch.sync_thinking_signature_strip_flag_to_api().await;
        orch.transcript.last_jsonl_uuid = Arc::new(Mutex::new(
            replayed.last_message_uuid.map(|u| u.to_string()),
        ));
        orch.compaction_runtime
            .compaction_cumulative_dropped_tokens
            .store(
                replayed.runtime_metadata.cumulative_dropped_tokens,
                std::sync::atomic::Ordering::Relaxed,
            );
        orch.compaction_runtime.compaction_tracking =
            Mutex::new(replayed.runtime_metadata.compaction_tracking);
        *orch
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            post_compact_skill_attachments_from_messages(&replayed.messages);
        orch.prompt_runtime
            .prompt_snapshot_resume
            .store(true, std::sync::atomic::Ordering::Release);
        *orch.prompt_runtime.prompt_snapshot.lock().await =
            replayed.runtime_metadata.prompt_snapshot.clone();
        if let Some(writer) = jsonl_writer {
            orch.transcript.jsonl_writer = Some(writer);
        }
        orch.sync_active_goal_stop_hook_for_current_state().await;
        if !replayed.runtime_metadata.deferred_tools.is_empty() {
            replay_deferred_tools_after_resume(
                &orch,
                replayed.runtime_metadata.deferred_tools.clone(),
            )
            .await
            .map_err(|error| ResumeError::DeferredReplay(error.to_string()))?;
        }
        Ok(orch)
    }
}
