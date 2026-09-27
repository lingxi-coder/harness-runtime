//! `tengu_agent_*` event schemas — 30 events emitted by the agent loop and
//! orchestration layer (M5 owner; M3-06 schema lock).
//!
//! Spec §7 line 757-762. Field sets sourced from the M3-02 plan reference
//! and the `claude-code` @ 6a25909 agent-loop emitters. User-derived string
//! fields are typed [`Verified`](crate::Verified); truncated user content
//! is [`PiiTagged`](crate::PiiTagged).

use crate::pii::{PiiTagged, Verified};
use serde::{Deserialize, Serialize};

// -- Event name constants (byte-locked) ---------------------------------------

/// `tengu_agent_started` — agent loop bootstrapped, before first turn.
pub const STARTED: &str = "tengu_agent_started";
/// `tengu_agent_completed` — agent loop exited normally (stop condition met).
pub const COMPLETED: &str = "tengu_agent_completed";
/// `tengu_agent_failed` — agent loop exited with an unrecoverable error.
pub const FAILED: &str = "tengu_agent_failed";
/// `tengu_agent_cancelled` — caller (or killswitch) cancelled the loop.
pub const CANCELLED: &str = "tengu_agent_cancelled";
/// `tengu_agent_turn_started` — fired before sending the model request for a turn.
pub const TURN_STARTED: &str = "tengu_agent_turn_started";
/// `tengu_agent_turn_completed` — fired after the turn's response/tool cycle finished.
pub const TURN_COMPLETED: &str = "tengu_agent_turn_completed";
/// `tengu_agent_turn_failed` — turn ended in an error (tool or API failure).
pub const TURN_FAILED: &str = "tengu_agent_turn_failed";
/// `tengu_agent_subagent_dispatched` — Task tool spawned a subagent.
pub const SUBAGENT_DISPATCHED: &str = "tengu_agent_subagent_dispatched";
/// `tengu_agent_subagent_completed` — dispatched subagent returned successfully.
pub const SUBAGENT_COMPLETED: &str = "tengu_agent_subagent_completed";
/// `tengu_agent_subagent_failed` — dispatched subagent errored.
pub const SUBAGENT_FAILED: &str = "tengu_agent_subagent_failed";
/// `tengu_agent_memory_loaded` — LINGXI.md / memory files merged into prompt.
pub const MEMORY_LOADED: &str = "tengu_agent_memory_loaded";
/// `tengu_agent_system_prompt_built` — final system prompt was assembled.
pub const SYSTEM_PROMPT_BUILT: &str = "tengu_agent_system_prompt_built";
/// `tengu_agent_persona_resolved` — output style / persona was selected.
pub const PERSONA_RESOLVED: &str = "tengu_agent_persona_resolved";
/// `tengu_agent_compaction_triggered` — compaction decided to fire this turn.
pub const COMPACTION_TRIGGERED: &str = "tengu_agent_compaction_triggered";
/// `tengu_agent_compaction_completed` — compaction summary was applied to history.
pub const COMPACTION_COMPLETED: &str = "tengu_agent_compaction_completed";
/// `tengu_agent_compaction_failed` — compaction errored; the loop fell back to raw history.
pub const COMPACTION_FAILED: &str = "tengu_agent_compaction_failed";
/// `tengu_agent_resume_started` — `/resume` began restoring a prior session.
pub const RESUME_STARTED: &str = "tengu_agent_resume_started";
/// `tengu_agent_resume_completed` — resume succeeded; the loop is ready to continue.
pub const RESUME_COMPLETED: &str = "tengu_agent_resume_completed";
/// `tengu_agent_resume_failed` — resume errored (corrupt state, missing transcript, etc.).
pub const RESUME_FAILED: &str = "tengu_agent_resume_failed";
/// `tengu_agent_fork_started` — fork began (compaction summarizer or side-quest).
pub const FORK_STARTED: &str = "tengu_agent_fork_started";
/// `tengu_agent_fork_completed` — forked agent returned its summary/result.
pub const FORK_COMPLETED: &str = "tengu_agent_fork_completed";
/// `tengu_agent_fork_failed` — forked agent errored.
pub const FORK_FAILED: &str = "tengu_agent_fork_failed";
/// `tengu_agent_state_persisted` — agent state checkpoint was written to disk.
pub const STATE_PERSISTED: &str = "tengu_agent_state_persisted";
/// `tengu_agent_state_loaded` — agent state was rehydrated from disk.
pub const STATE_LOADED: &str = "tengu_agent_state_loaded";
/// `tengu_agent_state_load_failed` — state rehydration errored.
pub const STATE_LOAD_FAILED: &str = "tengu_agent_state_load_failed";
/// `tengu_agent_idle_timeout` — no activity within the idle window; loop self-cancelled.
pub const IDLE_TIMEOUT: &str = "tengu_agent_idle_timeout";
/// `tengu_agent_killswitch_activated` — killswitch tripped (user / engine / remote).
pub const KILLSWITCH_ACTIVATED: &str = "tengu_agent_killswitch_activated";
/// `tengu_agent_loop_iteration` — periodic loop-state snapshot (sampled).
pub const LOOP_ITERATION: &str = "tengu_agent_loop_iteration";
/// `tengu_agent_message_added` — a message was appended to the conversation history.
pub const MESSAGE_ADDED: &str = "tengu_agent_message_added";
/// `tengu_agent_message_truncated` — a message exceeded the per-message cap and was truncated.
pub const MESSAGE_TRUNCATED: &str = "tengu_agent_message_truncated";

