//! Orchestrator-level traits — the public surface that slash commands
//! (via `SlashContext`, M5-09) and the CLI binary (M5-12) consume.
//!
//! The concrete `ConversationOrchestrator` lives in `lingxi-orchestrator`;
//! these traits live here so consumers can depend on them without pulling
//! in the orchestrator (preserves the leaf position of `core::host`).
//!
//! See spec §2.3 (key traits) for the matched design.

use crate::types::SessionId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;
use thiserror::Error;

/// Snapshot of the latest provider rate-limit headers seen on the live path.
///
/// Returned by [`OrchestratorHandle::last_rate_limit_info`] so callers (e.g.
/// the TUI rate-limit status ticker) can inspect all three header values
/// without depending on the orchestrator-internal `RateLimitInfo` struct.
///
/// All fields are `Option<String>` — a missing header means the provider did
/// not send it in the most-recent 2xx response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitSnapshot {
    /// `anthropic-ratelimit-type` (e.g. `"five_hour"`, `"seven_day"`).
    pub rate_limit_type: Option<String>,
    /// Overage status: `"allowed"`, `"allowed_warning"`, or `"rejected"`.
    pub overage_status: Option<String>,
    /// Why overage is disabled (e.g. `"out_of_credits"`) — drives upsell copy
    /// parity. Maps `anthropic-ratelimit-unified-overage-disabled-reason`.
    pub overage_disabled_reason: Option<String>,
}

/// Snapshot of cumulative cost at a single point in time.
///
/// Lightweight echo of `cost::SessionCostSummary` — see that type for
/// the canonical session-scope rollup. We keep a leaf-friendly mirror here
/// so `core::host` does not need to depend on `lingxi-cost`.
///
/// M5-11 added the `total_usd`, `input_tokens`, `output_tokens`, `api_calls`,
/// and `session_duration` fields used by the `/cost` slash command's
/// locked render template. The legacy `total_nano_usd` and `total_tokens`
/// fields remain for back-compat with M5-02's `EndTurn` output event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostSnapshot {
    /// Session whose cost this snapshot describes.
    pub session_id: SessionId,
    /// Cumulative cost in nano-USD (legacy field — still consumed by `EndTurn`).
    pub total_nano_usd: u64,
    /// Cumulative tokens (input + output across all models — legacy field).
    pub total_tokens: u64,
    /// Cumulative cost in USD (4-decimal precision in displays).
    #[serde(default)]
    pub total_usd: f64,
    /// Cumulative input tokens across all turns.
    #[serde(default)]
    pub input_tokens: u64,
    /// Cumulative output tokens across all turns.
    #[serde(default)]
    pub output_tokens: u64,
    /// Cumulative cache-read input tokens across all turns.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Cumulative cache-creation input tokens across all turns.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Cumulative successful `messages_create` calls.
    #[serde(default)]
    pub api_calls: u32,
    /// Elapsed time since the session started.
    #[serde(default)]
    pub session_duration: std::time::Duration,
    /// Cumulative API wall time summed across calls (claude-code `UL` /
    /// `Total duration (API)`). Projected from `CostState.total_api_duration_ms`.
    #[serde(default)]
    pub api_duration: std::time::Duration,
    /// Cumulative lines added across all edits this session (claude-code `RFe`).
    #[serde(default)]
    pub code_lines_added: u64,
    /// Cumulative lines removed across all edits this session (claude-code `xFe`).
    #[serde(default)]
    pub code_lines_removed: u64,
    /// Per-model usage rows for the "Usage by model" block (claude-code `cbg`).
    #[serde(default)]
    pub by_model: Vec<ModelUsageRow>,
    /// True if any used model had no pricing entry — drives the
    /// "(costs may be inaccurate due to usage of unknown models)" note (`Cqo`).
    #[serde(default)]
    pub unknown_models: bool,
    /// Token usage returned by the most recent successful model request.
    /// Unlike the cumulative counters above, this mirrors Claude Code's
    /// `context_window.current_usage` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_usage: Option<CurrentUsageSnapshot>,
    /// The `/cost` prompt-cache line, pre-rendered (CLI-4).
    ///
    /// Rendered by the orchestrator rather than carried as structured data: the
    /// ledger and its renderer both live in `cost`, which depends on this crate
    /// and not the other way round, so mirroring `CacheSummary` here would be a
    /// second copy of a type that exists to be printed once. `None` before the
    /// first API response, and the line is then omitted entirely rather than
    /// shown empty.
    #[serde(default)]
    pub prompt_cache_line: Option<String>,
    /// `/usage` Loops section (oracle `gl()`). Empty means the section is
    /// omitted entirely — it is not a tab.
    #[serde(default)]
    pub loops: Vec<LoopUsageRow>,
}

/// Supplies `/usage` Loops rows (oracle `gl()`). Hosts that run a cron
/// scheduler implement this; others leave it unwired and the section stays hidden.
#[async_trait]
pub trait LoopUsageProvider: Send + Sync {
    /// Current loop/cron usage rows, empty when none have run or been scheduled.
    async fn usage_rows(&self) -> Vec<LoopUsageRow>;
}

/// One scheduled loop/cron row on the `/usage` Loops section.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopUsageRow {
    /// Truncated prompt shown in the first column.
    pub prompt: String,
    /// Human interval (`5m`, `2h`, `at 09:00`, `dynamic`).
    pub every: String,
    /// Completed runs this session.
    pub runs: u64,
    /// Cumulative tokens for those runs.
    pub tokens: u64,
    /// Pre-formatted last-run label (`3m ago`, or `–` when never).
    pub last_run: String,
}

/// Token classes from the most recent successful model response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentUsageSnapshot {
    /// Uncached input tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Input tokens read from cache.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Input tokens written to cache.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

/// Live context-window usage paired with the cumulative billing snapshot.
///
/// `live_context_tokens` is allowed to decrease after compaction.
/// `cumulative_cost` is monotonic for the session and is intentionally kept
/// separate so `/context` cannot accidentally reset `/cost`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextUsageSnapshot {
    /// Estimated tokens in the history that would be sent on the next request.
    #[serde(default)]
    pub live_context_tokens: u64,
    /// Context-window capacity for the active model.
    #[serde(default)]
    pub max_context_tokens: u64,
    /// Stable category rows used by both the headless and interactive
    /// `/context` renderers. Older hosts omit this field and deserialize to an
    /// empty list; consumers must then fall back to the aggregate counters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<ContextUsageCategory>,
    /// Cumulative session usage/cost, unaffected by compaction.
    #[serde(default)]
    pub cumulative_cost: CostSnapshot,
}

impl ContextUsageSnapshot {
    /// Claude Code's explicit over-context warning (2.1.216+).
    ///
    /// `disable_compact` is the raw truthiness of `DISABLE_COMPACT`: callers
    /// pass it in so this wire-level projection stays deterministic and easy
    /// to test.
    #[must_use]
    pub fn overflow_warning(&self, disable_compact: bool) -> Option<String> {
        let over = self
            .live_context_tokens
            .checked_sub(self.max_context_tokens)?;
        if over == 0 || self.max_context_tokens == 0 {
            return None;
        }
        let action = if disable_compact {
            "/clear"
        } else {
            "/compact or /clear"
        };
        Some(format!(
            "Context exceeds the {}-token limit by {} tokens \u{2014} run {action} to continue.",
            compact_token_count(self.max_context_tokens),
            compact_token_count(over)
        ))
    }
}

fn compact_token_count(tokens: u64) -> String {
    const UNITS: [(u64, char); 4] = [
        (1_000_000_000_000, 't'),
        (1_000_000_000, 'b'),
        (1_000_000, 'm'),
        (1_000, 'k'),
    ];
    for (threshold, suffix) in UNITS {
        if tokens >= threshold {
            #[allow(clippy::cast_precision_loss)]
            let rounded = ((tokens as f64 / threshold as f64) * 10.0).round() / 10.0;
            let rendered = format!("{rounded:.1}");
            return format!(
                "{}{suffix}",
                rendered.strip_suffix(".0").unwrap_or(&rendered)
            );
        }
    }
    tokens.to_string()
}

/// One stable category in a [`ContextUsageSnapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageCategory {
    /// Category identity. The enum serializes to a stable snake-case wire value.
    #[serde(default)]
    pub kind: ContextUsageCategoryKind,
    /// Estimated tokens assigned to this category.
    #[serde(default)]
    pub tokens: u64,
}

impl ContextUsageCategory {
    /// Construct one category row.
    #[must_use]
    pub const fn new(kind: ContextUsageCategoryKind, tokens: u64) -> Self {
        Self { kind, tokens }
    }
}

/// Fixed `/context` category identities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextUsageCategoryKind {
    /// Main system prompt, excluding separately-accounted memory files.
    SystemPrompt,
    /// Built-in, plugin, and LSP tool definitions.
    SystemTools,
    /// MCP tool definitions.
    McpTools,
    /// Loaded `LINGXI.md`/memory-file content.
    MemoryFiles,
    /// Skill content attached to the active conversation.
    Skills,
    /// Persisted conversation messages.
    #[default]
    Messages,
    /// Reserved room used by automatic compaction.
    AutocompactBuffer,
    /// Remaining unallocated context capacity.
    FreeSpace,
}

/// SDK/stream-json request to register an additional repository root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRepoRootRequest {
    /// Directory to register (absolute or relative to the live session cwd).
    pub path: String,
    /// Reload instruction files after registration.
    #[serde(default)]
    pub reload_claude_md: bool,
    /// Reconcile the live skill/command catalog after registration.
    #[serde(default)]
    pub reload_skills: bool,
    /// Reconcile plugins and their MCP servers after registration.
    #[serde(default)]
    pub reload_plugins: bool,
}

/// Folded result of a `DirectoryAdded` hook dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryAddedHookSummary {
    /// Number of hooks that errored, timed out, or were cancelled.
    #[serde(default)]
    pub failure_count: u32,
    /// Bounded messages injected into session context.
    #[serde(default)]
    pub context_messages: Vec<String>,
}

/// Result of registering an additional repository root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRepoRootOutcome {
    /// Canonical directory registered in the session.
    pub directory: PathBuf,
    /// `true` when the trusted-root/sandbox set changed.
    #[serde(default)]
    pub added: bool,
    /// DirectoryAdded hook outcome.
    #[serde(default)]
    pub hooks: DirectoryAddedHookSummary,
    /// The skill catalog completed a live reconciliation.
    #[serde(default)]
    pub skills_reloaded: bool,
    /// The plugin catalog completed a live reconciliation.
    #[serde(default)]
    pub plugins_reloaded: bool,
    /// Non-fatal catalog reconciliation failures.
    #[serde(default)]
    pub reload_errors: Vec<String>,
}

/// Session-scoped `/goal` state surfaced through [`OrchestratorHandle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveGoalSnapshot {
    /// User-supplied goal condition.
    pub condition: String,
    /// When the goal became active.
    pub set_at: SystemTime,
    /// Most recent stop-time evaluation reason, when available.
    pub last_reason: Option<String>,
    /// Number of Stop evaluations completed since the goal was set.
    #[serde(default)]
    pub iterations: u64,
    /// Cumulative session tokens at activation time.
    #[serde(default)]
    pub tokens_at_start: u64,
}

/// Why a goal was torn down — the `reason` field of `tengu_goal_cleared`.
///
/// Upstream 2.1.267 emits the event through one helper that takes the reason as
/// an argument (`src_161508826.js`):
///
/// ```js
/// function kB(e,t){i("tengu_goal_cleared",{reason:u(t),iterations:e.iterations,
///   durationMs:Date.now()-e.setAt,origin:we(e.origin)})}
/// ```
///
/// and its call sites pass six distinct values. This enum is that argument, so
/// a teardown cannot silently report somebody else's reason:
///
/// | call site | reason |
/// |---|---|
/// | `/goal clear` (Stop hook removed) | `user_clear` |
/// | a new `/goal` over a live one | `superseded` |
/// | `kB(e,d==="context_limit"?…)` | `context_limit` / `api_error` |
/// | session clear | `session_clear` |
/// | resume replacing a restored goal | `resume_swap` |
///
/// ⚠️ This is the TELEMETRY reason and is independent of
/// [`GoalStatusKind`], which is the transcript attachment's shape. A
/// context-limit teardown still writes a `Cleared` attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalClearedReason {
    /// The user asked for it — `/goal clear` and friends.
    UserClear,
    /// A new goal replaced a live one.
    Superseded,
    /// Torn down because the context window ran out.
    ContextLimit,
    /// Torn down by a non-transient provider error.
    ApiError,
    /// The whole session was cleared.
    SessionClear,
    /// A resume replaced a goal restored from the transcript.
    ResumeSwap,
}

impl GoalClearedReason {
    /// The exact wire string upstream passes as `kB`'s second argument.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserClear => "user_clear",
            Self::Superseded => "superseded",
            Self::ContextLimit => "context_limit",
            Self::ApiError => "api_error",
            Self::SessionClear => "session_clear",
            Self::ResumeSwap => "resume_swap",
        }
    }
}

/// Which `goal_status` record to write.
///
/// NOT serialized any more: the 2.1.266 wire shape distinguishes the variants
/// with the `met`/`failed`/`sentinel` booleans on [`GoalStatusAttachment`], so
/// this enum is only the instruction to the builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatusKind {
    /// A goal was activated or replaced — `bKt(!1, condition)`.
    Set,
    /// The user explicitly cleared the goal — `bKt(!0, condition)`.
    Cleared,
    /// The Stop evaluator accepted the goal.
    Achieved,
    /// The Stop evaluator established that the goal is impossible.
    Failed,
    /// The condition did not hold this turn; the goal stays active.
    NotMet,
}

/// Typed, resumable `/goal` transcript attachment — the 2.1.266 shape.
///
/// Upstream emits four variants of `{type:"goal_status", …}` and they are told
/// apart by booleans, not by a status enum (`src_160454523.js` `bKt`, and the
/// evaluator @4211500 / @4212300):
///
/// | variant | fields |
/// |---|---|
/// | goal SET (sentinel) | `met:false, sentinel:true, condition` |
/// | goal CLEARED (sentinel) | `met:true, sentinel:true, condition` |
/// | achieved | `met:true, condition, reason, iterations, durationMs, tokens` |
/// | impossible | `met:false, failed:true, condition, reason, iterations, durationMs, tokens` |
/// | not met (this turn) | `met:false, condition, reason` |
///
/// `fRn` reads the transcript back for the "Goal achieved" panel by scanning for
/// the last record with `met && !sentinel`, which is why the sentinels must be
/// distinguishable from a real evaluation.
///
/// ## Divergence (reason)
///
/// [`Self::goal_state`] is a LingXi addition with no upstream counterpart: the
/// port restores an active goal on resume by reading it back out of this
/// attachment. It is `skip_serializing_if = "Option::is_none"`, so it never
/// appears on the variants that carry no goal to restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalStatusAttachment {
    /// Attachment discriminator.
    #[serde(rename = "type")]
    pub kind: String,
    /// `met` — the condition held (achieved), or, on a sentinel, that the goal
    /// is ENDING rather than starting.
    pub met: bool,
    /// `failed` — present and true only on the impossible-condition record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    /// `sentinel` — present and true on the set/clear announcements, which carry
    /// no evaluation of their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel: Option<bool>,
    /// User-supplied condition.
    pub condition: String,
    /// `reason` — the evaluator's stop reason. Absent on sentinels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Number of Stop evaluations performed. Absent on sentinels and on the
    /// not-met record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<u64>,
    /// Elapsed wall time since activation, on terminal records only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Tokens consumed since activation, on terminal records only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// LingXi-only: full active state for lossless resume. See the type docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_state: Option<ActiveGoalSnapshot>,
}

impl GoalStatusAttachment {
    fn new(condition: String, met: bool) -> Self {
        Self {
            kind: "goal_status".to_string(),
            met,
            failed: None,
            sentinel: None,
            condition,
            reason: None,
            iterations: None,
            duration_ms: None,
            tokens: None,
            goal_state: None,
        }
    }

    /// `bKt(!1, condition)` — the goal was just SET.
    #[must_use]
    pub fn sentinel_set(condition: String, goal_state: Option<ActiveGoalSnapshot>) -> Self {
        Self {
            sentinel: Some(true),
            goal_state,
            ..Self::new(condition, false)
        }
    }

    /// `bKt(!0, condition)` — the goal was just CLEARED.
    #[must_use]
    pub fn sentinel_cleared(condition: String) -> Self {
        Self {
            sentinel: Some(true),
            ..Self::new(condition, true)
        }
    }

    /// The evaluator confirmed the condition.
    #[must_use]
    pub fn achieved(
        condition: String,
        reason: Option<String>,
        iterations: u64,
        duration_ms: u64,
        tokens: u64,
    ) -> Self {
        Self {
            reason,
            iterations: Some(iterations),
            duration_ms: Some(duration_ms),
            tokens: Some(tokens),
            ..Self::new(condition, true)
        }
    }

    /// The evaluator declared the condition impossible.
    #[must_use]
    pub fn failed(
        condition: String,
        reason: Option<String>,
        iterations: u64,
        duration_ms: u64,
        tokens: u64,
    ) -> Self {
        Self {
            failed: Some(true),
            reason,
            iterations: Some(iterations),
            duration_ms: Some(duration_ms),
            tokens: Some(tokens),
            ..Self::new(condition, false)
        }
    }

    /// The condition did not hold this turn; the goal stays active.
    #[must_use]
    pub fn not_met(
        condition: String,
        reason: Option<String>,
        goal_state: Option<ActiveGoalSnapshot>,
    ) -> Self {
        Self {
            reason,
            goal_state,
            ..Self::new(condition, false)
        }
    }
}

/// One deferred hook tool that must be replayed after a session is resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredToolReplay {
    /// Original tool-use id from the persisted hook attachment.
    pub tool_use_id: String,
    /// Tool registry name to dispatch after resume.
    pub tool_name: String,
    /// Hook-updated tool input to replay.
    pub tool_input: serde_json::Value,
    /// Permission mode recorded when the hook deferred the tool.
    pub permission_mode: Option<String>,
    /// W3C traceparent captured at deferral time.
    pub traceparent: Option<String>,
}

