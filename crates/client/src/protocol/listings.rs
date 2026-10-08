//! Listing / screen DTOs — the pull/reply payloads for every client screen
//! (sessions, models, MCP, hooks, agents, slash commands, memory, status,
//! settings, auth, doctor, tasks) (plan F1-05).
//!
//! This module defines the SUPPORTING row/payload structs and nested enums.
//! The listing *events* themselves (`SessionList`, `McpServers`, `Agents`, …)
//! are variants of [`crate::protocol::events::ClientEvent`] — the single outbound
//! envelope — and carry these structs.
//!
//! **Name reconciliation** (plan line 149): the design spec §4.1 says
//! `AgentList`, but the WIRE name is `Agents` ([`ClientEvent::Agents`]).
//! Reconciled here while the snapshot is still unfrozen.
//!
//! These DTOs are derived as mechanically as possible from the existing engine
//! info structs so parity is structural (the engine→DTO lowering itself lives
//! in `client::adapter`, F1-11/F1-15, NOT here). The engine source is noted per
//! DTO:
//! - [`SessionRowDto`] ← `session::jsonl::loader::SessionMetadata` (`.path` is
//!   mapped DIRECTLY — it exists at `session/src/jsonl/loader.rs:33`, decision
//!   per plan line 152 — do NOT synthesize).
//! - [`McpServerDto`]/[`McpStatusDto`] ← `lingxi_core::host::orchestrator::{McpServerInfo,
//!   McpStatus}` (`McpStatus::Error(String)` is lowered to the STRUCT variant
//!   [`McpStatusDto::Error`] for `UniFFI` flatness, plan line 154).
//! - [`HookDto`] ← `lingxi_core::host::orchestrator::HookInfo`.
//! - [`AgentDto`] ← `lingxi_core::host::orchestrator::AgentInfo`.
//! - [`SlashCommandDto`] ← `command_api::model::SlashCommand` (display fields).
//! - [`MemoryEntryDto`]/[`MemoryTierDto`] ← `lingxi_core::types::{MemoryEntry,
//!   MemoryEntryTier}`.
//! - [`StatusSnapshotDto`] ← `lingxi_core::host::orchestrator::StatusSnapshot` (traits
//!   shape canonical; status-line fields appended OPTIONAL, plan line 155).
//! - [`AuthStateDto`] ← `Option<lingxi_core::host::auth::LoginInfo>`.
//! - [`DoctorReportDto`]/[`DoctorCheckDto`]/[`CheckStatusDto`]/[`DoctorSummaryDto`]
//!   ← `lingxi_core::host::orchestrator::{DoctorReport, DoctorCheck, CheckStatus,
//!   DoctorSummary}`.
//! - [`TaskRowDto`]/[`TaskStatusDto`] ← `lingxi_core::host::task_registry::TaskRecord` +
//!   `tasks::TaskStatus`. (`TaskOutputChunk` is carried inline by
//!   [`crate::protocol::events::ClientEvent::TaskOutputChunk`], mirroring
//!   `lingxi_core::host::task_registry::TaskOutputChunk`.)
//!
//! Frozen serde conventions (decision §0.1):
//! - internally tagged: `#[serde(tag = "type", rename_all = "snake_case")]`,
//! - reserved-extensible enums are `#[non_exhaustive]`,
//! - every optional field uses
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]`.
//!
//! Settings payloads (`effective_json`/`provenance_json`) are JSON **Strings**
//! on the wire (decision §0.4); `serde_json::Value` never enters this crate.

use serde::{Deserialize, Serialize};

// ── Sessions ─────────────────────────────────────────────────────────────────

/// Which capability profile a mobile session runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum SessionModeDto {
    /// Read-only conversational profile.
    Chat,
    /// Full development profile.
    Code,
}

/// One resumable-session row — the lowered `SessionMetadata`
/// (`session/src/jsonl/loader.rs:23`). Carried by [`crate::protocol::events::ClientEvent::SessionList`].
///
/// `modified` (`SystemTime`) is lowered to an RFC 3339 `String`,
/// `message_count` (`usize`) to a `u32`, and `uuid`/`path` to `String`s. `path`
/// is mapped DIRECTLY (plan line 152 — it is a real field, not synthesized).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SessionRowDto {
    /// The session UUID (parsed from the filename stem), as a stable string.
    pub uuid: String,
    /// The shared catalog title (≤ 200 Unicode characters + ellipsis); clients
    /// may truncate it visually without changing the UUID-backed identity.
    pub title: String,
    /// File mtime as an RFC 3339 timestamp (`SystemTime` lowered, decision §0.4).
    pub modified_rfc3339: String,
    /// Number of user/assistant messages visible in the restored conversation.
    pub message_count: u32,
    /// Capability profile this session persists under.
    pub mode: SessionModeDto,
    /// Absolute path to the `.jsonl` file — mapped directly from
    /// `SessionMetadata.path` so a client can request a re-load.
    pub path: String,
}

// ── Models ───────────────────────────────────────────────────────────────────
//
/// User-visible billing semantics for one provider/model route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ModelBillingModeDto {
    PerToken,
    Subscription,
    Free,
    Unknown,
}

/// One context-threshold price sheet, in USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[allow(missing_docs)]
pub struct ModelPricingTierDto {
    pub context_threshold_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million: Option<f64>,
}

/// Effective display pricing for one provider/model route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[allow(missing_docs)]
pub struct ModelPricingDto {
    pub billing_mode: ModelBillingModeDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million: Option<f64>,
    pub tiers: Vec<ModelPricingTierDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Provider-neutral model capability badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[allow(missing_docs)]
pub struct ModelCapabilitiesDto {
    pub streaming: bool,
    pub tools: bool,
    pub vision: bool,
    pub documents: bool,
    pub reasoning: bool,
    pub structured_output: bool,
}

/// Full details for one provider-qualified model choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[allow(missing_docs)]
pub struct ModelDetailsDto {
    pub reference: String,
    pub provider_id: String,
    pub provider_label: String,
    pub display_name: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<String>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_weights: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_control: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricingDto>,
    pub capabilities: ModelCapabilitiesDto,
    pub reasoning: crate::protocol::controls::ReasoningControlSpecDto,
    /// Whether this model/provider route supports the first-party fast tier.
    #[serde(default)]
    pub supports_fast_mode: bool,
    /// Whether this route could serve as the Fusion analyst: the model claims
    /// structured output AND its profile's wire codec can actually encode a
    /// `response_format`. Strictly stronger than
    /// `capabilities.structured_output`, which is a property of the MODEL only
    /// — a Gemini route claims the capability and then hard-fails at encode
    /// time, after every Fusion panel has already spent. Settings UIs that let
    /// an operator name the analyst must gate on THIS, not on the capability
    /// bit.
    #[serde(default)]
    pub fusion_analyst_capable: bool,
}

/// One provider's settings-visible conversation model catalog.
///
/// `provider_id` is the PROFILE name and stays the identity for credentials,
/// recents and `profile/model` references. When a vendor is reachable several
/// ways — a domestic and an international host, or several API keys — each way
/// is its own entry, and `group` is what says they are one vendor.
///
/// Settings UIs should collapse entries sharing a `group` under one provider
/// heading with a connection row each. Model PICKERS should not: two
/// connections can differ in billing (Zhipu ships the same eight model ids on a
/// subscription endpoint and a metered one), so the choice has to stay visible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[allow(missing_docs)]
pub struct ProviderModelCatalogEntryDto {
    pub provider_id: String,
    pub provider_label: String,
    pub models: Vec<ModelDetailsDto>,
    /// Vendor this entry is one connection of. `None` ⇒ it stands alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// This connection's id within `group` (`"cn"`, `"intl"`, `"coding"`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// One MCP server entry — the lowered `McpServerInfo`
/// (`crates/core/src/host/orchestrator.rs:122`). Carried by
/// [`crate::protocol::events::ClientEvent::McpServers`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct McpServerDto {
    /// Server name as registered in settings.
    pub name: String,
    /// Connection status at snapshot time.
    pub status: McpStatusDto,
    /// Transport kind: `"stdio"`, `"sse"`, or `"http"`.
    pub transport: String,
}

/// Configuration-management domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationDomainDto {
    /// Skills catalog and documents.
    Skill,
    /// MCP configuration and approval state.
    Mcp,
    /// Plugin catalogs, lifecycle, and configuration.
    Plugin,
    /// Hook configuration and registry state.
    Hook,
}

/// One configuration-operation lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationOperationStatusDto {
    /// The operation was accepted and began executing.
    Started,
    /// The operation emitted an intermediate progress update.
    Progress,
    /// Persistence and any requested runtime application succeeded.
    Succeeded,
    /// The operation failed without publishing a partial runtime state.
    Failed,
}

/// Whether a persisted configuration change was applied live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationEffectDto {
    /// The persisted change was also applied to the live runtime.
    Applied,
    /// The change is durable but needs a restart to affect the runtime.
    RestartRequired,
    /// Runtime application does not apply to this operation.
    NotApplicable,
}

/// Connection status for an MCP server — the lowered `McpStatus`
/// (`crates/core/src/host/orchestrator.rs:133`).
///
/// The engine's `McpStatus::Error(String)` is a *tuple* variant; it is lowered
/// here to the STRUCT variant [`McpStatusDto::Error`] for `UniFFI` flatness
/// (decision §0.4 / plan line 154). Internally tagged on `type`, `snake_case`.
/// `#[non_exhaustive]` so a future status is additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpStatusDto {
    /// Connected and healthy.
    Connected,
    /// Disconnected — either never connected or cleanly shut down.
    Disconnected,
    /// Connection failed with the wrapped reason. Lowered from the engine's
    /// tuple `McpStatus::Error(String)` to a STRUCT variant for `UniFFI`.
    Error {
        /// Human-readable failure reason.
        reason: String,
    },
}

