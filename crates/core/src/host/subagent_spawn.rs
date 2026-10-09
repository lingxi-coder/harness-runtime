//! `SubagentSpawner` — narrow trait abstracting `StateMachinePool::allocate`
//! plus the loop until the spawned subagent reports a terminal event.
//!
//! Concrete impl lives in `lingxi-agent` (production adapter over
//! `StateMachinePool` + `ToolRegistry` + `BudgetEnforcer`). Tests inject a
//! recording mock that captures the parent registry / budget Arcs so the
//! `Arc::ptr_eq` recursion-lock + budget-inheritance assertions can fire.
//!
//! See M4-05 wiring follow-up plan.

use crate::host::budget::BudgetEnforcerHandle;
use crate::host::tool_invoker::ToolInvoker;
use crate::types::{AgentId, ConversationMessage, SessionId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Current persisted observer schema version.
pub const OBSERVER_SCHEMA_VERSION: u32 = 1;
/// Internal query-source identity for independent observer sidecars.
pub const OBSERVER_QUERY_SOURCE: &str = "observer_activity";

const fn observer_schema_version() -> u32 {
    OBSERVER_SCHEMA_VERSION
}

const fn observer_default_true() -> bool {
    true
}

/// Versioned observer declaration carried by agent definitions and spawns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverSpec {
    /// Persistence schema version. Missing legacy values deserialize as v1.
    #[serde(default = "observer_schema_version")]
    pub schema_version: u32,
    /// Agent type to run as the observer.
    pub agent: String,
    /// Optional observer-specific instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether the declaration propagates to descendant spawns. Absent means
    /// true, matching Claude Code's `observeSubagents !== false`.
    #[serde(default = "observer_default_true")]
    pub observe_subagents: bool,
}

impl ObserverSpec {
    /// Create a v1 observer declaration with descendant propagation enabled.
    #[must_use]
    pub fn new(agent: impl Into<String>) -> Self {
        Self {
            schema_version: OBSERVER_SCHEMA_VERSION,
            agent: agent.into(),
            message: None,
            observe_subagents: true,
        }
    }
}

/// Per-turn `tool_choice` policy for a schema'd subagent spawn (see
/// [`SubagentSpawnRequest::structured_output_mode`]).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StructuredOutputMode {
    /// Force `tool_choice: {type:"tool", name:"StructuredOutput"}` on every
    /// round-trip of the run. Matches the pre-existing (pre-Fusion) runner
    /// behavior: the child cannot use any other tool and must answer from
    /// turn 1. The default, so every caller that predates this field keeps
    /// byte-identical behavior.
    #[default]
    Forced,
    /// Use the model's normal (auto) `tool_choice` while turns remain, so the
    /// child can call its other tools (Read/Grep/Glob/WebFetch/…) across
    /// multiple turns. `StructuredOutput` is forced only on the run's LAST
    /// turn, or once the model has produced two consecutive turns with no
    /// tool call and no `StructuredOutput` call — whichever comes first.
    WhenDone,
}

fn is_zero_parse_retries(value: &u32) -> bool {
    *value == 0
}

