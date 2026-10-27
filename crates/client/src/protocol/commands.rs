//! `ClientCommand` DTOs — inbound commands a client sends to the engine
//! (plan F1-06).
//!
//! This module freezes the FULL command set and ENCODES governing decision
//! §0.5: one transport connection owns ONE engine host that SWAPS its inner
//! `ConversationOrchestrator` in place on New/Resume. **`session_id` is a
//! CONNECTION ATTRIBUTE** — it is carried in the [`SessionStarted`] /
//! [`SessionResumed`] events ([`crate::protocol::events::ClientEvent`]), NOT as a
//! per-live-command param. [`ClientCommand::ResumeSession`] names a target
//! session to resume. Mobile durable-turn commands carry their expected owner
//! as a fence; they do not themselves switch sessions.
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

use crate::protocol::ask_user_question::AskUserQuestionAnswersDto;
use crate::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioOperationIdDto, AudioOperationResultDto,
};
use crate::protocol::computer_access::ComputerAccessResponseDto;
use crate::protocol::controls::ReasoningSelectionDto;
use crate::protocol::listings::TaskStatusDto;
use crate::protocol::media::{
    MediaConnectionResponseDto, MediaControlResponseDto, MediaDeviceResponseDto,
    MediaServiceRequestDto, MediaSessionModeDto,
};
use crate::protocol::permission::PermissionResponseDto;
use crate::protocol::text::ClientTextDto;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::collections::HashMap;

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
    /// Exact source skill or trash-entry identifier targeted by the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ClientTextDto>,
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

/// One nested Desktop Fusion-management command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FusionAdminCommandDto {
    /// Operation verb, such as `get_catalog`, `save`, or `resolve_strategy`.
    pub action: String,
    /// Correlation id for a mutating operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<u64>,
    /// Optional Fusion strategy ID targeted by the action.
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
macro_rules! define_client_command {
    ($($variants:tt)*) => {
        #[allow(missing_docs)]
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[non_exhaustive]
        pub enum ClientCommand { $($variants)* }

        #[derive(Deserialize)]
        // Private external tagging avoids serde's Content buffer, which cannot
        // deserialize RawValue-backed text. The public wire stays type-tagged.
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum ClientCommandWire { $($variants)* }

    };
}