// ── Skills ───────────────────────────────────────────────────────────────────

/// One discovered skill — the lowered `SkillInfo`
/// (`crates/core/src/host/orchestrator.rs`). Carried by
/// [`crate::protocol::events::ClientEvent::Skills`].
///
/// Skills are directory-discovered, not configured key-by-key: there is no
/// per-skill enable/disable wire shape, only this listing plus the
/// `reload-skills` slash command to re-scan disk.
///
/// Deliberately carries no `plugin` field: nothing on any live discovery
/// path can populate one today. Add it back additively when a real producer
/// exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SkillDto {
    /// Skill display name (matches its directory name, not frontmatter).
    pub name: String,
    /// The skill's own directory on disk, as a display string.
    pub source_dir: String,
}

// ── Hooks ────────────────────────────────────────────────────────────────────

/// One hook entry — the lowered `HookInfo` (`crates/core/src/host/orchestrator.rs:144`).
/// Carried by [`crate::protocol::events::ClientEvent::Hooks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct HookDto {
    /// Hook identifier.
    pub name: String,
    /// Hook event (`"PreToolUse"`, `"PostToolUse"`, `"Stop"`, `"Notification"`).
    pub event: String,
    /// Optional matcher regex (tool-name pattern). Skipped when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Timeout in milliseconds (default `60_000` if unset on the engine side).
    pub timeout_ms: u64,
    /// Executor kind (`command`, `http`, `agent`, `prompt`, `mcp_tool`, `builtin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_type: Option<String>,
    /// Human-readable origin/source label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Primary executor content (command line, URL, prompt, or handler id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Optional custom runtime status message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// Whether the hook blocks the foreground action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    /// Whether the hook runs in the background without blocking.
    #[serde(default, rename = "async", skip_serializing_if = "Option::is_none")]
    pub is_async: Option<bool>,
    /// Hook priority within the event bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Whether the hook re-wakes the agent loop after async completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_rewake: Option<bool>,
    /// Async timeout in milliseconds, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_timeout_ms: Option<u64>,
    /// Raw `if` condition, if declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
}

