//! `ClientEvent` DTOs — outbound events the engine streams to a client.
//!
//! The `Error` variant + `ErrorKindDto` land in F1-01 (the first variant, so
//! every later DTO inherits the frozen serde conventions). The live-turn
//! streaming variants land in F1-03.
//!
//! Frozen serde conventions (decision §0.1):
//! - internally tagged: `#[serde(tag = "type", rename_all = "snake_case")]`
//!   (matches `lingxi_core::types::ContentBlock` / api-client `StreamEvent`),
//! - top-level enum is `#[non_exhaustive]` (mirrors `lingxi_core::host::OutputEvent`),
//! - every optional field uses
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]`.
//!
//! Tool payloads are JSON **Strings** (`input_json`/`result_json`); the
//! `serde_json::Value` lowering happens in `client::adapter`, NOT here
//! (decision §0.4).

use crate::protocol::ask_user_question::AskUserQuestionRequestDto;
use crate::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioOperationIdDto, AudioOperationRequestDto,
};
use crate::protocol::controls::ConversationControlsDto;
use crate::protocol::listings::{
    AgentDto, AuthStateDto, ConfigurationDomainDto, ConfigurationEffectDto,
    ConfigurationOperationStatusDto, CoordinatorWorkerDto, DoctorReportDto, HookDto, McpServerDto,
    MemoryEntryDto, ModelDetailsDto, ProviderModelCatalogEntryDto, SessionAgentMessageRowDto,
    SessionAgentSummaryDto, SessionModeDto, SessionRowDto, SkillDto, SlashCommandDto,
    StatusSnapshotDto, TaskRowDto, TaskStatusDto,
};
use crate::protocol::message::MessageDto;
use crate::protocol::permission::PermissionResolutionDto;
use crate::protocol::text::ClientTextDto;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// Host-private Codex session. Never forward this payload to a renderer.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(deny_unknown_fields)]
pub struct OpenAiOAuthSessionDto {
    /// OAuth bearer token.
    pub access_token: String,
    /// Rotating refresh token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Expiration in Unix seconds.
    pub expires_at: u64,
    /// `ChatGPT` account identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Government account routing flag.
    #[serde(default)]
    pub fedramp: bool,
    /// Signed-in email, for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl std::fmt::Debug for OpenAiOAuthSessionDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiOAuthSessionDto")
            .finish_non_exhaustive()
    }
}

// Outbound events the engine streams to a client.
//
// Each live-turn variant notes its engine source (from the area maps).
// `ThinkingDelta` and `UsageUpdate` are now LIVE-FED by the §0.7 "light up
// thinking/usage" follow-up: `event_router` emits them via
// `OutputStream::emit_thinking` / `emit_usage`. The remaining reserved
// variants (e.g. `CoordinatorStatus`) are defined so the contract freezes now
// but still have no live engine source in the foundation (decisions §0.7 /
// §0.9) and round-trip only.
// Boxing the app payload would change the generated Swift/Kotlin protocol API.
#[allow(clippy::large_enum_variant)]
// UniFFI 0.32.2 stores an enum's variant/field documentation in the same
// fixed-size metadata buffer as its wire schema. Keep this high-cardinality
// envelope undocumented at the derive site; the payload DTOs and protocol
// snapshots remain the source of API documentation without risking a build
// failure when an additive event is introduced.
macro_rules! define_client_event {
    ($($variants:tt)*) => {
        #[doc = "Outbound events the engine streams to a client."]
        #[allow(clippy::large_enum_variant)]
        #[allow(missing_docs)]
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[non_exhaustive]
        pub enum ClientEvent { $($variants)* }

        #[allow(clippy::large_enum_variant)]
        #[derive(Deserialize)]
        // Deserialize through an externally tagged private carrier. The
        // public `ClientEvent` remains internally tagged for serialization,
        // while this carrier avoids serde's internally-tagged `Content`
        // buffering, which cannot deserialize `RawValue`-backed text fields.
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum ClientEventWire { $($variants)* }

    };
}

define_client_event! {

    /// Native realtime JSON with authoritative current-session identity.
    RealtimeAudioEvent {
        session_id: String,
        event_json: String,
    },
    /// Authoritative session selection for host-owned, follow-session audio.
    AudioSessionContext {
        session_id: String,
        profile_id: String,
        account_scope: String,
    },
    Error {
        kind: ErrorKindDto,
        message: ClientTextDto,
    },

    SystemNotice {
        message: ClientTextDto,
        is_error: bool,
    },

    AskUserQuestion {
        request: AskUserQuestionRequestDto,
    },

    AskUserQuestionResolved {
        request_id: u64,
    },

    PermissionRequestResolved {
        request_id: u64,
        resolution: PermissionResolutionDto,
    },

    // ── Live-turn streaming events (F1-03) ────────────────────────────────
    TextDelta {
        text: ClientTextDto,
    },

    ToolUseStarted {
        id: String,
        tool: String,
        input_json: String,
        header: crate::protocol::tool_display::ToolHeaderDto,
    },

    ToolHeartbeat {
        id: String,
        tool: String,
        elapsed_ms: u64,
    },

    ToolUseResult {
        id: String,
        tool: String,
        result_json: String,
        is_error: bool,
        display: crate::protocol::tool_display::ToolResultDisplayDto,
    },

    MessageComplete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<MessageDto>,
    },

    TurnStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
    },

    TurnEnded {
        outcome: TurnOutcomeDto,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        cost: CostDto,
    },

    CostUpdate {
        total_usd: f64,
        input_tokens: u64,
        output_tokens: u64,
        api_calls: u32,
        session_duration_secs: u64,
        formatted: String,
    },

    CompactionCompleted {
        messages_before: u32,
        messages_after: u32,
        bytes_saved: u64,
        #[serde(default)]
        summary: ClientTextDto,
    },

    // ── Listing / screen events (F1-05) ───────────────────────────────────
    //
    // The pull/reply payloads for every client screen. The supporting row /
    // payload structs live in `crate::protocol::listings`. Name reconciliation (plan
    // line 149): the design spec §4.1 says `AgentList`, but the WIRE name is
    // `Agents`.
    SessionStarted {
        session_id: String,
        mode: SessionModeDto,
    },

    SessionEnded,

    WorkspaceChanged {
        state: crate::protocol::listings::WorkspaceStateDto,
    },

    SessionResumed {
        session_id: String,
        mode: SessionModeDto,
        messages: Vec<MessageDto>,
    },

    SessionAgentList {
        session_id: String,
        agents: Vec<SessionAgentSummaryDto>,
    },

    SessionAgentTranscript {
        session_id: String,
        agent_id: String,
        messages: Vec<SessionAgentMessageRowDto>,
        next_message_index: u64,
        revision: u64,
    },

    SessionAgentUpdated {
        session_id: String,
        agent: SessionAgentSummaryDto,
    },

    SessionAgentMessage {
        session_id: String,
        agent_id: String,
        #[serde(flatten)]
        row: SessionAgentMessageRowDto,
    },

    SessionAgentTombstone {
        session_id: String,
        agent_id: String,
        message_uuid: String,
        display_only: bool,
    },

    SessionList {
        sessions: Vec<SessionRowDto>,
    },

    ModelList {
        models: Vec<String>,
        current: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_model_source: Option<ClientTextDto>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        details: Vec<ModelDetailsDto>,
    },

    ModelChanged {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_source: Option<ClientTextDto>,
    },

    PermissionModeChanged {
        mode: String,
    },

    ProviderCredentialStatus {
        operation_id: u64,
        configured_provider_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unavailable_provider_ids: Vec<String>,
        storage_encrypted: bool,
        // Display-safe credential previews keyed by provider id. Values are
        // fixed masks plus at most the final four characters; plaintext
        // credentials never cross the client protocol.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        credential_previews: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    McpServers {
        servers: Vec<McpServerDto>,
    },

    Hooks {
        hooks: Vec<HookDto>,
    },

    Agents {
        agents: Vec<AgentDto>,
    },

    SlashCommandCatalog {
        commands: Vec<SlashCommandDto>,
    },

    SlashCommandResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        display: ClientTextDto,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },

    MemoryEntries {
        entries: Vec<MemoryEntryDto>,
    },

    StatusSnapshot {
        snapshot: StatusSnapshotDto,
    },

    // The layered settings read path (`RefreshListings{Settings}`).
    //
    // Every structured payload here is a JSON **String**, not a nested
    // object: `serde_json::Value` must never enter this crate (decision
    // §0.4 — `Value` is not UniFFI-representable), so the bridge lowers
    // each map to a string exactly the way `ToolUseStarted.input_json`
    // does.
    //
    // `effective_json` is `{key: value}` after the merge; `provenance_json`
    // is `{key: layer}` naming which layer each merged value came from.
    // The three optional fields were ADDED to this variant (additive under
    // decision §0.10 — no major bump): a client that predates them keeps
    // reading the two required payloads unchanged.
    SettingsSnapshot {
        // `{key: value}` — the merged effective settings.
        effective_json: String,
        // `{key: layer}` — which layer each effective value came from.
        provenance_json: String,
        // `[{layer, path, exists, parsed, parse_error?}]` — the on-disk
        // state of every settings file layer, so the UI can show which file
        // backs a layer and whether it parsed. `parsed` reports JSON
        // validity only; it says nothing about OS write permission.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files_json: Option<String>,
        // `{key: value}` — the file-layer and managed values loaded at boot,
        // updated for keys successfully hot-reloaded. No `cli` or `env`
        // overlay is applied; those affect the effective view separately.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_json: Option<String>,
        // Keys an administrator pinned through the managed-settings layer;
        // the UI must not offer to edit these.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locked: Option<Vec<String>>,
        // `{layer: {key: value}}` — each FILE layer's OWN raw settings map,
        // unmerged. `effective_json` is a cross-layer merge and `active_json`
        // includes the managed overlay; NEITHER can
        // stand in for "what does layer L's file itself say", which a
        // layered editor needs before it writes back to one layer: the
        // generic `update_settings` command replaces a key WHOLESALE in one
        // layer's file (`migrations/src/settings_update.rs`'s "top-level
        // REPLACE, not deep-merge" contract), so pre-merging a write against
        // the cross-layer `effective_json` view — which can carry another
        // layer's entries for an object-valued key like `providers` — would
        // silently fork that other layer's data into the one being saved.
        // Additive under decision §0.10 — no major bump.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layers_json: Option<String>,
        // The keys in `effective_json` whose value is a CROSS-LAYER union
        // rather than one layer's value. The engine deep-merges or
        // concat-dedups a specific set of keys (`hooks`, `permissions`,
        // `providers`, `enabledPlugins`, `trustedDirectories`, … — its
        // `settings::schema::MERGE_STRATEGIES` table), so when more than one
        // layer contributes, the effective value belongs to no single layer
        // and `provenance_json`'s entry for that key names only the
        // highest-priority CONTRIBUTOR. A client must therefore not render a
        // single-layer provenance badge for a key listed here; it says the
        // value is merged across layers instead.
        //
        // Only keys the merge actually unioned are listed: a deep-merge key
        // whose entries the winning layer entirely redefines is absent,
        // because for that key the winning layer's badge is honest. A list
        // of every key both layers mention would be useless.
        //
        // Additive under decision §0.10 — no major bump.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merged_keys: Option<Vec<String>>,
    },

    AuthState {
        state: AuthStateDto,
    },

    DoctorReport {
        report: DoctorReportDto,
    },

    TaskRow {
        task: TaskRowDto,
    },

    TaskOutputChunk {
        task_id: String,
        content: String,
        total_lines: u64,
        truncated: bool,
    },

    TaskStatusChanged {
        task_id: String,
        status: TaskStatusDto,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<String>,
        // Failure reason accompanying a `failed` transition, when the
        // producing handler reported one. APPENDED field (additive default
        // `None`) — clients render it instead of a bare task id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    CommandsChanged {
        commands: Vec<SlashCommandDto>,
    },

    // ── Live thinking/usage (§0.7 follow-up) + reserved (§0.9) ────────────
    CoordinatorStatus {
        active_workers: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team: Option<String>,
    },

    CoordinatorWorker {
        worker: CoordinatorWorkerDto,
    },

    ThinkingDelta {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    UsageUpdate {
        // Complete restored counters, including valid zero values; absent for live deltas.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_snapshot: Option<bool>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    },

    Attachment {
        attachment: AttachmentDto,
    },

    ApiRetry {
        message: String,
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
    },

    // ⚠️ APPEND NEW VARIANTS BELOW THIS LINE, NEVER ABOVE IT.
    // UniFFI lowers this enum by 1-based ORDINAL, so inserting a variant
    // anywhere but the end silently shifts every later one and both mobile
    // clients mis-decode every event — with no compile error anywhere.
    PlanUpdated {
        tasks: Vec<crate::protocol::tool_display::PlanTaskDto>,
    },

    // A paused workflow was replaced by a newly launched resumed run.
    WorkflowResumed {
        previous_task_id: String,
        task: TaskRowDto,
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<String>,
    },

    ConversationControlsChanged {
        controls: ConversationControlsDto,
    },

    FastModeChanged {
        enabled: bool,
    },

    // Ask a client to perform one host-owned audio operation.
    AudioRequest {
        // Trusted owner, operation identity, budget, and requested operation.
        request: AudioOperationRequestDto,
    },
    // Authoritative durable state for one mobile turn. Emitted on attach,
    // resume, recovery gating, and every terminal transition.
    TurnRecoveryState {
        snapshot: TurnRecoverySnapshotDto,
    },

    // Sequenced retained copy of a turn event. It is emitted beside live
    // delivery and replayed after
    // [`ClientCommand::AttachTurn`](crate::protocol::commands::ClientCommand::AttachTurn).
    // `event_json` is the original serialized `ClientEvent`; keeping it a
    // string avoids a recursive UniFFI enum while preserving the exact wire
    // payload for future SSE/WebSocket transports.
    TurnEventReplay {
        session_id: String,
        turn_id: u64,
        sequence: u64,
        event_json: String,
    },

    Skills {
        skills: Vec<SkillDto>,
    },

    // Authoritative global TypeScript LSP policy. `effective` is `off` when
    // the pinned runtime is unavailable even if the persisted request is
    // `auto` or `on`.
    TypescriptLspModeChanged {
        requested: String,
        effective: String,
        available: bool,
    },

    // A context-preserving copy created in another session capability mode.
    // Kept at the end so existing UniFFI event ordinals remain stable.
    SessionForked {
        source_session_id: String,
        session_id: String,
        mode: SessionModeDto,
    },

    ProviderConnectionTested {
        operation_id: u64,
        provider_id: String,
        connected: bool,
        reachable: bool,
        authenticated: bool,
        model_available: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
        latency_ms: u64,
        message: String,
        used_stored_credential: bool,
    },

    ConfigurationOperation {
        domain: ConfigurationDomainDto,
        operation_id: u64,
        status: ConfigurationOperationStatusDto,
        effect: ConfigurationEffectDto,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details_json: Option<String>,
    },

    SkillCatalog {
        catalog_json: String,
    },

    SkillDocument {
        document_json: String,
    },

    McpConfigurationSnapshot {
        snapshot_json: String,
    },

    PluginCatalog {
        catalog_json: String,
    },

    ProviderModelCatalog {
        providers: Vec<ProviderModelCatalogEntryDto>,
    },

    // Observed compaction lifecycle, including attempts that fail or are cancelled.
    // Connection-scoped so an idle manual `/compact` can publish live progress.
    // Appended to preserve every existing UniFFI enum ordinal.
    CompactionStatus {
        // `preparing`, `summarizing`, `restoring`, `complete`, `skipped`, `error`, or `cancelled`.
        // A string allows clients to ignore future phases without decode failures.
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    // Authoritative scheduled task snapshot after a management operation.
    CronResult {
        request_id: String,
        jobs: Vec<CronJobDto>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    // Retract the current or just-completed rejected assistant attempt.
    MessageRetracted {
        message_id: String,
    },
    // Correlate the streamed assistant response with its stable identity.
    MessageIdentity {
        message_id: String,
    },

    // A `/loop` wakeup fired. `message` is the resume line, already rendered
    // host-side so its wording lives in one place.
    //
    // `streak` is how many consecutive QUIET ticks preceded it: `0` for an
    // ordinary wakeup, `N > 0` when the client folds the latest available
    // wakeup-to-tail slice, unions prior folded IDs, and shows `companion`.
    // `N` is the cumulative display count, not required local history depth.
    // Every wakeup carries this event — including `streak: 0` — so a client
    // can mark the group boundary without matching on the copy.
    //
    // Appended LAST: per §0.10 an additive event that moves no existing
    // native ordinal keeps `CLIENT_PROTOCOL_VERSION` and updates the contract
    // index only.
    LoopWakeup {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        companion: Option<String>,
        streak: u32,
        since_ms: u64,
    },

    // Additive SDK task lifecycle receipt. Keep new variants after the
    // frozen prefix so existing UniFFI variant ordinals remain unchanged.
    TaskLifecycle {
        event_json: String,
    },
    #[serde(rename = "openai_oauth_updated")]
    OpenAiOAuthUpdated {
        session: OpenAiOAuthSessionDto,
    },
    CronRunBound {
        run_id: String,
        error: Option<String>,
    },
    ScheduledRunFinished {
        run_id: String,
        summary: Option<String>,
        error: Option<String>,
    },
    // Execute a claimed scheduled run in a host-managed session.
    CronRunRequested {
        run_id: String,
        task: CronJobDto,
    },

    // Session-level fixed-schedule fire notice, emitted before TurnStarted.
    // Unlike LoopWakeup this carries no dynamic-loop fold boundary.
    // Appended to preserve every existing native enum ordinal.
    ScheduledTaskFire {
        message: String,
    },
    // Only emitted for a TaskList carrying request_id. Append-only for UniFFI.
    TaskListComplete {
        request_id: String,
        active_count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    // Cancel one pending audio operation without affecting other owners.
    AudioCancel {
        identity: AudioOperationIdDto,
    },
    // Publish the latest support/readiness snapshot to the UI client.
    AudioCapabilitiesChanged {
        capabilities: AudioCapabilitySnapshotDto,
    },
    // Native accepted-fallback route receipt.
    QueryModelChange {
        // Model selected by the accepted response.
        to_model: String,
    },
    // Host/client-only transient assistant-block key. Not a native SDK event.
    AssistantBlockStart {
        // Opaque key used to associate streamed deltas until stop-time identity.
        block_key: u64,
    },
    // Host/client-only mapping to the UUID assigned at block completion.
    // Not a native SDK event.
    AssistantBlockIdentity {
        // Opaque key previously emitted by `assistant_block_start`.
        block_key: u64,
        // Completed row UUID.
        message_uuid: String,
    },
    // Native accepted-fallback row tombstone.
    Tombstone {
        // Complete row envelope available at the host boundary.
        message: ServerFallbackTombstoneMessageDto,
        // Native `displayOnly` flag. Native consumers may still remove the
        // row from durable transcript storage when this is true.
        display_only: bool,
    },
    // Native refusal-continuation event. The display flag is a host/client
    // extension used by display-hook buffering and is not a native field.
    RefusalContinuation {
        // Native phase. Accepted fallback emits `Begin`.
        phase: RefusalContinuationPhaseDto,
        // Retained text passed through without modification.
        salvage_text: String,
        // Native concatenation rule. Accepted fallback emits `Exact`.
        join: RefusalContinuationJoinDto,
        // Original row UUIDs in native source order.
        replaces_uuids: Vec<String>,
        // Host/client display-hook policy extension; not in the native event.
        display_salvage_text: bool,
    },
    // A TUI user-row token resolved to its persisted JSONL UUID.
    // Appended to preserve existing UniFFI variant ordinals.
    UserTranscriptRowIdentity {
        // Internal token assigned before the persistence call completes.
        row_token: String,
        // UUID returned by the successful JSONL append.
        uuid: String,
    },
    // Persisted top-level JSONL UUIDs for text blocks in one assistant response.
    // Appended to preserve existing UniFFI variant ordinals.
    AssistantTranscriptRowUuids {
        // Internal response key used only to locate its rendered cells.
        message_id: String,
        // One slot per persisted text block; failed appends are `None`.
        uuids: Vec<Option<String>>,
    },
    // One plain-text `$.ui.log` line; never model-facing conversation text.
    // Appended to preserve existing UniFFI variant ordinals.
    UiLog {
        plugin: String,
        text: ClientTextDto,
    },
    // Transient `$.ui.toast` notification; never transcript or model text.
    // Appended to preserve existing UniFFI variant ordinals.
    UiToast {
        plugin: String,
        text: ClientTextDto,
        timeout_ms: u64,
    },
    // Per-plugin pinned status; null text clears it. Never transcript content.
    UiStatus {
        plugin: String,
        text: Option<ClientTextDto>,
    },
    // Correlated reply to one Mod UI control command or local UI operation.
    UiControlResult {
        // Client-command correlator; it is not part of the Native payload.
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        // Canonical control or host-local operation response JSON.
        response_json: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        // Host-only renderer metadata sidecar, separate from `response_json`.
        metadata_json: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        // Correlated host/control failure; omitted on success.
        error: Option<String>,
    },
    // Rendered client-module frame forwarded to the owning Product bridge.
    UiClientFrame {
        runtime_id: String,
        frame_json: String,
    },
    // Native system invalidation. The event name is fixed by the source at the
    // Product bridge boundary; `instances_json` preserves its targeted sites.
    UiInvalidate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instances_json: Option<String>,
        uuid: String,
        session_id: String,
    },
    // Structured API failure metadata for the terminal assistant turn.
    // This is a host/client event, not a Native provider output frame.
    // Appended to preserve existing UniFFI variant ordinals.
    TerminalApiError {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_error_status: Option<u16>,
        terminal_reason: String,
        stop_reason: String,
    },
    // A transient Mod notification without a duration/expiry field.
    // Appended to preserve every existing UniFFI event ordinal.
    UiNotify {
        plugin: String,
        text: ClientTextDto,
    },
    // Inline visualization slot in the live assistant text. Appended to
    // preserve existing UniFFI variant ordinals.
    VisualizationBlock {
        status: crate::protocol::message::VisualizationBlockStatusDto,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<crate::protocol::message::VisualizationRefDto>,
    },
    PromptSuggestion {
        suggestion: ClientTextDto,
        uuid: String,
        session_id: String,
    },
    /// Composer state for attached hosts; never emitted on the native SDK stream.
    PromptSuggestionState {
        enabled: bool,
        notice: Option<String>,
    },
    /// Scoped Fusion strategy catalog, including optimistic write revisions.
    FusionCatalog {
        catalog_json: String,
    },
    /// Authoritative selection captured before credential preparation.
    FusionStrategyResolved {
        operation_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection_json: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    // Lightweight child-conversation lifecycle events, appended to preserve
    // every protocol-25 UniFFI event ordinal.
    SideChatCreated {
        request_id: String,
        side_id: String,
    },
    SideChatList {
        side_ids: Vec<String>,
    },
    SideChatSnapshot {
        side_id: String,
        events_json: Vec<String>,
    },
    SideChatEvent {
        side_id: String,
        event_json: String,
    },
    SideChatPermission {
        side_id: String,
        request: crate::protocol::permission::PermissionRequest,
    },
    SideChatSubmitted {
        side_id: String,
        request_id: String,
    },
    SideChatEnded {
        side_id: String,
    },
    SideChatError {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        side_id: Option<String>,
        message: String,
    },
}

impl From<ClientEventWire> for ClientEvent {
    fn from(value: ClientEventWire) -> Self {
        match value {
            ClientEventWire::FusionCatalog { catalog_json } => Self::FusionCatalog { catalog_json },
            ClientEventWire::FusionStrategyResolved {
                operation_id,
                selection_json,
                error,
            } => Self::FusionStrategyResolved {
                operation_id,
                selection_json,
                error,
            },
            ClientEventWire::SideChatCreated {
                request_id,
                side_id,
            } => Self::SideChatCreated {
                request_id,
                side_id,
            },
            ClientEventWire::SideChatList { side_ids } => Self::SideChatList { side_ids },
            ClientEventWire::SideChatSnapshot {
                side_id,
                events_json,
            } => Self::SideChatSnapshot {
                side_id,
                events_json,
            },
            ClientEventWire::SideChatEvent {
                side_id,
                event_json,
            } => Self::SideChatEvent {
                side_id,
                event_json,
            },
            ClientEventWire::SideChatPermission { side_id, request } => {
                Self::SideChatPermission { side_id, request }
            }
            ClientEventWire::SideChatSubmitted {
                side_id,
                request_id,
            } => Self::SideChatSubmitted {
                side_id,
                request_id,
            },
            ClientEventWire::SideChatEnded { side_id } => Self::SideChatEnded { side_id },
            ClientEventWire::SideChatError {
                request_id,
                side_id,
                message,
            } => Self::SideChatError {
                request_id,
                side_id,
                message,
            },
            ClientEventWire::CoordinatorStatus {
                active_workers,
                team,
            } => Self::CoordinatorStatus {
                active_workers,
                team,
            },
            ClientEventWire::RealtimeAudioEvent {
                session_id,
                event_json,
            } => Self::RealtimeAudioEvent {
                session_id,
                event_json,
            },
            ClientEventWire::AudioSessionContext {
                session_id,
                profile_id,
                account_scope,
            } => Self::AudioSessionContext {
                session_id,
                profile_id,
                account_scope,
            },
            ClientEventWire::Error { kind, message } => Self::Error { kind, message },
            ClientEventWire::TerminalApiError {
                api_error_status,
                terminal_reason,
                stop_reason,
            } => Self::TerminalApiError {
                api_error_status,
                terminal_reason,
                stop_reason,
            },
            ClientEventWire::SystemNotice { message, is_error } => {
                Self::SystemNotice { message, is_error }
            }
            ClientEventWire::AskUserQuestion { request } => Self::AskUserQuestion { request },
            ClientEventWire::AskUserQuestionResolved { request_id } => {
                Self::AskUserQuestionResolved { request_id }
            }
            ClientEventWire::PermissionRequestResolved {
                request_id,
                resolution,
            } => Self::PermissionRequestResolved {
                request_id,
                resolution,
            },
            ClientEventWire::TextDelta { text } => Self::TextDelta { text },
            ClientEventWire::ToolUseStarted {
                id,
                tool,
                input_json,
                header,
            } => Self::ToolUseStarted {
                id,
                tool,
                input_json,
                header,
            },
            ClientEventWire::ToolHeartbeat {
                id,
                tool,
                elapsed_ms,
            } => Self::ToolHeartbeat {
                id,
                tool,
                elapsed_ms,
            },
            ClientEventWire::ToolUseResult {
                id,
                tool,
                result_json,
                is_error,
                display,
            } => Self::ToolUseResult {
                id,
                tool,
                result_json,
                is_error,
                display,
            },
            ClientEventWire::MessageComplete {
                stop_reason,
                message,
            } => Self::MessageComplete {
                stop_reason,
                message,
            },
            ClientEventWire::TurnStarted { turn_id } => Self::TurnStarted { turn_id },
            ClientEventWire::TurnEnded {
                outcome,
                stop_reason,
                cost,
            } => Self::TurnEnded {
                outcome,
                stop_reason,
                cost,
            },
            ClientEventWire::CostUpdate {
                total_usd,
                input_tokens,
                output_tokens,
                api_calls,
                session_duration_secs,
                formatted,
            } => Self::CostUpdate {
                total_usd,
                input_tokens,
                output_tokens,
                api_calls,
                session_duration_secs,
                formatted,
            },
            ClientEventWire::CompactionCompleted {
                messages_before,
                messages_after,
                bytes_saved,
                summary,
            } => Self::CompactionCompleted {
                messages_before,
                messages_after,
                bytes_saved,
                summary,
            },
            ClientEventWire::SessionStarted { session_id, mode } => {
                Self::SessionStarted { session_id, mode }
            }
            ClientEventWire::SessionEnded => Self::SessionEnded,
            ClientEventWire::WorkspaceChanged { state } => Self::WorkspaceChanged { state },
            ClientEventWire::SessionResumed {
                session_id,
                mode,
                messages,
            } => Self::SessionResumed {
                session_id,
                mode,
                messages,
            },
            ClientEventWire::SessionAgentList { session_id, agents } => {
                Self::SessionAgentList { session_id, agents }
            }
            ClientEventWire::SessionAgentTranscript {
                session_id,
                agent_id,
                messages,
                next_message_index,
                revision,
            } => Self::SessionAgentTranscript {
                session_id,
                agent_id,
                messages,
                next_message_index,
                revision,
            },
            ClientEventWire::SessionAgentUpdated { session_id, agent } => {
                Self::SessionAgentUpdated { session_id, agent }
            }
            ClientEventWire::SessionAgentMessage {
                session_id,
                agent_id,
                row,
            } => Self::SessionAgentMessage {
                session_id,
                agent_id,
                row,
            },
            ClientEventWire::SessionAgentTombstone {
                session_id,
                agent_id,
                message_uuid,
                display_only,
            } => Self::SessionAgentTombstone {
                session_id,
                agent_id,
                message_uuid,
                display_only,
            },
            ClientEventWire::SessionList { sessions } => Self::SessionList { sessions },
            ClientEventWire::ModelList {
                models,
                current,
                current_model_source,
                details,
            } => Self::ModelList {
                models,
                current,
                current_model_source,
                details,
            },
            ClientEventWire::ModelChanged {
                model,
                model_source,
            } => Self::ModelChanged {
                model,
                model_source,
            },
            ClientEventWire::PermissionModeChanged { mode } => Self::PermissionModeChanged { mode },
            ClientEventWire::ProviderCredentialStatus {
                operation_id,
                configured_provider_ids,
                unavailable_provider_ids,
                storage_encrypted,
                credential_previews,
                error,
            } => Self::ProviderCredentialStatus {
                operation_id,
                configured_provider_ids,
                unavailable_provider_ids,
                storage_encrypted,
                credential_previews,
                error,
            },
            ClientEventWire::McpServers { servers } => Self::McpServers { servers },
            ClientEventWire::Hooks { hooks } => Self::Hooks { hooks },
            ClientEventWire::Agents { agents } => Self::Agents { agents },
            ClientEventWire::SlashCommandCatalog { commands } => {
                Self::SlashCommandCatalog { commands }
            }
            ClientEventWire::SlashCommandResult {
                turn_id,
                display,
                is_error,
            } => Self::SlashCommandResult {
                turn_id,
                display,
                is_error,
            },
            ClientEventWire::MemoryEntries { entries } => Self::MemoryEntries { entries },
            ClientEventWire::StatusSnapshot { snapshot } => Self::StatusSnapshot { snapshot },
            ClientEventWire::SettingsSnapshot {
                effective_json,
                provenance_json,
                files_json,
                active_json,
                locked,
                layers_json,
                merged_keys,
            } => Self::SettingsSnapshot {
                effective_json,
                provenance_json,
                files_json,
                active_json,
                locked,
                layers_json,
                merged_keys,
            },
            ClientEventWire::AuthState { state } => Self::AuthState { state },
            ClientEventWire::DoctorReport { report } => Self::DoctorReport { report },
            ClientEventWire::TaskRow { task } => Self::TaskRow { task },
            ClientEventWire::TaskOutputChunk {
                task_id,
                content,
                total_lines,
                truncated,
            } => Self::TaskOutputChunk {
                task_id,
                content,
                total_lines,
                truncated,
            },
            ClientEventWire::TaskStatusChanged {
                task_id,
                status,
                origin_session_id,
                error,
            } => Self::TaskStatusChanged {
                task_id,
                status,
                origin_session_id,
                error,
            },
            ClientEventWire::CommandsChanged { commands } => Self::CommandsChanged { commands },
            ClientEventWire::CoordinatorWorker { worker } => Self::CoordinatorWorker { worker },
            ClientEventWire::ThinkingDelta {
                thinking,
                signature,
            } => Self::ThinkingDelta {
                thinking,
                signature,
            },
            ClientEventWire::UsageUpdate {
                is_snapshot,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => Self::UsageUpdate {
                is_snapshot,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            },
            ClientEventWire::Attachment { attachment } => Self::Attachment { attachment },
            ClientEventWire::ApiRetry {
                message,
                attempt,
                max_retries,
                delay_ms,
            } => Self::ApiRetry {
                message,
                attempt,
                max_retries,
                delay_ms,
            },
            ClientEventWire::PlanUpdated { tasks } => Self::PlanUpdated { tasks },
            ClientEventWire::WorkflowResumed {
                previous_task_id,
                task,
                run_id,
                origin_session_id,
            } => Self::WorkflowResumed {
                previous_task_id,
                task,
                run_id,
                origin_session_id,
            },
            ClientEventWire::ConversationControlsChanged { controls } => {
                Self::ConversationControlsChanged { controls }
            }
            ClientEventWire::FastModeChanged { enabled } => Self::FastModeChanged { enabled },
            ClientEventWire::AudioRequest { request } => Self::AudioRequest { request },
            ClientEventWire::TurnRecoveryState { snapshot } => Self::TurnRecoveryState { snapshot },
            ClientEventWire::TurnEventReplay {
                session_id,
                turn_id,
                sequence,
                event_json,
            } => Self::TurnEventReplay {
                session_id,
                turn_id,
                sequence,
                event_json,
            },
            ClientEventWire::Skills { skills } => Self::Skills { skills },
            ClientEventWire::TypescriptLspModeChanged {
                requested,
                effective,
                available,
            } => Self::TypescriptLspModeChanged {
                requested,
                effective,
                available,
            },
            ClientEventWire::SessionForked {
                source_session_id,
                session_id,
                mode,
            } => Self::SessionForked {
                source_session_id,
                session_id,
                mode,
            },
            ClientEventWire::ProviderConnectionTested {
                operation_id,
                provider_id,
                connected,
                reachable,
                authenticated,
                model_available,
                http_status,
                latency_ms,
                message,
                used_stored_credential,
            } => Self::ProviderConnectionTested {
                operation_id,
                provider_id,
                connected,
                reachable,
                authenticated,
                model_available,
                http_status,
                latency_ms,
                message,
                used_stored_credential,
            },
            ClientEventWire::ConfigurationOperation {
                domain,
                operation_id,
                status,
                effect,
                message,
                details_json,
            } => Self::ConfigurationOperation {
                domain,
                operation_id,
                status,
                effect,
                message,
                details_json,
            },
            ClientEventWire::SkillCatalog { catalog_json } => Self::SkillCatalog { catalog_json },
            ClientEventWire::SkillDocument { document_json } => {
                Self::SkillDocument { document_json }
            }
            ClientEventWire::McpConfigurationSnapshot { snapshot_json } => {
                Self::McpConfigurationSnapshot { snapshot_json }
            }
            ClientEventWire::PluginCatalog { catalog_json } => Self::PluginCatalog { catalog_json },
            ClientEventWire::ProviderModelCatalog { providers } => {
                Self::ProviderModelCatalog { providers }
            }
            ClientEventWire::CompactionStatus { phase, error } => {
                Self::CompactionStatus { phase, error }
            }
            ClientEventWire::CronResult {
                request_id,
                jobs,
                error,
            } => Self::CronResult {
                request_id,
                jobs,
                error,
            },
            ClientEventWire::MessageRetracted { message_id } => {
                Self::MessageRetracted { message_id }
            }
            ClientEventWire::MessageIdentity { message_id } => Self::MessageIdentity { message_id },
            ClientEventWire::LoopWakeup {
                message,
                companion,
                streak,
                since_ms,
            } => Self::LoopWakeup {
                message,
                companion,
                streak,
                since_ms,
            },
            ClientEventWire::TaskLifecycle { event_json } => Self::TaskLifecycle { event_json },
            ClientEventWire::OpenAiOAuthUpdated { session } => Self::OpenAiOAuthUpdated { session },
            ClientEventWire::CronRunBound { run_id, error } => Self::CronRunBound { run_id, error },
            ClientEventWire::ScheduledRunFinished {
                run_id,
                summary,
                error,
            } => Self::ScheduledRunFinished {
                run_id,
                summary,
                error,
            },
            ClientEventWire::CronRunRequested { run_id, task } => {
                Self::CronRunRequested { run_id, task }
            }
            ClientEventWire::ScheduledTaskFire { message } => Self::ScheduledTaskFire { message },
            ClientEventWire::TaskListComplete {
                request_id,
                active_count,
                error,
            } => Self::TaskListComplete {
                request_id,
                active_count,
                error,
            },
            ClientEventWire::AudioCancel { identity } => Self::AudioCancel { identity },
            ClientEventWire::AudioCapabilitiesChanged { capabilities } => {
                Self::AudioCapabilitiesChanged { capabilities }
            }
            ClientEventWire::QueryModelChange { to_model } => Self::QueryModelChange { to_model },
            ClientEventWire::AssistantBlockStart { block_key } => {
                Self::AssistantBlockStart { block_key }
            }
            ClientEventWire::AssistantBlockIdentity {
                block_key,
                message_uuid,
            } => Self::AssistantBlockIdentity {
                block_key,
                message_uuid,
            },
            ClientEventWire::Tombstone {
                message,
                display_only,
            } => Self::Tombstone {
                message,
                display_only,
            },
            ClientEventWire::RefusalContinuation {
                phase,
                salvage_text,
                join,
                replaces_uuids,
                display_salvage_text,
            } => Self::RefusalContinuation {
                phase,
                salvage_text,
                join,
                replaces_uuids,
                display_salvage_text,
            },
            ClientEventWire::UserTranscriptRowIdentity { row_token, uuid } => {
                Self::UserTranscriptRowIdentity { row_token, uuid }
            }
            ClientEventWire::AssistantTranscriptRowUuids { message_id, uuids } => {
                Self::AssistantTranscriptRowUuids { message_id, uuids }
            }
            ClientEventWire::UiLog { plugin, text } => Self::UiLog { plugin, text },
            ClientEventWire::UiToast {
                plugin,
                text,
                timeout_ms,
            } => Self::UiToast {
                plugin,
                text,
                timeout_ms,
            },
            ClientEventWire::UiStatus { plugin, text } => Self::UiStatus { plugin, text },
            ClientEventWire::UiControlResult {
                request_id,
                response_json,
                metadata_json,
                error,
            } => Self::UiControlResult {
                request_id,
                response_json,
                metadata_json,
                error,
            },
            ClientEventWire::UiClientFrame {
                runtime_id,
                frame_json,
            } => Self::UiClientFrame {
                runtime_id,
                frame_json,
            },
            ClientEventWire::UiInvalidate {
                instances_json,
                uuid,
                session_id,
            } => Self::UiInvalidate {
                instances_json,
                uuid,
                session_id,
            },
            ClientEventWire::UiNotify { plugin, text } => Self::UiNotify { plugin, text },
            ClientEventWire::VisualizationBlock { status, reference } => {
                Self::VisualizationBlock { status, reference }
            }
            ClientEventWire::PromptSuggestion {
                suggestion,
                uuid,
                session_id,
            } => Self::PromptSuggestion {
                suggestion,
                uuid,
                session_id,
            },
            ClientEventWire::PromptSuggestionState { enabled, notice } => {
                Self::PromptSuggestionState { enabled, notice }
            }
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

fn event_raw_required<T, E>(
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

fn event_raw_optional<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    match fields.remove(name) {
        Some(raw) => serde_json::from_str(raw.get()).map_err(E::custom),
        None => Ok(None),
    }
}

fn event_raw_object_json<E>(fields: HashMap<String, Box<RawValue>>) -> Result<String, E>
where
    E: serde::de::Error,
{
    let mut json = String::from("{");
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
    json.push('}');
    Ok(json)
}

fn event_raw_wire_json<E>(tag: &str, fields: HashMap<String, Box<RawValue>>) -> Result<String, E>
where
    E: serde::de::Error,
{
    let mut json = String::from("{");
    json.push_str(&serde_json::to_string(tag).map_err(E::custom)?);
    json.push(':');
    json.push_str(&event_raw_object_json::<E>(fields)?);
    json.push('}');
    Ok(json)
}

impl<'de> Deserialize<'de> for ClientEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawEnvelopeFields(mut fields) = RawEnvelopeFields::deserialize(deserializer)?;
        let tag = event_raw_required::<String, D::Error>(&mut fields, "type")?;
        match tag.as_str() {
            "ask_user_question" => Ok(Self::AskUserQuestion {
                request: event_raw_required(&mut fields, "request")?,
            }),
            "task_row" => Ok(Self::TaskRow {
                task: event_raw_required(&mut fields, "task")?,
            }),
            "workflow_resumed" => Ok(Self::WorkflowResumed {
                previous_task_id: event_raw_required(&mut fields, "previous_task_id")?,
                task: event_raw_required(&mut fields, "task")?,
                run_id: event_raw_required(&mut fields, "run_id")?,
                origin_session_id: event_raw_optional(&mut fields, "origin_session_id")?,
            }),
            "session_agent_message" => {
                let session_id = event_raw_required(&mut fields, "session_id")?;
                let agent_id = event_raw_required(&mut fields, "agent_id")?;
                let row_json = event_raw_object_json::<D::Error>(fields)?;
                let row = serde_json::from_str(&row_json).map_err(serde::de::Error::custom)?;
                Ok(Self::SessionAgentMessage {
                    session_id,
                    agent_id,
                    row,
                })
            }
            "session_ended" => Ok(Self::SessionEnded),
            _ => {
                let json = event_raw_wire_json::<D::Error>(&tag, fields)?;
                let wire: ClientEventWire =
                    serde_json::from_str(&json).map_err(serde::de::Error::custom)?;
                Ok(wire.into())
            }
        }
    }
}

/// The provider-message portion of a fallback tombstone row. `content_json`
/// retains every provider block without imposing the narrower display-block
/// projection used by [`MessageDto`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ServerFallbackProviderMessageDto {
    /// Provider-assigned inner message identifier, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Provider-selected model, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider stop reason, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Serialized provider refusal stop details, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details_json: Option<String>,
    /// Serialized provider usage facts, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_json: Option<String>,
    /// Serialized JSON array of the complete content blocks.
    pub content_json: String,
}

/// Known outer and provider fields of a native tombstone row. The host source
/// does not assign a `parentUuid`; timestamp and request fields are forwarded
/// only when the completed row actually carries them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ServerFallbackTombstoneMessageDto {
    /// Native row UUID.
    pub uuid: String,
    /// Native outer row kind, serialized under the native `type` key.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Native row-creation timestamp.
    pub timestamp: String,
    /// Request id associated with the row, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Serialized request-reference payload, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_ref_json: Option<String>,
    /// Provider message envelope.
    pub message: ServerFallbackProviderMessageDto,
    /// Whether the row represents an API error, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_api_error_message: Option<bool>,
    /// Native supersession metadata, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_uuids: Option<Vec<String>>,
}

/// Refusal-continuation phase used by the native stitch event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefusalContinuationPhaseDto {
    /// Seed the pending stitch from retained refusal text.
    Begin,
}

/// Refusal-continuation join rule used by the native stitch event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefusalContinuationJoinDto {
    /// Concatenate text with no inserted separator.
    Exact,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// A user-visible attachment. Internally tagged on `type`, `snake_case`, and
/// `#[non_exhaustive]` so the remaining oracle kinds are additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachmentDto {
    /// A nested LINGXI.md surfaced because a file under its directory was read.
    NestedMemory {
        /// Path shown to the user, relative to cwd.
        display_path: String,
    },
}