/// Locked subagent input passed to [`SubagentSpawner::spawn`].
///
/// Mirrors `AgentToolInput` in `lingxi-tools::builtin::agent` byte-for-byte
/// so the trait surface stays insulated from `lingxi-tools`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SubagentSpawnRequest {
    /// The subagent type to resolve (built-in or user/project catalog). NOT
    /// validated here — the spawner resolves it with claude-code precedence
    /// (catalog overrides built-ins; unknown → `general-purpose`).
    pub subagent_type: String,
    /// Initial prompt seeded into the subagent's first turn.
    pub prompt: String,
    /// Effective observer inherited or declared for this spawn. The runtime
    /// validates the agent name and observer chain before launching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer: Option<ObserverSpec>,
    /// Optional context-path files injected as system-tagged messages.
    #[serde(default)]
    pub context_paths: Vec<PathBuf>,
    // ===== AgentTool spawn-surface parity (coordinator batch D2a) =====
    // Additive optional fields mirroring claude-code's `AgentTool` schema
    // (`AgentTool.tsx:82-101`). All `#[serde(default)]`/`Option` so existing
    // call sites and serialized payloads remain valid (frozen-crate rule).
    /// Short (3-5 word) human description of the task (TS `description`,
    /// required in the model-facing schema). Carried for telemetry / display.
    #[serde(default)]
    pub description: Option<String>,
    /// Model-family override (`"sonnet"` | `"opus"` | `"haiku"`). Takes
    /// precedence over the resolved [`crate::host::…`] agent definition's model
    /// (TS `model`). The spawner maps this onto the agent model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Provider profile name for routing the child's model, e.g. the candidate's
    /// resolved profile; `None` = default/unscoped resolution. When set, the
    /// spawner uses [`Self::model`] verbatim as the explicit wire model and
    /// threads this profile through to the subagent api client so the round-trip
    /// targets the named provider (the dual-LLM dual-PROVIDER routing). When
    /// `None`, model resolution + provider selection are unchanged (the legacy
    /// default-provider path).
    #[serde(default)]
    pub model_profile: Option<String>,
    /// Session-assigned teammate display color. Internal runtime metadata,
    /// populated by identity reservation rather than model-facing Agent input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teammate_color: Option<String>,
    /// Whether to run the spawned agent in the background (TS `run_in_background`,
    /// `AgentTool.tsx:87` `z.boolean().optional()`). claude treats it as a
    /// boolean predicate (`run_in_background === true`), so it is collapsed to a
    /// plain `bool` (default `false`) at this seam — `None`/absent ⇒ `false` —
    /// rather than carrying an `Option<bool>`. The model-facing schema in
    /// `tools/agent` already declares + parses it; this field stops it being
    /// dropped when the request is built.
    #[serde(default)]
    pub run_in_background: bool,
    /// Name for the spawned agent, making it addressable via `SendMessage`
    /// (TS `name`). Carried through; teammate routing is deferred.
    #[serde(default)]
    pub name: Option<String>,
    /// Team name for spawning (TS `team_name`). Carried through; teammate
    /// routing is deferred.
    #[serde(default)]
    pub team_name: Option<String>,
    /// DISPLAY name of the teammate / subagent that created this spawn request.
    /// Distinct from the TARGET child `name` above.
    #[serde(default)]
    pub creator_teammate_name: Option<String>,
    /// Team name of the teammate / subagent that created this spawn request.
    /// Distinct from the TARGET child `team_name` above.
    #[serde(default)]
    pub creator_team_name: Option<String>,
    /// Persistent agent id of the teammate / subagent that created this spawn
    /// request. Distinct from the TARGET child `name`/`team_name` display
    /// fields above, and defaulted so legacy serialized payloads still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_agent_id: Option<AgentId>,
    /// Internal inherited permission mode for a persistent teammate. The Agent
    /// tool resolves this from the live parent mode, never from its deprecated
    /// model-facing `mode` parameter. The teammate runtime uses it for plan-mode
    /// requirements and child permission inheritance; ordinary Agent calls send
    /// `None` and inherit through their regular runtime context.
    #[serde(default)]
    pub mode: Option<String>,
    /// Isolation mode (`"worktree"` | `"remote"`, TS `isolation`). `worktree`
    /// is realized by the Agent/workflow spawn entrypoints before dispatch so
    /// both sync and async children run in the resolved cwd. `remote` is carried
    /// through for callers that understand it; local runners currently treat it
    /// as non-worktree execution.
    #[serde(default)]
    pub isolation: Option<String>,
    /// Absolute path to run the agent in (TS `cwd`). Carried through; the
    /// cwd-override behavior is deferred.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The isolation worktree `AgentTool` created for this agent
    /// (`isolation:"worktree"`), resolved BEFORE the sync/async dispatch branch
    /// (claude 2.1.207 runs `createAgentWorktree` before branching and threads
    /// the handle into both). Carried so the BACKGROUND lifecycle owner (the
    /// `local_agent` task handler) can run the terminal keep/cleanup judgment —
    /// claude's `getWorktreeResult` closure handed to the detached task
    /// ([`crate::host::worktree::agent_worktree_result`]) — since the tool returns
    /// `async_launched` immediately and must NOT clean up at launch. The sync
    /// path's judgment stays in `AgentTool` itself; the spawner ignores this
    /// field. `#[serde(default)]` + skip-when-`None` so legacy serialized
    /// payloads round-trip byte-identically (frozen-crate rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<crate::host::worktree::WorktreeHandle>,
    // ===== Fork-subagent path (codex #5) =====
    // Populated ONLY on the `AgentTool` fork path (subagent_type omitted + the
    // fork gate ON). For every non-fork spawn they stay `None` and the spawner
    // builds the child's system prompt from the resolved `AgentDefinition` body
    // as before. Both `#[serde(default)]` so existing serialized payloads stay
    // valid (frozen-crate rule).
    /// The byte-exact forked conversation prefix the child replays as its cache
    /// prefix (TS `forkContextMessages = toolUseContext.messages`, threaded as
    /// `buildForkedMessages(prompt, assistantMessage)` output: the cloned parent
    /// assistant message + a user message of placeholder tool_results + the
    /// per-child directive). `AgentTool` owns the parent assistant message, so it
    /// builds these and ships them here; on the fork path the spawner seeds
    /// `prompt_messages = []` and lets the runner replay this prefix verbatim.
    #[serde(default)]
    pub fork_context_messages: Option<Vec<crate::types::ConversationMessage>>,
    /// The parent's already-rendered system prompt bytes (TS
    /// `override.systemPrompt = forkParentSystemPrompt`, sourced from
    /// `toolUseContext.renderedSystemPrompt`). When set, the spawner uses these
    /// bytes VERBATIM as the child's system prompt and SKIPS the subagent
    /// `Notes:` trailer (re-appending it would bust the prompt cache). `None`
    /// (non-fork, or the orchestrator has not yet threaded the rendered prompt
    /// onto `ToolUseContext`) keeps the existing body+trailer behavior.
    #[serde(default)]
    pub fork_parent_system_prompt: Option<String>,
    /// Structured-output schema (JSON Schema, serialised as a string) the child
    /// must satisfy: the runner injects a forced `StructuredOutput` tool whose
    /// `input_schema` IS this schema, forces `tool_choice` to it, and returns the
    /// model's tool input as the result (claude-code's workflow `agent({schema})`
    /// — validation happens at the tool-call layer). `None` ⇒ free-form text.
    #[serde(default)]
    pub schema: Option<String>,
    /// Per-turn `tool_choice` policy applied while [`Self::schema`] is `Some`.
    /// Additive — `#[serde(default)]` resolves to [`StructuredOutputMode::Forced`],
    /// byte-identical to the pre-existing behavior for every caller that predates
    /// this field (workflow `agent({schema})`). Fusion panels (`fusion::panel::spawn_request`)
    /// request [`StructuredOutputMode::WhenDone`] so the panel can use its Read /
    /// Grep / Glob / WebFetch tools instead of being forced to call
    /// `StructuredOutput` on turn 1.
    #[serde(default)]
    pub structured_output_mode: StructuredOutputMode,
    /// Opt-in malformed StructuredOutput argument recovery; supported range is 0..=2.
    /// A Host-verified Create workflow also shares this budget with malformed
    /// arguments to advertised local tools; ordinary callers retain the default.
    #[serde(default, skip_serializing_if = "is_zero_parse_retries")]
    pub structured_output_parse_retries: u32,
    /// Per-spawn thinking-effort override (claude-code workflow `agent({effort})`
    /// — `me={...ie,effort:ae}`): a level string (`"low"`..`"max"`) or an integer
    /// budget. When set, the spawner overrides the resolved agent definition's
    /// `effort`. `None` ⇒ the definition's own effort (frontmatter) stands.
    #[serde(default)]
    pub effort: Option<serde_json::Value>,
    /// Originating `tool_use_id` of the spawning Agent tool call. Threaded into a
    /// BACKGROUND agent's task so its `<task-notification>` carries the
    /// `<tool-use-id>` line (claude-code stamps `toolUseId` on the async task).
    /// `None` for sync spawns / call sites that don't carry it.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Per-spawn system-prompt override: replaces the resolved `AgentDefinition`'s
    /// `system_prompt` field before the `Notes:` trailer is appended. Used by the
    /// workflow runtime to substitute the schema-variant xBp prompt when
    /// `agent({schema})` is called without an explicit `agentType`
    /// (claude-code `DBp.getSystemPrompt => xBp`). `None` ⇒ use the definition's own body.
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    /// Per-spawn system-prompt addendum: appended to the fully-rendered system
    /// prompt (after Notes + env block). Used by the workflow runtime to inject
    /// the HBp/IBp NOTE addendum when the caller specifies an explicit `agentType`
    /// (claude-code's appended NOTE for workflow-context agents). `None` ⇒ no addendum.
    #[serde(default)]
    pub system_prompt_addendum: Option<String>,
    /// Per-spawn additional disallowed tools: unioned with the resolved
    /// `AgentDefinition`'s `disallowed_tools` before tool resolution. Used by
    /// the workflow runtime to enforce `{SendUserMessage, Agent, Workflow}` on
    /// user-specified agentType spawns (claude-code's disallow union, §6). Empty ⇒ no extra tools denied.
    #[serde(default)]
    pub additional_disallowed_tools: Vec<String>,
    /// The CHILD's recursion depth = the spawning agent's depth + 1 (claude
    /// `spawnDepth = z6(parentContext) + 1`). The spawner stamps it onto the
    /// child's `SubagentContext.depth`, which the tool-resolver consults against
    /// `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` (default 3). `#[serde(default)]` ⇒
    /// `0` for legacy/serialized payloads, so a child that deserializes without
    /// it behaves like a top-level spawn (the conservative direction).
    #[serde(default)]
    pub depth: u32,
    /// Trusted originating session for nested tool calls and Fusion budget
    /// scoping. This is propagated explicitly across agent boundaries instead
    /// of being inferred from hook/session metadata; `None` preserves legacy
    /// call sites that have no owning session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<SessionId>,
    /// The parent / main-loop model to resolve this spawn's `AgentModel::Inherit`
    /// + bare-family aliases against (claude-code `AgentTool.tsx:418`
    /// `getAgentModel(selectedAgent.model, toolUseContext.options.mainLoopModel, …)`).
    /// `AgentTool` fills it from `ToolUseContext.options.main_loop_model` — the
    /// LIVE session model at the top level (so a mid-session `/model` switch is
    /// reflected), or the IMMEDIATE parent subagent's resolved model on a NESTED
    /// spawn (claude `runAgent.ts:678` seeds each child's `mainLoopModel:
    /// resolvedAgentModel`). When set (and non-empty) it takes precedence over the
    /// spawner's boot/live `default_model`; `None`/empty ⇒ the spawner's own
    /// default is used (non-`AgentTool` spawn paths / legacy serialized payloads).
    #[serde(default)]
    pub parent_model_override: Option<String>,
    /// The SKILL this background agent IS, when it was launched by a
    /// `context: fork` skill (claude `forkedSkillName`).
    ///
    /// Distinct from every other identity on this request: `subagent_type` is
    /// the agent definition, `name` the `SendMessage` handle. This is the skill
    /// whose permission scoping the run adopted — it keys the live-duplicate
    /// guard (one live fork per skill) and is the identity a later resume
    /// corroborates against the on-disk scoping record
    /// (`session::forked_skill`). `None` for every non-fork spawn.
    #[serde(default)]
    pub forked_skill_name: Option<String>,
    /// The display name a forked skill's run is attributed to (claude
    /// `attributionName`, from the launch's `spawnedBySkill`). Persisted in the
    /// scoping sidecar so a resume reconstructs the same attribution.
    #[serde(default)]
    pub forked_skill_attribution: Option<String>,
    /// The forked skill's declared effort, persisted with its scoping.
    #[serde(default)]
    pub forked_skill_effort: Option<String>,
    /// Command-deny rules FROZEN at fork time and replayed ahead of the live
    /// deny list on resume, so a later settings edit cannot widen what an
    /// already-running fork is allowed to run. Empty ⇒ no key is persisted.
    #[serde(default)]
    pub frozen_command_denies: Vec<String>,
    /// A conversation recovered from this agent's persisted transcript, when
    /// this spawn is a RESTORE of an agent that parked in an earlier process.
    ///
    /// Distinct from [`Self::fork_context_messages`], and deliberately so: that
    /// field is a PREFIX the runner adds ahead of the prompt and the
    /// `SubagentStart` / skills preload, whereas this REPLACES all three. A
    /// restore that reused the fork field would re-inject context the agent has
    /// already seen and re-fire start hooks for a run that began elsewhere.
    #[serde(default)]
    pub resumed_history: Option<Vec<crate::types::ConversationMessage>>,
    /// Lower the resolved definition's `max_turns` for this spawn (cannot raise it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_override: Option<u32>,
    /// Per-turn output token cap forwarded to the API client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens_per_turn: Option<u32>,
    /// Per-turn input payload cap in bytes (oldest messages dropped first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_bytes_per_turn: Option<u64>,
    /// COGS query-source label (e.g. `"fusion_panel"`). String to avoid a
    /// `core::host` → `sidequery` cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_source_label: Option<String>,
    /// Caller correlation id (Fusion run id + panel index).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Host-only registered model-call capability for this child. Neither
    /// model-authored input nor serialized task replay may mint this authority.
    ///
    /// Deliberately LAST: `main` grows this struct from the front, and both
    /// sides inserting one line after the same `{` is what turns a dozen
    /// mechanical additions into three-way conflicts.
    #[serde(skip)]
    pub model_attempt: Option<crate::host::ModelAttemptContext>,
}