// ── Agents ───────────────────────────────────────────────────────────────────

/// One subagent entry — the lowered `AgentInfo`
/// (`crates/core/src/host/orchestrator.rs:157`). Carried by
/// [`crate::protocol::events::ClientEvent::Agents`] (WIRE name; reconciled from spec
/// §4.1 `AgentList`, plan line 149).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AgentDto {
    /// Agent name (matches the markdown filename without extension).
    pub name: String,
    /// Human-readable description (may be truncated by callers).
    pub description: String,
    /// Tool allow-list (empty = all tools).
    pub tools_allowed: Vec<String>,
}

/// A session-scoped agent row. Unlike [`AgentDto`], which describes the
/// static `/agents` catalog, this projection describes one agent instance in
/// the current session and is therefore suitable for the mobile conversation
/// picker. All fields are intentionally strings/primitive values so the shape
/// remains UniFFI-friendly and can be rendered without engine types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SessionAgentSummaryDto {
    /// Stable agent id (`agent:<uuid>`); the main conversation uses `main`.
    pub agent_id: String,
    /// Display name (the spawn name, or a short id fallback).
    pub name: String,
    /// Agent type (for example `general-purpose` or `teammate`).
    pub agent_type: String,
    /// Concrete provider-local model selected for this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider profile paired with [`Self::model`], when pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    /// Lifecycle status — claude-code's task vocabulary: `running`, `pending`,
    /// `completed`, `failed`, `killed`, `cancelled`, or `unknown`.
    ///
    /// A parked persistent agent is `completed`, NOT `idle`: claude-code renders
    /// a finished background agent as `(done)` whatever its resumability, and
    /// reserves `idle` for the footer group those rows collapse into and for a
    /// teammate's own state (which still arrives here through
    /// `coordinator_worker`). Clients render `completed` as `done`.
    pub status: String,
    /// Compact, user-facing description of the most recent activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_activity: Option<String>,
    /// Last transcript update as Unix epoch milliseconds, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
}

