//! Local-apps DTOs — the wire shapes for the on-device "Apps" capability
//! (local-apps phase 1: agent-designed Next.js mini-apps).
//!
//! Protocol-local mirrors of the `local-apps` core types (`AppRecord`,
//! `AppCheckpoint`, `AppErrorCode`, …) — the core crate never appears here;
//! the engine lowers core ⇄ DTO at the dispatch boundary, exactly like the
//! other listing/row DTOs.
//!
//! Serde conventions (decision §0.1) with one DELIBERATE departure, following
//! the [`crate::protocol::computer_access::AccessTierDto`] precedent of staying
//! byte-identical to the source contract: the fieldless enums
//! ([`AppWorkflowStateDto`], [`AppRuntimeStateDto`], [`AppCreateOriginDto`],
//! [`AppSurfaceDto`], [`AppErrorCodeDto`], [`AppCheckpointKindDto`]) ride as
//! bare wire STRINGS
//! (`"draft"`, `"stopped"`, `"not_found"`, …) — a plain
//! `#[serde(rename_all = "snake_case")]` fieldless enum, byte-identical to
//! the core enums' canonical `as_str()` values.
//!
//! Every optional field uses
//! `#[serde(default, skip_serializing_if = "Option::is_none")]`;
//! `serde_json::Value` never enters this crate (decision §0.4).

// Most members below are wire records whose meaning is defined by their
// enclosing DTO and frozen snapshot. Keeping field comments out of UniFFI
// metadata also avoids its fixed per-item metadata buffer.
#![allow(missing_docs)]

use serde::{Deserialize, Serialize};

use crate::protocol::listings::SessionModeDto;

fn default_git_version_control() -> bool {
    true
}

/// Derived publication state of an app. A bare wire STRING
/// (`"draft"` / `"published_unverified"` / `"published_verified"`).
/// `#[non_exhaustive]` so a future state is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppWorkflowStateDto {
    /// No active build/catalog pair is published.
    Draft,
    /// The app has an active build/catalog pair but no UI verification
    /// evidence.
    PublishedUnverified,
    /// The app has an active build/catalog pair and UI verification evidence.
    PublishedVerified,
}

/// Runtime (dev-server) state of an app — mirrors the core `AppRuntimeState`
/// (spec §C). A bare wire STRING (`"stopped"`, …). `#[non_exhaustive]` so a
/// future state is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppRuntimeStateDto {
    /// No runtime process.
    Stopped,
    /// Runtime is starting up.
    Starting,
    /// Runtime is serving.
    Running,
    /// Runtime is shutting down.
    Stopping,
    /// Runtime failed; `last_error` explains why.
    Failed,
}

/// Where a [`CreateApp`](crate::protocol::commands::ClientCommand::CreateApp) originated.
/// A bare wire STRING (`"chat"` / `"library"`). `#[non_exhaustive]` so a future
/// origin is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppCreateOriginDto {
    /// Created from an agent conversation (carries a `conversation_id`).
    Chat,
    /// Created from the apps library screen.
    Library,
}

/// Which scaffold a [`CreateApp`](crate::protocol::commands::ClientCommand::CreateApp)
/// lays down — mirrors the core `AppSurface`. A bare wire STRING
/// (`"dom"` / `"canvas"`). `#[non_exhaustive]` so a future surface is additive.
///
/// Deliberately NOT called a template: the fixed template catalog was removed
/// from the protocol on purpose and a regression guard keeps those symbols out.
/// This names the SHAPE the app draws, which the create sheet must show and let
/// the user correct — a surface is immutable once scaffolded, so it cannot be
/// left to a value the user never saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppSurfaceDto {
    /// A routed, multi-screen interface built from Ionic components.
    Dom,
    /// A single drawn surface owning its own frame loop, rendering to `<canvas>`.
    Canvas,
}

/// One fixed runtime family from the global local-app catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[non_exhaustive]
pub enum AppRuntimeProfileDto {
    #[serde(rename = "react_dom")]
    ReactDom,
    #[serde(rename = "canvas_2d")]
    Canvas2d,
    #[serde(rename = "three_3d")]
    Three3d,
    #[serde(rename = "phaser_2d")]
    Phaser2d,
    #[serde(rename = "babylon_3d")]
    Babylon3d,
}

/// Stable machine-readable failure code carried on
/// [`AppOperationFailed`](crate::protocol::events::ClientEvent::AppOperationFailed) —
/// mirrors the core `AppErrorCode`. A bare wire STRING (`"not_found"`, …).
/// `#[non_exhaustive]`: the code set is extensible in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppErrorCodeDto {
    /// The addressed app (or sub-resource) does not exist.
    NotFound,
    /// A caller-supplied revision does not match the current draft revision.
    RevisionConflict,
    /// A caller-supplied interaction/suggestion id is not the pending one.
    InteractionInvalid,
    /// The operation is not legal in the app's current workflow state.
    WorkflowStateInvalid,
    /// The app runtime is in a state that blocks the operation.
    RuntimeBusy,
    /// The capability is gated behind a later phase (runtime = phase 4,
    /// git checkpoints = phase 5).
    NotYetAvailable,
    /// Persisted state failed to parse or violates invariants.
    StorageCorrupt,
    /// The request itself is malformed (bad id, empty name, …).
    InvalidRequest,
    /// Fresh LSP diagnostics blocked the requested build or authoring step.
    LspDiagnosticsFailed,
    /// Underlying I/O failure.
    Io,
    /// The model is unreachable: offline, unauthenticated, or timed out.
    LlmUnavailable,
    /// The model's output failed validation (bad shape or over a limit).
    LlmOutputRejected,
}

/// Whether a catalog row is the app's pinned init session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AppSessionKindDto {
    /// The pinned set-up conversation (`AppRecordDto::init_session_id`).
    Init,
    /// Any other conversation in the app's workspace catalog.
    Conversation,
}

/// One row of an app's workspace-scoped session catalog. Field-for-field the
/// shared [`SessionRowDto`](crate::protocol::listings::SessionRowDto) shape plus the
/// init marker — no file paths cross the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "snake_case")]
pub struct AppSessionRowDto {
    /// Bare session uuid (the resume key).
    pub uuid: String,
    /// Session capability mode (Chat vs Code) restored from transcript metadata.
    pub mode: SessionModeDto,
    /// Display title (custom > ai > summary > first-prompt derivation).
    pub title: String,
    /// Last-modified time, RFC 3339 seconds.
    pub modified_rfc3339: String,
    /// Transcript line count (`usize` lowered to `u32`, matching
    /// [`SessionRowDto`](crate::protocol::listings::SessionRowDto)).
    pub message_count: u32,
    /// Init marker for the pinned first row.
    pub kind: AppSessionKindDto,
}

/// Why a checkpoint was recorded — mirrors the core `AppCheckpointKind` (git
/// wiring is phase 5). A bare wire STRING (`"scaffold_created"`, …).
/// `#[non_exhaustive]` so a future kind is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppCheckpointKindDto {
    /// Initial scaffold committed.
    ScaffoldCreated,
    /// Generation output passed validation.
    GenerationValidated,
    /// User approved the preview.
    PreviewApproved,
    /// Explicit user-requested checkpoint.
    UserApproved,
    /// Automatic safety checkpoint taken before a restore.
    PreRestore,
}

/// Supported field types in an app-owned structured data collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppDataFieldTypeDto {
    Text,
    LongText,
    Integer,
    Decimal,
    Boolean,
    DateTime,
    Enum,
    ImageRef,
}

/// One field in a structured app data collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppDataFieldDto {
    pub id: String,
    pub label: String,
    pub field_type: AppDataFieldTypeDto,
    pub required: bool,
    /// Choices for an `enum` field; empty for every other type.
    pub options: Vec<String>,
}

/// A collection declared by the design and exposed through the native data API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppDataCollectionDto {
    pub id: String,
    pub label: String,
    pub fields: Vec<AppDataFieldDto>,
    /// Whether the plan enables this collection by default.
    pub enabled_by_default: bool,
}