/// Workflow-scoped model-query stall policy.
///
/// The timeout is an *idle* timeout: it is applied independently to opening a
/// response stream and to each `stream.next()` wait. A stream that continues to
/// produce events may run longer than this duration. `max_retries` counts
/// retries after the initial attempt (default: one initial attempt plus at most
/// five retries). Response-body recovery is disabled unless a trusted host-managed
/// creation workflow explicitly enables it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowQueryWatchdog {
    /// Maximum idle time for stream-open or the next stream event.
    pub stall_timeout_ms: u64,
    /// Retry limit for watchdog timeouts, also shared by opted-in body recovery.
    pub max_retries: u32,
    /// Host-only opt-in for host-managed app creation. Ordinary workflows retain their
    /// existing watchdog behavior, and serialized callers cannot enable it.
    #[serde(skip)]
    pub retry_response_body: bool,
}

impl WorkflowQueryWatchdog {
    /// Claude Code workflow query stall timeout: three minutes.
    pub const DEFAULT_STALL_TIMEOUT_MS: u64 = 180_000;
    /// Claude Code workflow query retry cap.
    pub const DEFAULT_MAX_RETRIES: u32 = 5;
}

impl Default for WorkflowQueryWatchdog {
    fn default() -> Self {
        Self {
            stall_timeout_ms: Self::DEFAULT_STALL_TIMEOUT_MS,
            max_retries: Self::DEFAULT_MAX_RETRIES,
            retry_response_body: false,
        }
    }
}

/// `#[serde(default = ...)]` for [`SubagentResult::Completed::usage_complete`]
/// — a wire payload with no such field must decode as `true` (a normal,
/// complete usage rollup), not `bool::default()`'s `false`.
fn usage_complete_default() -> bool {
    true
}

/// Token-usage rollup returned at the end of a successful spawn.
///
/// Mirrors the primary numeric buckets of claude-code's AgentTool result `usage`
/// object (`agentToolUtils.ts:238-256`): `input_tokens`, `output_tokens`,
/// `cache_creation_input_tokens`, `cache_read_input_tokens`. The legacy
/// [`SubagentUsage::total_tokens`] footer field is retained (existing consumers
/// in `tasks::handlers::local_agent` / `dream` spool a `<usage><total_tokens>`
/// footer from it).
///
/// The nullable sub-objects from claude's schema — `server_tool_use`
/// (`{web_search_requests, web_fetch_requests}`), `service_tier`
/// (`standard|priority|batch`), and `cache_creation`
/// (`{ephemeral_1h_input_tokens, ephemeral_5m_input_tokens}`) — are DEFERRED:
/// the runner's source `llm_runtime::ExecutionUsage` has no `web_fetch_requests`,
/// `service_tier`, or 1h/5m cache split to populate them faithfully, so they are
/// intentionally omitted rather than zero-faked. Extending `llm_runtime::ExecutionUsage`
/// is the prerequisite for carrying them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentUsage {
    /// Total token count consumed by the subagent's turn(s) — the legacy footer
    /// field consumers already read. When the runner populates this, it holds
    /// the same value as the result-level total (claude `getTokenCountFromUsage`
    /// of the final turn's usage = input + cache_creation + cache_read + output).
    pub total_tokens: u64,
    /// Billable input tokens (claude `usage.input_tokens`).
    pub input_tokens: u64,
    /// Billable output tokens (claude `usage.output_tokens`).
    pub output_tokens: u64,
    /// Cache-creation (write) input tokens (claude `usage.cache_creation_input_tokens`).
    pub cache_creation_input_tokens: u64,
    /// Cache-read input tokens (claude `usage.cache_read_input_tokens`).
    pub cache_read_input_tokens: u64,
    /// Reasoning/thinking output tokens (`llm_runtime::Usage::reasoning_tokens`) — a bucket some providers (OpenAI, Gemini,
    /// DeepSeek, several OpenRouter routes) bill separately from visible
    /// output tokens. Finding [1]: before this field existed, a subagent's
    /// (including a Fusion panel's) reasoning spend was dropped at this seam
    /// and never reached the caller's usage/pricing at all.
    pub reasoning_output_tokens: u64,
}

