//! `ClientCommand` DTOs — inbound commands a client sends to the engine
//! (plan F1-06).
//!
//! This module freezes the FULL command set and ENCODES governing decision
//! §0.5: one transport connection owns ONE engine host that SWAPS its inner
//! `ConversationOrchestrator` in place on New/Resume. **`session_id` is a
//! CONNECTION ATTRIBUTE** — it is carried in the [`SessionStarted`] /
//! [`SessionResumed`] events ([`crate::protocol::events::ClientEvent`]), NOT as a
//! per-live-command param. The ONE allowed occurrence is
//! [`ClientCommand::ResumeSession`], which NAMES a target session to resume;
//! the `no_live_command_carries_session_id` test in
//! `tests/commands_test.rs` enforces this lock structurally.
//!
//! **Explicitly NOT commands** (client-local; the adapter does not back them,
//! decision per plan line 171) — these are intentionally absent from the
//! [`ClientCommand`] enum and are documented here, not encoded as variants:
//! - `SearchMessages` / `JumpToMessage` — scrollback navigation is a pure
//!   client-side view concern over the already-streamed message set.
//! - `ExportSession` — a client-side serialization of local state.
//! - `SetTheme` — a pure client UI preference.
//! - prompt-history (recall/up-arrow) — a client-local input buffer.
//!
//! Frozen serde conventions (decision §0.1):
//! - internally tagged: `#[serde(tag = "type", rename_all = "snake_case")]`
//!   (matches `lingxi_core::types::ContentBlock` / api-client `StreamEvent`),
//! - the top-level enum is `#[non_exhaustive]` (mirrors `lingxi_core::host::OutputEvent`),
//! - every optional field uses
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]`.
//!
//! Tool/image payloads stay UniFFI-flat (decision §0.4 / §0.8): inline image
//! bytes are a base64 [`String`] in [`ImageRefDto`]; `serde_json::Value` never
//! enters this crate.

use crate::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioOperationIdDto, AudioOperationResultDto,
};
use crate::protocol::computer_access::ComputerAccessResponseDto;
use crate::protocol::controls::ReasoningSelectionDto;
use crate::protocol::listings::TaskStatusDto;
use crate::protocol::local_apps::{
    AppAuthorizationDecisionDto, AppBridgeRequestDto, AppCreateOriginDto, AppRuntimeProfileDto,
    AppSurfaceDto, PluginCommandDto,
};
use crate::protocol::permission::PermissionResponseDto;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_git_version_control() -> bool {
    true
}

fn is_default_git_version_control(value: &bool) -> bool {
    *value
}

/// How a [`CreateApp`](ClientCommand::CreateApp) creates the app. Protocol v9
/// accepts `Shell`; the retained `Scaffolded` wire value is rejected by the
/// host so runtime identity can only come from native confirmation plus a
/// one-shot scaffold receipt.
///
/// A bare wire STRING (`"shell"` / `"scaffolded"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AppCreateModeDto {
    /// Create the empty shell only: the record is written with
    /// `scaffolded: false` and no scaffold is laid down. In this mode
    /// `surface` MUST be `None` — the shape is decided when the scaffold
    /// lands, not before.
    Shell,
    /// Retained for an explicit error response; direct create-and-scaffold is
    /// not allowed in protocol v9.
    Scaffolded,
}

/// A provider credential carried over the authenticated local bridge.
///
/// The wire representation is a plain JSON string for TypeScript/UniFFI
/// compatibility, while the Rust `Debug` surface is always redacted so tracing
/// or assertion output cannot accidentally print the secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(transparent)]
pub struct ProviderCredentialSecretDto {
    /// Secret wire value. Callers should keep its lifetime as short as possible.
    pub value: String,
}

impl ProviderCredentialSecretDto {
    /// Wrap a provider credential for transport.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self { value }
    }

    /// Borrow the credential at the audited persistence boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Debug for ProviderCredentialSecretDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProviderCredentialSecretDto(<redacted>)")
    }
}

/// One nested Desktop skills-management command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SkillAdminCommandDto {
    /// Operation verb, such as `get_catalog`, `save`, or `trash`.
    pub action: String,
    /// Correlation id for a mutating operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Stable skill or trash-entry identifier targeted by the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Writable source scope (`user` or `project`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Expected SHA-256 revision used for compare-and-swap writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Action-specific JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// One nested Desktop MCP-management command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct McpAdminCommandDto {
    /// Operation verb, such as `get_snapshot`, `save_server`, or `set_approval`.
    pub action: String,
    /// Correlation id for a mutating operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Optional MCP server name targeted by the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Configuration scope (`user`, `local`, or `project`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Expected SHA-256 revision used for compare-and-swap writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Action-specific JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// One nested Desktop plugin-management command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PluginAdminCommandDto {
    /// Operation verb, such as `get_catalog`, `preview_operation`, or `configure`.
    pub action: String,
    /// Correlation id for a mutating operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Plugin or marketplace identifier targeted by the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Optional configuration scope reserved for compatible clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Expected SHA-256 catalog or settings revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Action-specific JSON payload; sensitive values are forbidden here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// One nested Desktop hooks-management command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct HookAdminCommandDto {
    /// Operation verb, such as `get_snapshot`, `preflight`, or `save`.
    pub action: String,
    /// Correlation id for a mutating operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Optional hook event or handler identifier targeted by the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Writable hook source scope (`user`, `local`, or `project`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Expected SHA-256 revision used for compare-and-swap writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Action-specific JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// Remote UI surface that can attach to a running session's Mod surface roster.