/// One app row — the lowered core `AppRecord`. Carried either by the full
/// [`AppsChanged`](crate::protocol::events::ClientEvent::AppsChanged) snapshot or by
/// the incremental `AppRecordChanged` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppRecordDto {
    /// Stable app id matching `^[a-z0-9][a-z0-9-]{0,63}$`.
    pub id: String,
    /// User-facing display name.
    pub name: String,
    /// One-line description the user gave at creation time.
    pub brief: String,
    /// Whether Git controls this app's source checkpoints and restores.
    #[serde(default = "default_git_version_control")]
    pub git_enabled: bool,
    /// Creation time, epoch milliseconds.
    pub created_at_ms: u64,
    /// Last mutation time, epoch milliseconds.
    pub updated_at_ms: u64,
    /// Current derived publication state.
    pub workflow_state: AppWorkflowStateDto,
    /// Conversation the app was created from (`origin: chat`), if any. Skipped
    /// from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// The app's pinned "init" session (bare uuid) — listed first in its
    /// session catalog. Skipped from the wire when `None` (pre-v3 records
    /// before the boot backfill runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_session_id: Option<String>,
    /// Workspace directory relative to the engine data root (always
    /// `apps/<id>/workspace`, forward slashes).
    pub workspace_rel: String,
    /// Whether the app's scaffold has landed. `false` is the empty shell the
    /// "+" button creates before the user confirms a shape; `true` is a fully
    /// formed app. REQUIRED on the wire with NO serde default: a record that
    /// omits it is not a record this contract can interpret, and a default
    /// would silently mint shells as formed apps.
    ///
    /// Appended LAST: UniFFI encodes record fields POSITIONALLY, so a field
    /// inserted above `workspace_rel` would be reinterpreted by a client built
    /// against the previous bindings.
    pub scaffolded: bool,
}

/// One restorable checkpoint of an app workspace — the lowered core
/// `AppCheckpoint` (phase 5 wires git). Carried by
/// [`AppCheckpointCreated`](crate::protocol::events::ClientEvent::AppCheckpointCreated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppCheckpointDto {
    /// Stable checkpoint id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Why the checkpoint was recorded.
    pub kind: AppCheckpointKindDto,
    /// Creation time, epoch milliseconds.
    pub created_at_ms: u64,
}

/// How an approved app build is served on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppRuntimeModeDto {
    StaticExport,
    NextProduction,
}

/// Why a runtime was stopped or suspended outside an explicit user stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppRuntimeSuspensionReasonDto {
    Backgrounded,
    MemoryWarning,
    RuntimeQuota,
    ProcessExited,
}

/// Foreground recovery state for a suspended local app runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppRuntimeRecoveryStateDto {
    NotNeeded,
    Pending,
    Recovering,
    Recovered,
    Failed,
}

/// Generated application manifest consumed by the host and app bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppManifestDto {
    pub schema_version: u32,
    /// Schema v2 manifests carry the runtime API major explicitly. Missing
    /// values are not treated as an implicit legacy compatibility mode.
    pub runtime_api_version: u16,
    pub app_id: String,
    pub name: String,
    pub design_revision: u64,
    pub collections: Vec<AppDataCollectionDto>,
    pub allowed_domains: Vec<String>,
    /// Capabilities the confirmed plan declared. Absent on the wire (and
    /// empty) for pre-capability manifests.
    #[serde(default)]
    pub capabilities: Vec<AppCapabilityKindDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_context: Option<DeviceContextDto>,
    /// Persisted scaffold surface. None only for an unscaffolded shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<AppSurfaceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_profile: Option<AppRuntimeProfileBindingDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_snapshot: Option<AppDependencySnapshotDto>,
}

/// Immutable runtime catalog binding for one scaffolded app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppRuntimeProfileBindingDto {
    pub family: AppRuntimeProfileDto,
    pub revision: u32,
    pub contract_sha256: String,
}

/// Host-derived health of an app's pinned runtime profile and dependency
/// evidence. A bare wire string; the set is deliberately finite so clients
/// can render actionable states without parsing host error prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppRuntimeProfileStatusDto {
    Verified,
    DependenciesDirty,
    CoreDependencyDrift,
    RebuildRequired,
    MigrationAvailable,
    RuntimeBundleMissing,
    RuntimeContractCorrupt,
}

/// Host-verified dependency snapshot for one scaffolded app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppDependencySnapshotDto {
    pub requested_sha256: String,
    pub package_sha256: String,
    pub lockfile_sha256: String,
    pub dependency_tree_sha256: String,
    pub sbom_sha256: String,
    pub toolchain_key: String,
    pub verified_profile_contract_sha256: String,
}

/// Native host target the app was generated for.
///
/// Host-derived, not model-authored, and deliberately just the stable pair:
/// viewport, safe area, color scheme, reduced motion and input mode are live
/// values the page reads from `window.lingxi.v2.deviceContext`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct DeviceContextDto {
    pub os: String,
    pub form_factor: String,
}

/// Runtime snapshot included in an app detail response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppRuntimeDetailsDto {
    pub state: AppRuntimeStateDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AppRuntimeModeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loopback_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspension_reason: Option<AppRuntimeSuspensionReasonDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_state: Option<AppRuntimeRecoveryStateDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Full application detail snapshot requested by the library/detail screens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppDetailsDto {
    pub app: AppRecordDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<AppManifestDto>,
    /// None for an unscaffolded shell; scaffolded apps always receive a
    /// host-derived runtime-profile health classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_profile_status: Option<AppRuntimeProfileStatusDto>,
    pub runtime: AppRuntimeDetailsDto,
    pub checkpoints: Vec<AppCheckpointDto>,
}

/// Operations accepted by the versioned `window.lingxi.v2` bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppBridgeOperationDto {
    QueryData,
    MutateData,
    NetworkRequest,
    RuntimeStatus,
    /// Capture one photo with the device camera (`device.capturePhoto`).
    CapturePhoto,
    /// Pick one image from the photo library (`device.pickImage`).
    PickImage,
    /// Start a microphone recording (`device.recordAudioStart`).
    RecordAudioStart,
    /// Stop the recording and return its bytes (`device.recordAudioStop`).
    RecordAudioStop,
    /// One-shot current location (`device.getLocation`).
    GetLocation,
    /// Listen once and return the transcript (`device.transcribeSpeech`).
    /// This is how speech reaches the model: no provider on this stack
    /// accepts raw audio in a messages call.
    TranscribeSpeech,
    /// Post a local notification (`device.postNotification`).
    PostNotification,
    /// Read plain text from the system clipboard (`clipboard.getText`).
    ClipboardGetText,
    /// Write plain text to the system clipboard (`clipboard.setText`).
    ClipboardSetText,
    /// Open the native share sheet (`device.share`).
    Share,
    /// Synthesize bounded text to audio (`device.synthesizeSpeech`).
    SynthesizeSpeech,
    /// Read an app-private file (`files.read`).
    FileRead,
    /// Write an app-private file (`files.write`).
    FileWrite,
    /// Read non-sensitive device status (`device.status`).
    DeviceStatus,
    /// Trigger one bounded haptic event (`device.haptics`).
    Haptics,
    /// Open one authorized external URL (`device.deepLink`).
    DeepLink,
    /// One side-query chat completion against the user's model (`llm.chat`).
    LlmChat,
    /// Stream one side-query chat completion through `AppBridgeStreamFrameDto`.
    LlmStream,
    /// Post one event into the app's conversation mailbox (`agent.post`).
    AgentPost,
    /// Create a persistent app-owned Agent session.
    AgentSessionCreate,
    /// List persistent app-owned Agent sessions.
    AgentSessionList,
    /// Resume one persistent app-owned Agent session.
    AgentSessionResume,
    /// Close one persistent app-owned Agent session.
    AgentSessionClose,
    /// Run one non-streaming turn in an app-owned Agent session.
    AgentSend,
    /// Run one streaming turn in an app-owned Agent session.
    AgentStream,
    /// Cancel one in-flight app-owned Agent turn.
    AgentCancel,
    /// Propose a user-approved App Agent Profile revision.
    AgentProfileProposeUpdate,
    /// Register a declarative app flow with the host background scheduler.
    BackgroundSchedule,
    /// List app-owned background task lifecycle records.
    BackgroundList,
    /// Read one app-owned background task lifecycle record.
    BackgroundStatus,
    /// Cancel one app-owned background task.
    BackgroundCancel,
    /// Requeue one failed or cancelled app-owned background task.
    BackgroundRetry,
    /// List bounded calendar events (`calendar.listEvents`).
    CalendarListEvents,
    /// Search bounded contact projections (`contacts.search`).
    ContactsSearch,
    /// Read one media handle retained by this app (`media.get`).
    MediaGet,
}

/// One host-bound bridge request. Payloads are data, never executable script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppBridgeRequestDto {
    pub request_id: String,
    pub app_id: String,
    pub operation: AppBridgeOperationDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// Result of a bridge request. JSON remains an opaque data string at `UniFFI`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppBridgeResponseDto {
    pub request_id: String,
    pub app_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable machine-readable failure code (`capability_not_declared`,
    /// `permission_denied`, `audio_session_busy`, `media_too_large`,
    /// `llm_busy`, …) so page code can branch without parsing `error` prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Runtime API version advertised by a generated Local App.  The v2 cutover
/// is explicit: a v1 page is not silently interpreted as a v2 page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppRuntimeApiVersionDto {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

/// v2 invocation origin.  The host derives this from the execution path and
/// does not trust a page-supplied value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppInvocationOriginDto {
    PageForeground,
    ConversationAgent,
    AppRuntimeHeadless,
    SystemScheduler,
}