/// The static prompt and inline tool descriptions captured by the optional
/// carved-slate prompt-cache path.  The field names intentionally follow the
/// transcript attachment schema (`systemPrompt` / `tools`); the aliases keep
/// resume tolerant of older host snapshots that used Rust-style names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptToolDescription {
    /// Tool name as advertised to the model.
    pub name: String,
    /// Description frozen when the tool first appeared inline.
    pub description: String,
}

/// Persisted prompt snapshot used to keep the static prompt prefix stable
/// across turns and process resumes. Dynamic system context is recomputed by
/// the orchestrator and is deliberately not stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSnapshot {
    /// Static system prompt members, joined by the provider adapter.
    #[serde(rename = "systemPrompt", alias = "system_prompt", default)]
    pub system_prompt: Vec<String>,
    /// Non-deferred inline tool descriptions in first-seen order.
    #[serde(
        rename = "tools",
        alias = "recordedToolDescriptions",
        alias = "toolDescriptions",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tools: Vec<PromptToolDescription>,
}

/// Transcript-adjacent runtime state needed by an in-place session resume.
///
/// This leaf-friendly mirror deliberately uses primitive fields instead of
/// depending on `session` or `compaction` (both depend on this crate). Without
/// it, bridge/mobile hot-resume adopted the messages but silently reset compact
/// bookkeeping and lost transcript-visibility flags already reconstructed by
/// the JSONL loader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeRuntimeSnapshot {
    /// Latest real assistant usage in the restored, post-compaction chain.
    pub current_usage: Option<CurrentUsageSnapshot>,
    /// Resolved session model to adopt on in-place resume. Empty means "leave
    /// the current live model unchanged" for legacy/default callers.
    pub model: String,
    /// Provider profile that resolved `model` when it was persisted. `None`
    /// preserves compatibility with transcripts written before profile
    /// metadata was recorded and lets the registry resolve globally by id.
    pub model_profile: Option<String>,
    /// Resolved transcript effort level, when the persisted session carried
    /// one. Present for parity plumbing; runtimes that cannot live-mutate their
    /// provider adapter may ignore it.
    pub effort: Option<String>,
    /// Resolved transcript reasoning selection, when the persisted session
    /// carried one. Legacy transcripts may leave this absent and use `effort`.
    pub reasoning_selection: Option<ReasoningSelection>,
    /// Persisted main-thread agent type for this session. `None` means the
    /// resumed session used default main-thread behavior.
    pub main_thread_agent_type: Option<String>,
    /// Integrity-checked immutable agent definition restored from the
    /// transcript. Hosts may leave this absent for legacy transcripts; the
    /// orchestrator then falls back to its live catalog using
    /// [`Self::main_thread_agent_type`].
    pub main_thread_agent_definition: Option<serde_json::Value>,
    /// Message ids hidden from the normal conversation projection.
    pub transcript_only_message_ids: Vec<crate::types::MessageId>,
    /// Message ids that are compact summaries.
    pub compact_summary_message_ids: Vec<crate::types::MessageId>,
    /// Persisted UI/meta messages that must never be replayed into a model
    /// request. Fusion completion envelopes use this lane.
    pub model_context_excluded_message_ids: Vec<crate::types::MessageId>,
    /// Tool names restored from compact-boundary ToolSearch metadata.
    pub loaded_tool_names: Vec<String>,
    /// Exact skill bodies associated with persisted post-compact attachment
    /// messages. The message id keeps dedup structural across hot resume;
    /// bodies remain opaque Markdown and are never delimiter-parsed.
    pub post_compact_skill_attachments: Vec<(crate::types::MessageId, Vec<String>)>,
    /// Persisted cumulative token count discarded by compaction.
    pub cumulative_dropped_tokens: u64,
    /// Whether this session has compacted at least once.
    pub compacted: bool,
    /// Turns since the most recent compact.
    pub turn_counter: u32,
    /// Identifier of the turn that most recently compacted.
    pub turn_id: String,
    /// Consecutive failed autocompact attempts.
    pub consecutive_failures: u32,
    /// Consecutive rapid-refill compactions.
    pub consecutive_rapid_refills: u32,
    /// Deferred hook tools persisted without a corresponding tool result.
    pub deferred_tools: Vec<DeferredToolReplay>,
    /// Static prompt/tool-description snapshot recovered from the transcript.
    /// Resume consumers must adopt it as-is and never create one while
    /// replaying a session.
    pub prompt_snapshot: Option<PromptSnapshot>,
}

/// One model's cumulative usage for the `/usage` "Usage by model" block
/// (claude-code `cbg`). Mirrors `cost::summary::ModelCostSummary`'s numeric
/// fields; defined here (not reused from `cost`) because `traits` cannot
/// depend on `cost` — that would be a dependency cycle (`cost` depends on
/// `traits`). `cost::render::usage_by_model_block` consumes `&[ModelUsageRow]`
/// directly.
///
/// The row carries only the model NAME (`model`) plus a pre-stringified
/// serving-provider id, not a full `cost::ModelRef`: `ModelRef`/`ProviderId`
/// live in `cost::pricing`, not `protocol`, so a typed field would require the
/// very `cost` dependency this type exists to avoid — and the renderer's label
/// is `${model}:` (just the name String), so the name is all the transport
/// needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsageRow {
    /// Model name for the row label (claude-code uses `${model}:`).
    pub model: String,
    /// Serving API provider for the result-frame `modelUsage.provider` field
    /// (cc 2.1.218 `n.provider=n_(r)`; open string — `"firstParty"` for the
    /// Anthropic first-party API, `"bedrock"` for Bedrock, LingXi provider
    /// names otherwise). `None` when the recording site cannot attribute one
    /// (legacy aggregate rows); the field is then omitted, matching the zod
    /// `.optional()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Cumulative cost for this model in nano-USD.
    pub total_nano_usd: u64,
    /// Cumulative input tokens for this model.
    pub input_tokens: u64,
    /// Cumulative output tokens for this model.
    pub output_tokens: u64,
    /// Cumulative tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    /// Cumulative tokens written into the prompt cache.
    pub cache_creation_input_tokens: u64,
}

/// Result of a `force_compact` operation. M5-10 wires `/compact` against
/// this surface; M5-02 only defines the type for forward compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    /// Number of messages in the session BEFORE compaction.
    pub messages_before: u32,
    /// Number of messages in the session AFTER compaction.
    pub messages_after: u32,
    /// Approximate bytes saved (summary token count delta × 4, as a UX
    /// estimate — exact accounting lives in `lingxi-compaction`).
    pub bytes_saved: u64,
    /// Full model-generated continuation summary retained after compaction.
    pub summary: String,
}

/// Outcome of a successful [`OrchestratorHandle::fork_conversation`] — the
/// spawned background agent's display name and its full agent id. `/fork`
/// renders `"⑂ forked {name} ({id-tail})"` from these two fields (the tail is
/// the last four chars of `agent_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOutcome {
    /// Human-readable name of the spawned background agent.
    pub name: String,
    /// Full agent id; `/fork` shows only its last four characters.
    pub agent_id: String,
}

/// Outcome of an [`OrchestratorHandle::generate_recap`] side query — the
/// read-only, tool-denied, single-turn recap primitive `/recap` renders.
///
/// `Text` carries the model's trimmed recap text (or, when the forked query
/// itself surfaced an API-error assistant message, that error's own text — the
/// runner returns it as `final_text` either way, so no branching is needed).
/// `Cancelled` maps to the fixed `"Recap cancelled."` line. A "no qualifying
/// turn yet" case is deliberately NOT a variant here: `/recap`'s handler gates
/// that against [`OrchestratorHandle::conversation_transcript`] BEFORE calling,
/// and any internal failure surfaces as `Err(HandleError)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecapOutcome {
    /// The recap text to display (already trimmed).
    Text(String),
    /// The caller aborted the recap mid-flight → the fixed cancellation line.
    Cancelled,
}

/// The current session's on-disk plan, when the plan tool has written one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshot {
    /// Resolved plan file path (including `plansDirectory` overrides).
    pub path: PathBuf,
    /// UTF-8 Markdown body.
    pub content: String,
}

/// One `/rewind` restore-point row: a user turn that has a file-history
/// checkpoint (claude-code `MessageSelector` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindRowData {
    /// The user message uuid (the checkpoint key the restore paths take).
    pub message_uuid: uuid::Uuid,
    /// A one-line preview of the user prompt.
    pub preview: String,
    /// A short dim label (turn ordinal / relative time).
    pub timestamp_label: String,
    /// Whether restoring to this point would change any file on disk
    /// (claude-code `fileHistoryHasAnyChanges`).
    pub has_code_changes: bool,
}

/// Errors surfaced through the orchestrator's public handle.
///
/// Distinct from `orchestrator::OrchestratorError` because the
/// handle surface deliberately hides the API-error variants from slash
/// command authors (they cannot meaningfully act on a 429). Implementations
/// MAY wrap `OrchestratorError` and project a coarse `HandleError`.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum HandleError {
    /// The requested action could not be completed (e.g. `clear` during a
    /// turn-in-flight). The payload is a human-readable reason.
    #[error("handle action failed: {0}")]
    ActionFailed(String),
    /// The requested operation is not implemented on this handle.
    /// Used by the default `OrchestratorHandle::run_turn_streaming_with_cancel`
    /// impl (M6-03) so existing handle implementations (M5-13 stdio REPL
    /// path) don't need to override.
    #[error("operation not implemented: {0}")]
    Unimplemented(String),
}

/// Outcome of a TUI-driven turn invoked via
/// [`OrchestratorHandle::run_turn_streaming_with_cancel`]. (M6-03)
///
/// Mirrors `orchestrator::TurnOutcome` so the trait surface in
/// `core::host` does not depend on the orchestrator crate. Map between
/// the two in `orchestrator::handle_impl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Model returned a natural stop reason and the turn loop ended.
    EndTurn,
    /// The orchestrator's `max_turns` budget was reached before `end_turn`.
    MaxTurns,
    /// The cancel token fired mid-turn; the orchestrator unwound the
    /// current API call and returned early.
    Cancelled,
}

/// Result of [`OrchestratorHandle::open_memory_editor`] (M5-10).
///
/// Returned to `/memory`'s handler so it can render the locked
/// `"Edited {path} (exit {code})."` template. Carries the path the editor
/// was launched against (which may have been created if absent) and the
/// editor process's exit code (0 on success; non-zero on user cancel /
/// editor error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEditorOutcome {
    /// The LINGXI.md path that was edited (may have been created if absent).
    pub edited_path: PathBuf,
    /// Exit code of the spawned `$EDITOR` process. 0 = success.
    pub exit_code: i32,
}

// ────────────────────────────────────────────────────────────────────────────
// M5-11 info structs (used by `/mcp`, `/hooks`, `/agents`, `/status`, `/doctor`)
// ────────────────────────────────────────────────────────────────────────────

/// One MCP server entry returned by [`OrchestratorHandle::list_mcp_servers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    /// Server name as registered in settings.
    pub name: String,
    /// Connection status at snapshot time.
    pub status: McpStatus,
    /// Transport kind: `"stdio"`, `"sse"`, or `"http"`.
    pub transport: String,
}

/// One skill entry returned by [`OrchestratorHandle::list_skills`].
///
/// Skills are discovered from `skills/` directories, not configured
/// key-by-key — this is a VIEW of what the loader found on disk, not an
/// editable settings row.
///
/// Deliberately carries no `plugin` field: nothing on any LIVE path can
/// populate one today (the file-based scan has no plugin provenance, and the
/// only Rust-side thing that models plugins — the composition root's
/// `SkillRegistry` — is a residual always-empty instance with no turn-loop
/// consumer). Add the field back when a real producer exists; that is
/// additive and needs no wire-version bump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    /// Skill display name (matches its directory name, not frontmatter).
    pub name: String,
    /// The skill's own directory on disk (e.g. `<root>/skills/<name>`).
    pub source_dir: std::path::PathBuf,
}

/// Connection status for an MCP server in [`McpServerInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStatus {
    /// Connected and healthy.
    Connected,
    /// Disconnected — either never connected or cleanly shut down.
    Disconnected,
    /// Connection failed with the wrapped reason.
    Error(String),
}

/// Fine-grained MCP server state for the `/mcp reconnect|enable|disable` action
/// handler — a faithful mirror of claude-code 2.1.206's MCP client `type`
/// discriminant (`f8e`/`R3s`). The coarser [`McpStatus`] projection (used by
/// `/status` and the `/mcp` listing) collapses several of these into
/// `Disconnected`; the action handler needs the full vocabulary to pick the
/// byte-exact state-aware message (e.g. a `Disabled` server routes to
/// "…enable it first", a `Pending` one to "…already reconnecting").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpActionState {
    /// Connected and healthy (claude `"connected"`).
    Connected,
    /// Still connecting / reconnecting (claude `"pending"`).
    Pending,
    /// Turned off via the `disabledMcpjsonServers` gate (claude `"disabled"`).
    Disabled,
    /// Tried to connect and failed / never connected (claude `"failed"`).
    Failed,
    /// Awaiting OAuth authentication (claude `"needs-auth"`).
    NeedsAuth,
    /// Awaiting manual approval before it may connect (claude `"needs-approval"`).
    /// No LingXi connection state currently produces this; the variant exists so
    /// the ported handler's approval branch stays byte-faithful and inert.
    NeedsApproval,
}

impl McpActionState {
    /// Human-readable label — a byte-exact port of claude-code's `pGd` map,
    /// used inside the reconnect failure message `(… ${pGd[k]} …)`.
    pub fn label(self) -> &'static str {
        match self {
            McpActionState::Connected => "connected",
            McpActionState::Pending => "connecting",
            McpActionState::Disabled => "disabled",
            McpActionState::Failed => "not connected",
            McpActionState::NeedsAuth => "needs authentication",
            McpActionState::NeedsApproval => "pending approval",
        }
    }
}

/// Post-op status of a single MCP server toggled by
/// [`OrchestratorHandle::set_mcp_servers_disabled`] — the port's analog of one
/// settled entry in claude-code's `Promise.allSettled(p.map(u))`. Only servers
/// that were NOT already in the requested state appear (claude's `p` filter — a
/// server already enabled/disabled is omitted), so an empty result marks the
/// "already enabled/disabled" no-op branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToggleOutcome {
    /// Server name.
    pub name: String,
    /// The resulting action state when the per-server toggle settled
    /// successfully (claude's fulfilled `{type}` — after enable this is the
    /// live post-connect state, e.g. `Connected` / `Failed` / `NeedsAuth`),
    /// or `None` when the per-server op was rejected (claude's rejected
    /// promise — the server "couldn't be changed", counting toward the
    /// aggregate `E` tally).
    pub state: Option<McpActionState>,
}

/// One hook entry returned by [`OrchestratorHandle::list_hooks`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookInfo {
    /// Hook identifier.
    pub name: String,
    /// Hook event (e.g. `"PreToolUse"`, `"PostToolUse"`, `"Stop"`, `"Notification"`).
    pub event: String,
    /// Optional matcher regex (tool-name pattern).
    pub matcher: Option<String>,
    /// Timeout in milliseconds (default `60_000` if unset).
    pub timeout_ms: u64,
    /// (hooks-detail-fields-divergent) Executor kind (claude-code
    /// `config.type`): `"command"` / `"http"` / `"agent"` / `"prompt"`, or the
    /// LingXi-only `"builtin"` (an in-process Rust handler; no TS analogue).
    pub hook_type: String,
    /// (hooks-detail-fields-divergent) Human-readable origin (claude-code
    /// `hookSourceDescriptionDisplayString`), e.g. `"User settings
    /// (~/.lingxi/settings.json)"`.
    pub source: String,
    /// (hooks-detail-fields-divergent) The executor's primary content field
    /// (claude-code `getContentFieldValue`): the shell command line for
    /// `"command"`, the URL for `"http"`, the prompt for `"agent"`/`"prompt"`,
    /// the handler id for the LingXi-only `"builtin"`.
    pub content: String,
    /// (hooks-detail-fields-divergent) Custom status message shown while the
    /// hook runs, if the definition set one.
    pub status_message: Option<String>,
    /// Whether the hook blocks the foreground action.
    pub blocking: bool,
    /// Priority used to order hooks within one event.
    pub priority: i32,
    /// Whether this hook is configured as background/async.
    pub async_rewake: bool,
    /// Async timeout in milliseconds, if present.
    pub async_timeout_ms: Option<u64>,
    /// The raw `if` condition pattern, if present.
    pub if_condition: Option<String>,
}

/// One subagent entry returned by [`OrchestratorHandle::list_agents`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentInfo {
    /// Agent name (matches the markdown filename without extension).
    pub name: String,
    /// Human-readable description (may be truncated by callers).
    pub description: String,
    /// Tool allow-list. Meaningful only when `wildcard_tools` is `false`;
    /// empty here then means "no tools", not "all tools" (agents-03).
    pub tools_allowed: Vec<String>,
    /// `true` when the agent's tool policy is `AgentToolPolicy::All`
    /// (claude-code: `tools` frontmatter omitted) — distinguishes "every
    /// tool" from an explicit empty allow-list, which `tools_allowed` alone
    /// cannot (both lower to an empty `Vec`).
    pub wildcard_tools: bool,
    /// (agents-08) Source-group display label (claude-code
    /// `AGENT_SOURCE_GROUPS`): `"User agents"`, `"Project agents"`, `"Local
    /// agents"`, `"Managed agents"`, `"Plugin agents"`, `"CLI arg agents"`,
    /// or `"Built-in agents"`. Drives the `/agents` list's section grouping.
    /// Empty string defaults rows into the trailing built-in section.
    pub source_group: String,
}

/// Aggregate diagnostic report returned by [`OrchestratorHandle::run_doctor_checks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorReport {
    /// Individual check results in execution order.
    pub checks: Vec<DoctorCheck>,
    /// Summary tallies (pass/warn/fail counts).
    pub summary: DoctorSummary,
}

/// One `/doctor` check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    /// Check identifier (`"config-dir"`, `"api-key"`, etc.).
    pub name: String,
    /// Pass/warn/fail outcome.
    pub status: CheckStatus,
    /// Optional detail string (rendered on a second indented line if `Some`).
    pub detail: Option<String>,
}

