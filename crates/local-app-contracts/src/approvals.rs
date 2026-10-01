//! What the service asks of the person using an app, and their answers:
//! capability and UI-automation requests, dependency changes, MCP proposal
//! review and agent profile proposals.
//!
//! The service builds these and a host puts them in front of a person; the
//! decision comes back as an [`AuthorizationDecision`]. They are plain data
//! with the serde attributes of the wire they travel on, so a host that
//! forwards them as JSON and one that maps them onto its own types see the
//! same shapes.
#![allow(missing_docs)] // the variants and fields are self-describing; the types carry the docs

use serde::{Deserialize, Serialize};

/// User decision for data/UI/capability requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationDecision {
    Deny,
    AllowOnce,
    AllowSession,
    AllowAlways,
}

/// Native capability whose first use requires a user decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    DataMutation,
    UiControl,
    NetworkDomain,
    RestoreCheckpoint,
    /// One-shot host approval for dependency add/update operations. This is an
    /// operational prompt, not a manifest-declared app capability.
    DependencyChange,
    Camera,
    PhotoLibrary,
    Microphone,
    Location,
    Notifications,
    /// Legacy combined files prompt. New grants use [`Self::FilesRead`] /
    /// [`Self::FilesWrite`].
    Files,
    Clipboard,
    Share,
    TextToSpeech,
    DeviceStatus,
    Haptics,
    DeepLink,
    Llm,
    AgentNotify,
    BackgroundSchedule,
    Calendar,
    Contacts,
    Media,
    FilesRead,
    FilesWrite,
}

/// A capability approval request surfaced by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub request_id: String,
    pub app_id: String,
    pub capability: CapabilityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub reason: String,
}

/// Allow-list of UI operations; arbitrary JavaScript is intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiActionKind {
    Inspect,
    Click,
    Fill,
    Select,
    Toggle,
    Scroll,
    Navigate,
    Back,
    Reload,
    /// Capture a still image of the app's own `WebView` and return it as a
    /// base64 image. Read-only like [`Self::Inspect`], but strictly more
    /// revealing: `Inspect` redacts `password`/`hidden` input values and a
    /// pixel capture cannot, so the client prompts for it instead of
    /// auto-approving.
    ///
    /// `CaptureView`, deliberately not `Screenshot`. Two reasons, and the
    /// second is the load-bearing one:
    ///
    /// - It keeps one name across all three layers — builtin
    ///   `LocalAppCaptureUi`, provider operation `capture_ui`, wire
    ///   `capture_view` — instead of the wire layer alone jumping vocabulary.
    /// - "Screenshot" is the device-level word (`computer` / `android_use` /
    ///   `ios_use` all spell it `screenshot`). Those tools cannot appear in the
    ///   mobile tool set, so there is no identifier collision — but the label
    ///   derived from this variant is interpolated into the user's
    ///   authorization sheet, and a prompt saying "screen" for one app's view
    ///   next to a device-level prompt saying "screen" for the whole device is
    ///   an authorization the user cannot correctly reason about.
    CaptureView,
    /// Dispatch a pointer event at viewport coordinates.
    ///
    /// Distinct from [`Self::Click`], which resolves an ELEMENT and calls
    /// `.click()` on it — a synthetic `MouseEvent` at `(0, 0)`. A canvas app
    /// has no element to resolve and listens for `pointerdown`/`pointermove`/
    /// `pointerup` with real coordinates, so `Click` can neither address nor
    /// reach it.
    Pointer,
    /// Dispatch a keyboard event.
    ///
    /// `UiRequest::value` carries `"key"` or `"key,phase"`. There is no
    /// existing key action at all, so a keyboard-driven app cannot be driven
    /// even in principle today.
    Key,
}

/// A structured target resolved by the `WebView` host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One permission-gated UI automation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRequest {
    pub request_id: String,
    pub app_id: String,
    pub action: UiActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<UiTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// The kind of one requested dependency change.  Removal is represented on
/// the wire even though the host does not require an approval prompt for a
/// removal-only batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyChangeKind {
    Add,
    Update,
    Remove,
}

/// One dependency operation shown in the native confirmation surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyChangeReview {
    pub kind: DependencyChangeKind,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Host-known cache state before resolution.  This is deliberately a
    /// status string: cache implementation details are not a client contract.
    pub cache_status: String,
    /// Whether this change may require a registry download.  The host must
    /// not inspect or modify the network/cache while building this value.
    pub download_status: String,
}

/// Native one-shot dependency-change confirmation request.
///
/// This is intentionally separate from [`CapabilityRequest`].  A
/// dependency change needs a reviewable per-package diff and supply-chain
/// policy evidence, not a generic allow/deny capability sentence.  The host
/// emits it before any registry access and only issues a dependency receipt
/// after approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyChangeConfirmationRequest {
    pub request_id: String,
    pub app_id: String,
    pub reason: String,
    pub changes: Vec<DependencyChangeReview>,
    pub license_risk: String,
    pub sbom_risk: String,
    pub lifecycle_scripts_blocked: bool,
    pub native_addons_blocked: bool,
    pub rollback_policy: String,
}

/// Verification badge state shown in native Local App surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Pending,
    Passed,
    Failed,
    Unverified,
    Unavailable,
}

/// One verification badge or status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationSummary {
    pub status: VerificationStatus,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// One named gate shown in a native approval or verification surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateStatus {
    pub gate_id: String,
    pub label: String,
    pub status: VerificationStatus,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Coarse kind of one MCP proposal diff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolChangeKind {
    Added,
    Removed,
    Changed,
}

/// One review-surface dimension whose before/after changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolField {
    Name,
    Title,
    Description,
    InputSchema,
    OutputSchema,
    Annotations,
    Execution,
    VisibleMeta,
    SemanticFlow,
    PermissionCeiling,
}

/// One reviewable MCP tool surface carried through Local App native UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolSurface {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_meta_json: Option<String>,
    pub semantic_flow_json: String,
    pub permission_ceiling: String,
}

/// One tool row in the native MCP proposal diff sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDiff {
    pub kind: McpToolChangeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<McpToolSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<McpToolSurface>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_fields: Vec<McpToolField>,
}

/// Native request for one Local App MCP proposal diff/approval sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpProposalApprovalRequest {
    pub request_id: String,
    pub app_id: String,
    pub workflow_run_id: String,
    pub summary: String,
    pub proposal_sha256: String,
    pub approval_contract_sha256: String,
    pub tool_surface_sha256: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_diffs: Vec<McpToolDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_flow_changes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_gates: Vec<GateStatus>,
}

/// A profile proposal is inert until a separate user approval is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfileProposal {
    pub app_id: String,
    pub approval_token: String,
    pub base_revision: u64,
    pub current_revision: u64,
    pub instructions: String,
    pub reason: String,
}