impl SubagentUsage {
    /// Whether this rollup contains any provider-reported token bucket.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.total_tokens == 0
            && self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_creation_input_tokens == 0
            && self.cache_read_input_tokens == 0
            && self.reasoning_output_tokens == 0
    }

    /// Return the newly observed portion of a monotonic cumulative rollup.
    #[must_use]
    pub fn saturating_sub(&self, previous: &Self) -> Self {
        Self {
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(previous.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_sub(previous.cache_read_input_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(previous.reasoning_output_tokens),
        }
    }
}

/// Host-owned sink for attributing a subagent's provider usage to its session.
///
/// The agent crate emits provider-neutral usage and never depends on the cost
/// implementation. Hosts that own a cost ledger can install a recorder on the
/// production spawner; test and legacy hosts can leave it unset.
#[async_trait]
pub trait SubagentUsageRecorder: Send + Sync {
    /// Record the supplied usage against the child origin session.
    async fn record_subagent_usage(
        &self,
        session_id: Option<SessionId>,
        model: &str,
        model_profile: Option<&str>,
        usage: SubagentUsage,
        duration: Duration,
        usage_complete: bool,
    );
}

/// Typed lifecycle/event stream for a spawned subagent.
///
/// This is additive over the legacy `spawn_with_progress` string channel: hosts
/// that need live structured updates can observe the real child event stream
/// without scraping transcript files, while existing callers keep their string
/// progress unchanged.
#[derive(Debug, Clone)]
pub enum SubagentObservation {
    /// The child slot has been allocated and the real agent id is now known.
    Allocated {
        /// Stable id of the allocated child.
        agent_id: AgentId,
        /// Resolved child agent type.
        agent_type: String,
        /// Optional caller-facing child name/description.
        name: Option<String>,
        /// Concrete wire model selected for this child after inheritance and
        /// agent-definition resolution.
        model: String,
        /// Provider profile used to route [`Self::model`], when pinned.
        model_profile: Option<String>,
        /// Whether this child parks after each successful turn-set and can be
        /// resumed later. A `Completed` observation for such a child means
        /// "idle", not terminal death.
        persistent: bool,
        /// First absolute client-message index emitted by this runner. Fresh
        /// children start at zero; restored persistent children start after
        /// the visible messages already present in their transcript.
        initial_message_index: u64,
        /// Session pinned by the spawning context, independent of later UI switches.
        origin_session_id: Option<crate::types::SessionId>,
    },
    /// The child emitted a typed progress beacon while still running.
    Progress {
        /// Child emitting the progress update.
        agent_id: AgentId,
        /// Tool calls completed so far.
        tool_use_count: u32,
        /// Cumulative billable tokens reported by completed model round-trips.
        token_count: u64,
    },
    /// A workflow-scoped model query stalled and will be retried.
    Retry {
        /// Child whose workflow query is retrying.
        agent_id: AgentId,
        /// One-based model-attempt number. Initial query is attempt 1, so the
        /// first retry is 2 and the final default retry is attempt 6.
        attempt: u32,
        /// Stable diagnostic explaining which watchdog phase stalled.
        reason: String,
    },
    /// The child emitted a conversation message.
    Message {
        /// Child that produced the message.
        agent_id: AgentId,
        /// Typed conversation message emitted directly by the runner.
        message: ConversationMessage,
    },
    /// The child completed successfully.
    Completed {
        /// Child that completed.
        agent_id: AgentId,
        /// Final child result payload.
        content: Value,
        /// Final-turn usage rollup.
        usage: SubagentUsage,
        /// Tool calls completed across the run.
        total_tool_use_count: u64,
        /// End-to-end child duration.
        total_duration_ms: u64,
        /// Assistant messages emitted across the run.
        assistant_message_count: u64,
        /// Provider request id from the final assistant turn.
        last_request_id: Option<String>,
    },
    /// The child failed terminally.
    Failed {
        /// Child that failed.
        agent_id: AgentId,
        /// Terminal error detail.
        error: String,
    },
    /// The child was cancelled/terminated.
    Killed {
        /// Child that was cancelled.
        agent_id: AgentId,
    },
}

/// Structured observer for a spawned subagent's live lifecycle.
#[async_trait]
pub trait SubagentSpawnObserver: Send + Sync {
    /// Selected model/effort after definition inheritance, before model work.
    async fn on_model_selected(&self, _event: &SubagentObservation, _effort: Option<&str>) {}

    /// Acknowledged startup hook after allocation, before any model/tool work.
    /// Unlike the asynchronous UI observer, foreground task registration must
    /// finish here so a fast child cannot outrun its owner record.
    async fn before_start(&self, _event: &SubagentObservation) -> Result<(), SubagentSpawnError> {
        Ok(())
    }

    /// Synchronous receipt emitted at the allocation boundary, before the
    /// normal asynchronous lifecycle stream.  Hosts use this for facts that
    /// affect accounting (for example, Fusion spawn-quota settlement) and
    /// must not derive them from a potentially delayed UI observer.
    ///
    /// The default is a no-op so existing observers remain source-compatible.
    fn on_allocated(&self, _event: &SubagentObservation) {}

    /// Receive one typed event from the child lifecycle.
    async fn on_event(&self, event: SubagentObservation);
}

/// Terminal result of one [`SubagentSpawner::spawn`] call.
///
/// The child pool [`crate::types::AgentId`] is carried on EVERY variant (matching
/// the source `agent::runner::SubagentEvent`, which already carries `agent_id`
/// on all of Completed/Failed/Killed). This lets the orchestrator key the
/// `SubagentStart`/`SubagentStop` lifecycle on the REAL child id (claude-code
/// `runAgent.ts:347` real `agentId`) instead of minting a fresh one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubagentResult {
    /// The subagent finished normally.
    Completed {
        /// The real child pool agent id (claude `runAgent.ts:347` `agentId`).
        agent_id: crate::types::AgentId,
        /// Free-form JSON payload returned by the subagent.
        content: Value,
        /// Token-usage rollup.
        usage: SubagentUsage,
        /// Number of tool-use blocks the subagent executed across the run
        /// (claude result-level `totalToolUseCount`, `agentToolUtils.ts:233`).
        total_tool_use_count: u64,
        /// Wall-clock duration of the run in milliseconds (claude result-level
        /// `totalDurationMs`, `agentToolUtils.ts:234` / `finalizeAgentTool`).
        total_duration_ms: u64,
        /// Token total = claude `getTokenCountFromUsage` of the FINAL turn's
        /// usage (`input + cache_creation + cache_read + output`; `tokens.ts:46-54`)
        /// — the result-level `totalTokens`, NOT a cross-turn sum. Holds the same
        /// value as [`SubagentUsage::total_tokens`] when the runner populates
        /// both from the final response usage.
        total_tokens: u64,
        /// Number of assistant messages the subagent produced across the run
        /// (claude `agentMessages.length` fed into `tengu_agent_tool_completed`'s
        /// `assistant_message_count`, `agentToolUtils.ts:329`). `0` on the stub
        /// path / when the runner does not surface it.
        #[serde(default)]
        assistant_message_count: u64,
        /// Number of text BLOCKS in the subagent's final response content
        /// (claude `response_char_count: content.length` →
        /// `tengu_agent_tool_completed`, `agentToolUtils.ts:328`). Despite the
        /// field name, claude emits `content.length` — the element count of the
        /// final response's `[{type:'text', text}]` array, NOT a summed char
        /// count. `0` when not surfaced.
        #[serde(default)]
        response_char_count: u64,
        /// The FINAL assistant turn's provider request id (claude
        /// `lastAssistantMessage.requestId`, `agentToolUtils.ts:338`) — used to
        /// emit `tengu_cache_eviction_hint` only when present
        /// (`agentToolUtils.ts:339`). `None` on the stub path / when unavailable
        /// → the cache-eviction hint is skipped, matching claude's truthy guard.
        #[serde(default)]
        last_request_id: Option<String>,
        /// Cross-turn summed usage. Distinct from the final-turn `usage` field
        /// (claude-compatible).
        #[serde(default)]
        cumulative_usage: SubagentUsage,
        /// `false` when `usage`/`cumulative_usage` above are known-STALE —
        /// carried over from a turn that completed successfully before a
        /// later, unrecovered provider error truncated the run (the runner's
        /// `api_error_partial` salvage path). Finding [9]: a caller that
        /// prices spend off this usage should mark the result an estimate
        /// rather than reporting an exact figure when this is `false`.
        /// `true` (the `#[serde(default)]` value) on every clean completion.
        #[serde(default = "usage_complete_default")]
        usage_complete: bool,
    },
    /// The subagent terminated with an error.
    Failed {
        /// The real child pool agent id (claude `runAgent.ts:347` `agentId`).
        agent_id: crate::types::AgentId,
        /// Human-readable reason.
        reason: String,
        /// Cross-turn summed usage from every turn that completed
        /// successfully BEFORE the one that failed (Finding [9]/[11]):
        /// a `Failed` result — provider error, idle-timeout watchdog,
        /// max-turns/structured-output exhaustion — still reflects real,
        /// already-billed provider spend when at least one turn succeeded
        /// first. `SubagentUsage::default()` (all zero) when nothing was
        /// ever billed (a spawn-time failure, before any provider call).
        #[serde(default)]
        usage: SubagentUsage,
    },
    /// The subagent was cancelled by the host.
    Killed {
        /// The real child pool agent id (claude `runAgent.ts:347` `agentId`).
        agent_id: crate::types::AgentId,
    },
}