/// `terminal` is host-owned and is never announced by a remote client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "lowercase")]
pub enum UiSurfaceDto {
    Desktop,
    Mobile,
    Vscode,
}

/// The inbound command envelope a client sends to the engine.
///
/// `#[non_exhaustive]` (mirrors `lingxi_core::host::OutputEvent`) so adding a command is
/// additive (no major bump). Internally tagged on `type`, `snake_case`
/// (decision §0.1).
///
/// **Session-lifecycle lock (decision §0.5):** no variant carries a
/// `session_id` EXCEPT [`Self::ResumeSession`], which names a target to resume.
/// `session_id` otherwise travels as a CONNECTION ATTRIBUTE on the
/// [`SessionStarted`](crate::protocol::events::ClientEvent::SessionStarted) /
/// [`SessionResumed`](crate::protocol::events::ClientEvent::SessionResumed) events.
//
// # Why the per-variant prose inside this enum is `//`, not `///`
//
// Under the `uniffi` feature the `uniffi::Enum` derive folds this enum into ONE
// compile-time metadata buffer: module path, variant names, field names, field
// type tags — AND every `///` docstring, on the variants and on their fields
// alike (`uniffi_macros::enum_::variant_metadata`). That buffer is a fixed
// `[u8; uniffi_core::metadata::BUF_SIZE]` with `BUF_SIZE` hardcoded to 16384,
// and overflowing it is not a warning or a truncation — it is a `const`-eval
// `assert!` failure, i.e. `client::protocol` stops compiling under `uniffi` and
// iOS/Android cannot be built at all. That is exactly what happened when this
// enum grew from 53 to 60 variants.
//
// Measured on this enum: the STRUCTURE (module path + variant/field names +
// type tags + counts) costs 3939 bytes. The `///` prose cost 16217 — four
// times the structure, and over the whole 16384-byte limit on its own. No
// amount of renaming or of dropping individual commands could have fixed that;
// the prose was the payload.
//
// So the prose stays here, verbatim and in place, as ordinary comments — it is
// simply no longer shipped into the FFI metadata. The wire, the Rust API, and
// the command set are untouched, and the enum is IDENTICAL under both feature
// settings (a cfg-gated variant set would have diverged the desktop and mobile
// builds — the exact blindness that let this regression land).
//
// `tests/uniffi_metadata_budget_test.rs` holds the line from here on: it names
// this enum and prints the byte count long before the hard wall is reached.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientCommand {
    // ── Turn driving ──────────────────────────────────────────────────────
    // Submit a user prompt to drive a turn. Carries the text, an optional
    // [`PromptModeDto`], inline image bytes (decision §0.8), and an optional
    // client turn correlator. Carries NO `session_id` (decision §0.5).
    SendPrompt {
        // The user prompt text.
        text: String,
        // Optional prompt-input mode. Skipped from the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_mode: Option<PromptModeDto>,
        // Inline image attachments. Uniform `{media_type, base64}` per
        // decision §0.8 (the adapter writes inline bytes to a temp file on the
        // engine host for the path-based engine entry). A non-optional `Vec`:
        // always present (empty when no images).
        images: Vec<ImageRefDto>,
        // Optional client-supplied turn correlator, echoed on
        // [`TurnStarted`](crate::protocol::events::ClientEvent::TurnStarted). Skipped
        // when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        // Inline visualization the user is following up on. The runtime
        // attaches its confirmed `modelContent` at send time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "uniffi", uniffi(default = None))]
        visualization_context: Option<crate::protocol::message::VisualizationRefDto>,
    },

    // Cancel the in-flight turn. The optional `turn_id` narrows the cancel to
    // a specific turn; `None` cancels the current one.
    Cancel {
        // Optional turn correlator to cancel. Skipped when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
    },

    // ── Permission resolution ─────────────────────────────────────────────
    // Approve a parked permission request, correlated by `request_id`
    // (the id-keyed gate, F1-14). The `response` distinguishes once / always.
    ApprovePermission {
        // Correlator with the originating
        // [`PermissionRequest`](crate::protocol::permission::PermissionRequest).
        request_id: u64,
        // The approval kind (`AllowOnce` / `AllowAlways`).
        response: PermissionResponseDto,
    },

    // Deny a parked permission request, correlated by `request_id`.
    DenyPermission {
        // Correlator with the originating
        // [`PermissionRequest`](crate::protocol::permission::PermissionRequest).
        request_id: u64,
    },

    // ── `computer` tool `request_access` resolution ──────────────────────────
    // Approve a parked `computer`-tool `request_access` prompt, correlated by
    // `request_id` (the connection-scoped
    // `ComputerAccessBroker`). `response` carries the granted app
    // subset + capability flags.
    ApproveComputerAccess {
        // Correlator with the originating
        // [`ComputerAccessRequestDto`](crate::protocol::computer_access::ComputerAccessRequestDto).
        request_id: u64,
        // The user's grant.
        response: ComputerAccessResponseDto,
    },

    // Deny a parked `computer`-tool `request_access` prompt, correlated by
    // `request_id`.
    DenyComputerAccess {
        // Correlator with the originating
        // [`ComputerAccessRequestDto`](crate::protocol::computer_access::ComputerAccessRequestDto).
        request_id: u64,
    },

    // ── `AskUserQuestion` resolution ──────────────────────────────────────
    // Submit the answers for a parked interactive questionnaire.
    AnswerAskUserQuestion {
        // Correlator from the originating
        // [`AskUserQuestionRequestDto`](crate::protocol::ask_user_question::AskUserQuestionRequestDto).
        request_id: u64,
        // Question text to the user's selected label(s) or free-text answer.
        answers: HashMap<String, String>,
    },

    // Cancel a parked interactive questionnaire.
    CancelAskUserQuestion {
        // Correlator from the originating request.
        request_id: u64,
    },

    // Change the live permission mode for subsequent tool checks. Confirmed
    // by a [`PermissionModeChanged`](crate::protocol::events::ClientEvent::PermissionModeChanged)
    // event carrying the authoritative mode after engine-side validation.
    SetPermissionMode {
        // Permission-mode wire id (`default`, `acceptEdits`, `plan`, `auto`,
        // `dontAsk`, or `bypassPermissions`).
        mode: String,
    },

    // ── Provider credentials ─────────────────────────────────────────────
    // Return non-secret availability and masked suffix previews for the
    // requested provider ids. Plaintext secret values are never returned.
    ListProviderCredentials {
        // Main-process correlator echoed by `ProviderCredentialStatus`.
        operation_id: u64,
        // Provider/keychain ids to inspect.
        provider_ids: Vec<String>,
        // Optional provider ids whose masked suffix should be returned. Keep
        // this empty for status lists so macOS can use an attribute-only
        // Keychain query without prompting to decrypt every saved secret.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preview_provider_ids: Vec<String>,
    },

    // Persist a provider credential through the engine's shared secure store.
    SetProviderCredential {
        // Main-process correlator echoed by `ProviderCredentialStatus`.
        operation_id: u64,
        // Provider/keychain id used by CLI, TUI, and Desktop.
        provider_id: String,
        // Secret received only over the authenticated loopback bridge.
        credential: ProviderCredentialSecretDto,
    },

    // Delete a provider credential from the engine's shared secure store.
    DeleteProviderCredential {
        // Main-process correlator echoed by `ProviderCredentialStatus`.
        operation_id: u64,
        // Provider/keychain id used by CLI, TUI, and Desktop.
        provider_id: String,
    },

    // ── Model ─────────────────────────────────────────────────────────────
    // Switch the active model. Confirmed by a
    // [`ModelChanged`](crate::protocol::events::ClientEvent::ModelChanged) event.
    SetModel {
        // The model name to activate.
        model: String,
    },

    // Request the available-model catalog. Replied with a
    // [`ModelList`](crate::protocol::events::ClientEvent::ModelList) event.
    ListModels,

    // ── Slash commands ────────────────────────────────────────────────────
    // Run a raw slash-command line. Local/display commands reply with
    // [`crate::protocol::events::ClientEvent::SlashCommandResult`]; prompt-expanding
    // commands reuse `turn_id` for the ordinary turn stream.
    RunSlashCommand {
        // The raw command line (e.g. `"/model opus"`), leading `/` included.
        raw: String,
        // Optional client turn id used to correlate prompt-like slash commands
        // with the normal streaming turn lifecycle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
    },

    // ── Listings ──────────────────────────────────────────────────────────
    // Refresh a set of screen listings by kind. Each requested
    // [`ListingKindDto`] produces its matching listing event
    // ([`McpServers`](crate::protocol::events::ClientEvent::McpServers), etc.).
    RefreshListings {
        // Which screen listings to (re)pull.
        which: Vec<ListingKindDto>,
    },

    // List the agent instances attached to the current session. The reply is
    // [`ClientEvent::SessionAgentList`](crate::protocol::events::ClientEvent::SessionAgentList)
    // and is scoped to the connection's active session.
    ListSessionAgents,

    // Load one session-agent transcript. `agent_id` accepts the stable
    // `agent:<uuid>` form returned by [`SessionAgentSummaryDto`], or `main`
    // for the parent conversation. Invalid ids are rejected by the engine.
    LoadSessionAgentTranscript {
        // Agent instance to load.
        agent_id: String,
    },

    // ── Session lifecycle (decision §0.5) ─────────────────────────────────
    // Start a fresh session on this connection (swaps the connection's inner
    // orchestrator). Carries an optional working directory + model, but NO
    // `session_id` — the new id is reported back via
    // [`SessionStarted`](crate::protocol::events::ClientEvent::SessionStarted).
    NewSession {
        // Optional working directory for the new session. Skipped when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        // Optional starting model. Skipped when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    // Resume a prior session on this connection. **The ONE allowed
    // `session_id`-carrying command** (decision §0.5): it NAMES a target
    // session to resume. Confirmed by a
    // [`SessionResumed`](crate::protocol::events::ClientEvent::SessionResumed) event.
    ResumeSession {
        // The id of the session to resume (the named target).
        session_id: String,
        // Optional working directory override. Skipped when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },

    // Request the resumable-session catalog. Replied with a
    // [`SessionList`](crate::protocol::events::ClientEvent::SessionList) event.
    ListSessions {
        // Optional cap on the number of rows. Skipped when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    // ── Auth + session control ────────────────────────────────────────────
    // Begin the login flow. Auth state surfaces via
    // [`AuthState`](crate::protocol::events::ClientEvent::AuthState).
    Login,

    // Sign out the current user.
    Logout,

    // Force a context compaction now. Reports via
    // [`CompactionCompleted`](crate::protocol::events::ClientEvent::CompactionCompleted).
    ForceCompact,

    // Clear the current session's history (rejected mid-turn, F2-08).
    ClearSession,

    // ── Tasks ─────────────────────────────────────────────────────────────
    // List background tasks, optionally filtered by status. Each row arrives
    // as a [`TaskRow`](crate::protocol::events::ClientEvent::TaskRow) event.
    TaskList {
        // Optional status filter. Skipped from the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_filter: Option<TaskStatusDto>,
        // Optional correlator requesting an authoritative completion receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    // Pull a task's accumulated output spool from `offset`. Replied with a
    // [`TaskOutputChunk`](crate::protocol::events::ClientEvent::TaskOutputChunk) event.
    TaskOutput {
        // 9-char task id.
        task_id: String,
        // Byte/line offset to read from.
        offset: u64,
    },

    // Stop a running task by id.
    TaskStop {
        // 9-char task id.
        task_id: String,
    },

    // Resume a paused local workflow in the current session. The engine
    // resolves the persisted script/checkpoint by task id; clients never
    // receive or submit script paths or arguments directly.
    ResumeWorkflow {
        // Paused workflow task id.
        task_id: String,
    },

    // ── Local apps ────────────────────────────────────────────────────────
    // List the local apps. Replied with an
    // [`AppsChanged`](crate::protocol::events::ClientEvent::AppsChanged) event carrying
    // the full record set.
    ListApps,

    // Request the complete detail snapshot for one app.
    GetAppDetails {
        // App whose detail snapshot is requested.
        app_id: String,
    },

    // Create a new local-app record and its workspace. Confirmed by an
    // [`AppsChanged`](crate::protocol::events::ClientEvent::AppsChanged) event.
    CreateApp {
        // User-facing display name.
        name: String,
        // Where the creation originated (`chat` / `library`).
        origin: AppCreateOriginDto,
        // One-line description of what the app should do — the seed the
        // LLM authors the questionnaire from. Distinct from `name`: a
        // display label is not a spec, and conflating the two used to leave
        // the questionnaire authored from a bare app name.
        brief: String,
        // Whether to keep Git-backed source versions. Defaults to enabled
        // when omitted by an older client.
        #[serde(
            default = "default_git_version_control",
            skip_serializing_if = "is_default_git_version_control"
        )]
        git_enabled: bool,
        // Provider-qualified model reference for the app creation workflow.
        // Skipped when the app follows the current session model -- which is
        // what every shipping client sends today: `LocalAppsStore.swift` and
        // `LocalAppsViewModel.kt` both hard-code the null, there is no picker.
        //
        // ⚠ INERT END TO END as of this comment. The engine accepts the
        // value, trims/byte-caps it (`local_apps::service`) and persists it as
        // `AppRecord::workflow_model`, and `commit_scaffold` preserves it
        // across scaffold -- but NOTHING in production reads the stored value
        // back. Every `.workflow_model` READ site in the workspace is a
        // `#[cfg(test)]` assertion, and the reader the design named for it,
        // `apply_local_app_build_default_model`, exists only under
        // `docs/local-apps/` and in no source tree at all. Setting it has no
        // side effect either: the scaffold contract
        // (`local_apps_host::formal_workspace_contract`) renders `name`,
        // `brief` and `mcp_intent`, never this.
        //
        // It is kept rather than dropped because a struct variant's fields are
        // encoded POSITIONALLY by the generated mobile bindings (see `surface`
        // below): removing it would silently shift `conversation_id` /
        // `surface` / `mode` / `request_id` on a client built against an older
        // binding. Close the gap by restoring a reader, not by deleting a
        // field mid-struct. `tasks/src/scope.rs`'s module docs hold the
        // long-form account and the greps behind it; do not upgrade this note
        // to "the build workflow defaults to it" until a reader actually
        // exists.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow_model: Option<String>,
        // Conversation the app was created from (`origin: chat`). Skipped
        // from the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        // Retained only for protocol-v9 error compatibility. New clients send
        // `None` with `Shell`; the later native runtime-profile selection owns
        // the immutable surface. Appended LAST: generated mobile bindings
        // encode struct variants positionally, so inserting a field above
        // `conversation_id` would silently reinterpret it on an older client.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface: Option<AppSurfaceDto>,
        // `Shell` creates the empty shell. `Scaffolded` is retained only so
        // the host can return a clear protocol-v9 error directing the caller
        // through native profile confirmation and receipt-bound scaffold.
        mode: AppCreateModeDto,
        // Client-generated correlation key, echoed verbatim on both the
        // success event (`AppEventDto::AppCreated`) and the failure event
        // (`ClientEvent::AppOperationFailed`) so the caller that started this
        // creation can recognise its own outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    // Start the app's dev-server runtime. Phase 1 validates the app exists,
    // then fails typed with
    // [`AppOperationFailed`](crate::protocol::events::ClientEvent::AppOperationFailed)
    // `{ code: not_yet_available }` (runtime is phase 4).
    StartApp {
        // App to start.
        app_id: String,
    },

    // Stop the app's dev-server runtime (`not_yet_available` until phase 4,
    // like [`Self::StartApp`]).
    StopApp {
        // App to stop.
        app_id: String,
    },

    // Restart the app's dev-server runtime (`not_yet_available` until phase
    // 4, like [`Self::StartApp`]).
    RestartApp {
        // App to restart.
        app_id: String,
    },

    // Execute one data-only request from the versioned local-app bridge.
    ExecuteAppBridgeRequest {
        // Data-only request to execute.
        request: AppBridgeRequestDto,
    },

    // Resolve a permission-gated structured `WebView` action.
    ResolveAppUiRequest {
        // Pending UI request correlator.
        request_id: String,
        // User's scoped authorization decision.
        decision: AppAuthorizationDecisionDto,
        // Structured `WebView` inspection/action result as JSON data.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_json: Option<String>,
        // Host-side action failure, if the authorized action failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // Resolve a native app capability request.
    ResolveAppCapabilityRequest {
        // Pending capability request correlator.
        request_id: String,
        // User's scoped authorization decision.
        decision: AppAuthorizationDecisionDto,
    },

    // Approve or reject a host-issued App Agent Profile proposal. The token
    // is minted by the engine and is never accepted from the local-app page.
    ResolveAppProfileProposal {
        // App whose profile is being changed.
        app_id: String,
        // One-time token from [`AppProfileProposal`](crate::protocol::local_apps::AppEventDto).
        approval_token: String,
        // `true` applies the exact proposal shown by the trusted client UI.
        approved: bool,
    },

    // Revoke every session and durable capability grant for one app. Future
    // gated operations prompt again; the design manifest is not changed.
    ResetAppPermissions {
        // App whose saved grants should be cleared.
        app_id: String,
    },

    // Page through one app's workspace-scoped session catalog. Replied with
    // an [`AppSessionsChanged`](crate::protocol::events::ClientEvent::AppSessionsChanged)
    // event. `offset`/`limit` page the modified-descending catalog
    // (default limit 50, max 100); the reply's `next_offset` is `None` on
    // the last page.
    ListAppSessions {
        // App whose sessions to list.
        app_id: String,
        // Zero-based row offset into the modified-descending catalog.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        // Page size (default 50, max 100).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    // List an app's restorable checkpoints. Phase 1 replies with an empty
    // list (git wiring is phase 5).
    ListAppCheckpoints {
        // App whose checkpoints to list.
        app_id: String,
    },

    // Restore an app workspace to a checkpoint. Phase 1 validates the app
    // exists, then fails typed with
    // [`AppOperationFailed`](crate::protocol::events::ClientEvent::AppOperationFailed)
    // `{ code: not_yet_available }` (git wiring is phase 5).
    RestoreAppCheckpoint {
        // App to restore.
        app_id: String,
        // Checkpoint to restore to.
        checkpoint_id: String,
    },

    // Delete an app record and its workspace. Confirmed by an
    // [`AppsChanged`](crate::protocol::events::ClientEvent::AppsChanged) event.
    DeleteApp {
        // App to delete.
        app_id: String,
    },

    // ── Lifecycle ─────────────────────────────────────────────────────────
    // Request a clean engine/connection shutdown.
    RequestExit,

    // Request the authoritative conversation-controls snapshot for the
    // active session/model.
    GetConversationControls,

    // Change the live reasoning selection for subsequent provider requests.
    SetReasoningSelection {
        // Provider-neutral reasoning selection.
        selection: ReasoningSelectionDto,
    },

    // Toggle the session's fast-mode tier for subsequent requests.
    SetFastMode {
        // Whether the user wants the fast tier enabled.
        enabled: bool,
    },

    // Resolve a native dependency-change confirmation request.  `approved`
    // is a one-shot decision; the host issues a dependency receipt only for
    // `true` and performs no registry access before that decision.
    ResolveAppDependencyChangeConfirmation {
        // Pending dependency-review request correlator.
        request_id: String,
        // Whether the user approved the exact package diff shown by native UI.
        approved: bool,
    },

    // ── Plugins (§17.1, §19.2) ────────────────────────────────────────────
    // Enable/disable/status for one builtin plugin. Nested in
    // [`PluginCommandDto`] so plugin operations do not consume a separate
    // top-level protocol variant for every operation.
    PluginCommand {
        command: PluginCommandDto,
    },

    // ── Desktop settings managers ────────────────────────────────────────
    // Structured skills manager for the Desktop settings screen.
    SkillAdmin {
        command: SkillAdminCommandDto,
    },

    // Structured MCP manager for the Desktop settings screen.
    McpAdmin {
        command: McpAdminCommandDto,
    },

    // Structured plugin manager for the Desktop settings screen.
    PluginAdmin {
        command: PluginAdminCommandDto,
    },

    // Structured hooks manager for the Desktop settings screen.
    HookAdmin {
        command: HookAdminCommandDto,
    },

    // ── Settings ─────────────────────────────────────────────────────────
    // Apply a batch of shallow, top-level settings edits to one writable
    // layer. `patch_json` is a JSON **object**; a `null` value for a key
    // deletes it, any other value sets it. `serde_json::Value` must not enter
    // this crate (decision §0.4), so the patch travels as a string exactly
    // the way [`crate::protocol::events::ClientEvent::ToolUseStarted`]'s `input_json`
    // does — the receiving end (`bridge-server::settings_bridge`) parses it.
    //
    // The `permissions` top-level key is refused here: it has a dedicated
    // writer — the three commands below. Only `User` / `Project` / `Local`
    // are valid destinations — the engine's full settings-layer enum also
    // has non-writable layers (`defaults` / `cli` / `managed` / `env`);
    // [`WritableScopeDto`] omits them so a write to one is
    // unrepresentable on the wire, rather than a runtime rejection.
    UpdateSettings {
        // Which writable layer's file to edit.
        destination: WritableScopeDto,
        // A JSON object of top-level key → new value (or `null` to delete).
        patch_json: String,
    },

    // ── Permissions (persisted) ─────────────────────────────────────────────
    // `permission/src/persist.rs` already owns permission writing (per-
    // destination exclusive locks, atomic root-confined replacement, alias-
    // normalizing de-duplication, unknown-key preservation) — these three
    // commands route to it directly instead of going through `UpdateSettings`,
    // which explicitly refuses the `permissions` top-level key precisely to
    // keep there from being two write paths to one key.
    // Add or remove rules in one behavior bucket
    // (`permissions.{allow,deny,ask}`) on a single writable layer, in one
    // locked atomic transaction.
    UpdatePermissionRules {
        // Which writable layer's file to edit.
        destination: WritableScopeDto,
        // The behavior bucket every rule in `add`/`remove` belongs to.
        behavior: PermissionBehaviorDto,
        // Rule strings (`"Tool"` or `"Tool(content)"`) to add. Parsing is
        // infallible — a malformed string degrades to a bare tool name,
        // matching claude-code's own parser — so there is no rejected-input
        // case here.
        add: Vec<String>,
        // Rule strings to remove, same syntax as `add`.
        remove: Vec<String>,
    },

    // Persist the DEFAULT permission mode a future session boots into.
    // Distinct from [`Self::SetPermissionMode`], which changes the mode for
    // the CURRENT session only and is never written to disk. Persisting
    // `"bypassPermissions"` is deliberately refused by the underlying writer
    // (persisting it would silently re-enter bypass mode on the next session
    // load) — the host reports that refusal rather than pretending it
    // happened.
    SetDefaultPermissionMode {
        // Which writable layer's file to edit.
        destination: WritableScopeDto,
        // Permission-mode wire id (`default`, `acceptEdits`, `plan`, `auto`,
        // `dontAsk`, or `bypassPermissions` — the last is always refused).
        mode: String,
    },

    // Add or remove entries in `permissions.additionalDirectories` on a
    // single writable layer, in one locked atomic transaction.
    UpdateWorkspaceDirectories {
        // Which writable layer's file to edit.
        destination: WritableScopeDto,
        // Directory strings to add, stored verbatim (no canonicalization).
        add: Vec<String>,
        // Directory strings to remove, compared verbatim against the file.
        remove: Vec<String>,
    },

    // ── MCP servers (persisted) ──────────────────────────────────────────
    // MCP does NOT use the settings-layer machinery above: it has its own
    // three storage locations (`bridge-server::mcp_bridge`) —
    // User/Local both live inside `~/.lingxi.json` (top-level `mcpServers`,
    // and `projects[<cwd>].mcpServers`, respectively), Project lives in
    // `<project>/.mcp.json`. Read access is already wired through
    // `RefreshListings{Mcp}` → `ClientEvent::McpServers`; these two commands
    // are the write side only.
    // Add or replace one MCP server definition in one writable scope.
    // `config_json` is a JSON **object** shaped like a `.mcp.json` entry
    // (`{"command": ..., "args": [...], "env": {...}}` or
    // `{"url": ..., "type": ...}`); `serde_json::Value` must not enter this
    // crate (decision §0.4), so the config travels as a string exactly the
    // way [`Self::UpdateSettings::patch_json`] does — the receiving end
    // (`bridge-server::mcp_bridge`) parses and validates it. `name` must be
    // non-empty (after trimming whitespace); an empty name is rejected
    // rather than silently written, since `mcp::json_config` would turn a
    // `""` map key into a nameless server entry.
    //
    // **Acknowledged by silence.** A successful upsert emits NO event — only
    // a failure emits [`crate::protocol::events::ClientEvent::Error`]. There is no
    // success event to wait for: the only re-emittable MCP listing
    // (`ClientEvent::McpServers`, via `RefreshListings{Mcp}`) is sourced
    // from the engine's in-memory `McpRegistry` snapshot, which a bare file
    // write does not touch — re-emitting it here would hand the caller a
    // stale list still missing the server just added, which reads as a
    // failure. Re-pulling that listing (on whatever cadence the caller
    // wants) is how a client observes the change.
    UpsertMcpServer {
        // Which writable MCP scope to edit.
        scope: WritableScopeDto,
        // Server name (the `mcpServers` map key). Must be non-empty.
        name: String,
        // A JSON object holding the server's transport config.
        config_json: String,
    },

    // Remove one MCP server definition from one writable scope. Idempotent:
    // removing an already-absent name is not an error. Same
    // acknowledged-by-silence contract as [`Self::UpsertMcpServer`] — see its
    // doc comment for why.
    RemoveMcpServer {
        // Which writable MCP scope to edit.
        scope: WritableScopeDto,
        // Server name (the `mcpServers` map key) to remove.
        name: String,
    },

    // ── Audio (engine -> device AudioService request/result) ────────────
    AudioResponse {
        // Globally unique identity echoed by the matching AudioRequest.
        identity: AudioOperationIdDto,
        // Terminal operation outcome, including structured errors.
        result: AudioOperationResultDto,
    },
    // Reattach a mobile client to a durable turn on the connection's active
    // session. The engine first emits the current recovery snapshot and then
    // replays events whose sequence is greater than `after_sequence`.
    AttachTurn {
        // Stable client turn id originally supplied to [`Self::SendPrompt`].
        turn_id: u64,
        // Last event sequence the client durably observed. `None` requests the
        // full retained event window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_sequence: Option<u64>,
    },

    // Resume a checkpointed durable turn when its recovery policy allows it.
    // Unsafe or permission-bound checkpoints remain `waiting_for_user` rather
    // than silently replaying external side effects.
    ResumeTurn {
        // Stable client turn id to resume on the active session.
        turn_id: u64,
    },

    // Persist a platform-lease expiration without converting it to cancel.
    PauseTurn {
        // Stable client turn id to pause on the active session.
        turn_id: u64,
        // Machine-readable platform reason such as `background_time_expired`.
        reason: String,
    },

    // Change the global TypeScript LSP activation policy. The command/path is
    // fixed by the engine; clients may choose only `auto`, `off`, or `on`.
    SetTypescriptLspMode {
        mode: String,
    },

    // Confirm the runtime profile family selected by native UI for one app.
    // Appended to preserve every previously frozen UniFFI variant ordinal.
    ResolveAppRuntimeProfileSelection {
        // Pending runtime selector correlator.
        request_id: String,
        // The runtime profile family the user selected.
        selected_family: AppRuntimeProfileDto,
    },

    // Fork the named session into a fresh session under `target_mode` while
    // keeping the current session's workspace. Appended to preserve every
    // previously frozen UniFFI variant ordinal.
    ForkSession {
        // The source session id (the named target to duplicate).
        session_id: String,
        // Capability profile the forked session should run under.
        target_mode: crate::protocol::listings::SessionModeDto,
    },

    // Test DNS/TLS, authentication, and model availability without exposing a
    // stored credential back to the client. An optional draft credential is
    // used for this request only and is never persisted. Appended to preserve
    // every previously frozen UniFFI variant ordinal.
    TestProviderConnection {
        // Main-process correlator echoed by `ProviderConnectionTested`.
        operation_id: u64,
        // Provider/keychain id used to select authentication semantics.
        provider_id: String,
        // Provider API base selected from the built-in catalog (or the saved
        // Anthropic override).
        api_base: String,
        // Model id to verify against a recognized model-list response.
        model: String,
        // Unsaved form value, when present. Omitted to use secure storage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_override: Option<ProviderCredentialSecretDto>,
    },
    /// Manage durable scheduled tasks in the active workspace.
    CronManage {
        request_id: String,
        request: CronRequestDto,
    },
    // Trusted human-client message to an existing task. Appended to preserve
    // every existing UniFFI ordinal; model tools never select this origin.
    TaskMessage {
        task_id: String,
        message: String,
    },
    CronRunStarted {
        run_id: String,
        session_id: String,
    },
    ScheduledRunTurn {
        run_id: String,
        prompt: String,
        model: String,
        reasoning: ReasoningSelectionDto,
    },
    /// Terminal acknowledgement for an authenticated host scheduled run.
    CronRunCompleted {
        run_id: String,
        session_id: Option<String>,
        summary: Option<String>,
        error: Option<String>,
    },
    /// Update the connection's shared device-audio capability snapshot.
    UpdateAudioCapabilities {
        capabilities: AudioCapabilitySnapshotDto,
    },

    // Explicit renderer lifecycle. These are separate from the bridge hello
    // and socket close: a renderer attaches only after its session is ready,
    // and the host uses connection ownership to clean up a lost renderer.
    UiAttach {
        surface: UiSurfaceDto,
        client_id: String,
    },
    UiDetach {
        client_id: String,
    },
    // Session-bound Mod UI control request. `request_json` is the canonical
    // Native control payload including its subtype; the correlation id is an
    // outer client-protocol field and must not be copied into that payload.
    UiRender {
        request_id: String,
        request_json: String,
    },
    UiClientModule {
        request_id: String,
        plugin: String,
    },
    UiMessage {
        request_id: String,
        request_json: String,
    },
    UiClientFault {
        request_id: String,
        request_json: String,
    },
    UiClientPress {
        request_id: String,
        request_json: String,
    },
    // Parent UI held-action controls. These are Native `$` controls and remain
    // distinct from the nested Client surface's `ui_client_press` route.
    UiPress {
        request_id: String,
        request_json: String,
    },
    UiInput {
        request_id: String,
        request_json: String,
    },
    UiSelect {
        request_id: String,
        request_json: String,
    },
    // Host-local UI VM adapter operation; this is not a Native $.ui subtype.
    UiClientOperation {
        request_id: String,
        operation_json: String,
    },
}

/// A settings tier the user can WRITE to, as named on the wire.
///
/// This was two enums — `SettingsDestinationDto` and `McpScopeDto` — with the
/// same three variants, the same bare `snake_case` wire strings, and the same
/// rationale written out twice: the read-only tiers (`Managed`, `Env`, `Cli`,
/// `Defaults`; `Dynamic`, `Enterprise`) are omitted rather than accepted and
/// rejected at runtime. One concept, so one type.
///
/// The lowered [`lingxi_core::types::WritableScope`]. A bare wire STRING
/// (`"user"` / `"project"` / `"local"`).
///
/// - `User` → `<lingxi_home>/settings.json`; for MCP, `~/.lingxi.json`
///   top-level `mcpServers`.
/// - `Project` → `<project>/<DOT_DIR>/settings.json`; for MCP,
///   `<project>/.mcp.json`.
/// - `Local` → `<project>/<DOT_DIR>/settings.local.json`; for MCP,
///   `~/.lingxi.json` under `projects[<cwd>].mcpServers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum WritableScopeDto {
    /// The user's own settings.
    User,
    /// Checked into the project.
    Project,
    /// Gitignored, per-clone.
    Local,
}

/// The behavior bucket a permission rule belongs to
/// (`permissions.{allow,deny,ask}` in a settings file), as named on the wire.
/// Mirrors `permission::PermissionBehavior` one-to-one; kept as a separate DTO
/// so the `permission` crate's type never crosses the wire boundary directly.
///
/// A bare wire STRING (`"allow"` / `"deny"` / `"ask"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehaviorDto {
    /// Allow the call without prompting.
    Allow,
    /// Deny the call.
    Deny,
    /// Ask the user.
    Ask,
}

/// Prompt-input mode for [`ClientCommand::SendPrompt`]. Internally tagged on
/// `type`, `snake_case`. `#[non_exhaustive]` so a future mode is additive.
///
/// Mirrors the TUI input modes (normal chat, `!` bash, `#` memory, plan mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PromptModeDto {
    /// Normal chat prompt.
    Normal,
    /// Bash-passthrough mode (`!`-prefixed input).
    Bash,
    /// Memory-edit mode (`#`-prefixed input).
    Memory,
    /// Plan mode (read-only planning before edits).
    Plan,
}

/// An inline image attachment for [`ClientCommand::SendPrompt`]. Uniform inline
/// `{media_type, base64}` on the wire (no transport-leaking enum, decision
/// §0.8): the adapter writes the inline bytes to a temp file on the engine host
/// for the path-based engine entry. Mobile inline image *input* is itself
/// DEFERRED (§5.12) — this DTO is the frozen shape, not a lit-up path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ImageRefDto {
    /// MIME media type (e.g. `"image/png"`, `"image/jpeg"`).
    pub media_type: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// The reply for [`ClientCommand::RunSlashCommand`] — slash dispatch is LOSSY,
/// so the result is a display string plus an optional prompt the command wants
/// injected as the next turn's input. (The rich `SlashCommandKind` dispatch
/// shape stays engine-side; this is what crosses the wire.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CommandResultDto {
    /// User-facing display text produced by the command.
    pub display: String,
    /// Optional prompt the command injects as the next turn input. Skipped from
    /// the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected: Option<String>,
}