/// Host-created v2 attribution metadata.  `call_chain` is retained on the
/// wire for audit/debugging but is still validated and rebuilt by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppInvocationContextDto {
    pub app_id: String,
    pub app_instance_id: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub origin: AppInvocationOriginDto,
    pub grant_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_chain: Vec<AppInvocationFrameDto>,
}

/// One nested invocation edge for recursion and audit enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppInvocationFrameDto {
    pub app_id: String,
    pub capability: String,
    pub input_hash: String,
}

/// Generic v2 bridge request.  `operation` is a registry id such as
/// `llm.stream` or `agent.sessions.resume`, not a handler name selected by the
/// app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppBridgeV2RequestDto {
    pub api_version: AppRuntimeApiVersionDto,
    pub context: AppInvocationContextDto,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
    #[serde(default)]
    pub stream: bool,
}

/// Generic v2 bridge response.  Streaming responses are delivered as
/// `AppBridgeStreamFrameDto` events with the same request id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppBridgeV2ResponseDto {
    pub request_id: String,
    pub app_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
}

/// Ordered v2 stream frame used by LLM and Agent session streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum AppBridgeStreamFrameDto {
    Started {
        app_id: String,
        request_id: String,
        stream_id: String,
    },
    Data {
        app_id: String,
        request_id: String,
        stream_id: String,
        seq: u64,
        data_json: String,
    },
    Completed {
        app_id: String,
        request_id: String,
        stream_id: String,
        seq: u64,
    },
    Error {
        app_id: String,
        request_id: String,
        stream_id: String,
        seq: u64,
        code: String,
        message: String,
    },
    Cancelled {
        app_id: String,
        request_id: String,
        stream_id: String,
        seq: u64,
        reason: String,
    },
}

/// Persistent app Agent session status on the v2 wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppAgentSessionStatusDto {
    Active,
    Paused,
    Closed,
}

/// Bounded Agent execution budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppAgentBudgetDto {
    pub max_tokens: u32,
    pub max_wall_ms: u64,
    pub max_turns: u32,
    pub max_bridge_calls: u32,
    pub max_mcp_calls: u32,
    pub max_recursion_depth: u16,
}

/// Host-owned persistent Agent session row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppAgentSessionDto {
    pub session_id: String,
    pub app_id: String,
    pub app_instance_id: String,
    pub status: AppAgentSessionStatusDto,
    pub prompt_profile_revision: u64,
    pub budget: AppAgentBudgetDto,
    pub turn_count: u32,
    #[serde(default)]
    pub output_tokens_used: u64,
    #[serde(default)]
    pub bridge_calls_used: u32,
    #[serde(default)]
    pub mcp_calls_used: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// User-approved prompt profile revision for an app Agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppAgentProfileDto {
    pub app_id: String,
    pub revision: u64,
    pub instructions: String,
    pub updated_at_ms: u64,
}

/// A profile proposal is inert until a separate user approval is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppAgentProfileProposalDto {
    pub app_id: String,
    pub approval_token: String,
    pub base_revision: u64,
    pub current_revision: u64,
    pub instructions: String,
    pub reason: String,
}

/// Allow-list of UI operations; arbitrary JavaScript is intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppUiActionKindDto {
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
    ///
    /// Appended LAST — uniffi assigns FFI discriminants by declaration order,
    /// so inserting mid-enum renumbers every later variant for clients that
    /// were built against the old ordering. Kept FIELDLESS for the same
    /// reason a data-carrying variant is avoided elsewhere: uniffi renders a
    /// mixed enum as a Kotlin `sealed class`, which would rename every
    /// existing constant (`CLICK` → `Click`).
    CaptureView,
    /// Dispatch a pointer event at viewport coordinates.
    ///
    /// Distinct from [`Self::Click`], which resolves an ELEMENT and calls
    /// `.click()` on it — a synthetic `MouseEvent` at `(0, 0)`. A canvas app
    /// has no element to resolve and listens for `pointerdown`/`pointermove`/
    /// `pointerup` with real coordinates, so `Click` can neither address nor
    /// reach it.
    ///
    /// Fieldless: the coordinates ride in `AppUiRequestDto::value` as
    /// `"x,y"` or `"x,y,phase"`, the same comma-packed convention
    /// [`Self::Scroll`] already uses. A data-carrying variant would make uniffi
    /// render this enum as a Kotlin `sealed class` and rename every existing
    /// constant (`CLICK` → `Click`) — an Android-only break.
    Pointer,
    /// Dispatch a keyboard event.
    ///
    /// `AppUiRequestDto::value` carries `"key"` or `"key,phase"`. There is no
    /// existing key action at all, so a keyboard-driven app cannot be driven
    /// even in principle today.
    Key,
}

/// A structured target resolved by the `WebView` host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppUiTargetDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One permission-gated UI automation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppUiRequestDto {
    pub request_id: String,
    pub app_id: String,
    pub action: AppUiActionKindDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AppUiTargetDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Native capability whose first use requires a user decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppCapabilityKindDto {
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AppCapabilityRequestDto {
    pub request_id: String,
    pub app_id: String,
    pub capability: AppCapabilityKindDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub reason: String,
}

/// One core package pinned by a runtime profile contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppRuntimeProfilePackageDto {
    pub name: String,
    pub version: String,
}

/// One runtime profile option surfaced by the native host selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppRuntimeProfileOptionDto {
    pub family: AppRuntimeProfileDto,
    pub revision: u32,
    pub contract_sha256: String,
    pub surface: AppSurfaceDto,
    pub core_packages: Vec<AppRuntimeProfilePackageDto>,
    pub cache_status: String,
    pub download_status: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The kind of one requested dependency change.  Removal is represented on
/// the wire even though the host does not require an approval prompt for a
/// removal-only batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppDependencyChangeKindDto {
    Add,
    Update,
    Remove,
}

/// One dependency operation shown in the native confirmation surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppDependencyChangeDto {
    pub kind: AppDependencyChangeKindDto,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Host-known cache state before resolution.  This is deliberately a
    /// status string: cache implementation details are not a client contract.
    pub cache_status: String,
    /// Whether this change may require a registry download.  The host must
    /// not inspect or modify the network/cache while constructing this DTO.
    pub download_status: String,
}

/// Native one-shot dependency-change confirmation request.
///
/// This is intentionally separate from [`AppCapabilityRequestDto`].  A
/// dependency change needs a reviewable per-package diff and supply-chain
/// policy evidence, not a generic allow/deny capability sentence.  The host
/// emits it before any registry access and only issues a dependency receipt
/// after approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct AppDependencyChangeConfirmationRequestDto {
    pub request_id: String,
    pub app_id: String,
    pub reason: String,
    pub changes: Vec<AppDependencyChangeDto>,
    pub license_risk: String,
    pub sbom_risk: String,
    pub lifecycle_scripts_blocked: bool,
    pub native_addons_blocked: bool,
    pub rollback_policy: String,
}

/// User decision for data/UI/capability requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppAuthorizationDecisionDto {
    Deny,
    AllowOnce,
    AllowSession,
    AllowAlways,
}

// ── Builtin plugin enable/disable/status (§17.1, §19.2) ────────────────────
//
// The bare-key three-way (`enabledPlugins["lingxi-local-app"]` explicit
// `true` / explicit `false` / key absent) is RESOLVED host-side, in the
// `lingxi-code/plugin` crate. That crate is not a `client::protocol`
// dependency (decision: this crate is pure wire DTOs, the engine lowers
// core ⇄ DTO at the dispatch boundary — see the module doc at the top of
// this file), so the resolution ALGORITHM and its persistence across a
// restart are out of scope here. What belongs in this crate is only the WIRE
// SHAPE the resolved outcome travels over, and that shape is designed so it
// CANNOT collapse "explicitly disabled" into "absent": [`PluginStatusDto`]
// is never optional on the wire and its `state` field has exactly two
// values, neither of which can be left unstated.
//
// ⚠️ HOST GAP — verified 2026-08-30, NOT closable from this crate.
// `PluginActivationStateDto::Disabled` currently has no producer for the
// cold-start case, because the host does not yet keep an explicitly-disabled
// plugin in its registry:
//
//   * `plugin::discovery::discover_effective_plugins`
//     (`plugin/src/discovery.rs:481`) resolves the three-way correctly
//     — `enabled.get(&identifier).copied().unwrap_or(manifest.default_enabled)`
//     — but then only PUSHES the entry when `active` is true. An explicitly
//     disabled plugin is dropped from the returned vec entirely.
//   * The boot path (`engine-desktop::discover_plugin_set`,
//     `runtime/src/desktop/mod.rs:4854`) feeds exactly that vec to
//     `PluginManager::enable`, so a plugin resolved to `false` is never
//     inserted into the manager's map at all.
//   * `PluginState::Disabled` is constructed at exactly ONE site,
//     `PluginManager::disable` (`plugin/src/manager.rs:606`), which requires
//     the plugin to be `Loaded` FIRST and otherwise returns
//     `PluginManagerError::NotFound`.
//
// Net effect, which is precisely the confusion §19.2 exists to forbid: after
// a restart with `enabledPlugins["lingxi-local-app"] = false` on disk, the
// plugin is ABSENT from the registry rather than present-and-`Disabled`. A
// later "is it installed?" answers no, and a native toggle back to `true`
// has no registry entry to flip — `disable()`/`enable()` keyed on the
// existing id both fail with `NotFound`. Closing this needs a change in
// `plugin/src/discovery.rs` + the boot path (return the inactive entries and
// register them as `Disabled`), neither of which is a file this task owns.
// The DTOs below are deliberately shaped so that, once the host is fixed, no
// wire change is needed to express the correct answer.