// -- AgentTool (claude-code) event-name constants -----------------------------
//
// claude-code's `AgentTool` emits these `tengu_agent_tool_*` (+ two flow-named)
// events. They are the PARITY-faithful names — distinct from LingXi's INTERNAL
// `telemetry::tengu::tool::AGENT_STARTED/AGENT_COMPLETED_M4_05/AGENT_FAILED`
// (`tengu_tool_agent_*`), which are kept for back-compat. The internal names are
// emitted AND these claude names are emitted alongside them (see
// `tools/agent/src/agent.rs`).
//
// These are deliberately NOT added to the count-locked [`NAMES`] / the byte-for-
// byte `ALL_EVENT_NAMES` registry fixture (`tengu_events.json`), which is a
// snapshot of an OLDER claude event set; adding them would break the fixture's
// 363-entry byte-parity lock. They live in [`AGENT_TOOL_NAMES`] for string-lock
// testing only — mirroring how `tool::FILE_READ_ANALYTICS_NAMES` is kept apart.

/// `tengu_agent_tool_selected` — `AgentTool` resolved the agent + model and is
/// about to dispatch (claude `AgentTool.tsx:419`). Fields: `agent_type`, `model`,
/// `source`, `color`, `is_built_in_agent`, `is_resume`, `is_async`, `is_fork`.
pub const TOOL_SELECTED: &str = "tengu_agent_tool_selected";
/// `tengu_agent_tool_completed` — `AgentTool` subagent finished (claude
/// `agentToolUtils.ts:322`). Fields: `agent_type`, `model`, `prompt_char_count`,
/// `response_char_count`, `assistant_message_count`, `total_tool_uses`,
/// `duration_ms`, `total_tokens`, `is_built_in_agent`, `is_async`.
pub const TOOL_COMPLETED: &str = "tengu_agent_tool_completed";
/// `tengu_agent_tool_terminated` — an ASYNC `AgentTool` subagent was killed by
/// the user (claude `agentToolUtils.ts:646`). Fields: `agent_type`, `model`,
/// `duration_ms`, `is_async`, `is_built_in_agent`, `reason:'user_kill_async'`.
pub const TOOL_TERMINATED: &str = "tengu_agent_tool_terminated";
/// `tengu_cache_eviction_hint` — signals inference that a subagent's cache chain
/// can be evicted (claude `agentToolUtils.ts:340`). Fields: `scope:'subagent_end'`,
/// `last_request_id`. (Not `tengu_agent_*`-prefixed, but belongs to the
/// AgentTool flow — placed here per the design.)
pub const CACHE_EVICTION_HINT: &str = "tengu_cache_eviction_hint";
/// `tengu_subagent_output_flagged` — the Agent-tool output guard neutralized
/// control/model-layer tags and/or flagged escalation patterns in a completed
/// subagent's returned text (claude 2.1.212 `tHu`, indirect-prompt-injection
/// hardening). Fields: `agent_id`, `surface:'finalize'`, `patterns` (sorted-
/// unique reportable pattern names, comma-joined), `categories` (sorted-unique),
/// `match_count`. Emitted only when at least one reportable pattern matched.
pub const SUBAGENT_OUTPUT_FLAGGED: &str = "tengu_subagent_output_flagged";
/// `tengu_agent_hooks_origin_untrusted` — an agent definition's frontmatter
/// `hooks:` were SKIPPED because the folder the definition came from is not
/// trusted (cc 2.1.218 `hvo`; the gate is `mvo`). Fields: `source` (the claude
/// `SettingSource` string), `surface` (`"subagent"` | `"mainThread"`),
/// `fromAdditionalDirectory` (`"true"` | `"false"`). 2.1.217 registered these
/// hooks unconditionally, so this event marks the new refusal.
pub const AGENT_HOOKS_ORIGIN_UNTRUSTED: &str = "tengu_agent_hooks_origin_untrusted";
/// `tengu_auto_mode_decision` — the handoff safety classifier's verdict on a
/// subagent's work (claude `agentToolUtils.ts:431`). 13 fields incl. `decision`,
/// `toolName`, `agentType`, `isHandoff:true`, the classifier-stage ids. (Not
/// `tengu_agent_*`-prefixed, but belongs to the AgentTool flow.) Only emitted
/// when the `TRANSCRIPT_CLASSIFIER` feature is ON (OFF by default in Rust).
pub const AUTO_MODE_DECISION: &str = "tengu_auto_mode_decision";