/// Which screen listing to (re)pull via [`ClientCommand::RefreshListings`].
/// Internally tagged on `type`, `snake_case`. `#[non_exhaustive]` so a future
/// screen kind is additive. Each kind maps to a listing event in
/// [`crate::protocol::events::ClientEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ListingKindDto {
    /// Resumable-session catalog → `SessionList`.
    Sessions,
    /// Available-model catalog → `ModelList`.
    Models,
    /// MCP server listing → `McpServers`.
    Mcp,
    /// Discovered-skill listing → `Skills`.
    Skills,
    /// Hook listing → `Hooks`.
    Hooks,
    /// Subagent listing → `Agents`.
    Agents,
    /// Slash-command catalog → `SlashCommandCatalog`.
    SlashCommands,
    /// LINGXI.md memory listing → `MemoryEntries`.
    Memory,
    /// `/status` panel snapshot → `StatusSnapshot`.
    Status,
    /// Effective settings + provenance → `SettingsSnapshot`.
    Settings,
    /// Auth state → `AuthState`.
    Auth,
    /// `/doctor` report → `DoctorReport`.
    Doctor,
    /// Task listing → `TaskRow` (one per task).
    Tasks,
    /// Coordinator-team roster → `CoordinatorWorker` (one per worker) (T18).
    Coordinator,
}

/// Structured management request for the existing local cron scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CronRequestDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<CronAutomationDto>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurring: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_expiry: Option<bool>,
}

/// Versioned scheduled execution settings shared by native clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct CronAutomationDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub version: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    pub model: String,
    pub reasoning: ReasoningSelectionDto,
    pub run_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_session_id: Option<String>,
    pub notification_policy: String,
    #[serde(default)]
    pub runs: Vec<CronRunDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct CronRunDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    /// Identifies the current claim, independently of the stable occurrence ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_generation: Option<u64>,
    /// Host identity for a manual occurrence, including adopted legacy retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_occurrence_at: Option<u64>,
    pub id: String,
    pub task_id: String,
    pub scheduled_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub status: String,
    pub model: String,
    pub reasoning: ReasoningSelectionDto,
    pub session_id: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}