/// One child transcript row with a stable UUID and its original stream index.
/// Deleted rows leave index gaps; later messages must not be renumbered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SessionAgentMessageRowDto {
    /// Monotonic child-session message position assigned by the Host.
    pub message_index: u64,
    /// Actual row UUID used by fallback tombstones and snapshot reconciliation.
    pub message_uuid: String,
    /// Client projection of the raw query message.
    pub message: crate::protocol::message::MessageDto,
    /// Complete host-created API-error row, preserving its independent inner
    /// ID, creation time, usage, request ID and refusal metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_json: Option<String>,
}

/// Connection-scoped live workflow/subagent progress row.
///
/// This mirrors Claude Code's `workflow_agent` reducer shape closely enough for
/// clients to key by `(run_id, index)` and update a row in place while keeping
/// every field UniFFI-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct WorkflowProgressDto {
    pub kind: String,
    pub index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
}

// ── Slash commands ───────────────────────────────────────────────────────────

/// One slash-command catalog entry — the display-relevant fields of
/// `command_api::model::SlashCommand` (`command-api/src/model.rs:12`). The rich
/// `SlashCommandKind` dispatch shape stays engine-side; the wire carries the
/// user-facing palette metadata. Carried by
/// [`crate::protocol::events::ClientEvent::SlashCommandCatalog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SlashCommandDto {
    /// Command name (without the leading `/`).
    pub name: String,
    /// Short user-facing description.
    pub description: String,
    /// Origin classification string (e.g. `"builtin"`, `"markdown"`,
    /// `"plugin"`, `"mcp"`) — the lowered `CommandSource`.
    pub source: String,
    /// Alternate names that also resolve to this command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Optional argument hint rendered alongside the command name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Compact menu label; when absent, clients fall back to `description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_description: Option<String>,
    /// Hidden commands stay resolvable by exact input but should not appear in
    /// a bare `/` menu.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

// ── Memory ───────────────────────────────────────────────────────────────────

/// One LINGXI.md memory entry — the lowered `lingxi_core::types::MemoryEntry`
/// (`protocol/src/messages.rs:201`). Carried by
/// [`crate::protocol::events::ClientEvent::MemoryEntries`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MemoryEntryDto {
    /// Absolute path the entry was loaded from.
    pub path: String,
    /// Tier the entry belongs to.
    pub tier: MemoryTierDto,
    /// Raw markdown body (frontmatter stripped, secrets redacted).
    pub body: String,
    /// Age in whole days from the load `now` (`0` for a just-written file).
    pub age_days: u64,
    /// File size in bytes (post-redaction body length).
    pub size_bytes: u64,
}

/// Memory tier — the lowered `lingxi_core::types::MemoryEntryTier`.
/// Internally tagged on `type`, `snake_case`. `#[non_exhaustive]` so a future
/// tier is additive.
///
/// NOT merged into [`crate::protocol::commands::WritableScopeDto`], which absorbed the
/// two DTOs that really were one concept. This one is a different set
/// (`session`/`team` exist here and nowhere else; `managed` does not exist
/// here) AND a different wire shape — those two are bare strings, this is
/// internally tagged. Merging would have widened a write destination to accept
/// `session`, and widened a memory tier to accept `local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryTierDto {
    /// Session-scoped entry (highest tier weight).
    Session,
    /// Project-scoped entry.
    Project,
    /// Team-scoped entry.
    Team,
    /// User-scoped entry (lowest tier).
    User,
}