/// Outcome of a single [`DoctorCheck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// Check succeeded.
    Pass,
    /// Check produced a warning (non-fatal anomaly).
    Warn,
    /// Check failed.
    Fail,
}

/// Pass/warn/fail tallies in a [`DoctorReport`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorSummary {
    /// Number of checks that returned [`CheckStatus::Pass`].
    pub passed: u32,
    /// Number of checks that returned [`CheckStatus::Warn`].
    pub warnings: u32,
    /// Number of checks that returned [`CheckStatus::Fail`].
    pub failed: u32,
}

/// Snapshot returned by [`OrchestratorHandle::get_status_snapshot`] for the
/// `/status` panel. All fields are populated synchronously at snapshot time.
///
/// Note: `Eq` is intentionally not derived because `total_cost_usd: f64` does
/// not implement `Eq`. Use `PartialEq` for assertions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusSnapshot {
    /// Current session id (as a stable string for display).
    pub session_id: String,
    /// Active model name (e.g. `"claude-opus-4-7"`).
    pub model: String,
    /// Active model's provider profile (e.g. `Some("copilot")`), disambiguating a
    /// model id shared across providers (e.g. `gpt-5.6-sol` on both OpenAI and
    /// Copilot). `None` when routing resolves the id by-provider (e.g. after a
    /// cross-provider resume that clears the profile).
    pub model_profile: Option<String>,
    /// Total messages in the session history.
    pub n_messages: u32,
    /// Cumulative cost in USD.
    pub total_cost_usd: f64,
    /// Cumulative input tokens.
    pub input_tokens: u64,
    /// Cumulative output tokens.
    pub output_tokens: u64,
    /// MCP servers currently in `Connected` state.
    pub n_mcp_connected: u32,
    /// MCP servers configured (any state).
    pub n_mcp_total: u32,
    /// Hooks registered.
    pub n_hooks: u32,
    /// Subagents available.
    pub n_agents: u32,
    /// Session start time, RFC 3339 (`"YYYY-MM-DDTHH:MM:SSZ"`, UTC).
    pub started_at: String,
    /// Working directory used to launch the session.
    pub cwd: PathBuf,
    /// Active coordinator-team workers at snapshot time (T21).
    ///
    /// `0` for a non-coordinator session (the `Default`), so existing
    /// constructors that use `..Default::default()` keep their behavior. A
    /// coordinator session surfaces the live `TeamRegistry::active_worker_count`
    /// here so `/status` can echo the same scalar the PUSH
    /// `CoordinatorStatus` feed carries.
    pub active_workers: u32,
    /// (settings-status-missing-mcp-and-setting-sources) Display strings for
    /// every settings-file tier that currently has a file on disk (claude-code
    /// `buildSettingSourcesProperties`'s `sourcesWithSettings` filter), e.g.
    /// `"Project settings (.lingxi/settings.json)"`. Empty when none exist
    /// (the `/status` row is omitted entirely, matching TS).
    pub setting_sources: Vec<String>,
}

/// How a provider charges the user for one model route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelBillingMode {
    /// Published per-token prices apply.
    PerToken,
    /// Usage is governed by a subscription or membership plan.
    Subscription,
    /// The provider explicitly publishes the route as free.
    Free,
    /// No reliable billing semantics are available.
    #[default]
    Unknown,
}

/// One alternate price sheet activated above a context-size threshold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub struct ModelPricingTier {
    /// Provider-published threshold in input/context tokens.
    pub context_threshold_tokens: u64,
    #[serde(default)]
    pub input_per_million: Option<f64>,
    #[serde(default)]
    pub output_per_million: Option<f64>,
    #[serde(default)]
    pub cache_read_per_million: Option<f64>,
    #[serde(default)]
    pub cache_write_per_million: Option<f64>,
    #[serde(default)]
    pub reasoning_per_million: Option<f64>,
}

/// Provider-published prices for one model route, in USD per million tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub struct ModelPricing {
    #[serde(default)]
    pub billing_mode: ModelBillingMode,
    #[serde(default)]
    pub input_per_million: Option<f64>,
    #[serde(default)]
    pub output_per_million: Option<f64>,
    #[serde(default)]
    pub cache_read_per_million: Option<f64>,
    #[serde(default)]
    pub cache_write_per_million: Option<f64>,
    #[serde(default)]
    pub reasoning_per_million: Option<f64>,
    #[serde(default)]
    pub tiers: Vec<ModelPricingTier>,
    /// `modelsDev`, `official`, or `userOverride` when known.
    #[serde(default)]
    pub source: Option<String>,
}

/// Published, provider-specific model facts used by every model picker.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub struct ModelMetadata {
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub knowledge_cutoff: Option<String>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub open_weights: Option<bool>,
    #[serde(default)]
    pub attachments: Option<bool>,
    #[serde(default)]
    pub temperature_control: Option<bool>,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

/// Provider-neutral capabilities displayed beside model metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub vision: bool,
    pub documents: bool,
    pub reasoning: bool,
    pub structured_output: bool,
}

/// Provider-neutral source of a model's active/default selection.
///
/// Keeping this separate from provider identity avoids inferring organization
/// ownership from a provider name or auth mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProvenance {
    /// A managed policy supplied or forced the model selection.
    ManagedAdministratorDefault,
    /// A user, CLI, or environment setting supplied the selection.
    UserOrEnv,
    /// The built-in/provider catalog supplied the selection.
    ProviderCatalogTier,
}

impl Default for ModelProvenance {
    fn default() -> Self {
        Self::ProviderCatalogTier
    }
}

impl ModelProvenance {
    /// Whether this provenance represents an administrator-managed model.
    #[must_use]
    pub const fn is_managed(self) -> bool {
        matches!(self, Self::ManagedAdministratorDefault)
    }
}

/// Split a profile name of the form `"<group>[:<connection>][#<slot>]"`.
///
/// Returns the group plus, when present, the connection id and the zero-based
/// key-slot index. A provider reachable one way yields `(name, None, None)`, so
/// every profile written before connections existed takes the unchanged path in
/// callers.
///
/// `#N` counts as a key slot only when `N` parses as a number, so a provider
/// literally named `foo#bar` keeps its whole name instead of losing half of it.
/// Shared by the TUI connect picker and the desktop provider header so the two
/// cannot drift into labelling the same profile differently.
#[must_use]
pub fn split_connection_profile(profile_name: &str) -> (&str, Option<&str>, Option<usize>) {
    let (head, slot) = match profile_name.rsplit_once('#') {
        Some((head, index)) => match index.parse::<usize>() {
            Ok(index) => (head, Some(index)),
            Err(_) => (profile_name, None),
        },
        None => (profile_name, None),
    };
    match head.split_once(':') {
        Some((group, connection)) if !group.is_empty() && !connection.is_empty() => {
            (group, Some(connection), slot)
        }
        _ => (head, None, slot),
    }
}

/// Which provider GROUP a route's profile belongs to.
///
/// A provider can be reachable several ways — a domestic and an international
/// host, or several API keys — and each of those is a separate profile with its
/// own credential. They are one vendor, and this is what says so.
///
/// `provider_id` deliberately stays the PROFILE name: it keys the availability
/// map, the picker's recents, and `provider_has_curated_list`, and it is what a
/// saved `profile/model` reference names. Grouping is additive on top.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRef {
    /// The vendor this profile is a connection of. `None` ⇒ it stands alone and
    /// callers should read `provider_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Connection id within the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

impl ConnectionRef {
    /// Whether this is the default (standalone) reference.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.group.is_none() && self.connection_id.is_none()
    }
}

/// and `provider_label` is the human provider header (e.g. "`GitHub` Copilot").
/// The active/default row carries its selection provenance separately in the
/// TUI model snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelListing {
    /// Human-facing model label (e.g. "`DeepSeek` Chat").
    pub display_model: String,
    /// Provider-local wire model id (what `switch_model` accepts).
    pub request_model: String,
    /// Stable provider key for grouping + recents (the catalog profile name).
    pub provider_id: String,
    /// Human provider header (e.g. "`DeepSeek`", "`GitHub` Copilot").
    pub provider_label: String,
    /// Optional one-line model description, rendered as a dimmed line beneath the
    /// row in the `/model` picker (claude-code `ListItem` renders it under the
    /// label with `paddingLeft={2}` + `color="inactive"`). `None` ⇒ no extra line.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether this model supports extended thinking / reasoning. Surfaced in the
    /// `/model` picker so a NON-thinking model is flagged — a session thinking
    /// budget silently does not apply to it (the surprising case; most models DO
    /// think). `#[serde(default)]` (`false`) keeps older serialized listings
    /// deserializing unchanged.
    #[serde(default)]
    pub supports_reasoning: bool,
    /// Full provider-specific catalog metadata. Missing values stay unknown.
    #[serde(default)]
    pub metadata: ModelMetadata,
    /// Capabilities used by route preflight and picker badges.
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// The exact reasoning controls accepted by this provider/model route.
    #[serde(default)]
    pub reasoning: ReasoningControlSpec,
    /// Whether this route could serve as the Fusion analyst — the model claims
    /// structured output AND its profile's codec can encode a `response_format`.
    /// The `/fusion setup` analyst picker gates on this so an operator cannot
    /// name a judge that fails only after every panel has already spent.
    #[serde(default)]
    pub fusion_analyst_capable: bool,
    /// The provider group this route's profile belongs to, when it is one
    /// connection of several.
    #[serde(default, skip_serializing_if = "ConnectionRef::is_default")]
    pub connection: ConnectionRef,
}

/// One provider section in the shared client-side model directory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelCatalogEntry {
    /// Stable profile/provider id (`anthropic`, `openai`, custom profile name, …).
    pub provider_id: String,
    /// Human-facing provider label shown in settings.
    pub provider_label: String,
    /// Provider-scoped model rows that remain eligible for conversation UIs.
    pub models: Vec<ModelListing>,
    /// The vendor this entry is one connection of, when it is. Settings UIs
    /// collapse entries sharing a group under one provider heading; the picker
    /// keeps them separate, because two connections can differ in billing and
    /// the user has to be able to choose.
    #[serde(default, skip_serializing_if = "ConnectionRef::is_default")]
    pub connection: ConnectionRef,
}

/// Provider-neutral user selection for reasoning / effort controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSelection {
    /// No user override; let the provider/model default apply.
    Automatic,
    /// Explicitly disable reasoning when the provider supports it.
    Disabled,
    /// Explicitly enable reasoning when the provider supports a bare toggle.
    Enabled,
    /// One provider-defined discrete reasoning level.
    Level { id: String },
    /// One provider-defined numeric reasoning budget.
    TokenBudget { tokens: u64 },
}

/// Numeric budget constraints for one reasoning control surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningBudgetRange {
    /// Minimum accepted token budget.
    pub min_tokens: u32,
    /// Maximum accepted token budget.
    pub max_tokens: u32,
    /// Whether a dynamic/provider-managed budget is supported.
    pub supports_dynamic: bool,
    /// Whether an explicit off/zero budget is supported.
    pub supports_disabled: bool,
}

/// Provider capability description for the active model's reasoning control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningControlSpec {
    /// Supported discrete/toggle selections for this model.
    pub available: Vec<ReasoningSelection>,
    /// Whether a chosen value may be persisted as the user's default.
    pub selections_persistable: bool,
    /// Optional numeric budget range when the provider exposes one.
    pub budget_range: Option<ReasoningBudgetRange>,
    /// The provider/model default selection when no override is sent.
    pub provider_default: ReasoningSelection,
    /// Whether the provider always reasons for this model.
    pub forced: bool,
    /// Whether the user may currently change the setting.
    pub modifiable: bool,
    /// Optional reason a control is currently disabled.
    pub disabled_reason: Option<String>,
}

impl Default for ReasoningControlSpec {
    fn default() -> Self {
        Self {
            available: vec![ReasoningSelection::Automatic],
            selections_persistable: false,
            budget_range: None,
            provider_default: ReasoningSelection::Automatic,
            forced: false,
            modifiable: false,
            disabled_reason: Some("reasoning_unavailable".to_string()),
        }
    }
}

/// Availability of one requested permission mode in the current session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionModeAvailability {
    /// Permission-mode wire id.
    pub mode: String,
    /// Whether the mode may currently be selected.
    pub available: bool,
    /// Optional reason this mode is unavailable.
    pub disabled_reason: Option<String>,
}

/// Authoritative permission state for the active session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionControlState {
    /// The user's requested mode.
    pub requested: String,
    /// The mode effectively applied by the engine.
    pub effective: String,
    /// Availability for every surfaced mode.
    pub modes: Vec<PermissionModeAvailability>,
}

/// Authoritative controls snapshot for the active conversation model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationControls {
    /// Provider-qualified active model reference.
    pub model_reference: String,
    /// Permission control state.
    pub permission: PermissionControlState,
    /// User-requested reasoning selection before model capability validation.
    pub requested_reasoning_selection: ReasoningSelection,
    /// Current reasoning selection after validation/reset.
    pub effective_reasoning_selection: ReasoningSelection,
    /// Provider/model reasoning capability description.
    pub reasoning_spec: ReasoningControlSpec,
}

fn level(id: &str) -> ReasoningSelection {
    ReasoningSelection::Level { id: id.to_string() }
}

fn auto_only_reasoning_spec() -> ReasoningControlSpec {
    ReasoningControlSpec {
        available: vec![ReasoningSelection::Automatic],
        selections_persistable: true,
        budget_range: None,
        provider_default: ReasoningSelection::Automatic,
        forced: false,
        modifiable: false,
        disabled_reason: Some("reasoning_unavailable".to_string()),
    }
}

fn supports_selection(spec: &ReasoningControlSpec, selection: &ReasoningSelection) -> bool {
    match selection {
        ReasoningSelection::Automatic => true,
        ReasoningSelection::TokenBudget { tokens } => {
            spec.budget_range.as_ref().is_some_and(|range| {
                (*tokens >= range.min_tokens as u64 && *tokens <= range.max_tokens as u64)
                    || (range.supports_disabled && *tokens == 0)
            })
        }
        other => spec.available.iter().any(|candidate| candidate == other),
    }
}