/// `tengu_auto_mode_denial_limit_exceeded` — the auto-mode classifier denial
/// **circuit breaker** tripped (claude `dSm`, binary v2.1.183 offset ~205930031).
/// Fired by [`permission::denial_tracking::DenialTrackingState::trip`] when the
/// consecutive (`>=3`) or total (`>=20`) classifier-denial limit is crossed.
/// Fields: `limit` (`"total"`|`"consecutive"`), `mode` (`"headless"`|`"cli"`,
/// from `shouldAvoidPermissionPrompts`), `messageID`, `consecutiveDenials`,
/// `totalDenials`, `toolName`. Like [`AUTO_MODE_DECISION`] this belongs to the
/// auto-mode/classifier flow and is deliberately NOT in the count-locked
/// [`NAMES`] / `ALL_EVENT_NAMES` registry fixture (a snapshot of an OLDER claude
/// event set). It is also kept OUT of [`AGENT_TOOL_NAMES`] (string-locked to the
/// 5 AgentTool-flow events); its name is byte-locked by its own test.
pub const AUTO_MODE_DENIAL_LIMIT_EXCEEDED: &str = "tengu_auto_mode_denial_limit_exceeded";

// WIZARD-06: the `/auto-mode-setup` wizard telemetry events. Like the two
// auto-mode consts above, these belong to the auto-mode-setup flow and are kept
// OUT of the count-locked [`NAMES`]/`ALL_EVENT_NAMES` registry fixture (an
// older-claude snapshot); each name is byte-locked by its own test.

/// `tengu_auto_mode_setup_wizard_shown` — the `/auto-mode-setup` review UI was
/// presented. Fields include `has_existing` (whether the user already had custom
/// auto-mode rules).
pub const AUTO_MODE_SETUP_WIZARD_SHOWN: &str = "tengu_auto_mode_setup_wizard_shown";

/// `tengu_auto_mode_setup_wizard_answers` — the user's per-category posture
/// answers during the review (fields include `posture`).
pub const AUTO_MODE_SETUP_WIZARD_ANSWERS: &str = "tengu_auto_mode_setup_wizard_answers";

/// `tengu_auto_mode_setup_wizard_resolved` — the wizard finished; `choice`
/// records how it ended (applied / cancelled / …).
pub const AUTO_MODE_SETUP_WIZARD_RESOLVED: &str = "tengu_auto_mode_setup_wizard_resolved";