/// Failure modes for [`SubagentSpawner::spawn`].
#[derive(Debug, Error)]
pub enum SubagentSpawnError {
    /// The agent pool is at capacity.
    #[error("SubagentSpawner: pool full")]
    PoolFull,
    /// The runtime rejected the spawn.
    #[error("SubagentSpawner: runtime error: {0}")]
    Runtime(String),
    /// An `agent.spawn` function hook denied the spawn.
    ///
    /// Distinct from [`Self::Runtime`] so callers can tell "a plugin said no"
    /// from "the runtime broke" — they warrant different messages to the user
    /// and only one of them is a bug.
    #[error("SubagentSpawner: spawn denied by an agent.spawn hook: {0}")]
    DeniedByHook(String),
    /// Any other internal failure.
    #[error("SubagentSpawner: internal error: {0}")]
    Internal(String),
}

/// Pre-spawn selection metadata for the `tengu_agent_tool_selected` event
/// (claude `AgentTool.tsx:419-428`).
///
/// `AgentTool` needs the resolved agent's `source` / `color` / `model` /
/// `is_built_in` BEFORE the spawn (to emit `tengu_agent_tool_selected`) without
/// a second catalog lookup. The production [`SubagentSpawner`] surfaces it via
/// [`SubagentSpawner::resolve_selection`]; the default impl returns a minimal
/// meta (the `agent_type` echoed, everything else empty/false) so existing
/// impls/tests/mocks are unaffected (frozen-crate rule).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedAgentMeta {
    /// Resolved agent type label (claude `selectedAgent.agentType`).
    pub agent_type: String,
    /// Validated observer declaration attached to the selected definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer: Option<ObserverSpec>,
    /// Resolved concrete model id (claude `resolvedAgentModel` =
    /// `getAgentModel(...)`). Empty when no default model is wired.
    pub resolved_model: String,
    /// Origin string mapped to claude's `SettingSource` / `'built-in'` / `'plugin'`
    /// literals (claude `selectedAgent.source`): one of `built-in`, `plugin`,
    /// `userSettings`, `projectSettings`, `localSettings`, `flagSettings`,
    /// `policySettings`. Empty when unresolved.
    pub source: String,
    /// The agent's configured color (claude `selectedAgent.color`), or `None`.
    pub color: Option<String>,
    /// Whether this is a built-in agent (claude `isBuiltInAgent(selectedAgent)`
    /// = `source === 'built-in'`).
    pub is_built_in: bool,
    /// The agent definition's `background` frontmatter flag (claude
    /// `selectedAgent.background`). claude computes
    /// `is_async = run_in_background || selectedAgent.background`
    /// (`AgentTool.tsx:426`), so a `background: true` agent is dispatched async
    /// even when the caller omits `run_in_background`. Defaults to `false`.
    #[serde(default)]
    pub background: bool,
    /// The agent definition's `isolation` frontmatter, surfaced before spawn so
    /// `AgentTool` can realize the same worktree/cwd setup whether isolation was
    /// requested inline (`Agent({... isolation })`) or declared on the resolved
    /// agent definition. `None` means no definition-level isolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
}

/// Result of an ASYNC spawn (claude `AgentTool.tsx:754-764` `async_launched`
/// return).
///
/// `AgentTool`'s async branch returns `{ status:'async_launched', agentId,
/// outputFile, canReadOutputFile, ... }` immediately; the lifecycle runs
/// detached. The production [`SubagentSpawner::spawn_async`] surfaces the real
/// child `agent_id` + the on-disk output path (claude `getTaskOutputPath`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsyncLaunch {
    /// The real child pool agent id (claude `agentBackgroundTask.agentId`).
    pub agent_id: crate::types::AgentId,
    /// Absolute path to the agent's on-disk output file (claude
    /// `getTaskOutputPath(agentId)`, `AgentTool.tsx:761`).
    pub output_file: String,
}

/// Inheritance bundle the parent agent hands to a child spawn.
///
/// The recursion-lock + budget-inheritance invariants are asserted in
/// `lingxi-tools` tests via `Arc::ptr_eq` on these trait-object pointers.
#[derive(Clone)]
pub struct SubagentInheritance {
    /// Parent's tool invoker (`Arc<ToolRegistry>` wrapped). The recursion
    /// lock requires the child to reuse this exact `Arc`, NOT a fresh one.
    pub tool_invoker: Arc<dyn ToolInvoker>,
    /// Parent's budget enforcer. The child inherits this `Arc` so budget
    /// charges aggregate across the whole agent tree.
    pub budget: Arc<dyn BudgetEnforcerHandle>,
}

impl std::fmt::Debug for SubagentInheritance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The trait-object handles intentionally have no Debug contract. Keep
        // task-input diagnostics useful without exposing or fabricating their
        // concrete implementations.
        f.debug_struct("SubagentInheritance")
            .finish_non_exhaustive()
    }
}

/// One resolved subagent type, surfaced to `AgentTool` so it can build the
/// dynamic tool prompt (claude-code's `formatAgentLine`,
/// `AgentTool/prompt.ts:43-46`: `- {agentType}: {whenToUse} (Tools: …)`).
///
/// Lives in `traits` (a leaf crate) so `tool-agent` — which must NOT depend on
/// the `agent` engine crate — can render the catalog without a cyclic dep. The
/// concrete [`SubagentSpawner`] impl in `agent` populates it from the resolved
/// built-in + user/project [`AgentDefinition`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentListingEntry {
    /// Agent type label (TS `agentType`).
    pub agent_type: String,
    /// "When to use" guidance (TS `whenToUse`) — the FULL text, which is what
    /// every non-listing surface reads.
    pub when_to_use: String,
    /// The shorter "when to use" a LEAN session renders instead (TS
    /// `whenToUseLean`). `None` for every definition that does not declare one,
    /// which in 2.1.266 is all of them but the built-in `Explore`.
    #[serde(default)]
    pub when_to_use_lean: Option<String>,
    /// Pre-rendered tools description (TS `getToolsDescription`): `All tools`,
    /// `All tools except X, Y`, an explicit `A, B, C`, or `None`.
    pub tools_description: String,
}

/// Format one agent catalog line, the single source of truth for claude-code's
/// `formatAgentLine` — 2.1.266 `U2n` (src_162329786.js @3554069):
///
/// ```js
/// function U2n(e,n){let r=H4o(e),o=n&&e.whenToUseLean||e.whenToUse;
///                   return `- ${e.agentType}: ${o} (Tools: ${r})`}
/// ```
///
/// `n` is the LEAN-prompt flag, computed by the caller from the model it is
/// rendering for (`VU(YK(mainLoopModel))` in the `agent_listing_delta`
/// producer, the tool's own `PromptOptions.model` in the inline path). A
/// non-lean session renders the FULL `whenToUse` even for a definition that
/// declares a lean variant.
///
/// The `||` is JS truthiness, so an EMPTY `whenToUseLean` falls through to
/// `whenToUse` rather than rendering a blank description; `filter(…is_empty)`
/// keeps that.
///
/// Lives here (a leaf crate) so BOTH the inline tool-prompt path (`tool-agent`)
/// and the `agent_listing_delta` attachment path (`agent` crate → orchestrator)
/// render identical lines without `tool-agent` taking a dep on the heavier
/// `agent` engine crate. The `tools_description` is pre-rendered by the
/// spawner / catalog (TS `getToolsDescription`).
#[must_use]
pub fn format_agent_line(entry: &SubagentListingEntry, lean: bool) -> String {
    let when_to_use = if lean {
        entry
            .when_to_use_lean
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&entry.when_to_use)
    } else {
        &entry.when_to_use
    };
    format!(
        "- {}: {} (Tools: {})",
        entry.agent_type, when_to_use, entry.tools_description
    )
}