/// Provider/model reasoning controls for one active model reference.
#[must_use]
pub fn reasoning_control_spec_for_model(
    model: &str,
    provider_id: Option<&str>,
) -> ReasoningControlSpec {
    let provider_id = provider_id.unwrap_or_default();
    let model = model.to_ascii_lowercase();

    match provider_id {
        "anthropic" | "builtin" => {
            let levels = if model.contains("haiku") {
                vec![
                    ReasoningSelection::Automatic,
                    level("low"),
                    level("medium"),
                    level("high"),
                ]
            } else if model.contains("sonnet-5")
                || model.contains("opus-4-8")
                || model.contains("opus-5")
                || model.contains("fable-5-1")
            {
                vec![
                    ReasoningSelection::Automatic,
                    level("low"),
                    level("medium"),
                    level("high"),
                    level("xhigh"),
                    level("max"),
                ]
            } else {
                vec![
                    ReasoningSelection::Automatic,
                    level("low"),
                    level("medium"),
                    level("high"),
                    level("xhigh"),
                ]
            };
            ReasoningControlSpec {
                available: levels,
                selections_persistable: true,
                budget_range: None,
                provider_default: level("high"),
                forced: false,
                modifiable: true,
                disabled_reason: None,
            }
        }
        "openai" | "openai-chatgpt" => {
            let (levels, can_disable) = match model.as_str() {
                "gpt-5" | "gpt-5-mini" | "gpt-5-nano" => {
                    (vec!["minimal", "low", "medium", "high"], false)
                }
                "gpt-5-pro" => (vec!["high"], false),
                // Astra rejects `reasoning.effort: "none"` outright (the model
                // page lists its efforts as low/medium/high/xhigh/max), so the
                // explicit Disabled selection must stay off this row even
                // though its siblings can be turned off.
                "gpt-6-astra" => (vec!["low", "medium", "high", "xhigh", "max"], false),
                "gpt-5.1"
                | "gpt-5.2"
                | "gpt-5.3-codex-spark"
                | "gpt-5.4"
                | "gpt-5.4-mini"
                | "gpt-5.4-nano"
                | "gpt-5.5"
                | "gpt-5.6"
                | "gpt-5.6-sol"
                | "gpt-5.6-terra"
                | "gpt-5.6-luna" => (vec!["low", "medium", "high", "xhigh", "max"], true),
                "gpt-5.1-codex" | "gpt-5.1-codex-mini" | "gpt-5.2-codex" => {
                    (vec!["low", "medium", "high"], false)
                }
                "gpt-5.1-codex-max" | "gpt-5.2-pro" | "gpt-5.4-pro" | "gpt-5.5-pro" => {
                    (vec!["medium", "high", "xhigh"], false)
                }
                _ => return auto_only_reasoning_spec(),
            };
            let mut available = vec![ReasoningSelection::Automatic];
            if can_disable {
                available.push(ReasoningSelection::Disabled);
            }
            available.extend(levels.into_iter().map(level));
            ReasoningControlSpec {
                available,
                selections_persistable: true,
                budget_range: None,
                provider_default: level("medium"),
                forced: false,
                modifiable: true,
                disabled_reason: None,
            }
        }
        "gemini" => {
            if model.starts_with("gemini-2.5-pro") {
                ReasoningControlSpec {
                    available: vec![ReasoningSelection::Automatic, ReasoningSelection::Disabled],
                    selections_persistable: true,
                    budget_range: Some(ReasoningBudgetRange {
                        min_tokens: 128,
                        max_tokens: 32_768,
                        supports_dynamic: true,
                        supports_disabled: true,
                    }),
                    provider_default: ReasoningSelection::Automatic,
                    forced: false,
                    modifiable: true,
                    disabled_reason: None,
                }
            } else if model.starts_with("gemini-2.5-flash-lite") {
                ReasoningControlSpec {
                    available: vec![ReasoningSelection::Automatic],
                    selections_persistable: true,
                    budget_range: Some(ReasoningBudgetRange {
                        min_tokens: 512,
                        max_tokens: 24_576,
                        supports_dynamic: true,
                        supports_disabled: false,
                    }),
                    provider_default: ReasoningSelection::Automatic,
                    forced: true,
                    modifiable: true,
                    disabled_reason: None,
                }
            } else if model.starts_with("gemini-2.5-flash") {
                ReasoningControlSpec {
                    available: vec![ReasoningSelection::Automatic, ReasoningSelection::Disabled],
                    selections_persistable: true,
                    budget_range: Some(ReasoningBudgetRange {
                        min_tokens: 0,
                        max_tokens: 24_576,
                        supports_dynamic: true,
                        supports_disabled: true,
                    }),
                    provider_default: ReasoningSelection::Automatic,
                    forced: false,
                    modifiable: true,
                    disabled_reason: None,
                }
            } else if model.starts_with("gemini-3") {
                let levels: &[&str] = if model.contains("pro-image") {
                    &[]
                } else if model.contains("pro-preview") && !model.contains("3.1") {
                    &["low", "high"]
                } else if model.contains("3.1-pro") {
                    &["low", "medium", "high"]
                } else if model.contains("image-preview") {
                    &["minimal", "high"]
                } else {
                    &["minimal", "low", "medium", "high"]
                };
                if levels.is_empty() {
                    ReasoningControlSpec {
                        available: vec![ReasoningSelection::Automatic],
                        selections_persistable: true,
                        budget_range: None,
                        provider_default: ReasoningSelection::Automatic,
                        forced: true,
                        modifiable: false,
                        disabled_reason: Some("reasoning_required".to_string()),
                    }
                } else {
                    ReasoningControlSpec {
                        available: std::iter::once(ReasoningSelection::Automatic)
                            .chain(levels.iter().copied().map(level))
                            .collect(),
                        selections_persistable: true,
                        budget_range: None,
                        provider_default: ReasoningSelection::Automatic,
                        forced: true,
                        modifiable: true,
                        disabled_reason: None,
                    }
                }
            } else {
                auto_only_reasoning_spec()
            }
        }
        "deepseek" => {
            if model == "deepseek-reasoner" {
                ReasoningControlSpec {
                    available: vec![ReasoningSelection::Enabled],
                    selections_persistable: false,
                    budget_range: None,
                    provider_default: ReasoningSelection::Enabled,
                    forced: true,
                    modifiable: false,
                    disabled_reason: Some("reasoning_required".to_string()),
                }
            } else if matches!(
                model.as_str(),
                "deepseek-flash" | "deepseek-v4-pro" | "deepseek-chat"
            ) {
                ReasoningControlSpec {
                    available: vec![
                        ReasoningSelection::Automatic,
                        ReasoningSelection::Disabled,
                        level("high"),
                        level("max"),
                    ],
                    selections_persistable: true,
                    budget_range: None,
                    provider_default: ReasoningSelection::Automatic,
                    forced: false,
                    modifiable: true,
                    disabled_reason: None,
                }
            } else {
                auto_only_reasoning_spec()
            }
        }
        "kimi" | "kimi-code" => {
            if matches!(model.as_str(), "kimi-k3" | "k3" | "k3-256k") {
                ReasoningControlSpec {
                    available: vec![
                        ReasoningSelection::Automatic,
                        level("low"),
                        level("high"),
                        level("max"),
                    ],
                    selections_persistable: true,
                    budget_range: None,
                    provider_default: level("high"),
                    forced: false,
                    modifiable: true,
                    disabled_reason: None,
                }
            } else if model.contains("k2-thinking") || model.contains("kimi-k2-thinking") {
                ReasoningControlSpec {
                    available: vec![ReasoningSelection::Enabled],
                    selections_persistable: false,
                    budget_range: None,
                    provider_default: ReasoningSelection::Enabled,
                    forced: true,
                    modifiable: false,
                    disabled_reason: Some("reasoning_required".to_string()),
                }
            } else if model.contains("kimi-for-coding") || model.contains("k2.7-code") {
                ReasoningControlSpec {
                    available: vec![ReasoningSelection::Enabled],
                    selections_persistable: false,
                    budget_range: None,
                    provider_default: ReasoningSelection::Enabled,
                    forced: true,
                    modifiable: false,
                    disabled_reason: Some("reasoning_required".to_string()),
                }
            } else if model.contains("k2.6") {
                ReasoningControlSpec {
                    available: vec![
                        ReasoningSelection::Automatic,
                        ReasoningSelection::Disabled,
                        ReasoningSelection::Enabled,
                    ],
                    selections_persistable: true,
                    budget_range: None,
                    provider_default: ReasoningSelection::Automatic,
                    forced: false,
                    modifiable: true,
                    disabled_reason: None,
                }
            } else {
                auto_only_reasoning_spec()
            }
        }
        _ => auto_only_reasoning_spec(),
    }
}

/// Validate a reasoning selection for the active model. Incompatible values
/// reset directly to automatic rather than nearest-mapping.
#[must_use]
pub fn validated_reasoning_selection_for_model(
    selection: &ReasoningSelection,
    model: &str,
    provider_id: Option<&str>,
) -> ReasoningSelection {
    let spec = reasoning_control_spec_for_model(model, provider_id);
    if spec.forced && !spec.modifiable {
        return spec.provider_default;
    }
    if supports_selection(&spec, selection) {
        selection.clone()
    } else {
        ReasoningSelection::Automatic
    }
}

/// Parse a (possibly `profile/model`) text reference against the live model
/// listings into `(request_model, profile)`.
///
/// Qualified only when the first `/`-segment is a known profile (`provider_id`)
/// AND the remainder is a `request_model` under that profile; otherwise the
/// whole string is returned as a bare id (so openrouter ids that contain `/`,
/// e.g. `openai/gpt-4o`, route as bare ids). `profile = None` ⇒ unscoped.
#[must_use]
pub fn parse_model_ref(input: &str, listings: &[ModelListing]) -> (String, Option<String>) {
    if let Some((prefix, rest)) = input.split_once('/') {
        if listings
            .iter()
            .any(|l| l.provider_id == prefix && l.request_model == rest)
        {
            return (rest.to_string(), Some(prefix.to_string()));
        }
    }
    (input.to_string(), None)
}

/// The curated "latest few" models surfaced per provider in the `/model` picker
/// and the no-arg `/model` listing. claude-code hand-picks a short list
/// (`utils/model/modelOptions.ts`) instead of dumping the whole catalog (~460
/// models, 338 of them OpenRouter); we mirror that. Keyed by the stable
/// `provider_id` (the catalog profile name, e.g. `"glm-coding"`, NOT the slice
/// filename) + the wire `request_model`. Wire ids can differ between provider
/// profiles, so both values are part of the key. Any provider/model not listed
/// is non-curated;
/// callers keep the user's current + recent models visible separately.
/// OpenRouter is restricted to its auto/latest aliases so its several-hundred
/// model passthrough catalog never floods the picker.
///
/// Shared by the TUI picker (`build_model_entries`/`is_shown_model`) and the
/// mobile/CLI listings so the whitelist has ONE source of truth.
#[must_use]
pub fn is_curated_model(provider_id: &str, request_model: &str) -> bool {
    match provider_id {
        "anthropic" | "builtin" => matches!(
            request_model,
            "claude-opus-5" | "claude-fable-5-1" | "claude-sonnet-5" | "claude-haiku-4-5"
        ),
        "openai" => matches!(
            request_model,
            "gpt-6-astra" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
        ),
        "openai-chatgpt" => matches!(
            request_model,
            "gpt-6-astra" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
        ),
        "deepseek" => matches!(request_model, "deepseek-flash" | "deepseek-v4-pro"),
        "kimi" => request_model == "kimi-k3",
        "kimi-code" => request_model == "k3",
        "gemini" => matches!(
            request_model,
            "gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash" | "gemini-3.1-pro-preview"
        ),
        "github-copilot" => matches!(
            request_model,
            "claude-opus-5"
                | "claude-sonnet-5"
                | "gemini-3.7-flash"
                | "gpt-5.6-sol"
                | "gpt-5.6-terra"
                | "gpt-5.6-luna"
        ),
        "zai" => matches!(request_model, "glm-5.3" | "glm-5.3-flash"),
        // The profile name is "glm-coding" (catalog presets); "zhipuai-coding-plan"
        // is only the vendored slice's filename.
        "glm-coding" => request_model == "glm-5.3",
        "openrouter" => {
            request_model == "openrouter/auto"
                || request_model == "openrouter/free"
                || request_model == "openai/gpt-chat-latest"
                || request_model.starts_with('~')
                || matches!(
                    request_model,
                    "cohere/north-mini-code:free"
                        | "z-ai/glm-5.2:free"
                        | "thinkingmachines/inkling:free"
                        | "thinkingmachines/inkling-small:free"
                        | "minimax/minimax-m3:free"
                        | "minimax/minimax-m2.7:free"
                        | "poolside/laguna-s-2.1:free"
                        | "inclusionai/ling-3.0-flash-fin:free"
                        | "nvidia/nemotron-3.5-lightning:free"
                        | "nvidia/nemotron-3-ultra-550b-a55b:free"
                )
        }
        _ => false,
    }
}

/// Whether `provider_id` has a curated shortlist in [`is_curated_model`] (an
/// explicit `match` arm). The `/model` picker trims curated-managed providers to
/// their shortlist, but for a connected custom provider with no arm there is no
/// meaningful shared "latest few", so the picker keeps that provider's own
/// catalog instead of hiding every entry. OpenRouter is deliberately managed
/// here because exposing its full aggregator catalog overwhelms every picker.
/// Keep this arm set in lockstep with [`is_curated_model`].
#[must_use]
pub fn provider_has_curated_list(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "anthropic"
            | "builtin"
            | "openai"
            | "openai-chatgpt"
            | "deepseek"
            | "kimi"
            | "kimi-code"
            | "gemini"
            | "github-copilot"
            | "zai"
            | "glm-coding"
            | "openrouter"
    )
}

/// Deterministic provider preference order for the boot-time connected-provider
/// default-model fallback (LingXi multi-provider divergence): when the
/// configured default model's provider is not connected at startup, the engine
/// picks the first CONNECTED provider in this order and boots on its
/// [`provider_default_model`]. Anthropic (the native route) ranks first; the
/// first-class API providers follow in the [`is_curated_model`] arm order;
/// OpenRouter — an aggregator passthrough — is the last resort.
#[must_use]
pub fn provider_fallback_order() -> &'static [&'static str] {
    &[
        "anthropic",
        "openai",
        "openai-chatgpt",
        "deepseek",
        "kimi",
        "kimi-code",
        "gemini",
        "github-copilot",
        "zai",
        "glm-coding",
        "openrouter",
    ]
}

/// The boot-default model for `provider_id` — the model a session lands on when
/// the connected-provider fallback (or a future onboarding flow) picks that
/// provider without an explicit user choice. For curated providers this is the
/// first-listed id of the [`is_curated_model`] shortlist; OpenRouter gets its
/// curated `auto` meta-router. `None` for unknown/user-defined providers —
/// callers fall back to the provider's own first listed model.
#[must_use]
pub fn provider_default_model(provider_id: &str) -> Option<&'static str> {
    Some(match provider_id {
        "anthropic" | "builtin" => "claude-sonnet-5",
        "openai" => "gpt-5.6-sol",
        "openai-chatgpt" => "gpt-5.6-sol",
        "deepseek" => "deepseek-flash",
        "kimi" => "kimi-k3",
        "kimi-code" => "k3",
        "gemini" => "gemini-3.7-flash",
        "github-copilot" => "claude-opus-5",
        "zai" => "glm-5.3",
        "glm-coding" => "glm-5.3",
        "openrouter" => "openrouter/auto",
        _ => return None,
    })
}

/// Build the stable model reference used by client model pickers.
///
/// A provider-qualified reference preserves routing identity when two providers
/// expose the same wire model id. Bare ids remain supported by
/// [`parse_model_ref`] for compatibility, but new listing surfaces should emit
/// qualified references whenever a provider profile is known.
#[must_use]
pub fn qualified_model_ref(request_model: &str, provider_id: Option<&str>) -> String {
    provider_id
        .filter(|provider| !provider.is_empty())
        .map_or_else(
            || request_model.to_string(),
            |provider| format!("{provider}/{request_model}"),
        )
}

/// Curate provider-qualified model references for shared client listing
/// surfaces (`ClientEvent::ModelList`, `/model` text command).
///
/// The result contains only the shared "latest and commonly used" shortlist,
/// plus the active model so the picker can always represent its current value.
/// Provider identity is never de-duplicated away: `openai/gpt-5.6-sol` and
/// `github-copilot/gpt-5.6-sol` are distinct choices and route deterministically.
///
/// When `listings` is empty (library/stub callers with no routing catalog), the
/// raw `available` list is preserved for backward compatibility.
#[must_use]
pub fn curated_model_refs(
    listings: &[ModelListing],
    available: &[String],
    current: &str,
    current_provider_id: Option<&str>,
) -> Vec<String> {
    if listings.is_empty() {
        let mut models = available.to_vec();
        if !current.is_empty() && current_provider_id.is_some() {
            let current_ref = qualified_model_ref(current, current_provider_id);
            if let Some(index) = models.iter().position(|model| model == current) {
                models[index] = current_ref;
            } else if !models.contains(&current_ref) {
                models.insert(0, current_ref);
            }
        }
        return models;
    }

    let curated = curated_model_listings(listings, current, current_provider_id);
    let mut refs = curated
        .into_iter()
        .map(|listing| qualified_model_ref(&listing.request_model, Some(&listing.provider_id)))
        .collect::<Vec<_>>();
    if !current.is_empty() {
        let inferred = current_provider_id.or_else(|| {
            let mut providers = listings
                .iter()
                .filter(|listing| listing.request_model == current)
                .map(|listing| listing.provider_id.as_str());
            let first = providers.next();
            matches!((first, providers.next()), (Some(_), None))
                .then_some(first)
                .flatten()
        });
        let current_ref = qualified_model_ref(current, inferred);
        if let Some(index) = refs.iter().position(|item| item == &current_ref) {
            if index != 0 {
                let current = refs.remove(index);
                refs.insert(0, current);
            }
        } else {
            refs.insert(0, current_ref);
        }
    }
    refs
}

/// Curate full provider-specific listings using the same ordering and current
/// model carve-out as [`curated_model_refs`].
#[must_use]
pub fn curated_model_listings(
    listings: &[ModelListing],
    current: &str,
    current_provider_id: Option<&str>,
) -> Vec<ModelListing> {
    let inferred_current_provider = if current_provider_id.is_none() && !current.is_empty() {
        let mut matches = listings
            .iter()
            .filter(|listing| listing.request_model == current)
            .map(|listing| listing.provider_id.as_str());
        let first = matches.next();
        match (first, matches.next()) {
            (Some(provider), None) => Some(provider),
            _ => None,
        }
    } else {
        None
    };
    let current_provider_id = current_provider_id.or(inferred_current_provider);

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if !current.is_empty() {
        if let Some(current_listing) = listings.iter().find(|listing| {
            listing.request_model == current
                && current_provider_id.is_none_or(|provider| listing.provider_id == provider)
        }) {
            seen.insert((
                current_listing.provider_id.clone(),
                current_listing.request_model.clone(),
            ));
            out.push(current_listing.clone());
        }
    }
    for listing in listings {
        if is_offered_model(&listing.provider_id, &listing.request_model)
            && listing.metadata.status.as_deref() != Some("deprecated")
        {
            let key = (listing.provider_id.clone(), listing.request_model.clone());
            if seen.insert(key) {
                out.push(listing.clone());
            }
        }
    }
    out
}

/// Filter model listings for the shared provider-settings directory.
///
/// This applies the same deprecated and provider-curation rules as the picker,
/// but keeps the natural provider/model order instead of injecting or moving a
/// session-specific current model.
#[must_use]
pub fn provider_model_catalog_listings(listings: &[ModelListing]) -> Vec<ModelListing> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for listing in listings {
        if !listing.capabilities.tools
            || !is_offered_model(&listing.provider_id, &listing.request_model)
            || listing.metadata.status.as_deref() == Some("deprecated")
        {
            continue;
        }
        let key = (listing.provider_id.clone(), listing.request_model.clone());
        if seen.insert(key) {
            out.push(listing.clone());
        }
    }
    out
}

/// Whether a listing belongs in a client's model picker.
///
/// For a provider the shared catalog curates, that means the "latest few"
/// shortlist. For one it does NOT curate — a user-defined proxy, a self-hosted
/// endpoint, any profile invented in settings — there is no shortlist to trim
/// to, and trimming to one hid every model the provider declared, leaving the
/// user unable to select the very models they had just configured. Such a
/// provider keeps its own catalog, which is the rule the TUI's row builder has
/// always applied and which [`provider_has_curated_list`] exists to express.
#[must_use]
fn is_offered_model(provider_id: &str, request_model: &str) -> bool {
    if provider_has_curated_list(provider_id) {
        is_curated_model(provider_id, request_model)
    } else {
        true
    }
}

/// Curate the full provider model directory shared by settings UIs.
///
/// This keeps the same deprecated/custom-provider trimming rule as
/// [`curated_model_listings`], but groups the surviving rows under their stable
/// provider id so clients can apply local visibility preferences without
/// guessing cross-provider ownership from a flat model list.
#[must_use]
pub fn provider_model_catalog(listings: &[ModelListing]) -> Vec<ProviderModelCatalogEntry> {
    let mut catalog: Vec<ProviderModelCatalogEntry> = Vec::new();
    let mut indexes: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for listing in provider_model_catalog_listings(listings) {
        let provider_id = listing.provider_id.clone();
        let provider_label = if listing.provider_label.is_empty() {
            provider_id.clone()
        } else {
            listing.provider_label.clone()
        };
        let index = *indexes.entry(provider_id.clone()).or_insert_with(|| {
            let index = catalog.len();
            catalog.push(ProviderModelCatalogEntry {
                provider_id: provider_id.clone(),
                provider_label,
                models: Vec::new(),
                connection: listing.connection.clone(),
            });
            index
        });
        catalog[index].models.push(listing);
    }

    catalog
}