/// Coarse error class carried by [`ClientEvent::Error`]. Internally tagged on
/// `type`, `snake_case`. `#[non_exhaustive]` so a future kind is additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKindDto {
    /// Transport-level failure (e.g. an `ApiError` while streaming).
    Transport,
    /// Protocol violation (e.g. malformed frame or stream-protocol error).
    Protocol,
    /// Server-reported error mid-stream.
    Server,
    /// The orchestrator's `max_turns` budget was reached.
    MaxTurns,
    /// A valid command was rejected by the active policy or runtime state.
    Rejected,
    /// Any other internal failure.
    Internal,
}

/// How a turn ended — the lowered analog of `lingxi_core::host::TurnOutcome`. Internally
/// tagged on `type`, `snake_case`. `#[non_exhaustive]` so a future outcome is
/// additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnOutcomeDto {
    /// Model returned a natural stop reason and the turn loop ended.
    EndTurn,
    /// The `max_turns` budget was reached before `end_turn`.
    MaxTurns,
    /// The cancel token fired mid-turn; the orchestrator returned early.
    Cancelled,
}

/// Durable execution state for a mobile turn. Backgrounding itself never
/// changes this state; only execution, a recovery gate, or an explicit cancel
/// does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnRecoveryStateDto {
    /// The turn is currently executing or attached to a live executor.
    Running,
    /// Execution is parked until the user supplies permission or input.
    WaitingForUser,
    /// The platform lease expired, but the checkpoint may be resumed safely.
    PausedRecoverable,
    /// The turn produced its normal final outcome.
    Completed,
    /// The turn ended with an execution failure.
    Failed,
    /// The user explicitly cancelled the turn; it must never be resumed.
    Cancelled,
}