/// `auto_mode_setup_write` — the non-interactive `auto-mode-setup --apply-file`
/// write attempt. NOTE the name has NO `tengu_` prefix (byte-exact vs 2.1.220,
/// which is inconsistent with the `wizard_*` events above). A `code` field
/// records the outcome: `unknown` (default) / `usage` / `bad_flag_grammar` /
/// `bad_path` / `read_denied` / `read_failed` / `too_large` / `missing_hash_arg`
/// / `bad_hash_arg` / `hash_mismatch` / `parse_failed` / `scope_mismatch` /
/// `write_failed`, plus the save-step codes added in 2.1.220: `invalid_input` /
/// `no_user_settings_path` / `invalid_merged` / `settings_file_invalid` /
/// `permissions_allow_skipped`. Count fields: `autoModeKeysWritten`,
/// `environmentEntriesPreserved`, `permissionsAllowRemoved`,
/// `permissionsAllowNotFound`, `permissionsAllowSkipped`. The code and field
/// spellings are owned (and byte-locked) by `permission::auto_mode_setup`.
pub const AUTO_MODE_SETUP_WRITE: &str = "auto_mode_setup_write";
/// `auto_mode_setup_propose` — the WIZARD-06 propose run's outcome. Like
/// [`AUTO_MODE_SETUP_WRITE`], the name carries NO `tengu_` prefix.
pub const AUTO_MODE_SETUP_PROPOSE: &str = "auto_mode_setup_propose";
/// `auto_mode_pregather` — a WIZARD-06 recon producer degraded. No `tengu_`
/// prefix, like its two siblings.
pub const AUTO_MODE_PREGATHER: &str = "auto_mode_pregather";

/// The 3 `/auto-mode-setup` wizard events, for the string-lock test. Kept
/// separate from [`NAMES`] (the count-locked registry) — see the note above.
pub const AUTO_MODE_SETUP_WIZARD_NAMES: &[&str] = &[
    AUTO_MODE_SETUP_WIZARD_SHOWN,
    AUTO_MODE_SETUP_WIZARD_ANSWERS,
    AUTO_MODE_SETUP_WIZARD_RESOLVED,
];

/// The 5 claude-named `AgentTool` flow events (see the consts above). Kept
/// SEPARATE from [`NAMES`] so the byte-for-byte `ALL_EVENT_NAMES` registry
/// fixture (an older-claude snapshot) stays locked. Used by string-lock tests.
pub const AGENT_TOOL_NAMES: &[&str] = &[
    TOOL_SELECTED,
    TOOL_COMPLETED,
    TOOL_TERMINATED,
    CACHE_EVICTION_HINT,
    AUTO_MODE_DECISION,
];

/// Order-locked array of all 30 names; consumed by `tengu::ALL_EVENT_NAMES`.
pub(crate) const NAMES: &[&str] = &[
    STARTED,
    COMPLETED,
    FAILED,
    CANCELLED,
    TURN_STARTED,
    TURN_COMPLETED,
    TURN_FAILED,
    SUBAGENT_DISPATCHED,
    SUBAGENT_COMPLETED,
    SUBAGENT_FAILED,
    MEMORY_LOADED,
    SYSTEM_PROMPT_BUILT,
    PERSONA_RESOLVED,
    COMPACTION_TRIGGERED,
    COMPACTION_COMPLETED,
    COMPACTION_FAILED,
    RESUME_STARTED,
    RESUME_COMPLETED,
    RESUME_FAILED,
    FORK_STARTED,
    FORK_COMPLETED,
    FORK_FAILED,
    STATE_PERSISTED,
    STATE_LOADED,
    STATE_LOAD_FAILED,
    IDLE_TIMEOUT,
    KILLSWITCH_ACTIVATED,
    LOOP_ITERATION,
    MESSAGE_ADDED,
    MESSAGE_TRUNCATED,
];

// -- Payload structs (deny_unknown_fields locked) -----------------------------

/// Agent dispatch kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentKind {
    /// User-facing top-level agent.
    Main,
    /// Subagent dispatched via the Task tool.
    Subagent,
    /// Forked agent (compaction or side-quest).
    Forked,
}

/// Payload for [`STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Whether this is a main agent, a Task-spawned subagent, or a fork.
    pub agent_kind: AgentKind,
    /// Parent agent id for [`AgentKind::Subagent`] / [`AgentKind::Forked`].
    pub parent_agent_id: Option<Verified>,
    /// Session id this agent runs inside.
    pub session_id: Verified,
}

/// Payload for [`COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Total turns the agent ran.
    pub turns: u32,
    /// Wall-clock duration of the agent in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Final error message (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`CANCELLED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelledPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Why the loop was cancelled (e.g. `user`, `budget`, `hook_stop`).
    pub reason: Verified,
}

/// Payload for [`TURN_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnStartedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// 1-based turn number within the agent.
    pub turn: u32,
}