/// Curate a flat list of display names for legacy text-only listing callers.
///
/// Mobile/CLI lack the TUI's per-provider availability maps, so this can't gate
/// `[Connect]`; instead it trims the catalog to the [`is_curated_model`] short
/// list (display names, de-duplicated, `current` kept first so it's always
/// selectable). When `listings` is empty (library/stub callers with no routing
/// client / catalog) it falls back to the raw `available` list unchanged —
/// there's nothing to curate against and the caller's prior behavior is kept.
#[must_use]
pub fn curated_model_names(
    listings: &[ModelListing],
    available: &[String],
    current: &str,
) -> Vec<String> {
    if listings.is_empty() {
        return available.to_vec();
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !current.is_empty() {
        seen.insert(current.to_string());
        out.push(current.to_string());
    }
    for l in listings {
        if is_offered_model(&l.provider_id, &l.request_model)
            && l.metadata.status.as_deref() != Some("deprecated")
            && seen.insert(l.display_model.clone())
        {
            out.push(l.display_model.clone());
        }
    }
    out
}

/// A user-visible attachment surfaced during a turn.
///
/// Oracle `k$o` (@237714543) does not return reminder TEXT — it returns records
/// `{type, path, content, displayPath}` which the renderer turns into the
/// "Listed directory …" / "Referenced file …" lines. The port took only the
/// render-to-text half, so the records were dropped and the TUI's attachment
/// cells had no producer.
///
/// `#[non_exhaustive]`: the oracle has nine of these. Only the one whose
/// producer is already ported is modelled; the rest arrive with their producers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttachmentKind {
    /// A nested LINGXI.md surfaced because a file under its directory was read.
    NestedMemory {
        /// Path shown to the user, relative to cwd (oracle `displayPath`).
        display_path: String,
    },
}

/// What `/output-style` needs to render its listing: every style this session
/// can switch to, plus the one in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyleListing {
    /// The style in force right now (the `default` style is named, not empty).
    pub current: String,
    /// Every selectable style: `(name, description)`. Order is the order the
    /// listing renders in.
    pub styles: Vec<(String, Option<String>)>,
}

/// Public handle to the orchestrator that slash commands operate against.
///
/// Summarize direction — oracle `xer`'s second parameter, defaulting `"from"`.
///
/// The two options differ in which HALF of the conversation survives verbatim,
/// and therefore where the summary sits:
///
/// | Direction | Summarized | Retained | Summary sits |
/// |---|---|---|---|
/// | [`Self::From`] | messages AFTER the chosen point | a HEAD | at the END |
/// | [`Self::UpTo`] | messages BEFORE the chosen point | a TAIL | at the START |
///
/// 🚨 The mapping is the opposite of the intuitive reading of the labels, and
/// the prompt bodies are what settle it: `SUMMARIZE_FROM_PROMPT` says the
/// earlier messages "are being kept intact", `SUMMARIZE_UP_TO_PROMPT` says the
/// summary "will be placed at the start of a continuing session".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarizeDirection {
    /// `"from"` — "Summarize from here". Everything from the chosen message on
    /// is summarized; earlier messages are kept verbatim BEFORE the summary.
    From,
    /// `"up_to"` — "Summarize up to here". Everything before the chosen message
    /// is summarized; that message and everything after it are kept verbatim
    /// AFTER the summary.
    UpTo,
}

impl SummarizeDirection {
    /// The wire spelling (`"from"` / `"up_to"`) used by the picker option and
    /// the persisted boundary metadata.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::From => "from",
            Self::UpTo => "up_to",
        }
    }
}

/// Wired in M5-09 (slash-command surface). M5-02 only defines the trait —
/// `ConversationOrchestrator` does NOT yet implement it.
#[async_trait]
pub trait OrchestratorHandle: Send + Sync {
    /// The session id currently driving the conversation.
    async fn current_session_id(&self) -> SessionId;

    /// Clear the in-memory session and start fresh.
    async fn clear_session(&self) -> Result<(), HandleError>;

    /// Force a compaction pass and return the summary.
    async fn force_compact(&self) -> Result<CompactionSummary, HandleError>;

    /// Force compaction while adding optional focus text to the summary prompt.
    /// The default delegates to [`Self::force_compact`] so external handles
    /// remain source-compatible; the live orchestrator overrides it.
    async fn force_compact_with_instructions(
        &self,
        _custom_instructions: &str,
    ) -> Result<CompactionSummary, HandleError> {
        self.force_compact().await
    }