/// Effective activation state of one builtin plugin, after the host has
/// resolved the bare-key three-way (§19.2). Deliberately two variants, not
/// three: an absent `enabledPlugins` key is resolved to one of these before
/// the wire is touched, so a client never reasons about "missing" itself.
/// `#[non_exhaustive]` in case a future lifecycle state becomes client-visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PluginActivationStateDto {
    /// Loaded — components are live in the engine registries.
    Loaded,
    /// Explicitly disabled, or never enabled — PRESENT in the registry, not
    /// dropped. A registry that drops a disabled plugin is indistinguishable
    /// from one that never found it; this variant is why that never happens
    /// on the wire.
    Disabled,
}

/// Resolved status of one builtin plugin. Always present for a known
/// builtin: the engine-compiled-in door (§19.1) means the host always knows
/// `lingxi-local-app`, so there is no "not found" wire state to confuse with
/// [`PluginActivationStateDto::Disabled`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PluginStatusDto {
    /// Bare `enabledPlugins` key (e.g. `"lingxi-local-app"`) — no
    /// `@marketplace` suffix. The same identifier
    /// [`PluginCommandDto::SetEnabled`] writes back: there is only ONE name
    /// for this plugin on the wire, read or write.
    pub plugin_id: String,
    /// The resolved three-way outcome.
    pub state: PluginActivationStateDto,
    /// The manifest's `defaultEnabled` value, surfaced so a client can
    /// distinguish "using the default" from an explicit override without a
    /// second round trip.
    pub manifest_default_enabled: bool,
}

/// Enable/disable/status operations for one builtin plugin. Nested under
/// [`crate::protocol::commands::ClientCommand::PluginCommand`] instead of flat
/// top-level `ClientCommand` variants: `uniffi_macros::create_metadata_items`
/// bills a nested enum's variants to ITS OWN 16 KiB metadata buffer, not
/// `ClientCommand`'s (see `CLIENT_COMMAND_METADATA_BUDGET` in `commands.rs`).
/// Future §17.1 additions (builtin inventory, template catalog, MCP proposal
/// diff, …) extend this enum, not `ClientCommand` again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PluginCommandDto {
    /// Write `enabledPlugins[plugin_id] = enabled`. This is the ONLY way a
    /// client turns a plugin on or off — there is no second "override" or
    /// "use default" flag alongside it. Confirmed by a
    /// `PluginStatusChanged` event carrying the new resolved status.
    SetEnabled {
        /// Bare `enabledPlugins` key.
        plugin_id: String,
        /// The value written back verbatim.
        enabled: bool,
    },
    /// Request the resolved status for one plugin. Replied with
    /// [`AppEventDto::PluginStatusChanged`].
    GetStatus {
        /// Bare `enabledPlugins` key.
        plugin_id: String,
    },
    /// Read the host-verified builtin-plugin inventory row for native settings.
    GetInventory {
        /// Bare `enabledPlugins` key.
        plugin_id: String,
    },
    /// Resolve one unified create-confirmation sheet.
    ResolveCreateConfirmation {
        /// Pending native confirmation correlator.
        request_id: String,
        /// Whether the user approved the exact reviewed surface.
        approved: bool,
    },
    /// Resolve one MCP proposal diff/approval sheet.
    ResolveMcpProposalApproval {
        /// Pending native confirmation correlator.
        request_id: String,
        /// Whether the user approved the exact reviewed surface.
        approved: bool,
    },
    /// Start the host-managed Local App MCP authoring flow for one app.
    StartLocalAppMcpAuthoring {
        /// App whose MCP surface should be authored or revised.
        app_id: String,
        /// User-visible goal that constrains the generated MCP surface.
        user_goal: String,
    },
    /// Enable or disable one Local App's MCP service with CAS protection.
    SetLocalAppMcpEnabled {
        /// App whose MCP exposure should change.
        app_id: String,
        /// Target enabled state.
        enabled: bool,
        /// Caller-observed settings revision used for CAS.
        expected_revision: u64,
    },
    /// Enable or disable one catalog tool with CAS protection.
    SetLocalAppMcpToolEnabled {
        /// Owning app for the MCP tool.
        app_id: String,
        /// Raw MCP tool name from the active catalog.
        tool_name: String,
        /// Target enabled state.
        enabled: bool,
        /// Caller-observed settings revision used for CAS.
        expected_revision: u64,
    },
    /// Pin or unpin one app's MCP surface into a conversation.
    SetLocalAppMcpConversationPinned {
        /// Conversation whose registry exposure should change.
        conversation_id: String,
        /// App whose MCP server should be pinned.
        app_id: String,
        /// Whether the app should stay pinned for this conversation.
        pinned: bool,
    },
    /// Read the managed Local App MCP inventory projection for native UI.
    GetManagedMcpInventory,
}

/// Client-protocol-level Local App failure code surfaced directly to native UI.
///
/// This is intentionally SEPARATE from [`AppErrorCodeDto`]: these failures come
/// from the Local App Plugin/native-control plane rather than the core app
/// runtime operations already covered by `AppOperationFailed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LocalAppPluginErrorCodeDto {
    PluginDisabled,
    BuiltinBundleUnavailable,
    TemplateUnavailable,
    ProposalInvalid,
    CatalogStale,
    ActiveStateCorrupt,
    RevisionConflict,
    InvalidMcpSettings,
    McpAuthoringRequired,
    RepairBudgetExhausted,
    ExposureCapacityReached,
}

/// Verification badge state shown in native Local App surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LocalAppVerificationStatusDto {
    Pending,
    Passed,
    // r3-never-wired-05: `Failed` HAD zero production producers -- that is
    // history now, and this paragraph must not be read as saying the variant
    // is unreachable. It has exactly one producer:
    //
    //   `runtime/src/mobile/local_apps_host.rs:2016-2036` -- the
    //   managed-MCP inventory loop (which also feeds
    //   `VerificationSummaryChanged`) marks ONE app `Failed` with
    //   `code: Some("active_state_corrupt")` when its active catalog's
    //   recorded `appId`/`buildId` does not match the app and build pointing
    //   at it. That condition used to `return Err(...)` and abort the WHOLE
    //   listing, so one corrupt app made every other app vanish from both
    //   clients. Pinned by the regression test
    //   `a_corrupt_active_mcp_catalog_fails_only_that_app_and_still_lists_the_others`
    //   (local_apps_host.rs:13717-13799).
    //
    // No `local_apps_verification_summary_active_state_corrupt` key exists in
    // `clients/translations/` (the only `active_state_corrupt` key there is
    // `local_apps_error_active_state_corrupt`, which belongs to
    // `LocalAppPluginErrorCodeDto`, not to this `code`), so both clients fall
    // back to the engine's English `summary` verbatim via their
    // `default:`/`else ->` arm while the STATUS badge beside it IS localized
    // (`local_apps_verification_status_failed`, live in
    // `LocalAppsViewModel.kt` / `LocalAppsScreen.kt` and on iOS).
    //
    // DO NOT delete the variant: it is declaration index 2 of 5, so removing
    // it shifts `Unverified` and `Unavailable` down one and silently corrupts
    // installed mobile clients (UniFFI encodes enum variants by declaration
    // index). That rule is about the ORDINAL, not about reachability -- it
    // held while there was no producer and it holds now that there is one, so
    // do not delete this paragraph if the producer above ever goes away.
    // Enforced by `tests/local_apps_ordinal_lock_test.rs`.
    Failed,
    Unverified,
    Unavailable,
}

/// One verification badge or status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppVerificationSummaryDto {
    pub status: LocalAppVerificationStatusDto,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Managed Local App MCP lifecycle state rendered in native inventory UIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ManagedLocalAppMcpStatusDto {
    Disabled,
    NeedsSetup,
    Authoring,
    Enabled,
    NeedsRevalidation,
    Error,
}