impl TurnRecoveryStateDto {
    /// Terminal durable states must never be resurrected by attach/resume.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Snapshot clients use to decide whether a durable turn can be reattached or
/// needs explicit user intervention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TurnRecoverySnapshotDto {
    /// Stable session owning the turn.
    pub session_id: String,
    /// Stable client-provided turn identity.
    pub turn_id: u64,
    /// Current durable lifecycle state.
    pub state: TurnRecoveryStateDto,
    /// First event sequence still retained for replay.
    pub first_sequence: u64,
    /// Last committed event sequence, or zero when no events were committed.
    pub last_sequence: u64,
    /// Whether replaying from the persisted boundary is known to be safe.
    pub safe_to_resume: bool,
    /// Optional machine-readable explanation for a paused or terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Cumulative cost snapshot — the lowered analog of `lingxi_core::host::CostSnapshot`
/// (`Duration` → whole seconds; only the display-relevant fields, decision
/// §0.4). Carried by [`ClientEvent::TurnEnded`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CostDto {
    /// Cumulative cost in USD.
    pub total_usd: f64,
    /// Cumulative input tokens across all turns.
    pub input_tokens: u64,
    /// Cumulative output tokens across all turns.
    pub output_tokens: u64,
    /// Cumulative successful API calls.
    pub api_calls: u32,
    /// Elapsed session time in whole seconds.
    pub session_duration_secs: u64,
    /// Pre-formatted display string (e.g. `"$0.0123"`).
    pub formatted: String,
}