/// Payload for [`TURN_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCompletedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// 1-based turn number within the agent.
    pub turn: u32,
    /// Wall-clock turn duration in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`TURN_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnFailedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// 1-based turn number within the agent.
    pub turn: u32,
    /// Failure description (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`SUBAGENT_DISPATCHED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentDispatchedPayload {
    /// Parent agent that dispatched the subagent.
    pub parent_agent_id: Verified,
    /// Stable identifier for the spawned subagent.
    pub subagent_id: Verified,
    /// Tool that did the dispatching (typically `Task`).
    pub tool: Verified,
}

/// Payload for [`SUBAGENT_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentCompletedPayload {
    /// Parent agent that dispatched the subagent.
    pub parent_agent_id: Verified,
    /// Stable identifier for the spawned subagent.
    pub subagent_id: Verified,
    /// Wall-clock subagent duration in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`SUBAGENT_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentFailedPayload {
    /// Parent agent that dispatched the subagent.
    pub parent_agent_id: Verified,
    /// Stable identifier for the spawned subagent.
    pub subagent_id: Verified,
    /// Failure description (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`MEMORY_LOADED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryLoadedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Number of memory files merged into the prompt.
    pub files_loaded: u32,
    /// Total bytes loaded across all memory files.
    pub total_bytes: u64,
}

/// Payload for [`SYSTEM_PROMPT_BUILT`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemPromptBuiltPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Token count of the final system prompt.
    pub prompt_tokens: u64,
}

/// Payload for [`PERSONA_RESOLVED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaResolvedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Persona / output-style id selected for this agent.
    pub persona: Verified,
}

/// Payload for [`COMPACTION_TRIGGERED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionTriggeredPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// What caused compaction to fire.
    pub trigger: CompactionTrigger,
    /// Number of messages in history at trigger time.
    pub message_count: u32,
}

/// What caused compaction to fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactionTrigger {
    /// Context window pressure crossed the threshold.
    AutoThreshold,
    /// User ran `/compact`.
    Manual,
    /// Hooks-mandated compaction (`PreCompact` returned an action).
    Hook,
}

/// Payload for [`COMPACTION_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionCompletedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Number of pre-existing messages folded into the summary.
    pub messages_summarized: u32,
    /// Wall-clock duration of compaction in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`COMPACTION_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionFailedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Failure description (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`RESUME_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeStartedPayload {
    /// Stable identifier for this agent instance (the resumed one).
    pub agent_id: Verified,
    /// Session id being resumed from.
    pub from_session_id: Verified,
}

/// Payload for [`RESUME_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeCompletedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Number of historical turns restored from disk.
    pub turns_restored: u32,
}

/// Payload for [`RESUME_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeFailedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Failure description (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`FORK_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkStartedPayload {
    /// Agent that initiated the fork.
    pub parent_agent_id: Verified,
    /// Stable identifier for the forked agent.
    pub fork_agent_id: Verified,
}

/// Payload for [`FORK_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkCompletedPayload {
    /// Agent that initiated the fork.
    pub parent_agent_id: Verified,
    /// Stable identifier for the forked agent.
    pub fork_agent_id: Verified,
    /// Wall-clock duration of the fork in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`FORK_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkFailedPayload {
    /// Agent that initiated the fork.
    pub parent_agent_id: Verified,
    /// Stable identifier for the forked agent.
    pub fork_agent_id: Verified,
    /// Failure description (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`STATE_PERSISTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePersistedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Bytes written to the on-disk checkpoint.
    pub bytes_written: u64,
}

/// Payload for [`STATE_LOADED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateLoadedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Bytes read from the on-disk checkpoint.
    pub bytes_read: u64,
}

/// Payload for [`STATE_LOAD_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateLoadFailedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Failure description (whitelisted, no PII).
    pub error: Verified,
}

/// Payload for [`IDLE_TIMEOUT`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleTimeoutPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Seconds the agent was idle before timing out.
    pub idle_secs: u64,
}

/// Payload for [`KILLSWITCH_ACTIVATED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillswitchActivatedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Who tripped the killswitch.
    pub source: KillswitchSource,
}

/// Who tripped the killswitch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KillswitchSource {
    /// User hit Ctrl-C / UI cancel.
    User,
    /// Engine policy (budget exceeded, hook returned `Stop`).
    Engine,
    /// Org-level remote killswitch (feature flag).
    Remote,
}