/// Whether the Agent catalog should be conveyed as a per-turn
/// `<system-reminder>` attachment (the `agent_listing_delta` path) instead of
/// embedded inline in the `AgentTool` description.
///
/// Port of claude-code `shouldInjectAgentListInMessages`
/// (`AgentTool/prompt.ts`): honor the `LINGXI_AGENT_LIST_IN_MESSAGES`
/// override (`isEnvTruthy` ⇒ true, `isEnvDefinedFalsy` ⇒ false), else the
/// default. **Default is now ON**: in claude-code v2.1.193 the agent catalog is
/// ALWAYS externalized to the per-turn `<system-reminder>` attachment (the
/// `AgentTool` description carries only the static pointer line — there is no
/// inline-catalog variant left). An explicit
/// `LINGXI_AGENT_LIST_IN_MESSAGES=false` opts back into a LEGACY inline
/// catalog (not a 2.1.193 form), retained only as an escape hatch.
#[must_use]
pub fn should_inject_agent_list_in_messages() -> bool {
    let v = std::env::var("LINGXI_AGENT_LIST_IN_MESSAGES").ok();
    if crate::host::env::is_env_truthy(v.as_deref()) {
        return true;
    }
    if crate::host::env::is_env_defined_falsy(v.as_deref()) {
        return false;
    }
    // v2.1.193: catalog is always externalized ⇒ default ON.
    true
}

/// Sentinel key wrapping a forwarded subagent assistant message on the
/// [`SubagentSpawner::spawn_with_progress`] `String` channel
/// (`--forward-subagent-text`, 2.1.212).
///
/// The `spawn_with_progress` progress channel is `String`-typed (a `traits →
/// tool-api` cycle blocks a richer type), so the pool spawner JSON-encodes a
/// forwarded assistant message as `{"<KEY>": <message>}` and the Agent tool
/// decodes it back into a structured `ToolProgress` for the stream-json sink.
/// Plain activity lines (`"Read(foo)"`) never parse as a JSON object carrying
/// this key, so the two payload kinds never collide.
pub const FORWARD_SUBAGENT_MESSAGE_SENTINEL: &str = "__forward_subagent_message__";

/// Stable prefix used when the provider-stream idle watchdog terminates a
/// subagent. Callers use this category without exposing provider error text.
pub const SUBAGENT_QUERY_TIMEOUT_REASON_PREFIX: &str = "subagent workflow query timeout:";

/// Claude Code 2.1.217's default number of concurrently-running subagents.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 20;

/// Claude Code 2.1.219's default subagent nesting depth.
///
/// A top-level caller has depth 0, so 3 permits main → child → grandchild →
/// great-grandchild. This was 1 through 2.1.217 (main could spawn one child,
/// and that child could spawn nothing); 2.1.219 raised it to 3, and
/// `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH=1` is how you get the old behaviour
/// back.
///
/// Leaving it at 1 made the port's depth-2+ stream-json forwarding
/// unreachable in a default configuration — code that existed, was tested, and
/// could never run.
///
/// ORACLE (`bee()`, 2.1.220 @230685726):
/// ```js
/// let e = Z.CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH;
/// if (e !== void 0) return e;                     // env wins outright
/// let r = getFeatureValue(pt_, aHu);              // "tengu_hazel_trellis"
/// Ous = (typeof r === "number" && Number.isInteger(r) && r >= 1) ? r : aHu;
/// var aHu = 3
/// ```
/// The gate value is accepted only when it is an INTEGER >= 1; anything else
/// falls back to 3. This build has no numeric feature-gate client, so it always
/// takes the default — the same result the oracle produces when Statsig is
/// unconfigured.
pub const DEFAULT_MAX_SUBAGENT_SPAWN_DEPTH: u32 = 3;

fn max_concurrent_subagents_from(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONCURRENT_SUBAGENTS)
}

fn max_subagent_spawn_depth_from(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_SUBAGENT_SPAWN_DEPTH)
}

/// Resolve `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`, defaulting to 20.
#[must_use]
pub fn max_concurrent_subagents() -> usize {
    max_concurrent_subagents_from(
        std::env::var("CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS")
            .ok()
            .as_deref(),
    )
}

/// Resolve `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`, defaulting to 1.
#[must_use]
pub fn max_subagent_spawn_depth() -> u32 {
    max_subagent_spawn_depth_from(
        std::env::var("CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH")
            .ok()
            .as_deref(),
    )
}