define_client_command! {

    /// Start native Agent realtime on this authenticated connection.
    StartRealtimeAudio {
        request_json: String,
    },
    /// Bounded microphone/playback controls; tools/history remain host-owned.
    RealtimeAudioInput {
        input_json: String,
    },
    /// Stop capture, provider generation, and native realtime tools.
    StopRealtimeAudio,
    /// Read the current session's exact, non-secret audio provider binding.
    GetAudioSessionContext,
    // ── Turn driving ──────────────────────────────────────────────────────
    // Submit a user prompt to drive a turn. Carries the text, an optional
    // [`PromptModeDto`], inline image bytes (decision §0.8), and an optional
    // client turn correlator. Mobile main-conversation callers must also
    // supply the owner captured when the user submitted the prompt.
    SendPrompt {
        // Captured session owner, or None for transports whose connection or
        // side-chat routing supplies the owner independently.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        // The user prompt text.
        text: ClientTextDto,
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
        visualization_context: Option<crate::protocol::message::VisualizationRefDto>,
    },

    // Cancel the in-flight turn. Mobile main-conversation clients must provide
    // the captured session owner, even for a current-turn cancel. Other
    // connection types may use None for their own current connection or side
    // chat semantics. The optional `turn_id` narrows the cancel to one turn.
    Cancel {
        // Expected session owner, or None for connection-scoped cancellation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
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
        answers: AskUserQuestionAnswersDto,
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
        // Exact model-selection source. Route/profile resolution remains host-owned.
        model: ClientTextDto,
    },

    // Request the available-model catalog. Replied with a
    // [`ModelList`](crate::protocol::events::ClientEvent::ModelList) event.
    ListModels,

    // ── Slash commands ────────────────────────────────────────────────────
    // Run a raw slash-command line. Local/display commands reply with
    // [`crate::protocol::events::ClientEvent::SlashCommandResult`]; prompt-expanding
    // commands reuse `turn_id` for the ordinary turn stream.
    RunSlashCommand {
        // Captured session owner, or None for independently scoped transports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        // The raw command line (e.g. `"/model opus"`), leading `/` included.
        raw: ClientTextDto,
        // Ordered inline image attachments supplied with this slash command.
        images: Vec<ImageRefDto>,
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
        // Optional exact starting model-selection source. Skipped when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ClientTextDto>,
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
    // Reattach to a durable turn owned by the expected active session. Each
    // attempt must use a fresh request id; the host validates it is nonempty
    // and at most 128 UTF-8 bytes and echoes it without modification. The
    // host emits a matching TurnAttachStarted, retained events after the
    // cursor, and TurnAttachFinished in order under the turn delivery gate.
    AttachTurn {
        // Expected session owner; does not switch the connection's session.
        session_id: String,
        // Stable client turn id originally supplied to [`Self::SendPrompt`].
        turn_id: u64,
        // Last event sequence actually projected by the client. `None`
        // requests the full retained event window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_sequence: Option<u64>,
        // Unique opaque correlation id for this attempt.
        request_id: String,
    },

    // Resume a checkpointed durable turn when its recovery policy allows it.
    // Unsafe or permission-bound checkpoints remain `waiting_for_user` rather
    // than silently replaying external side effects.
    ResumeTurn {
        // Expected active session owner; this does not switch sessions.
        session_id: String,
        // Stable client turn id to resume on the active session.
        turn_id: u64,
    },

    // Persist a platform-lease expiration without converting it to cancel.
    PauseTurn {
        // Expected active session owner; this does not switch sessions.
        session_id: String,
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
    // Explicit Native initialize opt-in state for this session.
    InitializeProactivity {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proactivity: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reinitialize: Option<bool>,
    },
    // Change the Native proactivity level for subsequent requests.
    SetProactivityLevel {
        level: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offer_bypass: Option<bool>,
    },
    /// Ephemeral composer observations; independent of the native SDK stream.
    UpdatePromptSuggestionContext {
        session_id: String,
        focus: String,
        input_empty: bool,
        pending_permission: bool,
        elicitation_active: bool,
    },
    PromptSuggestionFeedback {
        session_id: String,
        uuid: String,
        action: String,
    },
    /// Admission of a command executed entirely by the attached frontend.
    RecordPromptSuggestionSubmission {
        text: String,
    },
    /// Manage scoped Fusion strategies. Appended to preserve native ordinals.
    FusionAdmin {
        command: FusionAdminCommandDto,
    },
    // Lightweight child-conversation lifecycle. Keep additions at the enum tail
    // so protocol-25 UniFFI ordinals remain stable.
    SideChatCreate {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_input: Option<ClientTextDto>,
    },
    SideChatList {},
    SideChatGet {
        side_id: String,
    },
    SideChatEnd {
        side_id: String,
    },
    SideChatSubmit {
        side_id: String,
        request_id: String,
        command_json: String,
    },
    /// Response to a host-private media connection negotiation request.
    MediaConnectionResponse {
        response: MediaConnectionResponseDto,
    },
    /// Response to one bounded native media device callback request.
    MediaDeviceResponse {
        response: MediaDeviceResponseDto,
    },
    /// Client request to execute/read/cancel against the shared app media service.
    MediaServiceRequest {
        request: MediaServiceRequestDto,
    },
    /// Response to a host-private peer audio barrier request.
    MediaControlResponse {
        response: MediaControlResponseDto,
    },
    /// Update explicit native duplex modes independently of legacy audio operations.
    UpdateMediaSessionModes {
        modes: Vec<MediaSessionModeDto>,
    },
    // Inspect one queued mobile turn without executing or resuming it. The
    // request id follows AttachTurn's nonempty, 128-byte maximum contract.
    // The host replies with a session/turn/request-correlated status.
    QueryQueuedTurn {
        session_id: String,
        turn_id: u64,
        request_id: String,
    },
}

impl From<ClientCommandWire> for ClientCommand {
    fn from(value: ClientCommandWire) -> Self {
        match value {
            ClientCommandWire::FusionAdmin { command } => Self::FusionAdmin { command },
            ClientCommandWire::SideChatCreate {
                request_id,
                initial_input,
            } => Self::SideChatCreate {
                request_id,
                initial_input,
            },
            ClientCommandWire::SideChatList {} => Self::SideChatList {},
            ClientCommandWire::SideChatGet { side_id } => Self::SideChatGet { side_id },
            ClientCommandWire::SideChatEnd { side_id } => Self::SideChatEnd { side_id },
            ClientCommandWire::SideChatSubmit {
                side_id,
                request_id,
                command_json,
            } => Self::SideChatSubmit {
                side_id,
                request_id,
                command_json,
            },
            ClientCommandWire::StartRealtimeAudio { request_json } => {
                Self::StartRealtimeAudio { request_json }
            }
            ClientCommandWire::RealtimeAudioInput { input_json } => {
                Self::RealtimeAudioInput { input_json }
            }
            ClientCommandWire::StopRealtimeAudio => Self::StopRealtimeAudio,
            ClientCommandWire::GetAudioSessionContext => Self::GetAudioSessionContext,
            ClientCommandWire::SendPrompt {
                session_id,
                text,
                prompt_mode,
                images,
                turn_id,
                visualization_context,
            } => Self::SendPrompt {
                session_id,
                text,
                prompt_mode,
                images,
                turn_id,
                visualization_context,
            },
            ClientCommandWire::Cancel {
                session_id,
                turn_id,
            } => Self::Cancel {
                session_id,
                turn_id,
            },
            ClientCommandWire::ApprovePermission {
                request_id,
                response,
            } => Self::ApprovePermission {
                request_id,
                response,
            },
            ClientCommandWire::DenyPermission { request_id } => Self::DenyPermission { request_id },
            ClientCommandWire::ApproveComputerAccess {
                request_id,
                response,
            } => Self::ApproveComputerAccess {
                request_id,
                response,
            },
            ClientCommandWire::DenyComputerAccess { request_id } => {
                Self::DenyComputerAccess { request_id }
            }
            ClientCommandWire::AnswerAskUserQuestion {
                request_id,
                answers,
            } => Self::AnswerAskUserQuestion {
                request_id,
                answers,
            },
            ClientCommandWire::CancelAskUserQuestion { request_id } => {
                Self::CancelAskUserQuestion { request_id }
            }
            ClientCommandWire::SetPermissionMode { mode } => Self::SetPermissionMode { mode },
            ClientCommandWire::ListProviderCredentials {
                operation_id,
                provider_ids,
                preview_provider_ids,
            } => Self::ListProviderCredentials {
                operation_id,
                provider_ids,
                preview_provider_ids,
            },
            ClientCommandWire::SetProviderCredential {
                operation_id,
                provider_id,
                credential,
            } => Self::SetProviderCredential {
                operation_id,
                provider_id,
                credential,
            },
            ClientCommandWire::DeleteProviderCredential {
                operation_id,
                provider_id,
            } => Self::DeleteProviderCredential {
                operation_id,
                provider_id,
            },
            ClientCommandWire::SetModel { model } => Self::SetModel { model },
            ClientCommandWire::ListModels => Self::ListModels,
            ClientCommandWire::RunSlashCommand {
                session_id,
                raw,
                images,
                turn_id,
            } => Self::RunSlashCommand {
                session_id,
                raw,
                images,
                turn_id,
            },
            ClientCommandWire::RefreshListings { which } => Self::RefreshListings { which },
            ClientCommandWire::ListSessionAgents => Self::ListSessionAgents,
            ClientCommandWire::LoadSessionAgentTranscript { agent_id } => {
                Self::LoadSessionAgentTranscript { agent_id }
            }
            ClientCommandWire::NewSession { cwd, model } => Self::NewSession { cwd, model },
            ClientCommandWire::ResumeSession { session_id, cwd } => {
                Self::ResumeSession { session_id, cwd }
            }
            ClientCommandWire::ListSessions { limit } => Self::ListSessions { limit },
            ClientCommandWire::Login => Self::Login,
            ClientCommandWire::Logout => Self::Logout,
            ClientCommandWire::ForceCompact => Self::ForceCompact,
            ClientCommandWire::ClearSession => Self::ClearSession,
            ClientCommandWire::TaskList {
                status_filter,
                request_id,
            } => Self::TaskList {
                status_filter,
                request_id,
            },
            ClientCommandWire::TaskOutput { task_id, offset } => {
                Self::TaskOutput { task_id, offset }
            }
            ClientCommandWire::TaskStop { task_id } => Self::TaskStop { task_id },
            ClientCommandWire::ResumeWorkflow { task_id } => Self::ResumeWorkflow { task_id },
            ClientCommandWire::RequestExit => Self::RequestExit,
            ClientCommandWire::GetConversationControls => Self::GetConversationControls,
            ClientCommandWire::SetReasoningSelection { selection } => {
                Self::SetReasoningSelection { selection }
            }
            ClientCommandWire::SetFastMode { enabled } => Self::SetFastMode { enabled },
            ClientCommandWire::SkillAdmin { command } => Self::SkillAdmin { command },
            ClientCommandWire::McpAdmin { command } => Self::McpAdmin { command },
            ClientCommandWire::PluginAdmin { command } => Self::PluginAdmin { command },
            ClientCommandWire::HookAdmin { command } => Self::HookAdmin { command },
            ClientCommandWire::UpdateSettings {
                destination,
                patch_json,
            } => Self::UpdateSettings {
                destination,
                patch_json,
            },
            ClientCommandWire::UpdatePermissionRules {
                destination,
                behavior,
                add,
                remove,
            } => Self::UpdatePermissionRules {
                destination,
                behavior,
                add,
                remove,
            },
            ClientCommandWire::SetDefaultPermissionMode { destination, mode } => {
                Self::SetDefaultPermissionMode { destination, mode }
            }
            ClientCommandWire::UpdateWorkspaceDirectories {
                destination,
                add,
                remove,
            } => Self::UpdateWorkspaceDirectories {
                destination,
                add,
                remove,
            },
            ClientCommandWire::UpsertMcpServer {
                scope,
                name,
                config_json,
            } => Self::UpsertMcpServer {
                scope,
                name,
                config_json,
            },
            ClientCommandWire::RemoveMcpServer { scope, name } => {
                Self::RemoveMcpServer { scope, name }
            }
            ClientCommandWire::AudioResponse { identity, result } => {
                Self::AudioResponse { identity, result }
            }
            ClientCommandWire::AttachTurn {
                session_id,
                turn_id,
                after_sequence,
                request_id,
            } => Self::AttachTurn {
                session_id,
                turn_id,
                after_sequence,
                request_id,
            },
            ClientCommandWire::ResumeTurn {
                session_id,
                turn_id,
            } => Self::ResumeTurn {
                session_id,
                turn_id,
            },
            ClientCommandWire::PauseTurn {
                session_id,
                turn_id,
                reason,
            } => Self::PauseTurn {
                session_id,
                turn_id,
                reason,
            },
            ClientCommandWire::SetTypescriptLspMode { mode } => Self::SetTypescriptLspMode { mode },
            ClientCommandWire::ForkSession {
                session_id,
                target_mode,
            } => Self::ForkSession {
                session_id,
                target_mode,
            },
            ClientCommandWire::TestProviderConnection {
                operation_id,
                provider_id,
                api_base,
                model,
                credential_override,
            } => Self::TestProviderConnection {
                operation_id,
                provider_id,
                api_base,
                model,
                credential_override,
            },
            ClientCommandWire::CronManage {
                request_id,
                request,
            } => Self::CronManage {
                request_id,
                request,
            },
            ClientCommandWire::TaskMessage { task_id, message } => {
                Self::TaskMessage { task_id, message }
            }
            ClientCommandWire::CronRunStarted { run_id, session_id } => {
                Self::CronRunStarted { run_id, session_id }
            }
            ClientCommandWire::ScheduledRunTurn {
                run_id,
                prompt,
                model,
                reasoning,
            } => Self::ScheduledRunTurn {
                run_id,
                prompt,
                model,
                reasoning,
            },
            ClientCommandWire::CronRunCompleted {
                run_id,
                session_id,
                summary,
                error,
            } => Self::CronRunCompleted {
                run_id,
                session_id,
                summary,
                error,
            },
            ClientCommandWire::UpdateAudioCapabilities { capabilities } => {
                Self::UpdateAudioCapabilities { capabilities }
            }
            ClientCommandWire::UiAttach { surface, client_id } => {
                Self::UiAttach { surface, client_id }
            }
            ClientCommandWire::UiDetach { client_id } => Self::UiDetach { client_id },
            ClientCommandWire::UiRender {
                request_id,
                request_json,
            } => Self::UiRender {
                request_id,
                request_json,
            },
            ClientCommandWire::UiClientModule { request_id, plugin } => {
                Self::UiClientModule { request_id, plugin }
            }
            ClientCommandWire::UiMessage {
                request_id,
                request_json,
            } => Self::UiMessage {
                request_id,
                request_json,
            },
            ClientCommandWire::UiClientFault {
                request_id,
                request_json,
            } => Self::UiClientFault {
                request_id,
                request_json,
            },
            ClientCommandWire::UiClientPress {
                request_id,
                request_json,
            } => Self::UiClientPress {
                request_id,
                request_json,
            },
            ClientCommandWire::UiPress {
                request_id,
                request_json,
            } => Self::UiPress {
                request_id,
                request_json,
            },
            ClientCommandWire::UiInput {
                request_id,
                request_json,
            } => Self::UiInput {
                request_id,
                request_json,
            },
            ClientCommandWire::UiSelect {
                request_id,
                request_json,
            } => Self::UiSelect {
                request_id,
                request_json,
            },
            ClientCommandWire::UiClientOperation {
                request_id,
                operation_json,
            } => Self::UiClientOperation {
                request_id,
                operation_json,
            },
            ClientCommandWire::InitializeProactivity {
                proactivity,
                reinitialize,
            } => Self::InitializeProactivity {
                proactivity,
                reinitialize,
            },
            ClientCommandWire::SetProactivityLevel {
                level,
                offer_bypass,
            } => Self::SetProactivityLevel {
                level,
                offer_bypass,
            },
            ClientCommandWire::UpdatePromptSuggestionContext {
                session_id,
                focus,
                input_empty,
                pending_permission,
                elicitation_active,
            } => Self::UpdatePromptSuggestionContext {
                session_id,
                focus,
                input_empty,
                pending_permission,
                elicitation_active,
            },
            ClientCommandWire::PromptSuggestionFeedback {
                session_id,
                uuid,
                action,
            } => Self::PromptSuggestionFeedback {
                session_id,
                uuid,
                action,
            },
            ClientCommandWire::RecordPromptSuggestionSubmission { text } => {
                Self::RecordPromptSuggestionSubmission { text }
            }
            ClientCommandWire::MediaConnectionResponse { response } => {
                Self::MediaConnectionResponse { response }
            }
            ClientCommandWire::MediaDeviceResponse { response } => {
                Self::MediaDeviceResponse { response }
            }
            ClientCommandWire::MediaServiceRequest { request } => {
                Self::MediaServiceRequest { request }
            }
            ClientCommandWire::MediaControlResponse { response } => {
                Self::MediaControlResponse { response }
            }
            ClientCommandWire::UpdateMediaSessionModes { modes } => {
                Self::UpdateMediaSessionModes { modes }
            }
            ClientCommandWire::QueryQueuedTurn {
                session_id,
                turn_id,
                request_id,
            } => Self::QueryQueuedTurn {
                session_id,
                turn_id,
                request_id,
            },
        }
    }
}
struct RawEnvelopeFields(HashMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for RawEnvelopeFields {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawObjectVisitor;

        impl<'de> serde::de::Visitor<'de> for RawObjectVisitor {
            type Value = RawEnvelopeFields;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object with raw member values")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut fields = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    // Match JSON.parse's last-member-wins behavior while
                    // capturing each value before recursively decoding it.
                    fields.insert(key, map.next_value::<Box<RawValue>>()?);
                }
                Ok(RawEnvelopeFields(fields))
            }
        }

        deserializer.deserialize_map(RawObjectVisitor)
    }
}