    /// Force compaction with optional focus text and cooperative cancellation.
    ///
    /// The default preserves compatibility for lightweight/test handles that
    /// do not expose a cancellable compaction operation. The production
    /// orchestrator overrides this method so the interactive CLI can abort an
    /// in-flight summarization request with Escape.
    async fn force_compact_with_instructions_and_cancel(
        &self,
        custom_instructions: Option<&str>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<CompactionSummary, HandleError> {
        if cancel.is_cancelled() {
            return Err(HandleError::ActionFailed("compaction cancelled".into()));
        }
        self.force_compact_with_instructions(custom_instructions.unwrap_or_default())
            .await
    }

    /// `/rewind` → "Summarize from here" / "Summarize up to here".
    ///
    /// Summarizes one side of the conversation at `message_uuid` and keeps the
    /// other verbatim. `direction` is `"from"` or `"up_to"`; `user_context` is
    /// the free text typed into the picker row (`None` when submitted empty).
    ///
    /// ⚠️ Unlike the `/rewind` restore scopes, this must NOT unwind the app
    /// loop: it is a live summarizer call over the CURRENT conversation. A host
    /// that unwinds first has nothing left to summarize.
    ///
    /// The default is [`HandleError::Unimplemented`] rather than a silent
    /// no-op — a handle that cannot summarize must say so, because the caller
    /// otherwise reports a summary that never happened.
    ///
    /// # Errors
    /// [`HandleError::ActionFailed`] with the user-visible sentence, or
    /// [`HandleError::Unimplemented`] on a handle without a compactor.
    async fn summarize_at(
        &self,
        _message_uuid: &str,
        _user_context: Option<&str>,
        _direction: SummarizeDirection,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<CompactionSummary, HandleError> {
        Err(HandleError::Unimplemented("summarize_at".to_string()))
    }

    /// Snapshot the cumulative cost.
    async fn snapshot_cost(&self) -> CostSnapshot;

    /// Current session-scoped `/goal`, if any.
    async fn get_active_goal(&self) -> Option<ActiveGoalSnapshot> {
        None
    }

    /// Activate or replace the current session-scoped `/goal`.
    async fn set_active_goal(&self, _condition: &str) {}

    /// Clear the current session-scoped `/goal`, returning the removed value.
    async fn clear_active_goal(&self) -> Option<ActiveGoalSnapshot> {
        None
    }

    /// Update the active goal's most recent stop-time evaluation reason.
    async fn set_active_goal_last_reason(&self, _reason: Option<String>) {}

    /// Whether the current workspace is trusted for `/goal`.
    async fn workspace_trusted(&self) -> bool {
        true
    }

    /// Whether hook policy currently restricts `/goal`.
    async fn hooks_restricted(&self) -> bool {
        false
    }

    /// Switch the active model (and optional provider profile). Subsequent
    /// turns use the new model; the profile disambiguates shared model ids
    /// across providers (e.g. `"gpt-5.2"` on `"github-copilot"` vs `"openai"`).
    /// `None` profile = resolve unscoped (default / legacy behaviour).
    /// The `/status` oversized-memory warnings, recomputed NOW.
    ///
    /// Oracle `htf()` runs when the panel MOUNTS, against the live memory set
    /// and the live model — a LINGXI.md that grew this session, or a `/model`
    /// switch that moved the threshold, both change the answer, and a
    /// launch-time capture can see neither. The engine owns the memory
    /// provider, so it owns this.
    ///
    /// **Default unavailable**: `None` means the implementation cannot
    /// recompute the live set, while `Some(Vec::new())` is an authoritative
    /// result saying that all previously captured warnings have cleared.
    async fn large_memory_warnings(&self) -> Option<Vec<String>> {
        None
    }

    /// Host-validated beta additions that affect session-visible model
    /// capabilities such as the context window. The default remains empty for
    /// lightweight and test implementations.
    async fn active_betas(&self) -> Vec<String> {
        Vec::new()
    }

    async fn switch_model(&self, model: &str, profile: Option<&str>) -> Result<(), HandleError>;

    /// Switch the active model while preserving the operation's provenance for
    /// model-switch hooks. Existing handles may use the legacy
    /// [`Self::switch_model`] implementation; the default delegates to it so
    /// adding this seam does not break lightweight hosts or test doubles.
    async fn switch_model_with_source(
        &self,
        model: &str,
        profile: Option<&str>,
        _source: &str,
    ) -> Result<(), HandleError> {
        self.switch_model(model, profile).await
    }

    /// Read the session's fast-mode flag (`/fast`; the priority `speed:"fast"`
    /// tier). The DEFAULT is `false` so existing impls/mocks compile unchanged.
    async fn fast_mode(&self) -> bool {
        false
    }

    /// Toggle the session's fast-mode flag (`/fast`). When on, subsequent turns
    /// send `speed:"fast"` in the request body IF the active model supports it
    /// (opus-4-8 / opus-5). DEFAULT is an inert no-op so existing impls/mocks
    /// compile unchanged.
    async fn set_fast_mode(&self, on: bool) -> Result<(), HandleError> {
        let _ = on;
        Ok(())
    }

    /// Read the session's plan-mode flag (`/plan`). When set, the turn loop
    /// routes tool-permission checks through `check_in_plan_mode` and sends
    /// `permission_mode: "plan"` in the request body. DEFAULT is `false` so
    /// existing impls/mocks compile unchanged.
    async fn plan_mode(&self) -> bool {
        false
    }

    /// Enter (`on = true`) / leave (`on = false`) plan mode by flipping
    /// `SessionState.plan_mode` — the exact bool the `EnterPlanMode`/
    /// `ExitPlanMode` tools flip. `/plan` only ever enters. DEFAULT is an inert
    /// no-op so existing impls/mocks compile unchanged.
    async fn set_plan_mode(&self, on: bool) -> Result<(), HandleError> {
        let _ = on;
        Ok(())
    }

    /// Read the current session's plan file through the same plan-store path
    /// used by the plan-mode reminder. A missing file is `Ok(None)`.
    async fn current_plan(&self) -> Result<Option<PlanSnapshot>, HandleError> {
        Ok(None)
    }

    /// Open the current session's plan in the configured editor.
    async fn open_plan_editor(&self) -> Result<MemoryEditorOutcome, HandleError> {
        Err(HandleError::Unimplemented("open_plan_editor".into()))
    }

    /// Live effort sent on the next model request (`None` = automatic).
    async fn current_effort(&self) -> Option<String> {
        None
    }

    /// Whether dynamic workflows are enabled for THIS session.
    async fn dynamic_workflows_enabled(&self) -> bool {
        false
    }

    /// Whether enterprise policy owns the session's `enableWorkflows` value.
    async fn dynamic_workflows_managed(&self) -> bool {
        false
    }

    /// Effective session-owned `workflowSizeGuideline` value.
    async fn workflow_size_guideline(&self) -> String {
        "medium".to_string()
    }

    /// Whether enterprise policy owns the session's `workflowSizeGuideline`.
    async fn workflow_size_guideline_managed(&self) -> bool {
        false
    }

    /// Coherent session-owned `workflowSizeGuideline` snapshot.
    async fn workflow_size_guideline_state(
        &self,
    ) -> crate::host::session_flags::WorkflowSizeGuidelineSnapshot {
        crate::host::session_flags::WorkflowSizeGuidelineSnapshot {
            value: "medium",
            managed: false,
            is_default: true,
        }
    }

    /// Whether the session's workflow-size value is still the built-in default.
    async fn workflow_size_guideline_is_default(&self) -> bool {
        true
    }

    /// Update the live session-owned dynamic-workflow gate.
    async fn set_dynamic_workflows_enabled(
        &self,
        enabled: bool,
        managed: bool,
    ) -> Result<(), HandleError> {
        let _ = (enabled, managed);
        Ok(())
    }

    /// Update the live session-owned workflow-size setting.
    async fn set_workflow_size_guideline(
        &self,
        value: String,
        managed: bool,
        is_default: bool,
    ) -> Result<(), HandleError> {
        let _ = (value, managed, is_default);
        Ok(())
    }

    /// Change the live effort for this session as well as future transcript
    /// rows. Persistence of the default remains the slash command's concern.
    async fn set_effort_level(&self, _effort: Option<String>) -> Result<(), HandleError> {
        Ok(())
    }

    /// The output styles this session can switch to, and which one is in force
    /// — the listing half of `/output-style`.
    ///
    /// `None` from an engine with no prompt-assembly layer; the command then
    /// reports that it cannot see any styles rather than claiming there are
    /// none.
    async fn output_styles(&self) -> Option<OutputStyleListing> {
        None
    }

    /// Switch the output style for the rest of the session. Takes effect on the
    /// NEXT turn's system prompt, like upstream, which persists the setting and
    /// re-reads it during prompt assembly.
    ///
    /// The name must be one of [`Self::output_styles`]'s entries; an unknown
    /// name is an error rather than a silent no-op, because a silent no-op
    /// looks exactly like a successful switch.
    async fn set_output_style(&self, _name: &str) -> Result<(), HandleError> {
        Err(HandleError::Unimplemented("set_output_style".into()))
    }

    /// Read the active conversation controls snapshot.
    async fn conversation_controls(&self) -> Option<ConversationControls> {
        None
    }

    /// Replace the live reasoning / effort selection for subsequent requests.
    async fn set_reasoning_selection(
        &self,
        _selection: ReasoningSelection,
    ) -> Result<(), HandleError> {
        Ok(())
    }

    /// Read the live permission-mode wire id. Engines without a policy-backed
    /// permission gate return `None`; desktop and mobile production runtimes
    /// override this with their enforcing gate's authoritative mode.
    async fn permission_mode(&self) -> Option<String> {
        None
    }

    /// Change the live permission mode used by subsequent tool checks. The
    /// default is inert so lightweight embedders remain source-compatible.
    async fn set_permission_mode(&self, mode: &str) -> Result<(), HandleError> {
        let _ = mode;
        Ok(())
    }

    // M5-10 additions:

    /// Set the orchestrator's internal `should_exit` flag.
    ///
    /// The REPL (M5-13) checks this after each turn and breaks out of the
    /// loop. The flag is one-way: once set, it cannot be cleared (so a
    /// double-`/exit` is idempotent).
    ///
    /// Wired by M5-10 (`/exit` handler).
    async fn request_exit(&self);

    /// Read the current value of the `should_exit` flag without mutating it.
    ///
    /// The REPL (M5-13) calls this after every slash-command dispatch to
    /// decide whether to break the loop. Returns `true` iff `request_exit`
    /// has been called at least once.
    async fn current_should_exit(&self) -> bool;

    /// Open `$EDITOR` on `<config-dir>/claude/LINGXI.md` (creating the file
    /// if it does not exist), block until the editor exits, then return the
    /// outcome.
    ///
    /// The editor lookup order is `EDITOR` → `VISUAL` → `"vi"` (Unix) /
    /// `"notepad.exe"` (Windows). Empty env values are treated as missing.
    ///
    /// Wired by M5-10 (`/memory` handler).
    async fn open_memory_editor(&self) -> Result<MemoryEditorOutcome, HandleError>;

    // M5-11 additions:

    /// Snapshot the local IDE endpoint inventory and connection selection.
    /// The default is an empty inventory for hosts that do not expose local
    /// IDE integration.
    async fn ide_status(&self) -> crate::host::IdeStatus {
        crate::host::IdeStatus::default()
    }

    /// Connect the local IDE endpoint identified by `endpoint_id`.
    async fn ide_connect(&self, _endpoint_id: &str) -> Result<crate::host::IdeStatus, HandleError> {
        Err(HandleError::Unimplemented("ide_connect".into()))
    }

    /// Disconnect the selected local IDE endpoint.
    async fn ide_disconnect(&self) -> Result<crate::host::IdeStatus, HandleError> {
        Err(HandleError::Unimplemented("ide_disconnect".into()))
    }

    /// Ask the selected local IDE to open the live session CWD.
    async fn ide_open(&self) -> Result<String, HandleError> {
        Err(HandleError::Unimplemented("ide_open".into()))
    }

    /// Connect the only valid local IDE endpoint, if discovery finds exactly
    /// one. Returns `false` for zero or multiple endpoints.
    async fn ide_auto_connect_if_single(&self) -> Result<bool, HandleError> {
        Ok(false)
    }

    /// Enumerate currently registered MCP servers + their connection state.
    /// Used by `/mcp` and `/status`. Returns an empty vector when no MCP
    /// servers are configured.
    async fn list_mcp_servers(&self) -> Vec<McpServerInfo>;

    /// Enumerate skills discovered from `skills/` directories, scanning
    /// exactly three tiers: the project tier, the user tier, and the
    /// managed (org-policy) directory.
    ///
    /// Additional skill roots (multi-root `/add-dir` workspaces) are
    /// deliberately NOT scanned. This is not an oversight to widen later —
    /// it is parity with what the desktop's own sibling slash commands
    /// currently scan: `SkillsHandler` (`/skills`) is constructed with
    /// `additional_skill_dirs: Vec::new()` HARDCODED
    /// (`runtime/src/desktop/mod.rs:3351-3354`), and its
    /// `ReloadSkillsHandler` counterpart (`/reload-skills`) is wired to
    /// `DesktopRepoRootReloader.registered_roots`
    /// (`runtime/src/desktop/mod.rs:4919`, `:4964-4977`), which starts
    /// empty and has no desktop-side call site that ever grows it. If either
    /// of those two construction sites starts supplying real roots, this
    /// method's tiers must be revisited alongside them — until then, do NOT
    /// widen this to any other source of "additional directories" (e.g. the
    /// live session's trusted-directory set), which is a materially larger
    /// set and would make this listing show skills neither sibling command
    /// reports.
    ///
    /// Used by the desktop Skills settings view, which is a view over what
    /// the loader found — not an editor — plus a reload action
    /// (`/reload-skills`, routed as an ordinary slash command; no dedicated
    /// command exists for reloading). Returns an empty vector when no config
    /// home is wired, mirroring `list_mcp_servers`'s "no registry" default.
    async fn list_skills(&self) -> Vec<SkillInfo>;

    /// Reconnect MCP servers (`/mcp reconnect [<server>|all]`). `name = None`
    /// (or `"all"`) reconnects every registered server; otherwise just the
    /// named one. Returns `(reconnected, failures)` — the server names that
    /// reconnected cleanly and the `(name, reason)` pairs that did not. An
    /// unknown server name yields a single failure. The default (no MCP
    /// registry wired) returns empty vectors.
    async fn reconnect_mcp_servers(
        &self,
        _name: Option<&str>,
    ) -> (Vec<String>, Vec<(String, String)>) {
        (Vec::new(), Vec::new())
    }

    /// Fine-grained per-server state for the `/mcp reconnect|enable|disable`
    /// action handler (`McpActionState` mirrors claude-code's client `type`
    /// discriminant). Returns `(name, state)` pairs — the `"ide"` pseudo-server
    /// is included; the caller filters it, matching claude's
    /// `clients.filter(b => b.name !== "ide")`. The default (no MCP registry
    /// wired) returns an empty vector, so the handler falls through to its
    /// "no servers configured" path.
    async fn mcp_server_states(&self) -> Vec<(String, McpActionState)> {
        Vec::new()
    }

    /// Enable/disable a project MCP server (`/mcp enable|disable [<server>|all]`)
    /// in the current session and write the global config's
    /// `projects[<cwd>].disabledMcpjsonServers` list so the change survives a
    /// restart. `server = None` (or `"all"`) applies to every configured server;
    /// otherwise just the named one. `disabled = true` disables, `false`
    /// re-enables. Returns one [`McpToggleOutcome`] per server that was NOT
    /// already in the requested state (claude's `p` — the settled
    /// `allSettled(p.map(u))` set); an empty vector marks the "already
    /// enabled/disabled" no-op. The default (no MCP registry / config path
    /// wired) is a no-op.
    async fn set_mcp_servers_disabled(
        &self,
        _server: Option<&str>,
        _disabled: bool,
    ) -> Result<Vec<McpToggleOutcome>, String> {
        Ok(Vec::new())
    }

    /// Enumerate registered hooks (built-in + user). Used by `/hooks` and
    /// `/status`.
    async fn list_hooks(&self) -> Vec<HookInfo>;

    /// Fire the `DirectoryAdded` hook (2.1.219) after a working directory is
    /// added mid-session via `/add-dir` or the SDK `register_repo_root`.
    ///
    /// `source` is both a payload field and the hook MATCHER QUERY, so a
    /// matcher is tested against `slash_command` / `register_repo_root` rather than
    /// against the path.
    ///
    /// DEFAULTED to a no-op so the many existing implementations of this trait
    /// (test doubles, the mobile engine) compile unchanged — the frozen-trait
    /// idiom this codebase uses for additive surface. Only the desktop
    /// orchestrator overrides it.
    async fn fire_directory_added(
        &self,
        _directory: &str,
        _source: &str,
    ) -> DirectoryAddedHookSummary {
        DirectoryAddedHookSummary::default()
    }

    /// Validate and register a repository root for this live session.
    async fn register_repo_root(
        &self,
        _request: RegisterRepoRootRequest,
    ) -> Result<RegisterRepoRootOutcome, HandleError> {
        Err(HandleError::Unimplemented("register_repo_root".into()))
    }

    /// Enumerate registered subagents (markdown-defined + built-in). Used
    /// by `/agents` and `/status`.
    async fn list_agents(&self) -> Vec<AgentInfo>;

    /// Run the 6 doctor checks and return the aggregated report. Used by
    /// `/doctor`.
    async fn run_doctor_checks(&self) -> DoctorReport;

    /// Snapshot the full status panel. Used by `/status`.
    async fn get_status_snapshot(&self) -> StatusSnapshot;

    /// Append a meta user message without starting a model turn.
    ///
    /// Used by Fusion slash completion. Default is a no-op so test handles
    /// compile unchanged.
    async fn append_meta_user_message(&self, _text: &str) -> Result<(), HandleError> {
        Ok(())
    }

    /// Persist the visible input and display output of a local slash command
    /// without feeding either synthetic row back into the model on later turns.
    /// Hosts that do not own a durable transcript may keep the default no-op.
    async fn append_slash_command_transcript(
        &self,
        _raw: &str,
        _display: &str,
    ) -> Result<(), HandleError> {
        Ok(())
    }

    /// Persist a Fusion request or launch reply as model-visible conversation
    /// history, without starting a turn or emitting a duplicate UI event.
    async fn append_fusion_command_message(
        &self,
        _session_id: &str,
        _text: &str,
        _is_user: bool,
    ) -> Result<(), HandleError> {
        Err(HandleError::ActionFailed(
            "Fusion conversation persistence is unavailable".into(),
        ))
    }

    /// Append a transcript meta message to one specific session without
    /// starting a model turn.
    ///
    /// The default is deliberately fail-closed across sessions: lightweight
    /// handles may reuse the current-session append only when the requested id
    /// still matches. Production handles override this to route to the target
    /// session's durable transcript and exclude the message from model input.
    async fn append_meta_user_message_to_session(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<(), HandleError> {
        if self.current_session_id().await.to_string() != session_id {
            return Err(HandleError::ActionFailed(format!(
                "target session {session_id} is no longer active"
            )));
        }
        self.append_meta_user_message(text).await
    }

    /// Emit a user-visible system notice outside any active model turn — the
    /// completion path for a background `/fusion` run (F006: previously the
    /// only trace of a finished run was a `user_meta` transcript row nothing
    /// live ever surfaced). Routes through the connected client's
    /// `OutputStream::emit_system_notice` in production; default is a no-op
    /// so lightweight/test handles keep compiling unchanged.
    async fn emit_background_system_notice(&self, _body: &str) {}

    /// Open `$EDITOR` on `<config-dir>/claude/config.json` (creating if
    /// absent). Used by `/config`.
    async fn edit_config_file(&self) -> Result<MemoryEditorOutcome, HandleError>;

    /// Open `$EDITOR` on `<config-dir>/claude/permissions.json` (creating
    /// if absent). Used by `/permissions`.
    async fn edit_permissions_file(&self) -> Result<MemoryEditorOutcome, HandleError>;

    /// Enumerate model names the orchestrator will accept via
    /// [`Self::switch_model`]. Used by `/model` (no-arg list mode).
    async fn list_available_models(&self) -> Vec<String>;

    /// Richer model listing for the grouped `/model` picker (display name +
    /// provider label + wire id), sourced from the llm-runtime catalog. The
    /// DEFAULT returns empty so existing impls/mocks compile unchanged; only the
    /// live `ConversationOrchestrator` overrides it.
    async fn list_model_listings(&self) -> Vec<ModelListing> {
        Vec::new()
    }

    /// Return the most recently observed provider rate-limit header snapshot.
    ///
    /// Returns `Some(snapshot)` when the underlying `ProviderApiAdapter` has
    /// received at least one successful 2xx response with
    /// `anthropic-ratelimit-unified-*` headers; `None` until then.
    ///
    /// The returned [`RateLimitSnapshot`] avoids leaking the
    /// orchestrator-internal `RateLimitInfo` struct through the `core::host` crate
    /// (which must not depend on `orchestrator`).  It carries all three
    /// header-derived fields including `overage_disabled_reason`.
    ///
    /// **TUI wiring note:** the TUI's existing `rate_limit.rs` renderer is fed
    /// from `RenderedMessage::RateLimit` events that flow through the output
    /// stream (protocol layer).  Feeding this method's value into that path
    /// would require a new protocol event or a separate status-poll tick — both
    /// changes are outside the scope of this task (frozen protocol guard).
    /// This method is the handle-layer surface; callers that need live polling
    /// can call it from a ticker and push a `RenderedMessage::RateLimit` when
    /// the value changes.
    async fn last_rate_limit_info(&self) -> Option<RateLimitSnapshot> {
        None
    }

    // M6-03 addition:

    /// Streaming twin of the M5-13 cancel-aware turn entry point. The
    /// TUI calls this so Ctrl-C aborts the in-flight SSE stream cleanly.
    ///
    /// Default impl returns `Err(HandleError::Unimplemented(..))` so
    /// existing stdio REPL implementations (M5-13) need no override.
    /// `OrchestratorHandleImpl` (M6-03) overrides this to delegate to
    /// `ConversationOrchestrator::run_turn_streaming_with_cancel`.
    async fn run_turn_streaming_with_cancel(
        &self,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<TurnOutcome, HandleError> {
        let _ = (prompt, cancel);
        Err(HandleError::Unimplemented(
            "run_turn_streaming_with_cancel".into(),
        ))
    }

    /// Preserve queued-command provenance when driving a text-only follow-up.
    async fn run_queued_turn_streaming(
        &self,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
        _in_human_turn: bool,
    ) -> Result<TurnOutcome, HandleError> {
        self.run_turn_streaming_with_cancel(prompt, cancel).await
    }

    /// Streaming turn carrying pasted image file paths (TUI paste→image). Each
    /// path is read + base64-encoded into a `ContentBlock::Image` on the
    /// outgoing user message.
    ///
    /// Default delegates to [`Self::run_turn_streaming_with_cancel`] ignoring
    /// images, so non-TUI handle impls need no override.
    async fn run_turn_streaming_with_images(
        &self,
        prompt: &str,
        image_paths: &[std::path::PathBuf],
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<TurnOutcome, HandleError> {
        let _ = image_paths;
        self.run_turn_streaming_with_cancel(prompt, cancel).await
    }

    /// Start a turn that contains only pending asynchronous-hook meta
    /// responses. Implementations without an async-hook provider may no-op.
    async fn run_async_hook_rewake(&self) -> Result<TurnOutcome, HandleError> {
        Ok(TurnOutcome::EndTurn)
    }

    // ────────────────────────────────────────────────────────────────────
    // engine-data-commands additions (`/export`, `/files`, `/context`,
    // `/resume`). Each carries a benign default so the production
    // `ConversationOrchestrator` (orchestrator/src/handle_impl.rs) and the
    // test `MockOrchestratorHandle` (orchestrator/src/test_support.rs) keep
    // compiling unchanged; the production impl overrides them with real data.
    // ────────────────────────────────────────────────────────────────────

    /// Clone of the live, ordered conversation history.
    ///
    /// Single read-only accessor backing `/export` (render the transcript to
    /// a file) and underpinning `/summary` and `/diff`. Returns the already
    /// public [`crate::types::ConversationMessage`] so no new type is introduced.
    ///
    /// Default returns an empty `Vec`, so handle impls that do not track a
    /// session (e.g. the test mock) need no override.
    async fn conversation_transcript(&self) -> Vec<crate::types::ConversationMessage> {
        Vec::new()
    }

    /// Whether this session is a coordinator (team lead) session.
    ///
    /// Backs `/fork`'s `isEnabled:()=>!tv()` gate: forking is unavailable in a
    /// coordinator session (the user is pointed at `/branch` instead). Same
    /// additive-default shape as [`Self::emit_coordinator_status`].
    ///
    /// Default returns `false` (an ordinary, non-coordinator session), so
    /// handle impls that never enter coordinator mode need no override.
    async fn is_coordinator_session(&self) -> bool {
        false
    }

    /// Spawn a background agent that inherits the full conversation, per
    /// `/fork`. Returns the spawned agent's [`ForkOutcome`] (name + id) on
    /// success.
    ///
    /// Default returns `Err(HandleError::Unimplemented(..))` so existing
    /// handle impls (and the test mock) keep compiling; the composition roots
    /// override it against the real background-agent spawner.
    async fn fork_conversation(&self, directive: &str) -> Result<ForkOutcome, HandleError> {
        let _ = directive;
        Err(HandleError::Unimplemented("fork_conversation".into()))
    }

    /// Whether an exit handoff has a persistent backend and a genuine user seed.
    async fn can_background_conversation_on_exit(&self) -> bool {
        false
    }

    /// Copy the CURRENT conversation into a NEW BACKGROUND session and keep the
    /// interactive session live, per the redefined 2.1.212 `/fork` (`vAd`).
    /// `prompt` is the optional `[prompt]` argument to seed the background
    /// session's next turn. Returns the system-line to display in the live
    /// session (the composition root owns the exact text, since it knows the new
    /// session id).
    ///
    /// This is the seam the special dispatcher fills: `vAd` has no `load`
    /// function — upstream intercepts `name === "fork"` and routes it through
    /// the background (`--bg`/daemon) session-copy path while leaving the
    /// foreground session interactive.
    ///
    /// Default returns `Err(HandleError::Unimplemented(..))` so existing handle
    /// impls (and the test mock) keep compiling; the composition roots override
    /// it against the real background-session dispatch (`jobs/<short>/state.json`
    /// daemon supervisor + `__bg-run` worker).
    async fn fork_to_background_session(&self, prompt: &str) -> Result<String, HandleError> {
        let _ = prompt;
        Err(HandleError::Unimplemented(
            "fork_to_background_session".into(),
        ))
    }

    /// Move the current foreground conversation into a durable background
    /// session at the supplied live boundary.
    ///
    /// Unlike [`Self::fork_to_background_session`], this seam carries the
    /// partial assistant reply and in-flight classification needed by a
    /// mid-turn ← handoff. Implementations must preserve the snapshot before
    /// returning success; the UI cancels foreground work only for an
    /// `AbortThenFork` decision.
    async fn background_conversation(
        &self,
        snapshot: crate::host::BackgroundingSnapshot,
    ) -> Result<String, HandleError> {
        let _ = snapshot;
        Err(HandleError::Unimplemented("background_conversation".into()))
    }

    /// `/resume`-as-background (2.1.212, G06) — launch an EXISTING session (one
    /// from [`Self::list_resumable_sessions`]) as a NEW background session,
    /// rather than resuming it in the foreground. Reuses the same
    /// `BgSessionForker` seam as [`Self::fork_to_background_session`], minus the
    /// live-conversation snapshot (the session already exists on disk).
    ///
    /// Default returns `Err(HandleError::Unimplemented(..))` so existing impls +
    /// the test mock keep compiling; the composition roots override it against
    /// the real background-session dispatch.
    async fn resume_to_background_session(&self, session_id: &str) -> Result<String, HandleError> {
        let _ = session_id;
        Err(HandleError::Unimplemented(
            "resume_to_background_session".into(),
        ))
    }

    /// Generate a one-line session recap via an isolated, read-only,
    /// tool-denied, single-turn side query, per `/recap`. Returns the trimmed
    /// recap text (or [`RecapOutcome::Cancelled`] if aborted mid-flight).
    ///
    /// Read-only w.r.t. session state: unlike [`Self::force_compact`] this must
    /// NOT mutate history / cache (upstream `skipTranscript` / `skipCacheWrite`).
    ///
    /// Default returns `Err(HandleError::Unimplemented(..))` so existing handle
    /// impls (and the test mock) keep compiling; the composition roots override
    /// it against the real forked-agent side-query runner.
    async fn generate_recap(&self) -> Result<RecapOutcome, HandleError> {
        Err(HandleError::Unimplemented("generate_recap".into()))
    }

    /// Generate the kebab-case title used by a bare `/rename`, without adding
    /// the query or response to the conversation transcript.
    async fn generate_session_name(&self) -> Result<Option<String>, HandleError> {
        Err(HandleError::Unimplemented("generate_session_name".into()))
    }

    /// Build the `/rewind` restore-point rows LIVE from the session's user turns
    /// that have a file-history checkpoint. DEFAULT is empty so impls/mocks
    /// without checkpointing compile unchanged.
    async fn rewind_rows(&self) -> Vec<RewindRowData> {
        Vec::new()
    }

    /// Answer a one-off side question (`/btw`) via the SAME isolated,
    /// read-only, tool-denied, single-turn side query `/recap` uses — a
    /// lightweight agent that shares the parent's cache-safe prompt prefix but
    /// NEVER appends to the conversation history. Returns the model's trimmed
    /// answer as [`RecapOutcome::Text`] (or [`RecapOutcome::Cancelled`] if
    /// aborted). Read-only w.r.t. session state, exactly like
    /// [`Self::generate_recap`]; the only difference is the prompt (the
    /// caller's wrapped question instead of the fixed recap prompt).
    ///
    /// Default returns `Err(HandleError::Unimplemented(..))` so existing handle
    /// impls (and the test mock) keep compiling; the composition roots override
    /// it against the real forked-agent side-query runner.
    async fn answer_side_question(&self, question: &str) -> Result<RecapOutcome, HandleError> {
        let _ = question;
        Err(HandleError::Unimplemented("answer_side_question".into()))
    }

    /// `/rename`: persist a user-set custom title for the current session by
    /// appending a `custom-title` entry to the transcript JSONL (1:1 with
    /// claude-code `saveCustomTitle`). Best-effort; the default returns
    /// `Unimplemented` so non-persisting handles (the test mock, library
    /// callers with no writer) keep compiling and degrade gracefully.
    async fn rename_session(&self, _name: String) -> Result<(), HandleError> {
        Err(HandleError::Unimplemented("rename_session".into()))
    }

    /// File paths currently tracked in the session's read-file-state cache.
    ///
    /// Backs `/files`, which renders each path relative to the cwd (cwd comes
    /// from [`Self::get_status_snapshot`]) and prints `"No files in context"`
    /// when empty — 1:1 with `files.ts`.
    ///
    /// Default returns an empty `Vec`, matching the TS "No files in context"
    /// branch when no read-file-state cache is wired.
    async fn files_in_context(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    /// Snapshot the live context separately from cumulative session cost.
    ///
    /// Compaction replaces the live history but must never reset billing/cost
    /// counters. The default keeps lightweight handles source-compatible while
    /// production handles project the current history through the same
    /// estimator used by auto-compaction.
    async fn context_usage_snapshot(&self) -> ContextUsageSnapshot {
        ContextUsageSnapshot {
            cumulative_cost: self.snapshot_cost().await,
            ..ContextUsageSnapshot::default()
        }
    }

    /// `(used_tokens, max_tokens)` for the current context window.
    ///
    /// Minimal primitive-tuple accessor backing the `/context` flat panel
    /// (`**Tokens:** {used} / {max} ({pct}%)`). `used_tokens` comes from the
    /// live session history estimate; `max_tokens` from the active model's
    /// context budget. The model name itself already comes from
    /// [`Self::get_status_snapshot`], so no new struct is needed.
    ///
    /// Default returns `(0, 0)` when no usage is recorded.
    async fn context_window_usage(&self) -> (u64, u64) {
        let snapshot = self.context_usage_snapshot().await;
        (snapshot.live_context_tokens, snapshot.max_context_tokens)
    }

    /// `Vec<(session_id, label)>` of prior on-disk sessions, newest-first.
    ///
    /// Single accessor for a non-interactive `/resume` listing (one
    /// `id` + `label` per line), built from `std` types only. The interactive
    /// picker and replaying a chosen session are handled elsewhere; this
    /// delivers only the enumeration half.
    ///
    /// (2.1.212 G06) claude-code also lists SOFT-DELETED sessions in the picker.
    /// LingXi has NO soft-delete/trash model for conversation sessions — a
    /// session is either a live `<uuid>.jsonl` on disk or hard-removed (the `rm`
    /// command only deletes background *jobs*, not conversation transcripts).
    /// There is therefore nothing "deleted" to surface, so the deleted-inclusion
    /// half of G06 is structurally inert here rather than a wired flag; the
    /// resume-AS-background half is delivered via
    /// [`Self::resume_to_background_session`].
    ///
    /// Default returns an empty `Vec` when no on-disk store is present.
    async fn list_resumable_sessions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Adopt a previously-loaded session IN PLACE: replace the live history with
    /// `history`, adopt the NAMED `session_id` (resume does NOT mint a fresh id —
    /// that is `clear_session`'s job), and seed the JSONL parent-uuid chain to
    /// `last_jsonl_uuid` so any future append chains via `parent_uuid` off the
    /// resumed tail. The model is unchanged — resume keeps the live model.
    ///
    /// The symmetric twin of [`Self::clear_session`]: where `clear_session`
    /// wipes + re-mints, `resume_session` adopts the on-disk session's id +
    /// transcript so the next turn continues with the prior context. The
    /// caller (engine host) has already loaded + validated the JSONL via the
    /// orchestrator's resume path; this method only swaps the loaded values
    /// into the running orchestrator.
    ///
    /// Default returns `Err(HandleError::Unimplemented(..))` so non-resuming
    /// handle impls (the stdio REPL path, the test `MockOrchestratorHandle`)
    /// keep compiling unchanged; the production `ConversationOrchestrator`
    /// overrides it.
    async fn resume_session(
        &self,
        session_id: SessionId,
        history: Vec<crate::types::ConversationMessage>,
        last_jsonl_uuid: Option<String>,
        active_goal: Option<ActiveGoalSnapshot>,
        runtime: ResumeRuntimeSnapshot,
    ) -> Result<(), HandleError> {
        let _ = (session_id, history, last_jsonl_uuid, active_goal, runtime);
        Err(HandleError::Unimplemented("resume_session".into()))
    }
}

/// Captured output emission. Useful for tests and (M5-13) the stdio sink.
///
/// The enum is `non_exhaustive` so M5-04 can add a `StreamingDelta` variant
/// without a breaking change.
///
/// M5-11: `Eq` was dropped (and replaced with `PartialEq` only) because
/// `CostSnapshot` now carries `f64` + `Duration` fields whose `Eq` impl is
/// not defined. Callers that need set semantics should bucket by the
/// `session_id` or other integer fields instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OutputEvent {
    /// Correlate the current assistant response with its conversation identity.
    MessageIdentity {
        /// Stable conversation message identifier.
        message_id: crate::types::MessageId,
    },
    /// Retract a failed assistant attempt from the live transcript.
    MessageRetracted {
        /// The rejected assistant message identifier.
        message_id: crate::types::MessageId,
    },
    /// Plain text from the assistant.
    Text {
        /// The text payload emitted.
        text: String,
    },
    /// A user-visible system notice that is not assistant/model output.
    SystemNotice {
        /// Sanitized notice text.
        body: String,
        /// Whether consumers should render the notice as an error.
        is_error: bool,
    },
    /// An allowlisted terminal escape sequence to write to the terminal
    /// (#6 main-loop parity, [`OutputStream::emit_terminal_sequence`]).
    TerminalSequence {
        /// The validated OSC/BEL sequence.
        seq: String,
    },
    /// A tool invocation about to dispatch.
    ToolCall {
        /// Stable id (the `tool_use_id` echoed in the matching `ToolResult`).
        /// Added in M6-04 so the TUI can correlate calls with results and
        /// key the per-tool expanded-state map.
        id: crate::types::ToolUseId,
        /// Name of the tool being invoked.
        tool: String,
        /// JSON input passed to the tool.
        input: serde_json::Value,
    },
    /// Periodic heartbeat for a still-running tool call.
    ToolHeartbeat {
        /// Correlator with the matching `ToolCall` / `ToolResult`.
        id: crate::types::ToolUseId,
        /// Name of the tool still running.
        tool: String,
        /// Milliseconds elapsed since the tool dispatch began.
        elapsed_ms: u64,
    },
    /// A tool result returning to the conversation.
    ToolResult {
        /// Correlator with the matching `ToolCall`.
        id: crate::types::ToolUseId,
        /// Name of the tool that returned.
        tool: String,
        /// JSON result payload.
        result: serde_json::Value,
    },
    /// End-of-turn marker with cost.
    EndTurn {
        /// Stop reason reported by the model (e.g. `"end_turn"`, `"max_tokens"`).
        stop_reason: String,
        /// Cumulative cost snapshot at end of turn.
        cost: CostSnapshot,
    },
    /// Emitted once a successful `force_compact` finishes. (M6-08)
    CompactionCompleted {
        /// Message count BEFORE compaction.
        messages_before: u32,
        /// Message count AFTER compaction (including the appended
        /// `[Compacted]` boundary marker).
        messages_after: u32,
        /// UX estimate of bytes freed.
        bytes_saved: u64,
        /// Full model-generated compact summary displayed when the transcript
        /// is expanded (Ctrl-O). This is the same continuation summary stored
        /// in the post-compact history.
        summary: String,
    },
    /// A streaming reasoning ("thinking") delta as it arrives. (§0.7
    /// "light up thinking/usage"). Recorded by `MockOutputStream` so the
    /// orchestrator SSE-pump tests can assert `emit_thinking` fired. The
    /// `signature` is `None` for live deltas (it only arrives on the
    /// completed thinking block, not per-delta).
    Thinking {
        /// The reasoning fragment emitted.
        thinking: String,
        /// Cryptographic signature, `None` for live deltas.
        signature: Option<String>,
    },
    /// An incremental token-usage update for the latest API call. (§0.7
    /// "light up thinking/usage"). Recorded by `MockOutputStream` so the
    /// orchestrator SSE-pump tests can assert `emit_usage` fired with the
    /// right counts.
    Usage {
        /// Input tokens billed.
        input_tokens: u64,
        /// Output tokens billed.
        output_tokens: u64,
        /// Input tokens served from cache.
        cache_read_tokens: u64,
        /// Input tokens used to create a fresh cache entry.
        cache_creation_tokens: u64,
    },
    /// The latest unified rate-limit header snapshot, forwarded by the
    /// orchestrator after a completed API call ONLY when it differs from the
    /// previously emitted snapshot (emit-on-change). (llm-runtime future-work
    /// batch 3, Task 8.) Each field is parsed from an
    /// `anthropic-ratelimit-unified-*` response header (claude-code
    /// `claudeAiLimits.ts`); `None` means the provider did not send that
    /// header on the most recent 2xx response.
    RateLimit {
        /// `anthropic-ratelimit-unified-status` — `"allowed"` /
        /// `"allowed_warning"` / `"rejected"`.
        status: Option<String>,
        /// `anthropic-ratelimit-unified-representative-claim` — which window
        /// is representative (`"five_hour"` / `"seven_day"` /
        /// `"seven_day_opus"` / `"seven_day_sonnet"`).
        rate_limit_type: Option<String>,
        /// Per-claim `anthropic-ratelimit-unified-{abbrev}-utilization` —
        /// the representative claim's 0-1 utilization fraction (abbrev `5h`
        /// / `7d` / `overage`).
        utilization: Option<f64>,
        /// `anthropic-ratelimit-unified-reset` — Unix-epoch seconds when the
        /// representative window resets.
        resets_at: Option<u64>,
        /// Per-claim `anthropic-ratelimit-unified-{abbrev}-reset` —
        /// Unix-epoch seconds when the representative claim's window resets.
        claim_resets_at: Option<u64>,
        /// `anthropic-ratelimit-unified-overage-status` — `"allowed"` /
        /// `"allowed_warning"` / `"rejected"`.
        overage_status: Option<String>,
        /// `anthropic-ratelimit-unified-overage-reset` — Unix-epoch seconds
        /// when the overage window resets.
        overage_resets_at: Option<u64>,
        /// `anthropic-ratelimit-unified-overage-disabled-reason` — why
        /// overage spend is disabled (e.g. `"out_of_credits"`).
        overage_disabled_reason: Option<String>,
        /// `anthropic-ratelimit-unified-fallback` strict-equals
        /// `"available"`; `None` when the header is absent.
        fallback_available: Option<bool>,
        /// `anthropic-ratelimit-unified-upgrade-paths` — comma-separated
        /// upgrade offers (e.g. `"overage"`), parsed to a list; `None` when
        /// the header is absent or empty (2.1.206 `ClaudeAILimits`).
        upgrade_paths: Option<Vec<String>>,
        /// Whether the most recent 429's error body carried
        /// `error.error.details.error_code === "credits_required"` (2.1.206),
        /// or the representative claim is `seven_day_overage_included`.
        credits_required: bool,
    },
    /// Raw per-window unified rate-limit utilization — claude-code
    /// `rawUtilization` (`claudeAiLimits.ts:145-179`), tracked on every API
    /// response (unlike the warning-gated [`Self::RateLimit`] fields) and
    /// consumed by the statusline command input (`StatusLine.tsx:50-65`). A
    /// window is `None` when the response lacked either of its two headers.
    /// (llm-runtime future-work batch 5, Task 1.)
    RawUtilization {
        /// `anthropic-ratelimit-unified-5h-utilization` (0-1 fraction).
        five_hour_utilization: Option<f64>,
        /// `anthropic-ratelimit-unified-5h-reset` (unix epoch seconds).
        five_hour_resets_at: Option<u64>,
        /// `anthropic-ratelimit-unified-7d-utilization` (0-1 fraction).
        seven_day_utilization: Option<f64>,
        /// `anthropic-ratelimit-unified-7d-reset` (unix epoch seconds).
        seven_day_resets_at: Option<u64>,
    },
}

/// Severity / color of a context-pressure banner — mirrors the `<Text>` color
/// in claude-code's `TokenWarning.tsx:169` (`dimColor` for the auto-compact
/// countdown, `error` / `warning` for the "Context low" line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPressureLevel {
    /// `dimColor` — the auto-compact countdown branch.
    Dim,
    /// `color="warning"` — "Context low" below the error threshold.
    Warning,
    /// `color="error"` — "Context low" at/above the error threshold.
    Error,
}

/// A rendered context-pressure banner: the byte-exact label + its severity.
///
/// Computed by the orchestrator each turn (1:1 with claude-code's `TokenWarning`
/// component, via `compaction::token_warning_banner`) and pushed to the UI
/// through [`OutputStream::emit_context_pressure`]. `None` at that sink clears a
/// previously-shown banner once the context drops back below the warning
/// threshold (e.g. after a compaction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPressureBanner {
    /// The byte-exact banner text (no surrounding chrome).
    pub text: String,
    /// The text color / severity.
    pub level: ContextPressureLevel,
}

/// Sink for orchestrator-emitted output events.
///
/// The stdio CLI (M5-12) and the future TUI (M6) both implement this.
/// M5-02 ships `MockOutputStream` (in `lingxi-orchestrator::test_support`)
/// for unit tests.
#[async_trait]
pub trait OutputStream: Send + Sync {
    /// Emit a session-scoped SDK `system/task_started` or `task_updated` frame.
    async fn emit_task_lifecycle(&self, _event: &serde_json::Value) {}

    /// Identify the assistant response immediately before its completion boundary.
    async fn emit_assistant_message_identity(&self, _message_id: &crate::types::MessageId) {}

    /// Retract an assistant attempt that is being retried.
    async fn emit_message_retracted(&self, _message_id: &crate::types::MessageId) {}

    /// Signal a model turn that did not originate from a direct UI submit,
    /// such as an `asyncRewake` hook completion.
    async fn emit_turn_started(&self) {}

    /// Emit a piece of plain assistant text. In M5-02 this is called once
    /// per `Text` content block per turn (whole-body). M5-04 will switch
    /// to per-SSE-delta emission without changing this signature.
    async fn emit_text(&self, text: &str);

    /// Emit a user-visible system notice without adding model-facing text.
    ///
    /// This is a default no-op so embedded consumers that do not have a
    /// transcript/status surface remain source-compatible.
    async fn emit_system_notice(&self, _body: &str, _is_error: bool) {}

    /// Emit a tool-call notification immediately before dispatch.
    ///
    /// `id` is the `tool_use_id` echoed in the matching ToolResult. Added
    /// in M6-04 so consumers can correlate calls with results.
    async fn emit_tool_call(
        &self,
        id: &crate::types::ToolUseId,
        tool: &str,
        input: &serde_json::Value,
    );

    /// Emit a tool-result notification immediately after dispatch.
    ///
    /// `model_text` is the exact string the model saw for this tool result
    /// (the SDK frame's `tool_result.content`); `result` is the pure-metadata
    /// `data` payload (carried in `toolUseResult`).
    async fn emit_tool_result(
        &self,
        id: &crate::types::ToolUseId,
        tool: &str,
        model_text: &str,
        result: &serde_json::Value,
    );

    /// [`Self::emit_tool_result`] for a tool that was DENIED rather than run.
    ///
    /// `denial_kind` is claude-code's message-level `toolDenialKind`. The five
    /// values produced by the kind classifier (`jDd`, binary offset 235392096)
    /// are `user-rejected`, `permission-rule`, `automode-blocked`,
    /// `automode-unavailable`, `automode-parsing-error` — but that is NOT the
    /// whole value set: the abort paths stamp `cancelled` (offsets 235399713 /
    /// 235408905) and `interrupted` (via `YDd`, offset 235394375, which returns
    /// `cancelled` for a `background` abort reason and `interrupted` otherwise).
    /// Both are ordinary `toolDenialKind` values that DO produce a
    /// `tool_result_meta` entry — the builder gates only on the field being
    /// absent — and a downstream consumer (offset 245793995) reads them to
    /// classify a turn as `tool_abort` rather than `tool_denial`. Verified
    /// against real 2.1.220 transcripts, which contain `"interrupted"`.
    ///
    /// Transports that expose the provenance (stream-json emits
    /// `tool_result_meta`) override this; every other implementor inherits the
    /// default, which drops it and behaves exactly like
    /// [`Self::emit_tool_result`]. Defaulted deliberately: this trait has many
    /// implementors and a signature change would touch all of them for a field
    /// only the stream-json transport can carry.
    ///
    /// STAMPED by LingXi today: the five classifier kinds, `cancelled` on the
    /// pre-cancel guard (which claude-code hardcodes rather than deriving), and
    /// `interrupted` on a tool whose own `call` returned
    /// [`tool_api::ToolError::Aborted`].
    ///
    /// The `interrupted` stamp attaches at the SAME site claude-code uses: the
    /// per-tool execution catch `oQ_` (2.1.220 @235424972) puts
    /// `toolDenialKind: YDd(err, signal)` on the frame it builds there, so no
    /// emission-point change was needed — an earlier note here claimed the
    /// oracle derived it from the post-substitution message stream, which the
    /// binary contradicts (that path is the SYNTHETIC's `"user-rejected"`,
    /// `createSyntheticErrorMessage` @232972360, a different frame).
    /// Note that `YDd` (@235394375) splits `cancelled` from `interrupted` on a
    /// `background` abort reason, a concept LingXi has no equivalent of — every
    /// LingXi abort takes the `interrupted` branch. Its `hW.interrupted`
    /// (ShellError) branch is likewise unmodeled: LingXi's Bash tool returns an
    /// `Ok` interrupted result rather than an `Err`.
    ///
    /// The streaming driver buffers dispatch-side SDK frames and releases them
    /// from the executor's received-order drain point. That lets a cancelled
    /// tool substitute its final synthetic content and denial provenance before
    /// the frame is emitted, while still giving a queued-then-cancelled tool a
    /// frame even though dispatch never ran.
    async fn emit_tool_result_denied(
        &self,
        id: &crate::types::ToolUseId,
        tool: &str,
        model_text: &str,
        result: &serde_json::Value,
        _denial_kind: &str,
    ) {
        self.emit_tool_result(id, tool, model_text, result).await;
    }

    /// Emit a heartbeat for a tool call that is still running.
    ///
    /// Long-running tools can otherwise leave transport clients silent between
    /// `emit_tool_call` and `emit_tool_result`. `elapsed_ms` is the wall-clock
    /// age of the tool call, in milliseconds, from the dispatch point.
    ///
    /// **Default no-op**: sinks that do not surface tool heartbeats keep
    /// compiling unchanged.
    async fn emit_tool_heartbeat(
        &self,
        _id: &crate::types::ToolUseId,
        _tool: &str,
        _elapsed_ms: u64,
    ) {
    }

    /// Emit the end-of-turn marker with the cost snapshot.
    async fn emit_end_turn(&self, stop_reason: &str, cost: &CostSnapshot);

    /// Emit when compaction has STARTED (before the summarizer call). The
    /// consumer (TUI / CLI / bridge) should display a progress indicator
    /// (spinner / "Compacting…") until `emit_compaction_completed`
    /// fires.
    ///
    /// **Default no-op**: pre-existing sinks (CLI, mocks, NDJSON) keep
    /// compiling unchanged. The TUI bridge overrides this to show a spinner
    /// during the compaction wait.
    async fn emit_compaction_started(&self) {}

    /// Report an observed compaction phase (`summarizing` or `restoring`).
    /// Default no-op preserves existing CLI/SDK event sequences. Progress-aware
    /// clients use this together with started/finished, never a timer estimate.
    async fn emit_compaction_phase(&self, _phase: &str) {}

    /// Emit the committed compact boundary with its persistent UUID and metadata.
    /// SDK transports use this to preserve resume/relink information.
    async fn emit_compact_boundary(
        &self,
        _boundary_uuid: &str,
        _metadata: &crate::types::CompactBoundaryMetadata,
    ) {
    }

    /// End the compaction status lifecycle on success or failure. Unlike
    /// `emit_compaction_completed`, this also clears status after failed attempts.
    async fn emit_compaction_finished(&self, _error: Option<&str>) {}

    /// A started attempt did not need a compaction transition. Legacy streams
    /// retain their successful finish event; progress-aware clients can label
    /// this outcome without claiming that summary/restoration completed.
    async fn emit_compaction_skipped(&self) {
        self.emit_compaction_finished(None).await;
    }

    /// Emit a transcript-only synthetic summary after its compact boundary.
    async fn emit_compact_summary(&self, _summary_uuid: &str, _summary: &str) {}

    /// Emit a compaction-completed event, its size delta, and the summary that
    /// transcript UIs reveal in verbose mode.
    /// Default no-op for adapters that do not render compaction state.
    async fn emit_compaction_completed(
        &self,
        _messages_before: u32,
        _messages_after: u32,
        _bytes_saved: u64,
        _summary: &str,
    ) {
    }

    /// Emit a streaming reasoning ("thinking") delta as it arrives.
    ///
    /// Added by the §0.7 "light up thinking/usage" follow-up. Called once
    /// per `ThinkingDelta` SSE chunk from `event_router`. `signature` is
    /// `None` for live deltas — the cryptographic signature only arrives on
    /// the completed thinking block (`SignatureDelta`), not per-delta, so
    /// the live-delta path always passes `None`.
    ///
    /// **Default no-op**: pre-existing sinks (TUI, CLI, `MockOutputStream`)
    /// that don't render reasoning keep compiling unchanged. The
    /// client-adapter overrides this to surface a `ClientEvent::ThinkingDelta`.
    async fn emit_thinking(&self, _thinking: &str, _signature: Option<&str>) {}

    /// Preserve an opaque redacted-thinking block at the completed-message
    /// boundary. Most interactive surfaces have no incremental representation
    /// for this provider payload, so the default remains a no-op.
    async fn emit_redacted_thinking(&self, _data: &str) {}

    /// Update how subsequent thinking blocks are presented. `"omitted"`
    /// suppresses them; `"summarized"` restores the sink's normal collapsed or
    /// structured representation. The default is a no-op for sinks that never
    /// render thinking.
    fn set_thinking_display(&self, _mode: Option<&str>) {}

    /// Emit one nested execution line from a RUNNING subagent (its tool calls,
    /// as they happen) so the UI can surface the subagent's work under its
    /// `Task` cell — the parity gap where only the top-level Task line showed.
    /// `text` is a pre-formatted one-line summary (e.g. `"Read(src/main.rs)"`).
    ///
    /// **Default no-op**: sinks that don't render nested progress (CLI,
    /// mocks) keep compiling unchanged; the TUI bridge overrides it.
    async fn emit_subagent_activity(&self, _text: &str) {}

    /// Emit a spawned subagent's assistant TEXT + THINKING as an `assistant`
    /// frame whose `parent_tool_use_id` is the spawning `Task`/`Agent`
    /// tool_use_id — the `--forward-subagent-text` (2.1.212) deep-forwarding
    /// path. `message` is the serialized subagent `ConversationMessage`
    /// (`agent::SubagentEvent::Message`); `parent_tool_use_id` is the parent
    /// turn's Task tool_use_id (`ToolUseId::as_str`).
    ///
    /// **Default no-op**: only the stream-json sink overrides this (and only
    /// re-emits when its `--forward-subagent-text` gate is on); every other sink
    /// (TUI, mocks) ignores it, so they compile unchanged.
    async fn emit_forwarded_subagent_message(
        &self,
        _message: &serde_json::Value,
        _parent_tool_use_id: &str,
    ) {
    }

    /// Emit a user-visible attachment record surfaced during this turn.
    ///
    /// **Default no-op**: sinks that render no attachments keep compiling; the
    /// client adapter overrides it.
    async fn emit_attachment(&self, _attachment: AttachmentKind) {}

    /// Emit a retry-backoff status while an API request is being retried, so the
    /// UI can show Claude Code's `SystemAPIErrorMessage` line —
    /// `"<error> · Retrying in Ns… (attempt X/Y)"` — during an otherwise-silent
    /// backoff. `delay_ms` seeds the live countdown; `attempt`/`max_retries`
    /// mirror `retryAttempt`/`maxRetries`.
    ///
    /// **Default no-op**: sinks that don't render retry status (CLI, mocks) keep
    /// compiling unchanged; the TUI bridge overrides it.
    async fn emit_api_retry(
        &self,
        _message: &str,
        _attempt: u32,
        _max_retries: u32,
        _delay_ms: u64,
    ) {
    }

    /// Emit an incremental token-usage update for the latest API call.
    ///
    /// Added by the §0.7 "light up thinking/usage" follow-up. Called from
    /// `event_router` when a `MessageDelta`/`MessageStart` SSE event carries
    /// a `usage` payload. Counts are passed as bare `u64`s (rather than a
    /// `cost::TokenUsage`) to keep `core::host` a leaf crate: `lingxi-cost`
    /// already depends on `core::host`, so a `cost` dependency here would
    /// form a cycle. The four arguments map field-for-field onto both
    /// `cost::TokenUsage` (caller side, in the orchestrator) and
    /// `ClientEvent::UsageUpdate` (adapter side).
    ///
    /// **Default no-op**: pre-existing sinks keep compiling unchanged. The
    /// client-adapter overrides this to surface a `ClientEvent::UsageUpdate`.
    async fn emit_usage(
        &self,
        _input_tokens: u64,
        _output_tokens: u64,
        _cache_read_tokens: u64,
        _cache_creation_tokens: u64,
    ) {
    }

    /// Emit a coordinator team-status update (active worker count + team name).
    ///
    /// Added by the coordinator-activation program. Called by the
    /// `CoordinatorStatusSink` after each teammate status transition that
    /// changes the active-worker tally, so a coordinator-mode session can
    /// surface a live roster count.
    ///
    /// **Default no-op**: pre-existing sinks (TUI, CLI, `MockOutputStream`)
    /// keep compiling unchanged. The client-adapter overrides this to surface
    /// a `ClientEvent::CoordinatorStatus`.
    async fn emit_coordinator_status(&self, _active_workers: u32, _team: Option<&str>) {}

    /// Push a changed coordinator roster row, including independent plan-review state.
    async fn emit_coordinator_worker(&self, _worker: &crate::host::team_registry::WorkerInfo) {}

    /// Emit the latest unified rate-limit header snapshot.
    ///
    /// Added by llm-runtime future-work batch 3 (Task 8). Called by the
    /// orchestrator turn drivers after each completed API call whose
    /// rate-limit snapshot DIFFERS from the previously emitted one — the
    /// orchestrator dedupes, so sinks only ever see changes. The first nine
    /// arguments map field-for-field onto [`OutputEvent::RateLimit`]; see
    /// that variant's per-field docs for the `anthropic-ratelimit-unified-*`
    /// header each value is parsed from (claude-code `claudeAiLimits.ts`).
    /// `upgrade_paths` / `credits_required` were added for 2.1.206 and map
    /// onto the same variant's trailing two fields.
    ///
    /// **Default no-op**: pre-existing sinks (TUI, CLI, `MockOutputStream`)
    /// keep compiling unchanged. The TUI bridge overrides this to surface
    /// the rate-limit status message.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors OutputEvent::RateLimit's eleven header-derived fields; the bare-argument shape matches the emit_usage convention on this trait"
    )]
    async fn emit_rate_limit(
        &self,
        _status: Option<&str>,
        _rate_limit_type: Option<&str>,
        _utilization: Option<f64>,
        _resets_at: Option<u64>,
        _claim_resets_at: Option<u64>,
        _overage_status: Option<&str>,
        _overage_resets_at: Option<u64>,
        _overage_disabled_reason: Option<&str>,
        _fallback_available: Option<bool>,
        _upgrade_paths: Option<&[String]>,
        _credits_required: bool,
    ) {
    }

    /// Push the current context-pressure banner, or `None` to clear it.
    ///
    /// Called by the turn driver before every API call with the result of
    /// `compaction::token_warning_banner` for the current context estimate — the
    /// orchestrator-side twin of claude-code's `<TokenWarning>` render
    /// (`PromptInput/Notifications.tsx:321`), which recomputes
    /// `calculateTokenWarningState` as `tokenUsage` grows. Default no-op so
    /// non-interactive sinks (print mode, tests) ignore it.
    /// `used_fraction` is the current context usage as a 0-1 fraction of the
    /// model's effective context window (claude-code
    /// `calculateContextPercentages(currentUsage, contextWindowSize).used`),
    /// carried alongside the (optional) banner so a consumer that wants the raw
    /// percentage — the custom statusline's `context_window.used_percentage` —
    /// gets it on every turn, not only when the warning banner is showing.
    /// `used_tokens` / `context_window_tokens` are the raw inputs behind the
    /// fraction (the same token estimate + effective window the auto-compact
    /// gate uses), so the statusline can also populate the 2.1.206 payload's
    /// `context_window.total_input_tokens` / `context_window_size` and derive
    /// `exceeds_200k_tokens`.
    async fn emit_context_pressure(
        &self,
        _banner: Option<ContextPressureBanner>,
        _used_fraction: f32,
        _used_tokens: u64,
        _context_window_tokens: u64,
    ) {
    }

    /// Push a raw-utilization snapshot.
    ///
    /// Added by llm-runtime future-work batch 5 (Task 1). Called by the
    /// orchestrator turn drivers after every completed API call — unlike the
    /// deduped, warning-gated [`Self::emit_rate_limit`], this mirrors
    /// claude-code's `rawUtilization` tracking (`claudeAiLimits.ts:145-179`),
    /// which records the per-window headers on every response for the
    /// statusline command input (`StatusLine.tsx:50-65`). The four arguments
    /// map field-for-field onto [`OutputEvent::RawUtilization`]; see that
    /// variant's per-field docs for the `anthropic-ratelimit-unified-*`
    /// header each value is parsed from.
    ///
    /// **Default no-op**: hosts without a statusline ignore it, and every
    /// pre-existing sink (TUI, CLI, `MockOutputStream`) keeps compiling
    /// unchanged.
    async fn emit_raw_utilization(
        &self,
        _five_hour_utilization: Option<f64>,
        _five_hour_resets_at: Option<u64>,
        _seven_day_utilization: Option<f64>,
        _seven_day_resets_at: Option<u64>,
    ) {
    }

    /// Write an allowlisted terminal escape sequence to the active terminal.
    ///
    /// The orchestrator calls this (#6 main-loop parity) after a hook returns a
    /// `terminalSequence` that PASSES the OSC/BEL allowlist validator
    /// (claude-code `szn`→`BEo`, which writes the validated sequence to the
    /// controlling terminal). The orchestrator process holds no TTY — the TUI
    /// owns the terminal — so it forwards the validated bytes through this seam;
    /// the interactive host (TUI) writes them to its stdout. `seq` is the
    /// already-validated, BEL-normalized string.
    ///
    /// **Default no-op**: non-interactive hosts (print mode, tests, CLI sink)
    /// and any host without a controlling terminal ignore it, and every
    /// pre-existing sink keeps compiling unchanged.
    async fn emit_terminal_sequence(&self, _seq: &str) {}

    /// Signal that an Anthropic API message has begun streaming (i.e. on
    /// `message_start`). Passes the API-assigned message id and model so sinks
    /// that accumulate per-message blocks can record them before any deltas arrive.
    ///
    /// **Default no-op**: every pre-existing `OutputStream` impl keeps compiling
    /// unchanged.
    ///
    /// # Arguments
    /// * `message_id` — the provider-assigned message id (e.g. `msg_01…`).
    /// * `model` — the model id that produced this message.
    async fn emit_message_start(&self, _message_id: &str, _model: &str) {}

    /// Signal that one Anthropic API message has completed streaming.
    ///
    /// Called by the streaming loop immediately after [`RouterAction::EndOfStream`]
    /// (i.e. when the `message_stop` SSE event arrives). Consumers that need to
    /// batch text+thinking+tool_use blocks into a single `assistant` frame
    /// (stream-json output) flush their accumulator here.
    ///
    /// **Default no-op**: every pre-existing `OutputStream` impl (TUI,
    /// CLI `SinkAdapter`, `MockOutputStream`) keeps compiling unchanged.
    ///
    /// # Arguments
    /// * `stop_reason` — the `stop_reason` from the final `message_delta` (e.g.
    ///   `"end_turn"`), or `None` if the delta carried no stop reason.
    /// * `request_id` — the HTTP `request-id` header from the API response, when
    ///   available (used by stream-json's `assistant.request_id` field).
    async fn emit_message_boundary(&self, _stop_reason: Option<&str>, _request_id: Option<&str>) {}

    /// Emit a raw SSE stream event frame (`stream_event`) for
    /// `--include-partial-messages`.
    ///
    /// `event_json` is a JSON string reconstructed from the parsed `HistoryEvent`
    /// (semantically equivalent to the original Anthropic SSE event, but NOT
    /// byte-for-byte identical since LingXi already parsed the raw bytes).
    /// `is_message_start` is `true` only for the `message_start` event, which
    /// is the only event that carries `ttft_ms` in the GROUND-TRUTH output.
    ///
    /// **Default no-op**: every pre-existing `OutputStream` impl keeps
    /// compiling unchanged. Only `StreamJsonStream` overrides this.
    async fn emit_stream_event(&self, _event_json: &str, _is_message_start: bool) {}

    /// Whether [`Self::emit_stream_event`] will actually consume reconstructed
    /// SSE JSON. Default `false` so the streaming loop can skip serialize work
    /// for TUI / mock sinks. Stream-json with `--include-partial-messages` returns
    /// true.
    fn wants_partial_stream_events(&self) -> bool {
        false
    }

    /// Emit a `system/hook_started` frame for `--include-hook-events`.
    ///
    /// Called before each hook dispatches. `hook_id` and `hook_name` identify
    /// the hook; `hook_event` is the `Debug` name of the `HookEventType`
    /// (e.g. `"SessionStart"`, `"PreToolUse"`).
    ///
    /// **Default no-op**: every pre-existing `OutputStream` impl keeps
    /// compiling unchanged. Only `StreamJsonStream` overrides this.
    async fn emit_hook_started(&self, _hook_id: &str, _hook_name: &str, _hook_event: &str) {}

    /// Publish transient hook progress for interactive renderers.
    ///
    /// Unlike [`Self::emit_hook_started`], this callback is not part of the
    /// stream-json protocol. It exists only for live surfaces such as the TUI,
    /// so a hook's configured `statusMessage` can replace the generic spinner
    /// text without adding a conversation or transcript record.
    async fn emit_hook_progress_started(
        &self,
        _progress_id: &str,
        _hook_name: &str,
        _hook_event: &str,
        _status_message: Option<&str>,
    ) {
    }

    /// Clear one transient hook-progress row after its run reaches a terminal
    /// outcome. The default remains a no-op for non-interactive output sinks.
    async fn emit_hook_progress_finished(&self, _progress_id: &str) {}

    /// SH-07 — `Q9i(hookEvent)` (oracle 2.1.238 @ 296463298): is this sink
    /// currently streaming hook lifecycle frames for `hook_event`?
    ///
    /// ```js
    /// function Q9i(e){ if(TjT.includes(e))return!0;
    ///   return Z4m().allHookEventsEnabled&&n9.includes(e) }
    /// ```
    /// with `TjT=["SessionStart","Setup"]`.
    ///
    /// Upstream reads it as the HEAD of `tWi` — `if(!Q9i(e.hookEvent))return
    /// ()=>{}` — so a host that is not streaming hook events never pays for the
    /// 1 s progress poll or the live pipe reads at all. The hook executor asks
    /// this before arming the poll for the same reason.
    ///
    /// **Default `false`**: a sink that does not implement
    /// [`Self::emit_hook_progress_frame`] would drop the frames anyway, so the
    /// default keeps every non-stream-json host (TUI, mocks) on the cheap
    /// buffered read path.
    fn hook_events_streamed(&self, _hook_event: &str) -> bool {
        false
    }

    /// SH-07 — emit a `system/hook_progress` stream-json frame
    /// (`--include-hook-events`).
    ///
    /// This is the WIRE frame, not the transient TUI row that
    /// [`Self::emit_hook_progress_started`] drives. Oracle 2.1.238 @ 296463298:
    ///
    /// ```js
    /// function EjT(e){ if(!Q9i(e.hookEvent))return;
    ///   u0({type:"system",subtype:"hook_progress",hook_id:e.hookId,
    ///       hook_name:e.hookName,hook_event:e.hookEvent,
    ///       stdout:e.stdout,stderr:e.stderr,output:e.output}) }
    /// ```
    ///
    /// It shares the `hook_started`/`hook_response` gate (`Q9i`): SessionStart
    /// and Setup always stream, everything else needs the flag.
    ///
    /// **Default no-op**: every pre-existing `OutputStream` impl keeps
    /// compiling unchanged. Only `StreamJsonStream` overrides this.
    async fn emit_hook_progress_frame(
        &self,
        _hook_id: &str,
        _hook_name: &str,
        _hook_event: &str,
        _stdout: &str,
        _stderr: &str,
        _output: &str,
    ) {
    }

    /// Emit a `system/hook_response` frame for `--include-hook-events`.
    ///
    /// Called after each hook completes. Parameters carry the hook identity
    /// fields (same as [`Self::emit_hook_started`]) plus the full result:
    /// `output` is the combined hook response text (stdout + any
    /// `systemMessage`); `exit_code` is `None` for non-command hooks;
    /// `outcome` is `"success"`, `"error"`, `"block"`, or `"timeout"`.
    ///
    /// **Default no-op**: every pre-existing `OutputStream` impl keeps
    /// compiling unchanged. Only `StreamJsonStream` overrides this.
    #[allow(clippy::too_many_arguments)]
    async fn emit_hook_response(
        &self,
        _hook_id: &str,
        _hook_name: &str,
        _hook_event: &str,
        _output: &str,
        _stdout: &str,
        _stderr: &str,
        _exit_code: Option<i32>,
        _outcome: &str,
    ) {
    }
}

#[cfg(test)]
#[path = "orchestrator_test.rs"]
mod orchestrator_test;