/// Spawn-a-subagent seam used by `AgentTool`.
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    /// Atomically reserve the entire panel group after activation. A host
    /// without this capability must fail closed, not fall back to racing
    /// ordinary spawns. Queue time counts against the original deadline.
    async fn reserve_fusion_panel_group(
        &self,
        _count: usize,
        _deadline: tokio::time::Instant,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::host::PanelPoolLease, SubagentSpawnError> {
        Err(SubagentSpawnError::Runtime(
            "atomic panel admission is unavailable".into(),
        ))
    }

    /// Consume a previously reserved slot from this same pool. Implementations
    /// must preserve watchdog/observer cleanup and must never reacquire.
    async fn spawn_workflow_with_observer_admitted(
        &self,
        _request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
        _progress: Option<tokio::sync::mpsc::Sender<String>>,
        _observer: Option<Arc<dyn SubagentSpawnObserver>>,
        _watchdog: WorkflowQueryWatchdog,
        _permit: crate::host::PanelPoolPermit,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        Err(SubagentSpawnError::Runtime(
            "admitted panel spawning is unavailable".into(),
        ))
    }

    /// Deliver an intentional user message to a retained foreground runner.
    async fn resume_foreground(
        &self,
        _agent_id: &AgentId,
        _message: String,
    ) -> Result<(), SubagentSpawnError> {
        Err(SubagentSpawnError::Internal(
            "foreground resume is not wired".into(),
        ))
    }
    /// Connect the allocated foreground runner to the host's mailbox router.
    async fn connect_foreground_route(
        &self,
        _agent_id: AgentId,
        _task_id: &str,
        _name: Option<&str>,
    ) -> Result<(), SubagentSpawnError> {
        Ok(())
    }

    /// Trusted transcript path for a child when this spawner persists it.
    fn transcript_path(&self, _agent_id: crate::types::AgentId) -> Option<std::path::PathBuf> {
        None
    }

    /// Normalize a recipient for the Agent schema's reserved-name refinement.
    /// Hosts with a Unicode-aware catalog override this with the same canonical
    /// normalizer used by live name resolution, avoiding a second name policy.
    fn normalize_teammate_recipient(&self, name: &str) -> String {
        crate::host::live_sessions::normalize_name(name)
    }

    /// Whether this session exposes its implicit teammate team.
    fn teammate_enabled(&self) -> bool {
        false
    }

    /// Launch a persistent member of this session's implicit team.
    async fn spawn_teammate(
        &self,
        _request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
    ) -> Result<crate::host::team_spawn::TeammateLaunch, SubagentSpawnError> {
        Err(SubagentSpawnError::Internal(
            "Teammate spawner is not wired".into(),
        ))
    }

    /// Allocate a subagent slot, pump its state machine to completion, and
    /// return the terminal [`SubagentResult`].
    ///
    /// `inherit` carries the parent's tool invoker (registry recursion lock)
    /// and budget enforcer; the implementation MUST hand the same `Arc`s on
    /// to the child without cloning the inner value.
    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError>;

    /// Like [`Self::spawn`], but forwards a one-line summary of each nested
    /// subagent step (its tool calls, as they run) to `progress` so the caller
    /// (`AgentTool`) can surface the subagent's work under its `Task` cell.
    ///
    /// Defaulted to [`Self::spawn`] (dropping `progress`) so existing impls and
    /// tests need no change (frozen-crate rule); the production pool spawner
    /// overrides it to pump non-terminal events through. `progress` is a
    /// plain `String` channel — `traits` cannot depend on `tool-api`'s
    /// `ToolProgress` (cycle), so the AgentTool bridges the string into a
    /// `ToolProgress` on its side.
    async fn spawn_with_progress(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        _progress: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn(request, inherit).await
    }

    /// Like [`Self::spawn_with_progress`], but also exposes a typed live event
    /// stream for the spawned child.
    ///
    /// Defaulted to [`Self::spawn_with_progress`] so existing impls/tests need
    /// no change; the production pool spawner overrides it to forward the
    /// actual child event stream.
    async fn spawn_with_observer(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        _observer: Option<Arc<dyn SubagentSpawnObserver>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_with_progress(request, inherit, progress).await
    }

    /// Spawn path with a bounded idle watchdog around model stream
    /// establishment and each response event. Workflow and Fusion use this
    /// path; the watchdog deliberately excludes tool execution. The default
    /// delegates to [`Self::spawn_with_observer`], preserving compatibility for
    /// mock and non-pool spawners; the production pool implementation applies
    /// `watchdog` to this child only.
    async fn spawn_workflow_with_observer(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn SubagentSpawnObserver>>,
        _watchdog: WorkflowQueryWatchdog,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_with_observer(request, inherit, progress, observer)
            .await
    }

    /// Number of subagents currently occupying this spawner's runtime pool.
    /// The default keeps legacy/mock spawners uncapped; the production pool
    /// overrides it so `AgentTool` can reject at Claude's pre-spawn boundary.
    async fn concurrent_subagent_count(&self) -> usize {
        0
    }

    /// The resolved subagent catalog (built-ins + any wired user/project
    /// agents), used by `AgentTool` to render its dynamic tool prompt.
    ///
    /// Defaulted to empty so existing impls/tests need no change (frozen-crate
    /// rule). The production [`SubagentSpawner`] overrides this to surface the
    /// real catalog with claude-code's later-wins precedence.
    async fn agent_listing(&self) -> Vec<SubagentListingEntry> {
        Vec::new()
    }

    /// The agent types that are UNAVAILABLE because every tool they may use is
    /// denied by the current permission settings — claude 2.1.238 `NJa`
    /// (@290291941), the availability filter behind `vki` / `$Gr` and the
    /// single-agent probe `mdr`:
    ///
    /// ```js
    /// function NJa(e,t){return e.filter((r)=>{
    ///   if(r.source!=="built-in"||!r.tools||r.tools.length===0||att(r.tools)!==null)return!0;
    ///   return r.tools.some((n)=>{ if(n==="*")return!1;
    ///     let o=Lp(n).toolName; return!ak(t,{name:o})&&_Tv(o) })})}
    /// function mdr(e,t){return NJa([e],t).length===0}
    /// ```
    ///
    /// This is the DATA SEAM `AgentTool` lacks on its own: the raw per-agent
    /// `tools` list and the live tool-wide deny rules both live behind the
    /// spawner ([`SubagentListingEntry`] carries only a pre-rendered
    /// `tools_description`), and `tool-agent` must not depend on the `agent`
    /// engine crate. Returning the already-computed TYPE NAMES keeps the seam
    /// additive: `AgentTool` treats the result exactly like the `Agent(<type>)`
    /// deny list — the types are dropped from every `Available agents:` tail and
    /// from the advertised catalog, and requesting one raises claude `hdr`'s
    /// "every tool it may use is denied" error.
    ///
    /// Defaulted to EMPTY so existing impls / mocks / tests need no change
    /// (frozen-crate rule) and an unwired host filters nothing; the production
    /// [`SubagentSpawner`] overrides it.
    async fn tools_denied_agent_types(&self) -> Vec<String> {
        Vec::new()
    }

    /// Resolve the `required_mcp_servers` declared by the agent definition that
    /// `subagent_type` resolves to (claude-code `AgentDefinition.requiredMcpServers`,
    /// `loadAgentsDir.ts:122`). `AgentTool` calls this BEFORE the spawn to run the
    /// pre-spawn MCP-servers gate (`AgentTool.tsx:367-409`): an agent that requires
    /// MCP servers is unavailable until those servers are connected + authenticated
    /// (i.e. expose tools).
    ///
    /// Defaulted to empty so existing impls/tests need no change (frozen-crate
    /// rule); the production [`SubagentSpawner`] overrides this to read the
    /// resolved definition's `required_mcp_servers`. An empty result means "no
    /// requirement" → the gate is skipped.
    async fn resolve_required_mcp_servers(&self, _subagent_type: &str) -> Vec<String> {
        Vec::new()
    }

    /// Resolve the pre-spawn selection metadata for the
    /// `tengu_agent_tool_selected` event (claude `AgentTool.tsx:419-428`): the
    /// resolved agent `source`, `color`, concrete `model`, and `is_built_in`
    /// flag — surfaced WITHOUT a second catalog lookup or a spawn.
    ///
    /// Defaulted to a minimal meta (the `subagent_type` echoed, everything else
    /// empty/false) so existing impls/tests need no change (frozen-crate rule);
    /// the production [`SubagentSpawner`] overrides it. `model` is the caller's
    /// optional model-family override (claude `model` schema field).
    async fn resolve_selection(
        &self,
        subagent_type: &str,
        _model: Option<&str>,
    ) -> SelectedAgentMeta {
        SelectedAgentMeta {
            agent_type: subagent_type.to_string(),
            ..SelectedAgentMeta::default()
        }
    }

    /// Launch the subagent ASYNC (claude `run_in_background` /
    /// `selectedAgent.background`, `AgentTool.tsx:686-764`): allocate the slot,
    /// drive its lifecycle DETACHED, and return immediately with the
    /// `async_launched` info.
    ///
    /// Defaulted to a CLEAR error (`SubagentSpawnError::Internal`) so an unwired
    /// async branch surfaces an explicit message rather than silently falling
    /// back to a sync spawn (the task forbids a silent wrong path). The
    /// production [`SubagentSpawner`] overrides it once the disk-output +
    /// notification lifecycle exists.
    async fn spawn_async(
        &self,
        _request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        Err(SubagentSpawnError::Internal(
            "async subagent spawn (run_in_background) is not wired in this build".to_string(),
        ))
    }

    /// Restore a persisted background agent under its original stable id.
    ///
    /// The default delegates to [`Self::spawn_async`] for hosts that do not
    /// persist background agents. Durable hosts override this so mailbox
    /// routing, transcript paths, and parked rows retain the identity already
    /// exposed to the user before the process restart.
    async fn restore_async(
        &self,
        _agent_id: crate::types::AgentId,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        self.spawn_async(request, inherit).await
    }

    /// Restore the persisted agent identity and its previously exposed task
    /// address. Durable implementations install the task alias before startup.
    async fn restore_async_task(
        &self,
        _task_id: &str,
        agent_id: crate::types::AgentId,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        self.restore_async(agent_id, request, inherit).await
    }

    /// Register `name → agent_id` for `SendMessage` routing of a spawned ASYNC
    /// subagent (claude `AppState.agentNameRegistry.set`, `AgentTool.tsx:704-711`).
    ///
    /// Defaulted to a no-op so existing impls/tests need no change; the
    /// production [`SubagentSpawner`] overrides it to store in its internal
    /// name→id map (or delegate to a wired
    /// [`crate::host::agent_name_registry::AgentNameRegistry`]). Sync agents are NOT
    /// registered (claude `AgentTool.tsx:700-702`).
    async fn register_name(&self, _name: &str, _agent_id: crate::types::AgentId) {}

    /// Resolve a previously-registered async-agent name to its id (the read side
    /// of [`Self::register_name`]). Defaulted to `None`; the production spawner
    /// overrides it so a `SendMessage({ to: name })` resolver can route to a
    /// running async subagent.
    async fn resolve_name(&self, _name: &str) -> Option<crate::types::AgentId> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn workflow_body_recovery_is_host_only_and_defaults_to_disabled() {
        use super::WorkflowQueryWatchdog;

        let policy = WorkflowQueryWatchdog::default();
        assert!(!policy.retry_response_body);
        let value = serde_json::to_value(policy).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "stallTimeoutMs": WorkflowQueryWatchdog::DEFAULT_STALL_TIMEOUT_MS,
                "maxRetries": WorkflowQueryWatchdog::DEFAULT_MAX_RETRIES,
            })
        );
        let restored: WorkflowQueryWatchdog = serde_json::from_value(value).unwrap();
        assert!(!restored.retry_response_body);
        let forged: WorkflowQueryWatchdog = serde_json::from_value(serde_json::json!({
            "stallTimeoutMs": 1000,
            "maxRetries": 2,
            "retryResponseBody": true,
        }))
        .unwrap();
        assert!(
            !forged.retry_response_body,
            "JSON must not grant the Host opt-in"
        );
    }

    #[test]
    fn structured_output_parse_retries_defaults_and_round_trips() {
        let request = super::SubagentSpawnRequest::default();
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("structured_output_parse_retries").is_none());
        let restored: super::SubagentSpawnRequest = serde_json::from_value(value).unwrap();
        assert_eq!(restored.structured_output_parse_retries, 0);
        let enabled = super::SubagentSpawnRequest {
            structured_output_parse_retries: 2,
            ..request
        };
        let restored: super::SubagentSpawnRequest =
            serde_json::from_value(serde_json::to_value(enabled).unwrap()).unwrap();
        assert_eq!(restored.structured_output_parse_retries, 2);
    }

    #[test]
    fn model_attempt_authority_is_not_restored_from_serialized_spawn() {
        // A spawn request round-trips through task replay and the wire, so a
        // persisted or model-authored payload must not be able to mint the
        // host's registered model-call authority.
        let request = super::SubagentSpawnRequest::default();
        let mut value = serde_json::to_value(&request).unwrap();
        assert!(value.get("model_attempt").is_none());
        value["model_attempt"] =
            serde_json::json!({"logical_call_id": 1, "stage": "panel", "panel_slot": 0});
        let restored: super::SubagentSpawnRequest = serde_json::from_value(value).unwrap();
        assert!(restored.model_attempt.is_none());
    }
    use super::*;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn SubagentSpawner>> = None;
    }

    #[test]
    fn format_agent_line_matches_ts_shape() {
        let entry = SubagentListingEntry {
            agent_type: "Explore".into(),
            when_to_use: "Read-only search agent".into(),
            when_to_use_lean: None,
            tools_description: "All tools except Edit, Write".into(),
        };
        // No lean variant declared: both arms render `whenToUse`.
        for lean in [false, true] {
            assert_eq!(
                format_agent_line(&entry, lean),
                "- Explore: Read-only search agent (Tools: All tools except Edit, Write)"
            );
        }
    }

    /// `U2n`'s `o = n && e.whenToUseLean || e.whenToUse`: the lean text is
    /// rendered ONLY on the lean arm, and an empty one falls through to
    /// `whenToUse` the way JS truthiness does.
    #[test]
    fn format_agent_line_renders_the_lean_variant_only_on_the_lean_arm() {
        let entry = SubagentListingEntry {
            agent_type: "Explore".into(),
            when_to_use: "the long one".into(),
            when_to_use_lean: Some("the short one".into()),
            tools_description: "All tools".into(),
        };
        assert_eq!(
            format_agent_line(&entry, false),
            "- Explore: the long one (Tools: All tools)"
        );
        assert_eq!(
            format_agent_line(&entry, true),
            "- Explore: the short one (Tools: All tools)"
        );

        let empty = SubagentListingEntry {
            when_to_use_lean: Some(String::new()),
            ..entry
        };
        assert_eq!(
            format_agent_line(&empty, true),
            "- Explore: the long one (Tools: All tools)",
            "an empty lean variant is falsy in JS and must fall through",
        );
    }

    #[test]
    fn claude_agent_limits_resolve_defaults_and_overrides() {
        assert_eq!(max_concurrent_subagents_from(None), 20);
        assert_eq!(max_concurrent_subagents_from(Some(" 7 ")), 7);
        assert_eq!(max_concurrent_subagents_from(Some("0")), 0);
        assert_eq!(max_concurrent_subagents_from(Some("invalid")), 20);

        // 2.1.219 raised the default nesting depth from 1 to 3.
        assert_eq!(max_subagent_spawn_depth_from(None), 3);
        // The env override wins outright, including the documented way to get
        // the pre-2.1.219 behaviour back.
        assert_eq!(max_subagent_spawn_depth_from(Some("1")), 1);
        assert_eq!(max_subagent_spawn_depth_from(Some(" 5 ")), 5);
        assert_eq!(max_subagent_spawn_depth_from(Some("0")), 0);
        // Unparseable ⇒ the default, not a panic and not 0 (0 would silently
        // disable subagents entirely).
        assert_eq!(max_subagent_spawn_depth_from(Some("invalid")), 3);
    }

    /// The gate defaults ON in v2.1.193 (catalog always externalized). Guarded by
    /// a process-wide lock because it mutates a shared env var.
    #[test]
    fn agent_list_gate_default_on_and_env_override() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        std::env::remove_var("LINGXI_AGENT_LIST_IN_MESSAGES");
        assert!(
            should_inject_agent_list_in_messages(),
            "v2.1.193 default must be ON (catalog externalized)"
        );

        std::env::set_var("LINGXI_AGENT_LIST_IN_MESSAGES", "1");
        assert!(should_inject_agent_list_in_messages(), "truthy ⇒ ON");

        std::env::set_var("LINGXI_AGENT_LIST_IN_MESSAGES", "false");
        assert!(
            !should_inject_agent_list_in_messages(),
            "explicit defined-falsy ⇒ OFF (legacy inline escape hatch)"
        );

        std::env::remove_var("LINGXI_AGENT_LIST_IN_MESSAGES");
    }

    #[test]
    fn subagent_usage_delta_is_saturating_and_zero_is_detected() {
        let previous = SubagentUsage {
            total_tokens: 20,
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
            reasoning_output_tokens: 7,
        };
        let current = SubagentUsage {
            total_tokens: 31,
            input_tokens: 15,
            output_tokens: 8,
            cache_creation_input_tokens: 4,
            cache_read_input_tokens: 4,
            reasoning_output_tokens: 9,
        };
        assert_eq!(
            current.saturating_sub(&previous),
            SubagentUsage {
                total_tokens: 11,
                input_tokens: 5,
                output_tokens: 3,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 2,
                reasoning_output_tokens: 2,
            }
        );
        assert!(SubagentUsage::default().is_zero());
        assert!(!current.is_zero());
        assert!(previous.saturating_sub(&current).is_zero());
    }
}