fn command_raw_required<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    serde_json::from_str(raw.get()).map_err(E::custom)
}

fn command_raw_optional<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    match fields.remove(name) {
        Some(raw) => serde_json::from_str::<Option<T>>(raw.get()).map_err(E::custom),
        None => Ok(None),
    }
}

fn command_raw_wire_json<E>(tag: &str, fields: HashMap<String, Box<RawValue>>) -> Result<String, E>
where
    E: serde::de::Error,
{
    let mut json = String::from("{");
    json.push_str(&serde_json::to_string(tag).map_err(E::custom)?);
    json.push_str(":{");
    let mut first = true;
    for (key, value) in fields {
        if !first {
            json.push(',');
        }
        first = false;
        json.push_str(&serde_json::to_string(&key).map_err(E::custom)?);
        json.push(':');
        json.push_str(value.get());
    }
    json.push_str("}}");
    Ok(json)
}

impl<'de> Deserialize<'de> for ClientCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawEnvelopeFields(mut fields) = RawEnvelopeFields::deserialize(deserializer)?;
        let tag = command_raw_required::<String, D::Error>(&mut fields, "type")?;
        match tag.as_str() {
            // Unit variants have no raw payload to decode.
            "stop_realtime_audio" => Ok(Self::StopRealtimeAudio),
            "get_audio_session_context" => Ok(Self::GetAudioSessionContext),
            "list_models" => Ok(Self::ListModels),
            "list_session_agents" => Ok(Self::ListSessionAgents),
            "login" => Ok(Self::Login),
            "logout" => Ok(Self::Logout),
            "force_compact" => Ok(Self::ForceCompact),
            "clear_session" => Ok(Self::ClearSession),
            "request_exit" => Ok(Self::RequestExit),
            "get_conversation_controls" => Ok(Self::GetConversationControls),
            // These prompt fields are Source text. Decode their raw member
            // tokens directly so serde's internally-tagged Content buffer
            // cannot normalize isolated UTF-16 surrogates first.
            "send_prompt" => Ok(Self::SendPrompt {
                session_id: command_raw_optional(&mut fields, "session_id")?,
                text: command_raw_required(&mut fields, "text")?,
                prompt_mode: command_raw_optional(&mut fields, "prompt_mode")?,
                images: command_raw_required(&mut fields, "images")?,
                turn_id: command_raw_optional(&mut fields, "turn_id")?,
                visualization_context: command_raw_optional(&mut fields, "visualization_context")?,
            }),
            "run_slash_command" => Ok(Self::RunSlashCommand {
                session_id: command_raw_optional(&mut fields, "session_id")?,
                raw: command_raw_required(&mut fields, "raw")?,
                images: command_raw_required(&mut fields, "images")?,
                turn_id: command_raw_optional(&mut fields, "turn_id")?,
            }),
            // Skill ids are source text. Decode the nested DTO from its raw
            // member so ClientTextDto sees the original JSON string token.
            "skill_admin" => Ok(Self::SkillAdmin {
                command: command_raw_required(&mut fields, "command")?,
            }),
            "answer_ask_user_question" => Ok(Self::AnswerAskUserQuestion {
                request_id: command_raw_required(&mut fields, "request_id")?,
                answers: command_raw_required(&mut fields, "answers")?,
            }),
            _ => {
                let json = command_raw_wire_json::<D::Error>(&tag, fields)?;
                let wire: ClientCommandWire =
                    serde_json::from_str(&json).map_err(serde::de::Error::custom)?;
                Ok(wire.into())
            }
        }
    }
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

/// An inline image attachment for SendPrompt or RunSlashCommand. Uniform inline
/// `{media_type, base64}` on the wire (no transport-leaking enum, decision
/// §0.8): the adapter writes the inline bytes to a temp file on the engine host
/// for the path-based engine entry. Mobile slash dispatch retains this DTO in
/// its durable checkpoint and restores it with the expanded prompt batch.
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