// ── Status ───────────────────────────────────────────────────────────────────

/// The `/status` panel snapshot — the lowered `StatusSnapshot`
/// (`crates/core/src/host/orchestrator.rs:214`). The traits-shape fields are canonical;
/// status-line fields are appended OPTIONAL (plan line 155). Carried by
/// [`crate::protocol::events::ClientEvent::StatusSnapshot`].
///
/// `Eq` is intentionally NOT derived (matches the engine struct) because
/// `total_cost_usd: f64` does not implement `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct StatusSnapshotDto {
    /// Current session id (as a stable string for display).
    pub session_id: String,
    /// Active model name (e.g. `"claude-opus-4-7"`).
    pub model: String,
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
    /// Session start time, RFC 3339 (UTC) — already a String in the engine.
    pub started_at: String,
    /// Working directory used to launch the session (`PathBuf` lowered).
    pub cwd: String,
    /// Optional pre-rendered status-line string — an APPENDED optional field
    /// (plan line 155). Skipped from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<String>,
    /// Active coordinator-team workers at snapshot time — an APPENDED OPTIONAL
    /// field (T21). `None` (skipped from the wire) for a non-coordinator
    /// session; `Some(n)` echoes the same scalar the PUSH
    /// [`crate::protocol::events::ClientEvent::CoordinatorStatus`] feed carries. Adding an
    /// optional field is a `Compatible` change (no `CLIENT_PROTOCOL_VERSION`
    /// major bump — version-guard `adding_optional_field_is_compatible`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_workers: Option<u32>,
}

// ── Auth ─────────────────────────────────────────────────────────────────────

/// Auth state — the lowered `Option<lingxi_core::host::auth::LoginInfo>`
/// (`crates/core/src/host/auth.rs:13`). Carried by
/// [`crate::protocol::events::ClientEvent::AuthState`]. Internally tagged on `type`,
/// `snake_case`. `#[non_exhaustive]` so a future state (e.g. `LoggingIn`) is
/// additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthStateDto {
    /// No stored credentials (`None` from `current_user`).
    SignedOut,
    /// A signed-in user (`Some(LoginInfo)` from `current_user`).
    SignedIn {
        /// Email address identifying the signed-in user.
        email: String,
        /// Anthropic organization id (`"org_..."` prefix).
        org_id: String,
    },
}

// ── Doctor ───────────────────────────────────────────────────────────────────

/// Aggregate diagnostic report — the lowered `DoctorReport`
/// (`crates/core/src/host/orchestrator.rs:168`). Carried by
/// [`crate::protocol::events::ClientEvent::DoctorReport`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DoctorReportDto {
    /// Individual check results in execution order.
    pub checks: Vec<DoctorCheckDto>,
    /// Summary tallies (pass/warn/fail counts).
    pub summary: DoctorSummaryDto,
}

/// One `/doctor` check result — the lowered `DoctorCheck`
/// (`crates/core/src/host/orchestrator.rs:177`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DoctorCheckDto {
    /// Check identifier (`"config-dir"`, `"api-key"`, etc.).
    pub name: String,
    /// Pass/warn/fail outcome.
    pub status: CheckStatusDto,
    /// Optional detail string. Skipped from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Outcome of a single [`DoctorCheckDto`] — the lowered `CheckStatus`
/// (`crates/core/src/host/orchestrator.rs:188`). Internally tagged on `type`,
/// `snake_case`. `#[non_exhaustive]` so a future outcome is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckStatusDto {
    /// Check succeeded.
    Pass,
    /// Check produced a warning (non-fatal anomaly).
    Warn,
    /// Check failed.
    Fail,
}

