//! Permission DTOs — the request/response shape sourced from
//! `platform_api::PermissionGate::check` (plan F1-04).
//!
//! The orchestrator binds `Arc<dyn PermissionGate>` and calls
//! `check(name, &Value) -> PermissionDecision` (governing decision §0.6,
//! verified `tui/src/permission_bridge.rs:64-117`). The `client-adapter`'s
//! `AdapterPermissionGate` (F1-14) translates that call into a
//! [`PermissionRequest`] emitted to the client and parks the turn future on a
//! oneshot until a [`PermissionResolved`] (carrying a [`PermissionResponseDto`])
//! arrives on the inbound command path.
//!
//! Live vs. reserved (decision §0.6):
//! - [`PermissionKindDto::ToolUseConfirm`] is the ONLY kind with a live engine
//!   source — `check()` can source only it.
//! - [`PermissionKindDto::ExitPlanMode`] and
//!   [`PermissionKindDto::BypassPermissionsMode`] are RESERVED / feed-deferred:
//!   defined here so the contract freezes now, but they MUST NOT be wired to a
//!   live source in the foundation. They mirror the engine-side
//!   `platform_api::PermissionRequest` (`prompting_gate.rs:32`) three-variant shape.
//!
//! Frozen serde conventions (decision §0.1):
//! - internally tagged: `#[serde(tag = "type", rename_all = "snake_case")]`,
//! - reserved-extensible enums are `#[non_exhaustive]`,
//! - every optional field uses
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]`.
//!
//! Tool payloads are JSON **Strings** (`tool_input_json`); `serde_json::Value`
//! never enters the contract crate (decision §0.4). The engine-side
//! `PromptDefault` (`platform-api/src/prompting_gate.rs:18`) is collapsed to
//! `default_allow: bool` (`AllowByDefault` ⇒ `true`).

use serde::{Deserialize, Serialize};

/// Outbound permission request — the `PermissionGate::check` call lowered to
/// the wire. Correlated by `request_id` so concurrent worker + main requests
/// multiplex over one connection (the id-keyed gate, F1-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PermissionRequest {
    /// Connection-scoped correlator assigned by the gate (via an `AtomicU64`,
    /// F1-14); echoed back in the matching [`PermissionResolved`].
    pub request_id: u64,
    /// What the user is being asked to approve.
    pub kind: PermissionKindDto,
    /// Worker identity, when the request originates from a sub-agent. RESERVED:
    /// no wire identity exists today (`WorkerPermissionInfo` is TUI-side only,
    /// `tui/src/components/permissions/worker.rs:17`), so this is always `None`
    /// in the foundation. Optional + skipped when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerInfoDto>,
    /// Immutable owner captured when the request is parked. Older senders omit
    /// it; clients must continue to correlate by `request_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<PermissionOwnerDto>,
    /// When true, the client must not offer or persist an `AllowAlways` rule.
    /// Omitted/false preserves ordinary permission-prompt behavior.
    #[serde(default, skip_serializing_if = "is_false")]
    pub suppress_always_allow_rule: bool,
    /// Engine-computed optional Auto action. Clients render this value but do
    /// not derive eligibility from the tool or request payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_mode_prompt: Option<AutoModePromptDto>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Stable wire description of an engine-approved Auto prompt action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AutoModePromptDto {
    /// Workflow-agent Bash: approve this call and switch to Auto mode.
    WorkflowBash,
    /// ExitPlanMode: approve the plan and continue in Auto mode.
    ExitPlanMode,
}

/// Session/turn or worker that owns one parked permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PermissionOwnerDto {
    /// Origin session, when the host has mounted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Client turn correlator for a main-agent request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    /// Worker display name for a child-agent request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_name: Option<String>,
}

/// What the user is being asked to approve. Internally tagged on `type`,
/// `snake_case`. `#[non_exhaustive]` so a future kind is additive (no major
/// bump). Mirrors the engine-side `platform_api::PermissionRequest` three-variant
/// shape (`prompting_gate.rs:32`), but only `ToolUseConfirm` is live-sourced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionKindDto {
    /// Generic per-tool permission confirmation — the ONLY live-sourced kind
    /// (`PermissionGate::check`). `tool_input_json` is the model's tool input
    /// lowered to a JSON **String** (§0.4); `default_allow` is the collapsed
    /// `PromptDefault` (`tool_default(name)`: `AllowByDefault` ⇒ `true`).
    ToolUseConfirm {
        /// Canonical tool name (e.g. `"Bash"`, `"Agent"`).
        tool_name: String,
        /// The model's tool input as a JSON String.
        tool_input_json: String,
        /// The collapsed per-tool default: bare-accept ⇒ Allow (`true`) when
        /// the tool's `PromptDefault` is `AllowByDefault`.
        default_allow: bool,
    },

    /// Approve exiting plan mode. **RESERVED / feed-deferred** (§0.6): present
    /// so the contract freezes now, but `check()` never sources it in the
    /// foundation.
    ExitPlanMode {
        /// The plan text presented for approval.
        plan: String,
    },

    /// Opt in to bypass-permissions (dangerous) mode. **RESERVED /
    /// feed-deferred** (§0.6): present so the contract freezes now, but
    /// `check()` never sources it in the foundation. Unit-style (no payload).
    BypassPermissionsMode,
}

/// Worker identity carried on a [`PermissionRequest`] when it originates from a
/// sub-agent. **RESERVED**: there is no wire worker identity in the foundation
/// (the only `WorkerPermissionInfo`, `tui/src/components/permissions/worker.rs:17`,
/// is TUI-side and never crosses the engine boundary). Defined so the contract
/// freezes now; always `None` on a live `PermissionRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct WorkerInfoDto {
    /// Worker display name (rendered `@name`).
    pub name: String,
    /// Worker color name.
    pub color: String,
    /// Optional team name. Optional + skipped when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
}

/// Inbound resolution of a [`PermissionRequest`], correlated by `request_id`.
/// The transport delivers this (from an `ApprovePermission`/`DenyPermission`
/// command) and the gate sends [`Self::response`] on the parked oneshot
/// (F1-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PermissionResolved {
    /// Correlator with the originating [`PermissionRequest::request_id`].
    pub request_id: u64,
    /// The user's decision.
    pub response: PermissionResponseDto,
}

/// The user's decision for a permission request. Internally tagged on `type`,
/// `snake_case`. `#[non_exhaustive]` so a future response is additive.
///
/// `AllowAlways` additionally appends a session `PermissionRule` (handled in the
/// gate, F1-14); on the wire it is just a tagged unit variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionResponseDto {
    /// Allow this single invocation.
    AllowOnce,
    /// Allow and persist a session rule for matching future invocations.
    AllowAlways,
    /// Allow this invocation and request the engine to atomically switch to
    /// Auto mode. This does not create a persistent permission rule.
    AllowAuto,
    /// Deny this invocation.
    Deny,
}

/// Authoritative engine-side terminal state for a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum PermissionResolutionDto {
    /// The engine accepted the client's approval.
    Approved,
    /// The engine accepted a denial.
    Denied,
    /// The owning turn was cancelled before the user answered.
    Cancelled,
    /// The request exceeded the engine timeout.
    Expired,
}