/// Payload for [`LOOP_ITERATION`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopIterationPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// 1-based loop iteration counter.
    pub iteration: u32,
    /// Free-form snapshot data (todo counts, tool stats, etc.). The audit
    /// macro whitelists `serde_json::Value` only for this exact field name.
    pub extra: serde_json::Value,
}

/// Payload for [`MESSAGE_ADDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageAddedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// Speaker role for the appended message.
    pub role: MessageRole,
    /// Size of the appended message body in bytes.
    pub content_bytes: u64,
}

/// Message role for [`MessageAddedPayload`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageRole {
    /// End-user input.
    User,
    /// Model output.
    Assistant,
    /// System / persona instructions.
    System,
    /// Tool result message.
    Tool,
}

/// Payload for [`MESSAGE_TRUNCATED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageTruncatedPayload {
    /// Stable identifier for this agent instance.
    pub agent_id: Verified,
    /// PII-tagged: truncated text fragment for diagnostic logging only.
    pub fragment: PiiTagged,
    /// Original message size in bytes (pre-truncation).
    pub original_bytes: u64,
    /// Retained size in bytes (post-truncation).
    pub kept_bytes: u64,
}

#[cfg(test)]
mod agent_tool_event_name_tests {
    use super::*;

    /// String-lock the 5 claude `AgentTool`-flow event names (G11) — byte-for-byte
    /// vs claude-code (`AgentTool.tsx` / `agentToolUtils.ts`).
    #[test]
    fn agent_tool_event_names_are_locked() {
        assert_eq!(TOOL_SELECTED, "tengu_agent_tool_selected");
        assert_eq!(TOOL_COMPLETED, "tengu_agent_tool_completed");
        assert_eq!(TOOL_TERMINATED, "tengu_agent_tool_terminated");
        assert_eq!(CACHE_EVICTION_HINT, "tengu_cache_eviction_hint");
        assert_eq!(AUTO_MODE_DECISION, "tengu_auto_mode_decision");
    }

    /// String-lock the auto-mode classifier denial circuit-breaker event name
    /// (finding #81) — byte-for-byte vs claude-code `dSm` (`j(...)` emit).
    #[test]
    fn auto_mode_denial_limit_event_name_is_locked() {
        assert_eq!(
            AUTO_MODE_DENIAL_LIMIT_EXCEEDED,
            "tengu_auto_mode_denial_limit_exceeded"
        );
    }

    /// WIZARD-06: string-lock the 3 `/auto-mode-setup` wizard event names
    /// byte-for-byte vs 2.1.218.
    #[test]
    fn auto_mode_setup_wizard_event_names_are_locked() {
        assert_eq!(
            AUTO_MODE_SETUP_WIZARD_SHOWN,
            "tengu_auto_mode_setup_wizard_shown"
        );
        assert_eq!(
            AUTO_MODE_SETUP_WIZARD_ANSWERS,
            "tengu_auto_mode_setup_wizard_answers"
        );
        assert_eq!(
            AUTO_MODE_SETUP_WIZARD_RESOLVED,
            "tengu_auto_mode_setup_wizard_resolved"
        );
        assert_eq!(AUTO_MODE_SETUP_WIZARD_NAMES.len(), 3);
        // The apply-file write event has NO tengu_ prefix (byte-exact vs 2.1.218).
        assert_eq!(AUTO_MODE_SETUP_WRITE, "auto_mode_setup_write");
        assert_eq!(AUTO_MODE_SETUP_PROPOSE, "auto_mode_setup_propose");
        assert_eq!(AUTO_MODE_PREGATHER, "auto_mode_pregather");
    }

    #[test]
    fn agent_tool_names_array_is_complete() {
        assert_eq!(AGENT_TOOL_NAMES.len(), 5);
        assert!(AGENT_TOOL_NAMES.contains(&TOOL_SELECTED));
        assert!(AGENT_TOOL_NAMES.contains(&TOOL_COMPLETED));
        assert!(AGENT_TOOL_NAMES.contains(&TOOL_TERMINATED));
        assert!(AGENT_TOOL_NAMES.contains(&CACHE_EVICTION_HINT));
        assert!(AGENT_TOOL_NAMES.contains(&AUTO_MODE_DECISION));
    }
}