/// One named gate shown in a native approval or verification surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppGateStatusDto {
    pub gate_id: String,
    pub label: String,
    pub status: LocalAppVerificationStatusDto,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Count summary for the builtin Local App Plugin inventory row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppPluginComponentCountsDto {
    pub skills: u32,
    pub agents: u32,
    pub workflows: u32,
    pub templates: u32,
}

/// Native settings/inventory projection for the builtin Local App Plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppPluginInventoryDto {
    pub plugin_id: String,
    pub display_name: String,
    pub source: String,
    pub version: String,
    pub bundle_sha256: String,
    pub state: PluginActivationStateDto,
    pub manifest_default_enabled: bool,
    pub counts: LocalAppPluginComponentCountsDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_error: Option<String>,
}

/// One rejected selector candidate shown only in trusted native UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppRejectedCandidateDto {
    pub template_id: String,
    pub reason: String,
}

/// Display-only summary of the selected template on the create sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppTemplateSummaryDto {
    pub template_id: String,
    pub surface: AppSurfaceDto,
    pub summary: String,
}

/// One reviewable MCP tool surface carried through Local App native UI.
///
/// Complex JSON-valued MCP fields stay as STRINGS here, matching the broader
/// client::protocol rule that structured payloads crossing the wire remain
/// UniFFI-flat strings instead of `serde_json::Value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppMcpToolSurfaceDto {
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

/// Native request for one unified create confirmation sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppCreateConfirmationRequestDto {
    pub request_id: String,
    pub app_id: String,
    pub name: String,
    pub brief: String,
    pub selected_template: LocalAppTemplateSummaryDto,
    pub runtime_profile: AppRuntimeProfileOptionDto,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<LocalAppRejectedCandidateDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_tools: Vec<LocalAppMcpToolSurfaceDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_gates: Vec<LocalAppGateStatusDto>,
}

/// One review-surface dimension whose before/after changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LocalAppMcpToolFieldDto {
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

/// Coarse kind of one MCP proposal diff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LocalAppMcpToolChangeKindDto {
    Added,
    Removed,
    Changed,
}

/// One tool row in the native MCP proposal diff sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppMcpToolDiffDto {
    pub kind: LocalAppMcpToolChangeKindDto,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<LocalAppMcpToolSurfaceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<LocalAppMcpToolSurfaceDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_fields: Vec<LocalAppMcpToolFieldDto>,
}

/// Native request for one Local App MCP proposal diff/approval sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct LocalAppMcpProposalApprovalRequestDto {
    pub request_id: String,
    pub app_id: String,
    pub workflow_run_id: String,
    pub summary: String,
    pub proposal_sha256: String,
    pub approval_contract_sha256: String,
    pub tool_surface_sha256: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_diffs: Vec<LocalAppMcpToolDiffDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_flow_changes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_gates: Vec<LocalAppGateStatusDto>,
}

/// Host-managed widget resource surfaced alongside one Local App MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct McpAppWidgetDto {
    pub resource_uri: String,
    pub mime_type: String,
    pub resource_sha256: String,
}

/// One managed Local App MCP logical-server row for native inventory UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct ManagedLocalAppMcpServerDto {
    pub server_name: String,
    pub app_id: String,
    pub app_name: String,
    #[serde(default)]
    pub enabled: bool,
    pub status: ManagedLocalAppMcpStatusDto,
    pub settings_revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    /// Whether this app is pinned in the currently active conversation.
    #[serde(default)]
    pub pinned_to_current_conversation: bool,
    pub build_id: String,
    pub catalog_sha256: String,
    pub tool_surface_sha256: String,
    pub tool_count: u32,
    pub authoring_revision: u64,
    pub publication_state: AppWorkflowStateDto,
    pub mcp_verification: LocalAppVerificationSummaryDto,
    pub ui_verification: LocalAppVerificationSummaryDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<McpAppWidgetDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<LocalAppMcpToolSurfaceDto>,
}