/// A durable task from the workspace's existing cron scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CronJobDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<crate::protocol::commands::CronAutomationDto>,
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
    pub permanent: bool,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[cfg(test)]
mod session_agent_identity_contract_tests {
    use super::*;
    use crate::protocol::message::MessageBlockDto;

    fn row(index: u64, uuid: &str) -> SessionAgentMessageRowDto {
        SessionAgentMessageRowDto {
            message_index: index,
            message_uuid: uuid.into(),
            message: MessageDto {
                transcript_uuid: None,
                sent_at_ms: None,
                role: "assistant".into(),
                blocks: vec![MessageBlockDto::Text {
                    text: "declined".into(),
                }],
                images: vec![],
                loop_wakeup: None,
                visualization_context: None,
            },
            api_error_json: Some(
                "{\"isApiErrorMessage\":true,\"requestId\":\"event-request\"}".into(),
            ),
        }
    }

    #[test]
    fn live_child_row_requires_uuid_and_preserves_complete_error_json() {
        let event = ClientEvent::SessionAgentMessage {
            session_id: "session".into(),
            agent_id: "agent".into(),
            row: row(7, "row-7"),
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["message_index"], 7);
        assert_eq!(encoded["message_uuid"], "row-7");
        assert!(encoded.get("row").is_none());
        assert_eq!(
            serde_json::from_value::<ClientEvent>(encoded.clone()).unwrap(),
            event
        );
        let mut missing_identity = encoded;
        missing_identity
            .as_object_mut()
            .unwrap()
            .remove("message_uuid");
        assert!(serde_json::from_value::<ClientEvent>(missing_identity).is_err());
    }