/// Pass/warn/fail tallies in a [`DoctorReportDto`] — the lowered
/// `DoctorSummary` (`crates/core/src/host/orchestrator.rs:198`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DoctorSummaryDto {
    /// Number of checks that returned [`CheckStatusDto::Pass`].
    pub passed: u32,
    /// Number of checks that returned [`CheckStatusDto::Warn`].
    pub warnings: u32,
    /// Number of checks that returned [`CheckStatusDto::Fail`].
    pub failed: u32,
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// One task row — the lowered `TaskRecord` (`crates/core/src/host/task_registry.rs:36`).
/// The engine's `status` wire `String` is lowered to a [`TaskStatusDto`] enum.
/// Carried by [`crate::protocol::events::ClientEvent::TaskRow`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskRowDto {
    /// 9-char `[bartwmdksfe][0-9a-z]{8}` task id.
    pub task_id: String,
    /// Task type wire string (one of the 7 byte-locked variants).
    pub task_type: String,
    /// Task status.
    pub status: TaskStatusDto,
    /// Human-readable description.
    pub description: String,
    /// Whether this row represents a paused local workflow that can be
    /// resumed through the explicit `ResumeWorkflow` command.
    #[serde(default, skip_serializing_if = "is_false")]
    pub can_resume: bool,
    /// Wall-clock start time, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Terminal failure reason for a [`TaskStatusDto::Failed`] row, when the
    /// producing handler reported one (`TaskRecord.error`). Clients render it
    /// next to `description` so a failed background task is not a bare id.
    /// APPENDED field, additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `local_fusion` only (F005): the run's current progress-stage label
    /// (e.g. "Running panels 2/3"), lowered from `TaskRecord.stage`. `None`
    /// for every other task type, and before the first progress event lands.
    ///
    /// `uniffi(default = None)` (the `images: Vec<..>` precedent at
    /// `message.rs:27`) keeps this ADDITIVE field additive for the generated
    /// Kotlin/Swift constructors too: without it, every existing hand-written
    /// `TaskRowDto(...)` call site — 6 positional args, one per field before
    /// this one — stops compiling the moment mobile bindings are regenerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub stage: Option<String>,
    /// The teammate is awaiting the leader's plan decision, independent of lifecycle status.
    #[serde(default, skip_serializing_if = "is_false")]
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
    pub awaiting_plan_approval: bool,
    /// Additive shell specialization, e.g. a command event monitor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub kind: Option<String>,
    /// Agent completion has not yet been delivered or consumed.
    #[serde(default, skip_serializing_if = "is_false")]
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
    pub unread: bool,
    /// Concrete model used by an agent task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub model: Option<String>,
    /// String effort label; numeric budgets are omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub effort: Option<String>,
    /// Persistent identity of the agent executing a `local_agent` task.
    /// Never the creator identity of a shell or other background task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub agent_id: Option<String>,
}

/// Task status — the lowered `tasks::TaskStatus` (`tasks/src/state.rs:11`), the
/// Internally tagged on `type`, `snake_case`.
/// `#[non_exhaustive]` so a future status is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskStatusDto {
    /// Created but not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Checkpointed and waiting for an explicit resume.
    Paused,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// Stopped by the user before completion.
    Cancelled,
}

// ── Coordinator (T18 — per-worker roster) ─────────────────────────────────────

/// One coordinator-team worker row — the lowered `lingxi_core::host::team_registry::WorkerInfo`
/// (itself the POD projection of the coordinator's `WorkerAgent`). Field-shaped
/// to lower 1:1 onto the TUI `WorkerRow` (`tui/src/multiagent/state.rs:25`):
/// `agent_id` / `name` / `agent_type` / `status`. Carried by
/// [`crate::protocol::events::ClientEvent::CoordinatorWorker`] (one per worker, the same
/// shape as [`TaskRowDto`] / [`crate::protocol::events::ClientEvent::TaskRow`]).
///
/// This is DISTINCT from `permission::WorkerInfoDto` (`{name, color, team}`,
/// reserved for sub-agent permission requests) — that shape does not match the
/// `WorkerRow` roster row.
///
/// `status` is a simplified label `String` (the same convention as
/// [`TaskRowDto::status`]'s source `TaskRecord.status`) so this crate need not
/// import the concrete `WorkerStatus` enum.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CoordinatorWorkerDto {
    /// Worker agent id, stringified (the `AgentId` display / wire form).
    pub agent_id: String,
    /// Display name (rendered in the roster).
    pub name: String,
    /// Agent-type string (e.g. `"explorer"`, `"writer"`).
    pub agent_type: String,
    /// Simplified status label (e.g. `"idle"`, `"working"`, `"failed"`).
    pub status: String,
}