/// Extensible local-app event payload carried by the single top-level
/// `ClientEvent::AppEvent` envelope to keep `UniFFI` enum metadata bounded.
// Boxing variants would change the generated mobile binding API; this is
// infrequent control-plane data rather than a hot-path value.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppEventDto {
    AppDetailsChanged {
        details: AppDetailsDto,
    },
    AppBridgeResponse {
        response: AppBridgeResponseDto,
    },
    AppUiRequest {
        request: AppUiRequestDto,
    },
    AppCapabilityRequested {
        request: AppCapabilityRequestDto,
    },
    AppCheckpointsChanged {
        app_id: String,
        checkpoints: Vec<AppCheckpointDto>,
    },
    /// An app-initiated `llm.chat` side query started (`active: true`) or
    /// finished/failed (`active: false`). Drives the client's "app is
    /// calling AI" indicator; always emitted in pairs.
    AppLlmActivityChanged {
        app_id: String,
        active: bool,
    },
    /// An app posted one event into its conversation mailbox via
    /// `agent.post`. Deliberately carries NO body — clients badge on it;
    /// the payload is read only through the MCP `read_app_events` tool.
    AppAgentEventPosted {
        app_id: String,
        seq: u64,
        topic: String,
        created_at_ms: u64,
    },
    /// One ordered v2 bridge stream frame.  This is appended so UniFFI enum
    /// ordinals for existing local-app events remain stable.
    AppBridgeStreamFrame {
        frame: AppBridgeStreamFrameDto,
        /// Pre-serialized camelCase frame for WebView delivery on FFI clients.
        #[serde(rename = "frameJson")]
        frame_json: String,
    },
    /// Incremental app-record update; appended to preserve existing enum ordinals.
    AppRecordChanged {
        record: AppRecordDto,
    },
    /// A trusted client must explicitly approve or reject this proposal.
    AppProfileProposal {
        proposal: AppAgentProfileProposalDto,
    },
    /// Host-owned local-app background task outcome. The result is bounded
    /// and optional; clients can fetch the durable record through the app's
    /// background status/list API when they need the full lifecycle view.
    /// Appended to preserve existing UniFFI enum ordinals.
    AppBackgroundTaskChanged {
        app_id: String,
        task_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_json: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        retryable: bool,
    },
    /// A new app record committed. `AppsChanged` carries the whole list; this
    /// names the one that is new, which a client that started the creation
    /// needs in order to land the user on it.
    ///
    /// Appended at the END, and that is load-bearing rather than tidy: UniFFI
    /// encodes this enum POSITIONALLY, so a variant inserted anywhere above
    /// renumbers every variant after it. A client still running the previous
    /// bindings would then decode one event as another — silently, because
    /// mobile has no protocol handshake to catch it. Verified after
    /// regeneration by reading the ordinals out of the generated Kotlin.
    AppCreated {
        record: AppRecordDto,
        /// Correlation key from the originating `CreateApp`, echoed verbatim.
        /// `None` when the creation had no client request behind it (an
        /// agent-tool create, a backfill). A client that started a creation
        /// matches on this to land the user on the app it just asked for.
        ///
        /// Appended after `record` for the same positional reason as the
        /// variant itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A native one-shot dependency review must resolve with approval or
    /// cancellation before the host may resolve/install any package.
    ///
    /// Appended at the END to preserve UniFFI enum ordinals for older clients.
    AppDependencyChangeConfirmationRequested {
        request: AppDependencyChangeConfirmationRequestDto,
    },
    /// Resolved status for one builtin plugin, in reply to
    /// `PluginCommandDto::GetStatus` and confirming a `SetEnabled` write-back.
    ///
    /// Appended at the END to preserve UniFFI enum ordinals for older clients.
    PluginStatusChanged {
        status: PluginStatusDto,
    },
    /// Host-verified inventory row for the builtin Local App Plugin.
    PluginInventoryChanged {
        inventory: LocalAppPluginInventoryDto,
    },
    /// One trusted native create-confirmation sheet is waiting for a decision.
    CreateConfirmationRequested {
        request: LocalAppCreateConfirmationRequestDto,
    },
    /// One trusted native MCP proposal diff/approval sheet is waiting for a decision.
    McpProposalApprovalRequested {
        request: LocalAppMcpProposalApprovalRequestDto,
    },
    /// Managed Local App MCP inventory changed.
    ManagedMcpInventoryChanged {
        servers: Vec<ManagedLocalAppMcpServerDto>,
    },
    /// Derived publication plus UI/MCP verification summary for one app.
    VerificationSummaryChanged {
        app_id: String,
        publication_state: AppWorkflowStateDto,
        mcp_verification: LocalAppVerificationSummaryDto,
        ui_verification: LocalAppVerificationSummaryDto,
    },
    /// Local App Plugin/native-control-plane failure surfaced directly to clients.
    LocalAppOperationFailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        code: LocalAppPluginErrorCodeDto,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::AppBridgeStreamFrameDto;

    #[test]
    fn v2_stream_frame_fields_use_camel_case_on_json_wire() {
        let value = serde_json::to_value(AppBridgeStreamFrameDto::Data {
            app_id: "abc12345".into(),
            request_id: "request-1".into(),
            stream_id: "stream-1".into(),
            seq: 0,
            data_json: "{}".into(),
        })
        .expect("serialize stream frame");
        assert_eq!(value["appId"], "abc12345");
        assert_eq!(value["requestId"], "request-1");
        assert_eq!(value["streamId"], "stream-1");
        assert_eq!(value["dataJson"], "{}");
        assert!(value.get("app_id").is_none());
    }

    // ── Plugin enable/disable/status wire contract (§17.1, §19.2) ──────────
    //
    // `client::protocol` has no dependency on the `plugin` crate and no
    // persistence layer of its own, so two of P1.9's five required test
    // names are NOT reachable from this file and are deliberately NOT
    // reproduced here under those names:
    //
    // - `missing_key_uses_the_manifest_default` — this is the bare-key
    //   THREE-WAY RESOLUTION ALGORITHM's behavior on an absent
    //   `enabledPlugins` entry. That algorithm is
    //   `plugin::discovery::discover_effective_plugins`
    //   (`enabled.get(id).copied().unwrap_or(manifest.default_enabled)`,
    //   `lingxi-code/plugin/src/discovery.rs`) — a crate this one does not
    //   depend on. `PluginStatusDto` has no "missing" wire state to begin
    //   with (by design: the host resolves before the wire is touched), so
    //   there is nothing about "missing key" for a DTO-only test to assert.
    // - `state_survives_restart` — persistence-across-restart is
    //   `plugin::manager::PluginManager` + `installed_plugins.json`
    //   durability, entirely outside a wire-DTO crate with no disk I/O.
    //
    // Note also that `explicit_false_is_disabled_not_absent`'s BEHAVIOURAL
    // claim is currently FALSE at the host on a cold start — see the
    // "HOST GAP" block above `PluginActivationStateDto`. Naming a passing
    // wire-shape test after it would read as evidence that the behaviour
    // holds, which is exactly backwards.
    //
    // Reproducing either under its exact required name here, with content
    // that could only ever assert something about THIS crate's own trivial
    // plumbing, would be the reviewed-against "weaker thing under the
    // stronger name" — so instead: the WIRE-SHAPE properties that ARE this
    // crate's job are pinned below under their own honest names, and
    // `native_toggle_writes_back_the_same_bare_key` (fully reachable here)
    // keeps its required name.
    use super::{
        AppErrorCodeDto, AppEventDto, AppRuntimeProfileDto, AppRuntimeProfileOptionDto,
        AppRuntimeProfilePackageDto, AppSurfaceDto, AppWorkflowStateDto,
        LocalAppCreateConfirmationRequestDto, LocalAppGateStatusDto,
        LocalAppMcpProposalApprovalRequestDto, LocalAppMcpToolChangeKindDto,
        LocalAppMcpToolDiffDto, LocalAppMcpToolFieldDto, LocalAppMcpToolSurfaceDto,
        LocalAppPluginComponentCountsDto, LocalAppPluginErrorCodeDto, LocalAppPluginInventoryDto,
        LocalAppRejectedCandidateDto, LocalAppTemplateSummaryDto, LocalAppVerificationStatusDto,
        LocalAppVerificationSummaryDto, ManagedLocalAppMcpServerDto, ManagedLocalAppMcpStatusDto,
        McpAppWidgetDto, PluginActivationStateDto, PluginCommandDto, PluginStatusDto,
    };

    fn canonical_profile_option() -> AppRuntimeProfileOptionDto {
        AppRuntimeProfileOptionDto {
            family: AppRuntimeProfileDto::ReactDom,
            revision: 1,
            contract_sha256: "8".repeat(64),
            surface: AppSurfaceDto::Dom,
            core_packages: vec![
                AppRuntimeProfilePackageDto {
                    name: "react".into(),
                    version: "19.0.0".into(),
                },
                AppRuntimeProfilePackageDto {
                    name: "@ionic/react".into(),
                    version: "9.0.0".into(),
                },
            ],
            cache_status: "bundled".into(),
            download_status: "bundled".into(),
            available: true,
            reason: None,
        }
    }

    fn canonical_tool_surface(name: &str) -> LocalAppMcpToolSurfaceDto {
        LocalAppMcpToolSurfaceDto {
            name: name.into(),
            title: Some("Track habits".into()),
            description: Some("Create one completed-habits entry.".into()),
            input_schema_json:
                r#"{"type":"object","properties":{"date":{"type":"string"}},"required":["date"]}"#
                    .into(),
            output_schema_json: Some(
                r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}"#
                    .into(),
            ),
            annotations_json: Some(r#"{"readOnlyHint":false}"#.into()),
            execution_json: Some(r#"{"taskSupport":"optional"}"#.into()),
            visible_meta_json: Some(r#"{"anthropic/requiresUserInteraction":true}"#.into()),
            semantic_flow_json: r#"{"flowId":"local-app-save","source":"active"}"#.into(),
            permission_ceiling: "ask".into(),
        }
    }

    fn canonical_gate() -> LocalAppGateStatusDto {
        LocalAppGateStatusDto {
            gate_id: "ui_runner".into(),
            label: "UI runner available".into(),
            status: LocalAppVerificationStatusDto::Pending,
            available: true,
            detail: Some("Will run after approval.".into()),
        }
    }

    fn canonical_widget() -> McpAppWidgetDto {
        McpAppWidgetDto {
            resource_uri:
                "ui://local-app/habits-1a2b/8888888888888888888888888888888888888888888888888888888888888888/mcp-app.html"
                    .into(),
            mime_type: "text/html;profile=mcp-app".into(),
            resource_sha256: "8".repeat(64),
        }
    }

    #[test]
    fn app_error_code_dto_round_trips_new_lsp_failure_code() {
        let json = serde_json::to_value(AppErrorCodeDto::LspDiagnosticsFailed)
            .expect("serialize app error code");
        assert_eq!(json, "lsp_diagnostics_failed");
        assert_eq!(
            serde_json::from_value::<AppErrorCodeDto>(json).expect("deserialize app error code"),
            AppErrorCodeDto::LspDiagnosticsFailed
        );
    }

    /// The wire-shape half of "explicit `false` is Disabled, not absent"
    /// (§19.2): a Disabled status serializes to a concrete, present JSON
    /// object with its own distinct `state` tag — never `null`, never an
    /// omitted field, never the same value `Loaded` serializes to. Whether
    /// an explicit `false` setting actually RESOLVES to `Disabled` is
    /// `plugin::discovery`'s job, not this crate's.
    #[test]
    fn plugin_status_disabled_state_is_present_and_distinct_from_loaded() {
        let disabled = serde_json::to_value(PluginStatusDto {
            plugin_id: "lingxi-local-app".into(),
            state: PluginActivationStateDto::Disabled,
            manifest_default_enabled: true,
        })
        .expect("serialize disabled status");
        assert_eq!(disabled["state"], "disabled");
        assert_eq!(disabled["plugin_id"], "lingxi-local-app");

        let loaded = serde_json::to_value(PluginStatusDto {
            plugin_id: "lingxi-local-app".into(),
            state: PluginActivationStateDto::Loaded,
            manifest_default_enabled: true,
        })
        .expect("serialize loaded status");
        assert_ne!(
            disabled["state"], loaded["state"],
            "Disabled must not serialize the same as Loaded"
        );

        // The load-bearing half. "Disabled, not absent" is a claim about what
        // the wire CANNOT say, so it is asserted as UN-REPRESENTABILITY: no
        // payload may name a plugin while leaving its activation unstated.
        //
        // An `assert!(json.is_object())` does NOT test this. Every
        // `derive(Serialize)` struct serializes to an object, so that
        // assertion is vacuously true here — and it stays true for a
        // `#[serde(default)] state: Option<_>` field, i.e. for exactly the
        // design this test exists to forbid. It was removed for that reason.
        //
        // POSITIVE CONTROL first, so a rejection below cannot be credited to
        // an unrelated malformed input rather than to the missing `state`.
        let complete = serde_json::json!({
            "plugin_id": "lingxi-local-app",
            "state": "disabled",
            "manifest_default_enabled": true,
        });
        let control: PluginStatusDto = serde_json::from_value(complete.clone())
            .expect("positive control: a complete status payload must parse");
        assert_eq!(
            control.state,
            PluginActivationStateDto::Disabled,
            "positive control must actually reach the Disabled state"
        );

        for (label, tampered) in [
            ("an omitted", {
                let mut v = complete.clone();
                v.as_object_mut().expect("object").remove("state");
                v
            }),
            ("a null", {
                let mut v = complete.clone();
                v["state"] = serde_json::Value::Null;
                v
            }),
        ] {
            let parsed = serde_json::from_value::<PluginStatusDto>(tampered);
            assert!(
                parsed.is_err(),
                "a status with {label} `state` must be REJECTED, got {parsed:?}: \
                 if unstated activation were representable, \"explicitly \
                 disabled\" and \"not found\" would collapse into one payload \
                 again — the exact confusion PluginActivationStateDto exists \
                 to prevent"
            );
        }
    }

    /// The wire-shape half of "explicit `true` overrides a `false` manifest
    /// default": the two fields ride independently, so `Loaded` alongside
    /// `manifest_default_enabled: false` is representable and round-trips
    /// without either field being silently derived from the other. Whether
    /// an explicit `true` actually RESOLVES to `Loaded` when the manifest
    /// defaults to `false` is `plugin::discovery`'s job, not this crate's.
    #[test]
    fn plugin_status_can_represent_loaded_alongside_a_false_manifest_default() {
        let status = PluginStatusDto {
            plugin_id: "lingxi-local-app".into(),
            state: PluginActivationStateDto::Loaded,
            manifest_default_enabled: false,
        };
        let json = serde_json::to_value(&status).expect("serialize");
        assert_eq!(json["state"], "loaded");
        assert_eq!(json["manifest_default_enabled"], false);

        let back: PluginStatusDto = serde_json::from_value(json).expect("deserialize back");
        assert_eq!(back, status, "round trip must preserve both fields exactly");
    }

    /// The `PluginStatusChanged` event round-trips under its own wire tag.
    ///
    /// This variant is otherwise COMPLETELY uncovered:
    /// `snapshot_test::every_variant_has_a_golden` enumerates the tags
    /// declared by `ClientCommand` and `ClientEvent` only — it never looks at
    /// `AppEventDto`, so a new payload variant carries no golden and nothing
    /// in the repo complains. Without this test the only reply channel the
    /// enable/disable protocol has would ship unexercised.
    #[test]
    fn plugin_status_changed_event_round_trips_under_its_wire_tag() {
        let status = PluginStatusDto {
            plugin_id: "lingxi-local-app".into(),
            state: PluginActivationStateDto::Disabled,
            manifest_default_enabled: false,
        };
        let event = AppEventDto::PluginStatusChanged {
            status: status.clone(),
        };
        let json = serde_json::to_value(&event).expect("serialize event");
        assert_eq!(json["type"], "plugin_status_changed");
        assert_eq!(json["status"]["plugin_id"], "lingxi-local-app");
        assert_eq!(json["status"]["state"], "disabled");
        assert_eq!(json["status"]["manifest_default_enabled"], false);

        let back: AppEventDto = serde_json::from_value(json).expect("deserialize event");
        assert_eq!(
            back, event,
            "the reply envelope must carry the resolved status through unchanged"
        );
    }

    /// `PluginActivationStateDto`'s two wire tags, pinned so a future rename
    /// (or a third variant reusing one of these strings) is a visible diff.
    #[test]
    fn plugin_activation_state_wire_values_are_stable() {
        assert_eq!(
            serde_json::to_value(PluginActivationStateDto::Loaded).unwrap(),
            "loaded"
        );
        assert_eq!(
            serde_json::to_value(PluginActivationStateDto::Disabled).unwrap(),
            "disabled"
        );
    }

    /// A native toggle writes back the SAME bare key it reads — there is no
    /// second enable flag (§19.2). `SetEnabled`'s JSON carries exactly
    /// `{type, plugin_id, enabled}`: nothing else could silently diverge
    /// from `enabled`, and `plugin_id` is spelled identically to
    /// `GetStatus`/`PluginStatusDto` — this is the regression guard against
    /// a future accidental rename on just one side of the read/write pair.
    #[test]
    fn native_toggle_writes_back_the_same_bare_key() {
        let set = serde_json::to_value(PluginCommandDto::SetEnabled {
            plugin_id: "lingxi-local-app".into(),
            enabled: false,
        })
        .expect("serialize SetEnabled");
        let mut keys: Vec<&str> = set
            .as_object()
            .expect("SetEnabled must serialize to a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["enabled", "plugin_id", "type"],
            "SetEnabled must carry exactly one enable-controlling field \
             alongside its tag and the bare key — no second override flag"
        );
        assert_eq!(set["type"], "set_enabled");
        assert_eq!(set["plugin_id"], "lingxi-local-app");
        assert_eq!(set["enabled"], false);

        let status = serde_json::to_value(PluginCommandDto::GetStatus {
            plugin_id: "lingxi-local-app".into(),
        })
        .expect("serialize GetStatus");
        assert_eq!(
            status["plugin_id"], set["plugin_id"],
            "GetStatus and SetEnabled must name the plugin with the SAME bare key"
        );
    }

    #[test]
    fn plugin_inventory_and_new_plugin_commands_round_trip_under_exact_keys() {
        let inventory = LocalAppPluginInventoryDto {
            plugin_id: "lingxi-local-app".into(),
            display_name: "Local App Plugin".into(),
            source: "builtin".into(),
            version: "2.0.0-dev".into(),
            bundle_sha256: "a".repeat(64),
            state: PluginActivationStateDto::Loaded,
            manifest_default_enabled: true,
            counts: LocalAppPluginComponentCountsDto {
                skills: 27,
                agents: 1,
                workflows: 6,
                templates: 4,
            },
            validation_error: Some("missing verified root".into()),
        };
        let event = AppEventDto::PluginInventoryChanged {
            inventory: inventory.clone(),
        };
        let json = serde_json::to_value(&event).expect("serialize plugin inventory event");
        assert_eq!(json["type"], "plugin_inventory_changed");
        assert_eq!(json["inventory"]["pluginId"], "lingxi-local-app");
        assert_eq!(json["inventory"]["counts"]["templates"], 4_u64);
        assert_eq!(
            serde_json::from_value::<AppEventDto>(json)
                .expect("deserialize plugin inventory event"),
            event
        );

        for command in [
            PluginCommandDto::GetInventory {
                plugin_id: "lingxi-local-app".into(),
            },
            PluginCommandDto::ResolveCreateConfirmation {
                request_id: "create-0001".into(),
                approved: true,
            },
            PluginCommandDto::ResolveMcpProposalApproval {
                request_id: "proposal-0001".into(),
                approved: false,
            },
            PluginCommandDto::StartLocalAppMcpAuthoring {
                app_id: "habits-1a2b".into(),
                user_goal: "Let the model save and summarize my habit data.".into(),
            },
            PluginCommandDto::SetLocalAppMcpEnabled {
                app_id: "habits-1a2b".into(),
                enabled: true,
                expected_revision: 4,
            },
            PluginCommandDto::SetLocalAppMcpToolEnabled {
                app_id: "habits-1a2b".into(),
                tool_name: "save_habit".into(),
                enabled: false,
                expected_revision: 5,
            },
            PluginCommandDto::SetLocalAppMcpConversationPinned {
                conversation_id: "conv-0001".into(),
                app_id: "habits-1a2b".into(),
                pinned: true,
            },
            PluginCommandDto::GetManagedMcpInventory,
        ] {
            let json = serde_json::to_value(&command).expect("serialize plugin command");
            assert_eq!(
                serde_json::from_value::<PluginCommandDto>(json.clone())
                    .expect("deserialize plugin command"),
                command
            );
            match command {
                PluginCommandDto::GetInventory { .. } => assert_eq!(json["type"], "get_inventory"),
                PluginCommandDto::ResolveCreateConfirmation { .. } => {
                    assert_eq!(json["type"], "resolve_create_confirmation")
                }
                PluginCommandDto::ResolveMcpProposalApproval { .. } => {
                    assert_eq!(json["type"], "resolve_mcp_proposal_approval")
                }
                PluginCommandDto::StartLocalAppMcpAuthoring { .. } => {
                    assert_eq!(json["type"], "start_local_app_mcp_authoring")
                }
                PluginCommandDto::SetLocalAppMcpEnabled { .. } => {
                    assert_eq!(json["type"], "set_local_app_mcp_enabled")
                }
                PluginCommandDto::SetLocalAppMcpToolEnabled { .. } => {
                    assert_eq!(json["type"], "set_local_app_mcp_tool_enabled")
                }
                PluginCommandDto::SetLocalAppMcpConversationPinned { .. } => {
                    assert_eq!(json["type"], "set_local_app_mcp_conversation_pinned")
                }
                PluginCommandDto::GetManagedMcpInventory => {
                    assert_eq!(json["type"], "get_managed_mcp_inventory")
                }
                _ => unreachable!("covered above"),
            }
        }
    }

    #[test]
    fn create_confirmation_preserves_exact_wire_keys() {
        let request = LocalAppCreateConfirmationRequestDto {
            request_id: "create-0001".into(),
            app_id: "habits-1a2b".into(),
            name: "Habits".into(),
            brief: "Track streaks and notes".into(),
            selected_template: LocalAppTemplateSummaryDto {
                template_id: "react-dom-r1".into(),
                surface: AppSurfaceDto::Dom,
                summary: "Best for forms and lists".into(),
            },
            runtime_profile: canonical_profile_option(),
            reason: "The user asked for a compact list app.".into(),
            rejected: vec![LocalAppRejectedCandidateDto {
                template_id: "three-3d-r1".into(),
                reason: "3D would add unnecessary runtime weight.".into(),
            }],
            initial_tools: vec![canonical_tool_surface("save_habit")],
            required_gates: vec![canonical_gate()],
        };
        let event = AppEventDto::CreateConfirmationRequested {
            request: request.clone(),
        };
        let json = serde_json::to_value(&event).expect("serialize create confirmation");
        assert_eq!(json["type"], "create_confirmation_requested");
        assert_eq!(
            json["request"]["selectedTemplate"]["templateId"],
            "react-dom-r1"
        );
        assert_eq!(
            json["request"]["runtimeProfile"]["contractSha256"],
            "8".repeat(64)
        );
        assert!(
            json["request"].get("receipt").is_none(),
            "r1-backlog-native-confirmation-13 removed `receipt`; the wire must not \
             carry it again without a coordinated client change: {json}"
        );
        assert_eq!(
            serde_json::from_value::<AppEventDto>(json.clone())
                .expect("deserialize create confirmation"),
            event
        );
    }

    #[test]
    fn mcp_proposal_diff_and_managed_inventory_round_trip() {
        let request = LocalAppMcpProposalApprovalRequestDto {
            request_id: "proposal-0001".into(),
            app_id: "habits-1a2b".into(),
            workflow_run_id: "wf-0002".into(),
            summary: "Expose one habit-save tool and retire the summary tool.".into(),
            proposal_sha256: "3".repeat(64),
            approval_contract_sha256: "4".repeat(64),
            tool_surface_sha256: "5".repeat(64),
            tool_diffs: vec![
                LocalAppMcpToolDiffDto {
                    kind: LocalAppMcpToolChangeKindDto::Removed,
                    name: "summarize_habits".into(),
                    before: Some(canonical_tool_surface("summarize_habits")),
                    after: None,
                    changed_fields: Vec::new(),
                },
                LocalAppMcpToolDiffDto {
                    kind: LocalAppMcpToolChangeKindDto::Changed,
                    name: "save_habit".into(),
                    before: Some(canonical_tool_surface("save_habit")),
                    after: Some(LocalAppMcpToolSurfaceDto {
                        description: Some("Create or update one completed-habits entry.".into()),
                        ..canonical_tool_surface("save_habit")
                    }),
                    changed_fields: vec![
                        LocalAppMcpToolFieldDto::Description,
                        LocalAppMcpToolFieldDto::InputSchema,
                        LocalAppMcpToolFieldDto::PermissionCeiling,
                    ],
                },
            ],
            required_flow_changes: vec!["Add a save step for notes.".into()],
            excluded_capabilities: vec!["calendar".into()],
            pending_gates: vec![canonical_gate()],
        };
        let event = AppEventDto::McpProposalApprovalRequested {
            request: request.clone(),
        };
        let json = serde_json::to_value(&event).expect("serialize proposal approval");
        assert_eq!(json["type"], "mcp_proposal_approval_requested");
        assert_eq!(json["request"]["toolDiffs"][0]["kind"], "removed");
        assert_eq!(json["request"]["toolDiffs"][0]["name"], "summarize_habits");
        assert_eq!(
            serde_json::from_value::<AppEventDto>(json).expect("deserialize proposal approval"),
            event
        );

        let inventory_event = AppEventDto::ManagedMcpInventoryChanged {
            servers: vec![ManagedLocalAppMcpServerDto {
                server_name: "local_app_habits-1a2b".into(),
                app_id: "habits-1a2b".into(),
                app_name: "Habits".into(),
                enabled: true,
                status: ManagedLocalAppMcpStatusDto::Enabled,
                settings_revision: 6,
                enabled_tools: vec!["save_habit".into()],
                pinned_to_current_conversation: true,
                build_id: "build-0001".into(),
                catalog_sha256: "6".repeat(64),
                tool_surface_sha256: "7".repeat(64),
                tool_count: 2,
                authoring_revision: 3,
                publication_state: AppWorkflowStateDto::PublishedUnverified,
                mcp_verification: LocalAppVerificationSummaryDto {
                    status: LocalAppVerificationStatusDto::Passed,
                    summary: "MCP schema, binding and isolation checks passed.".into(),
                    code: None,
                },
                ui_verification: LocalAppVerificationSummaryDto {
                    status: LocalAppVerificationStatusDto::Unavailable,
                    summary: "No UI runner is available on this device.".into(),
                    code: Some("verification_unavailable".into()),
                },
                widget: Some(canonical_widget()),
                tools: vec![canonical_tool_surface("save_habit")],
            }],
        };
        let inventory_json =
            serde_json::to_value(&inventory_event).expect("serialize managed inventory");
        assert_eq!(inventory_json["type"], "managed_mcp_inventory_changed");
        assert_eq!(inventory_json["servers"][0]["enabled"], true);
        assert_eq!(inventory_json["servers"][0]["status"], "enabled");
        assert_eq!(inventory_json["servers"][0]["settingsRevision"], 6_u64);
        assert_eq!(
            inventory_json["servers"][0]["pinnedToCurrentConversation"],
            true
        );
        assert_eq!(
            inventory_json["servers"][0]["enabledTools"][0],
            "save_habit"
        );
        assert_eq!(inventory_json["servers"][0]["toolCount"], 2_u64);
        assert_eq!(
            inventory_json["servers"][0]["uiVerification"]["status"],
            "unavailable"
        );
        assert_eq!(
            inventory_json["servers"][0]["widget"]["resourceUri"],
            "ui://local-app/habits-1a2b/8888888888888888888888888888888888888888888888888888888888888888/mcp-app.html"
        );
        assert_eq!(
            serde_json::from_value::<AppEventDto>(inventory_json)
                .expect("deserialize managed inventory"),
            inventory_event
        );
    }

    #[test]
    fn verification_and_error_events_keep_status_and_error_layers_separate() {
        let summary = AppEventDto::VerificationSummaryChanged {
            app_id: "habits-1a2b".into(),
            publication_state: AppWorkflowStateDto::PublishedVerified,
            mcp_verification: LocalAppVerificationSummaryDto {
                status: LocalAppVerificationStatusDto::Passed,
                summary: "Catalog and MCP verification are current.".into(),
                code: None,
            },
            ui_verification: LocalAppVerificationSummaryDto {
                status: LocalAppVerificationStatusDto::Unverified,
                summary: "UI verification has not run on this build.".into(),
                code: None,
            },
        };
        let json = serde_json::to_value(&summary).expect("serialize summary");
        assert_eq!(json["type"], "verification_summary_changed");
        assert_eq!(json["mcp_verification"]["status"], "passed");
        assert_eq!(json["ui_verification"]["status"], "unverified");
        assert_eq!(
            serde_json::from_value::<AppEventDto>(json).expect("deserialize summary"),
            summary
        );

        let failure = AppEventDto::LocalAppOperationFailed {
            app_id: None,
            code: LocalAppPluginErrorCodeDto::InvalidMcpSettings,
            message: "The saved MCP settings for this app are invalid.".into(),
            request_id: Some("plugin-read-1".into()),
        };
        let json = serde_json::to_value(&failure).expect("serialize local app failure");
        assert_eq!(json["type"], "local_app_operation_failed");
        assert_eq!(json["code"], "invalid_mcp_settings");
        assert_eq!(json["request_id"], "plugin-read-1");
        assert_eq!(
            serde_json::from_value::<AppEventDto>(json).expect("deserialize local app failure"),
            failure
        );
    }

    // ---------------------------------------------------------------------
    // Ordinal locks for the local-app wire types live in
    // `client::protocol/tests/local_apps_ordinal_lock_test.rs`, NOT here.
    //
    // That file `include_str!`s this one and freezes the declaration order of
    // `AppRecordDto`, `PluginCommandDto`, `AppEventDto`,
    // `LocalAppVerificationStatusDto`, `LocalAppCreateConfirmationRequestDto`
    // and `LocalAppMcpProposalApprovalRequestDto`. UniFFI encodes enum
    // variants by declaration index and record fields positionally, so an
    // insertion, removal or reorder inside a frozen prefix silently
    // reinterprets data on installed clients; `version_guard_test.rs` cannot
    // see it, because it fingerprints an UNORDERED "<type>::<member>" ->
    // "<wire type>" map.
    //
    // Appending AFTER a frozen prefix is allowed and must NOT be added there.
    // Run `cargo test -p client::protocol --test local_apps_ordinal_lock_test`
    // after touching any of those six types.
    // ---------------------------------------------------------------------
}