    #[test]
    fn child_snapshot_retains_index_gaps_and_high_water_mark() {
        let event = ClientEvent::SessionAgentTranscript {
            session_id: "session".into(),
            agent_id: "agent".into(),
            messages: vec![row(7, "row-7"), row(12, "row-12")],
            next_message_index: 20,
            revision: 3,
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["messages"][0]["message_index"], 7);
        assert_eq!(encoded["messages"][1]["message_index"], 12);
        assert_eq!(encoded["next_message_index"], 20);
        assert_eq!(
            serde_json::from_value::<ClientEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn child_tombstone_carries_session_agent_uuid_and_display_flag() {
        let event = ClientEvent::SessionAgentTombstone {
            session_id: "session".into(),
            agent_id: "agent".into(),
            message_uuid: "row-7".into(),
            display_only: true,
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "session_agent_tombstone", "session_id": "session",
                "agent_id": "agent", "message_uuid": "row-7", "display_only": true
            })
        );
        assert_eq!(
            serde_json::from_value::<ClientEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn ui_log_preserves_source_text_through_full_event_envelope() {
        let wire = r#"{"type":"ui_log","plugin":"test","text":"LSP \ud800 failure"}"#;
        let event: ClientEvent = serde_json::from_str(wire).expect("deserialize full event");
        let expected = vec![
            b'L' as u16,
            b'S' as u16,
            b'P' as u16,
            b' ' as u16,
            0xd800,
            b' ' as u16,
            b'f' as u16,
            b'a' as u16,
            b'i' as u16,
            b'l' as u16,
            b'u' as u16,
            b'r' as u16,
            b'e' as u16,
        ];
        let ClientEvent::UiLog { text: message, .. } = &event else {
            panic!("expected UI log event");
        };
        assert_eq!(message.code_units, expected);

        let encoded = serde_json::to_string(&event).expect("serialize full event");
        assert!(encoded.contains(r#""type":"ui_log""#));
        assert!(encoded.contains(r#""text":"LSP \ud800 failure""#));
        let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip full event");
        let ClientEvent::UiLog { text: message, .. } = decoded else {
            panic!("expected UI log event");
        };
        assert_eq!(message.code_units, expected);
    }

    #[test]
    fn terminal_api_error_keeps_terminal_and_stop_reasons_and_omits_unknown_status() {
        let event = ClientEvent::TerminalApiError {
            api_error_status: None,
            terminal_reason: "api_error".into(),
            stop_reason: "stop_sequence".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"terminal_api_error","terminal_reason":"api_error","stop_reason":"stop_sequence"}"#
        );
        assert_eq!(serde_json::from_str::<ClientEvent>(&json).unwrap(), event);

        let image_error = ClientEvent::TerminalApiError {
            api_error_status: Some(413),
            terminal_reason: "image_error".into(),
            stop_reason: "stop_sequence".into(),
        };
        let json = serde_json::to_string(&image_error).unwrap();
        assert_eq!(
            json,
            r#"{"type":"terminal_api_error","api_error_status":413,"terminal_reason":"image_error","stop_reason":"stop_sequence"}"#
        );
        assert_eq!(
            serde_json::from_str::<ClientEvent>(&json).unwrap(),
            image_error
        );

        let refusal = ClientEvent::TerminalApiError {
            api_error_status: None,
            terminal_reason: "api_error".into(),
            stop_reason: "refusal".into(),
        };
        let json = serde_json::to_string(&refusal).unwrap();
        assert_eq!(
            json,
            r#"{"type":"terminal_api_error","terminal_reason":"api_error","stop_reason":"refusal"}"#
        );
        assert_eq!(serde_json::from_str::<ClientEvent>(&json).unwrap(), refusal);
    }
}
