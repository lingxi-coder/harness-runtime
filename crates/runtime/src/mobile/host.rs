//! Shared mobile session-host module (plan F3-03).
//!
//! This is the mobile sibling of `harness_runtime::desktop::build` (F2-01): the single
//! place that wires an off-device-buildable [`ConversationOrchestrator`] from a
//! deterministic [`MobileConfig`] + an `Arc<dyn Platform>`, binding the
//! transport-agnostic [`client::adapter::AdapterOutputStream`] and the id-keyed
//! [`client::adapter::AdapterPermissionGate`] as its sinks. The same lowering
//! pipeline therefore feeds the mobile [`ClientEventListener`] exactly as it
//! feeds the bridge-server WebSocket — governing decision §0.1 / §0.2.
//!
//! ## Why this lives in `harness-runtime::mobile` (not the FFI crates)
//!
//! The FFI packagers (`ios-framework` / `android-aar`) must NOT each re-derive
//! the runtime wiring — that would let iOS and Android drift. Instead they
//! re-export the shared host built here (F3-04 grows `MobileEngineHandle` to own
//! a [`MobileRuntime`]; F3-05 adds the async `submit`). F3-03 only builds the
//! orchestrator + binds the adapter sinks.
//!
//! ## Off-device determinism
//!
//! `build_mobile` takes every functional input through [`MobileConfig`] and the
//! OS handles through `Arc<dyn Platform>`. It reads a SMALL, fixed set of
//! `std::env` vars purely to mirror desktop parity behavior — the
//! `LINGXI_MEMDIR_PREFETCH` activation gate, the `ANTHROPIC_SMALL_FAST_MODEL`
//! / `ANTHROPIC_DEFAULT_HAIKU_MODEL` model ids a `prompt` hook may resolve to, and
//! `HOME` for the permission `FsRoots` (absent on a sandboxed device ⇒ `None`).
//! These are all unset on a real device, so on-device behavior stays
//! deterministic (prefetch off, no model override, no home root). No other env /
//! argv is read. On the host (CI) a fake `Platform` shim (see the `tests` module)
//! supplies portable handles so the orchestrator is constructed and the adapter
//! sinks are exercised without a device — exactly the spec §8 "prove from a
//! Swift/Kotlin unit test" smoke path, runnable on the host. The real device
//! `Platform` (`platform-ios` / `platform-android`) is `cfg(target_os)`-gated in
//! `Cargo.toml`, so this module never names a device crate.

mod assembly;
mod audio_context;
mod audio_realtime_owner;
mod automation;
mod configuration;
mod linux_runtime;
mod list_agents;
mod provider_services;
mod restoration;
mod side_chat;

pub use audio_context::MediaSessionContext;
pub use audio_realtime_owner::MediaAgentHandoffRequest;
pub use orchestrator::{MediaConversationAttachment, RealtimeAgentSessionIo};

pub use assembly::{
    build_mobile, build_mobile_engine, build_mobile_engine_inner, build_mobile_inner,
};
use automation::{
    cron_task_active, finalize_cron_occurrence, fired_cron_dto, mobile_automation_runtime,
    mobile_cron_schedule_error, mobile_cron_session_gate, read_cron_tasks,
    supervise_mobile_automation,
};
pub use automation::{
    CronDueOccurrenceDto, CronFireStatusDto, CronTaskDto, FiredCronJobDto, MobileCronStoreHandle,
};
use automation::{MobileTurnFirer, MOBILE_CRON_HANDLES};
pub use configuration::{parse_mobile_provider_config_json, MobileBuildError, MobileConfig};
pub use linux_runtime::MobileLinuxStatusDto;
use linux_runtime::{
    build_mobile_runtime_environment, build_mobile_subagent_env_renderer, gate_mobile_git_ctx,
    gate_mobile_shell_ctx, lower_mobile_linux_status, mobile_launch_is_interactive,
    mobile_typescript_lsp_ready, model_visible_mobile_cwd,
};
use provider_services::{
    anthropic_models, apply_mobile_profile_allowlist, builtin_provider_catalog,
    classify_provider_connection_response, lower_auth_state, mobile_provider_settings,
    model_listings, probe_provider, provider_catalog_from_profiles, provider_connection_failure,
    provider_id_is_valid, provider_model_catalog_from_listings, resolve_default_model_ref,
};
pub use provider_services::{
    MobileOAuthManager, MobileOAuthSessionDto, MobileOAuthStateDto, ProviderCatalogEntryDto,
    ProviderConnectionTestDto,
};
use restoration::{canonical_cwd_string, lower_session_mode};
mod workspace_commands;
pub use workspace_commands::MobileSessionDiscoveryScope;

#[cfg(test)]
use lingxi_core::host::{Clock, FileSystem};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

fn split_model_selection_source(
    source: Utf16JsonProjection,
    listings: &[lingxi_core::host::ModelListing],
) -> (
    Utf16JsonProjection,
    Option<lingxi_core::host::ModelRouteAnchor>,
) {
    let Some(units) = source.string_units("") else {
        return (source, None);
    };
    let Ok(text) = String::from_utf16(&units) else {
        return (source, None);
    };
    let trimmed = lingxi_core::host::effort::trim_js_whitespace(&text);
    let (model, profile) = lingxi_core::host::parse_model_ref(&trimmed, listings);
    let route_hint = profile.map(|profile| lingxi_core::host::ModelRouteAnchor {
        model: model.clone(),
        profile: Some(profile),
    });
    if route_hint.is_some() {
        (source, route_hint)
    } else if model != trimmed {
        (Utf16JsonProjection::from(model), route_hint)
    } else {
        (source, route_hint)
    }
}

#[cfg(test)]
use provider_services::{
    anthropic_route_id, parse_mobile_oauth_callback, take_mobile_oauth_session,
    validate_mobile_oauth_session, MobileOAuthProvider, PendingMobileOAuthSession,
    IOS_OAUTH_REDIRECT_URI, MOBILE_OAUTH_SESSION_TTL,
};

#[cfg(test)]
use automation::{
    cron_replay_session, cron_scheduled_turn_outcome, cron_target_session_mode, cron_task_dto,
    decode_cron_automation, ImmediateDenyPermissionSink, MobileCronBorrow,
};

mod configuration_admin;
mod fast_mode_preference;
mod model_preference;
mod permission_preference;
mod settings_commands;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::session_agent_transcript::{
    parse_main_session_agent_message_rows, parse_session_agent_message_rows,
    read_session_agent_next_message_index, session_agent_conversation_is_visible,
    session_identity_snapshot_for_path,
};
use async_trait::async_trait;
use client::adapter::controls::{decode_reasoning_selection, lower_conversation_controls};
use client::adapter::lowering::lower_status_snapshot;
use client::adapter::{
    AdapterOutputStream, AdapterPermissionGate, ClientEventListener, TurnEventEmitter,
};
use client::protocol::commands::{
    ClientCommand, ImageRefDto, ListingKindDto as ProtocolListingKind, PromptModeDto,
    ProviderCredentialSecretDto,
};
use client::protocol::controls::{ConversationControlsDto, ReasoningSelectionDto};
use client::protocol::error::ClientError;
use client::protocol::events::{
    ClientEvent, ErrorKindDto, TurnOutcomeDto, TurnRecoverySnapshotDto, TurnRecoveryStateDto,
};
use client::protocol::listings::{
    ModelDetailsDto, ProviderModelCatalogEntryDto, SessionAgentSummaryDto, SessionModeDto,
    SlashCommandDto,
};
use client::protocol::permission::PermissionResponseDto;
use client::protocol::text::ClientTextDto;
use command_api::RegistrySlashDispatcher;
use cron::CronJobFirer;

use mcp::{McpRegistry, McpServerConfig};

use mobile_linux_api::MobileLinuxRuntime;

use orchestrator::ConversationOrchestrator;
use permission::gate::PermissionGate;
use permission::PermissionMode;

use lingxi_core::host::audio::{
    AudioError, AudioErrorKind, AudioOperation, AudioOperationContext, AudioOperationId,
    AudioOperationSuccess, AudioOwner, AudioService,
};
use lingxi_core::host::{AuthHandle, OrchestratorHandle, Platform, SlashCommandDispatcher};

use secret::CredentialManager;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tool_api::ToolRegistry;
use tool_workflow::WorkflowLauncher as _;

static NEXT_MOBILE_AUDIO_TEARDOWN_GENERATION: AtomicU64 = AtomicU64::new(1);

fn trim_model_source_text(source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = source.string_units("").unwrap_or_default();
    let is_js_whitespace = |unit: u16| {
        char::from_u32(u32::from(unit))
            .is_some_and(lingxi_core::host::effort::javascript_whitespace)
    };
    let mut start = 0;
    let mut end = units.len();
    while start < end && is_js_whitespace(units[start]) {
        start += 1;
    }
    while end > start && is_js_whitespace(units[end - 1]) {
        end -= 1;
    }
    let units = units[start..end].to_vec();
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text is derived from the same UTF-16 source units")
}
const MOBILE_AUDIO_OWNER_TEARDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

async fn end_mobile_audio_owner(
    service: &Arc<dyn AudioService>,
    recording_handles: &Arc<
        tokio::sync::Mutex<HashMap<AudioOwner, lingxi_core::host::audio::AudioRecordingHandle>>,
    >,
    owner: AudioOwner,
) -> Result<(), AudioError> {
    let capabilities = service.capabilities();
    let context = AudioOperationContext {
        identity: AudioOperationId::new(
            NEXT_MOBILE_AUDIO_TEARDOWN_GENERATION
                .fetch_add(1, Ordering::Relaxed)
                .max(1),
            capabilities.service_epoch,
        ),
        owner: owner.clone(),
        initiator: None,
        timeout_budget_ms: Some(
            u64::try_from(MOBILE_AUDIO_OWNER_TEARDOWN_BUDGET.as_millis()).unwrap_or(u64::MAX),
        ),
        max_payload_bytes: capabilities.max_payload_bytes,
    };
    let result = tokio::time::timeout(
        MOBILE_AUDIO_OWNER_TEARDOWN_BUDGET,
        service.execute(context, AudioOperation::EndOwner),
    )
    .await
    .map_err(|_| {
        AudioError::new(
            AudioErrorKind::Timeout,
            "audio owner teardown exceeded its shutdown budget",
        )
    })??;

    match result {
        AudioOperationSuccess::OwnerEnded => {
            recording_handles.lock().await.remove(&owner);
            Ok(())
        }
        _ => Err(AudioError::new(
            AudioErrorKind::NativeFailure,
            "audio service returned an invalid owner-teardown result",
        )),
    }
}

use crate::mobile::{
    skill_loader::command_visible_in_session_mode,
    turn_durability::{
        DurableBashResult, DurableSkillPrompt, DurableTurnStore, DurableTurnStoreError,
        QueueDeliveryState, QueueOperationKind, ResumeDisposition,
    },
};

/// Everything a mobile host needs to drive a conversation, built deterministically
/// by [`build_mobile`] from a [`MobileConfig`] + an `Arc<dyn Platform>`.
///
/// The mobile analog of `harness_runtime::desktop::DesktopRuntime`. F3-04 grows
/// `MobileEngineHandle` to OWN one of these (plus the handle-owned tokio
/// runtime); F3-05 adds the async `submit` that drives the orchestrator and
/// resolves the permission gate.
pub struct MobileRuntime {
    /// Official provider settings data from the same settled startup snapshot.
    pub provider_catalog: Vec<ProviderCatalogEntryDto>,
    /// Per-host owner for V2 installed-plugin archive materializations.
    /// Plugin discovery and the manager retain clones of this same session.
    pub(crate) plugin_archive_session: Arc<plugin::PluginArchiveSession>,
    /// Ready services used to create RAM-only lightweight conversations.
    pub side_chat_factory: Arc<dyn crate::side_chat::SideChatFactory>,
    /// Independent Local App tasks over ready native host services.
    pub app_agent_factory: Arc<super::app_agent::AppAgentFactory>,
    /// Product region captured with the running model client.
    pub provider_region: llm_runtime::Region,
    /// App-scoped media facade shared by tools, UI and remote media callers.
    pub media_service: Option<Arc<dyn media_api::MediaService>>,
    /// Host adapter for Conversation-intent media sessions.
    pub(crate) media_conversation_attachment: Option<Arc<dyn MediaConversationAttachment>>,
    /// Identity-bound queue for deferred media Agent continuation.
    pub(crate) media_agent_handoffs: Arc<MobileMediaAgentHandoffQueue>,
    /// Shared settings baseline used by both the Mod reader and the public
    /// settings context attached to the engine handle.
    pub(super) settings_active:
        Arc<std::sync::RwLock<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
    pub(super) settings_managed: lingxi_core::types::utf16_json::Utf16JsonProjection,
    pub(super) audio_services: llm_runtime::services::ProviderServices,
    pub(super) audio_credential_ids: std::collections::BTreeMap<String, String>,
    /// Only interactive hosts inherit device-owned permission preferences.
    interactive_launch: bool,
    /// The fully-constructed orchestrator, bound to the adapter output stream
    /// and the id-keyed permission gate.
    pub orchestrator: Arc<ConversationOrchestrator>,
    /// Live dynamic /loop scheduler cell shared with ScheduleWakeup.
    pub(crate) wakeup_scheduler: tool_cron::WakeupSchedulerCell,
    /// Slash-command dispatcher seeded with the builtin + mobile handlers.
    pub dispatcher: RegistrySlashDispatcher,
    /// Shared registry snapshot that both dispatch and listings read. Mobile
    /// must not maintain a second slash-command table beside the live engine
    /// registry.
    pub slash_registry: Arc<RwLock<command_api::CommandRegistry>>,
    workspace_catalog: workspace_commands::MobileWorkspaceCatalog,
    /// D1 (P-1.5 review): the VERY handle `build_mobile` passed to
    /// `.with_skill_listing(...)`, retained so a test can interrogate the
    /// per-turn skill listing the orchestrator actually reads.
    ///
    /// A test that rebuilds its own provider from [`Self::slash_registry`]
    /// proves only that a provider over that registry works — it never
    /// observes which registry the WIRED provider was handed, so a refactor
    /// that gives the listing provider a registry of its own leaves the
    /// model's per-turn skill listing permanently empty on device with the
    /// whole suite still green. Test-only: nothing in production reads it, and
    /// retaining it in shipping builds would only keep an `Arc` alive.
    #[cfg(test)]
    pub(crate) wired_skill_listing_provider:
        Arc<dyn orchestrator::prompt::skill_listing::SkillListingProvider>,
    /// D1 (P-1.5 review): the VERY `SkillLoader` `build_mobile` handed to the
    /// mobile tool registry, retained for the same reason as
    /// [`Self::wired_skill_listing_provider`] — the Skill tool's view of the
    /// registry must be observable, not re-derived by the test.
    #[cfg(test)]
    pub(crate) wired_skill_loader: Arc<dyn tool_skill::skill::SkillLoader>,
    /// P1.8 (§19.2): the mobile `PluginManager` `build_mobile_inner` composed
    /// and registered the compiled-in plugin through, retained so a test can
    /// materialize a REAL fixture plugin via [`plugin::PluginManager::enable`]
    /// (never `register_verified_builtin` — that symbol keeps its single
    /// production call site in `lib.rs`) and observe the mutation through the
    /// SAME live surfaces the model reads, rather than asserting against a
    /// manager the test built itself.
    #[cfg(test)]
    pub(crate) wired_plugin_manager: Arc<plugin::PluginManager>,
    /// Test-only handles for proving every plugin-workflow consumer shares one
    /// production registry allocation.
    #[cfg(test)]
    pub(crate) wired_plugin_workflow_registry: Arc<workflow::PluginWorkflowRegistry>,
    #[cfg(test)]
    pub(crate) wired_local_workflow_handler: Arc<tasks::handlers::LocalWorkflowHandler>,
    #[cfg(test)]
    pub(crate) wired_workflow_tool: Arc<tool_workflow::WorkflowTool>,
    /// The live plugin lifecycle manager used by the mobile plugin command
    /// route.  This is deliberately the same allocation retained by the
    /// test-only `wired_plugin_manager` field above, so enable/disable/status
    /// commands mutate the registries that the rest of this runtime reads.
    pub(crate) plugin_manager: Arc<plugin::PluginManager>,
    /// P1.8: the VERY agent catalog `plugin_manager` was built
    /// `.with_agent_catalog(..)` over. A plugin-declared agent lands here.
    #[cfg(test)]
    pub(crate) wired_agent_catalog: Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>>,
    /// P1.8: the subagent spawner's OWN set-once agent-catalog cell — the
    /// object the real invocation path (`PoolSubagentSpawner::spawn`)
    /// actually consults. Retained separately from
    /// [`Self::wired_agent_catalog`] so a test can prove the two are
    /// `Arc::ptr_eq` — i.e. the SAME allocation — rather than two catalogs
    /// that merely started out holding equal builtin content.
    #[cfg(test)]
    pub(crate) wired_subagent_agent_catalog_cell:
        Arc<std::sync::OnceLock<Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>>>>,
    /// The real subagent spawner's skill-preload cell. Phase 2 Plugin agents
    /// declare frontmatter skills, so leaving this empty makes every declared
    /// preload silently warn-and-skip on mobile.
    #[cfg(test)]
    pub(crate) wired_subagent_skill_loader_cell:
        Arc<agent::RuntimeLink<Arc<dyn lingxi_core::host::skill_loader::SkillLoader>>>,
    /// Auth handle for `/login` and `/logout`.
    pub auth: Arc<dyn AuthHandle>,
    /// Native mobile OAuth coordinator. It owns the provider-specific handles
    /// and the one pending PKCE callback, while the foreign UI only receives a
    /// redacted session descriptor and returns the callback URL.
    pub oauth: Arc<MobileOAuthManager>,
    /// The connection-scoped [`AdapterPermissionGate`] handle. Mobile ALWAYS
    /// binds the adapter gate (no always-allow mode), so unlike desktop this is
    /// never `None`: F3-05's `submit(ApprovePermission/DenyPermission)` calls
    /// [`AdapterPermissionGate::resolve`] on it to satisfy a parked `check()`.
    pub permission_gate: Arc<AdapterPermissionGate>,
    /// The enforcing policy gate. The iOS UI records an explicit risk
    /// acknowledgement here before it sends a live bypass-mode transition.
    pub permission_policy_gate: Arc<permission::PolicyPermissionGate>,
    /// User-requested permission mode before model/provider auto resolution.
    pub requested_permission_mode: Arc<StdMutex<String>>,
    /// Capability profile attached to this source/session lane.
    pub session_mode: session::jsonl::SessionMode,
    /// Mode assigned to a newly-created session when it has no transcript
    /// metadata of its own. Existing sessions always restore their own value.
    pub session_default_permission_mode: String,
    /// The registered foreign event listener. Held so F3-04's handle can own /
    /// re-surface it; the adapter already feeds it via a [`ListenerSink`].
    pub listener: Arc<dyn ClientEventListener>,
    /// The connection's [`client::adapter::ClientEventSink`] (a [`ListenerSink`]
    /// over `listener`). The orchestrator's [`AdapterOutputStream`] already pushes
    /// streamed turn events here; F3-05's `submit` reuses the SAME sink to
    /// synthesize boundary events (`TurnStarted` / `MessageComplete`) and emit
    /// listing replies, so everything rides one outbound channel.
    pub event_sink: Arc<dyn client::adapter::ClientEventSink>,
    /// In-memory child transcript rows for agents without a durable JSONL
    /// writer. This shares the live observer's UUID/index allocator and keeps
    /// full server-fallback API-error envelopes available to snapshots.
    pub(super) session_agent_transcript_cache: SessionAgentTranscriptCache,
    /// Cloneable handle to the response accumulator behind `output`, retained
    /// so hard turn failures cannot leak partial message blocks into a later
    /// prompt on this long-lived mobile connection.
    pub message_output: AdapterOutputStream,
    /// The session transcript writer shared with the orchestrator.
    ///
    /// Mobile keeps one orchestrator alive while New/Resume changes the active
    /// session, so the command path retargets this writer to the new UUID before
    /// another turn can begin.
    pub session_writer: Arc<session::jsonl::writer::JsonlWriter>,
    /// Shared logical workspace identity; never an OS container path.
    pub session_cwd: Arc<tool_api::SessionCwd>,
    /// Whether the wired secure-storage backend can actually PERSIST credentials
    /// (i.e. is a real OS Keychain/Keystore, `is_encrypted() == true`). Mobile
    /// currently wires the non-persisting `PlainTextSecureStorage` stub, so this
    /// is `false` and OAuth `/login` cannot persist its tokens — the Login arm
    /// short-circuits with a clear message instead of failing at the persist step
    /// with a cryptic `BackendUnavailable` (audit re-pass, secure-storage finding;
    /// the real native store is a §11 / Plan-17 follow-up). Becomes `true`
    /// automatically once a native Keychain/Keystore SecureStorage is injected.
    pub oauth_supported: bool,
    /// Shared provider credential manager used by the live LLM client, OAuth,
    /// and the mobile provider-settings commands. Retaining this handle is what
    /// lets a credential written after boot take effect on the next request
    /// without rebuilding the engine.
    pub credentials: Arc<CredentialManager>,
    /// Mobile-only Linux userspace runtime seam (Android PRoot / iOS iSH),
    /// when the platform wires one. `None` preserves the pre-migration state.
    pub mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    /// The mobile MCP registry. It loads the app-private `settings.json` plus
    /// project `.mcp.json` entries using the shared MCP parser.
    pub mcp_registry: Arc<McpRegistry>,
    /// The exact registry/context pair used to materialize MCP tools for the
    /// main conversation.
    mcp_tool_registry: Arc<ToolRegistry>,
    mcp_tool_context: tool_api::BuiltinToolContext,
    /// Shared mobile LSP registry used by plugin registration, file sync, and
    /// passive diagnostics.
    pub lsp_registry: Arc<lsp::LspRegistry>,
    /// Whether the pinned TypeScript runtime passed the rootfs/target probe.
    /// Kept separate from the requested policy so the UI can show a degraded
    /// effective `off` without overwriting the user's preference.
    pub typescript_lsp_runtime_available: bool,
    /// Per-server CAS watermarks for asynchronous MCP settings reconciliation.
    /// A detached reload job may only clean up or publish work while its
    /// watermark is still current.
    mcp_reload_generations: Arc<StdMutex<HashMap<String, MobileMcpReloadIntent>>>,
    /// Most recent MCP OAuth authorization URL. Retained for hosts without a
    /// native deep-link opener so the user can copy it.
    pub mcp_oauth_authorization_url: Arc<StdMutex<Option<String>>>,
    /// Every `(provider, model)` this connection can actually route to — the
    /// LIVE client config after `apply_mobile_profile_allowlist`.
    ///
    /// `OrchestratorHandle::list_model_listings` cannot answer this. It returns
    /// the STATIC llm-runtime catalog: every builtin preset whether or not the
    /// user configured it, and — because it is assembled from
    /// `builtin_presets()` — no user-defined provider at all. Reading it left
    /// the picker wrong in both directions, advertising providers nobody enabled
    /// while hiding the custom endpoint someone had just configured.
    ///
    /// Desktop needs no equivalent: its picker gates the same static catalog on
    /// per-provider availability maps that mobile does not have.
    pub routable_listings: Vec<lingxi_core::host::ModelListing>,
    /// Settings-visible provider model directory generated before the mobile
    /// routing allowlist is applied.
    pub provider_model_catalog: Vec<ProviderModelCatalogEntryDto>,
    /// v3 Phase 1 (workflow-on-mobile): the connection's task registry —
    /// backs the `Workflow` tool's `LocalWorkflow` tasks, the Task command
    /// family (`TaskList`/`TaskOutput`/`TaskStop`), and the per-turn
    /// `<task-notification>` drain.
    pub(crate) task_registry: Arc<tasks::registry::TaskRegistry>,
    /// The same live ordered name store consumed by the mobile `/list-agents`
    /// provider. Retained only so host tests can seed the exact composed store.
    #[cfg(test)]
    pub(crate) wired_agent_name_registry:
        Arc<dyn lingxi_core::host::agent_name_registry::AgentNameRegistry>,
    /// Durable workflow handoff store used to adopt interrupted runs as paused
    /// when their owning session is resumed after a process restart.
    pub(crate) workflow_checkpoints:
        Arc<crate::mobile::workflow_support::MobileWorkflowCheckpointStore>,
    /// Status/progress sink shared by the registered workflow handler and its
    /// launcher so events buffered during task registration can be flushed.
    pub(crate) workflow_status_sink: Arc<crate::mobile::workflow_support::MobileWorkflowStatusSink>,
    /// The same launcher used by the Workflow tool. Keeping one instance here
    /// makes explicit UI resume use the identical validation/checkpoint path.
    pub(crate) workflow_launcher: Arc<crate::mobile::workflow_support::MobileWorkflowLauncher>,
    /// The live current-session uuid. Updated by `retarget_session_writer` on
    /// every session change.
    pub(crate) active_session_uuid: Arc<std::sync::Mutex<String>>,
    /// 2.1.266 `Zl`/`ay`: the session's plan-file identity, shared with the boot
    /// permission policy. Re-published by `retarget_session_writer` on every
    /// session change, so the carve-out and `ExitPlanMode` always name the
    /// CURRENT session's plan file rather than the one this host booted on.
    pub(crate) plan_files: Arc<permission::plan_files::PlanFileMatcher>,
    pub(crate) parked_agent_restore_inheritance: lingxi_core::host::SubagentInheritance,
    pub(crate) agent_resume_gate: Arc<dyn lingxi_core::host::fork_resume_gate::ForkResumeGate>,
    pub(crate) main_report_waker: Arc<crate::main_report_waker::MainReportWakeRouter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashAuthoritySnapshot {
    session_id: String,
    model: String,
    model_source: Option<ClientTextDto>,
    permission_mode: String,
    auth: client::protocol::listings::AuthStateDto,
    catalog: Vec<SlashCommandDto>,
}

fn lower_controls(
    controls: lingxi_core::host::ConversationControls,
    requested_permission: String,
    proactivity: lingxi_core::host::NativeProactivityStatusSnapshot,
) -> ConversationControlsDto {
    let mut controls = lower_conversation_controls(controls, proactivity);
    controls.permission.requested = requested_permission;
    for option in &mut controls.reasoning.spec.options {
        if matches!(&option.selection, ReasoningSelectionDto::Level { id } if id == "max") {
            option.persistable = false;
        }
    }
    controls
}

fn lower_model_details(listing: &lingxi_core::host::ModelListing) -> ModelDetailsDto {
    client::adapter::lowering::lower_model_details(listing)
}

fn lower_model_source(source: Option<&Utf16JsonProjection>) -> Option<ClientTextDto> {
    source.and_then(|source| ClientTextDto::from_projection(source).ok())
}

#[cfg(test)]
#[path = "host/tests/mobile_oauth_callback_tests.rs"]
mod mobile_oauth_callback_tests;

#[cfg(test)]
#[path = "host/tests/mobile_tool_gate_tests.rs"]
mod mobile_tool_gate_tests;

#[cfg(test)]
#[test]
fn mobile_cancellation_and_submission_futures_are_send() {
    fn assert_send<T: Send>(_: T) {}
    fn check(handle: &MobileEngineHandle) {
        assert_send(handle.cancel_inactive_turn("session", 1, None));
        assert_send(handle.cancel_active_turn("session", Some(1)));
        assert_send(handle.submit(ClientCommand::Cancel {
            session_id: Some("session".into()),
            turn_id: Some(1),
        }));
    }
    let _ = check as fn(&MobileEngineHandle);
}

#[cfg(all(test, feature = "uniffi"))]
#[test]
fn mobile_command_methods_remain_in_uniffi_metadata() {
    // These methods straddle the private admission helpers. Referencing the
    // generated metadata verifies both public export blocks are retained.
    assert!(!UNIFFI_META_HARNESS_RUNTIME_METHOD_MOBILEENGINEHANDLE_SUBMIT.is_empty());
    assert!(
        !UNIFFI_META_HARNESS_RUNTIME_METHOD_MOBILEENGINEHANDLE_TEST_PROVIDER_CONNECTION.is_empty()
    );
    assert!(
        !UNIFFI_META_HARNESS_RUNTIME_METHOD_MOBILEENGINEHANDLE_RESET_MOBILE_LINUX_ROOTFS.is_empty()
    );
}

// Phase 2a-mobile: the multi-provider client config / chains / credential
// sources / pricing catalog are now assembled by `provider_config::assemble`
// (which owns the byte-equivalent Anthropic profile + the builtin catalog
// presets + the settings-`providers` merge). The old single-Anthropic
// `builtin_anthropic_config` / `apply_settings_providers` /
// `parse_routing_overrides` helpers from `platform_common::llm_config` are no
// longer wired here; they remain in `platform_common` (the desktop e2e tests
// still reach them via fully-qualified paths). Provider networking uses the
// shared SDK transport.

#[cfg(test)]
fn mobile_skill_listing_provider(
    registry: Arc<RwLock<command_api::CommandRegistry>>,
    session_mode: session::jsonl::SessionMode,
    read_file_state: Option<tool_api::read_file_state::ReadFileStateMap>,
) -> Arc<dyn orchestrator::prompt::skill_listing::SkillListingProvider> {
    mobile_skill_listing_provider_with_settings(
        registry,
        session_mode,
        read_file_state,
        Arc::new(|| Ok(false)),
        None,
        None,
    )
}

fn mobile_skill_listing_provider_with_settings(
    registry: Arc<RwLock<command_api::CommandRegistry>>,
    session_mode: session::jsonl::SessionMode,
    read_file_state: Option<tool_api::read_file_state::ReadFileStateMap>,
    include_code_review_suggestion: Arc<
        dyn Fn() -> Result<bool, lingxi_core::settings::SettingsError> + Send + Sync,
    >,
    listing_config: Option<MobileConfig>,
    command_snapshot_provider: Option<command_api::CommandSnapshotProvider>,
) -> Arc<dyn orchestrator::prompt::skill_listing::SkillListingProvider> {
    // Session state: once a touched file has revealed a conditional skill, a
    // later turn must not hide it again (the read-state map is an LRU, so the
    // matching path can age out).
    let conditional = Arc::new(std::sync::Mutex::new(skill::ConditionalSkills::new()));
    let precommit_registry = registry.clone();
    let precommit_snapshot_provider = command_snapshot_provider.clone();
    Arc::new(
        orchestrator::prompt::skill_listing::LazySkillListingProvider::new(move || {
            let registry = registry.clone();
            let conditional = conditional.clone();
            let read_file_state = read_file_state.clone();
            let listing_config = listing_config.clone();
            let command_snapshot_provider = command_snapshot_provider.clone();
            async move {
                if let Some(provider) = &command_snapshot_provider {
                    provider().await;
                }
                use command_api::CommandSource;
                let reg = registry.read().await;
                let effective_settings = listing_config
                    .as_ref()
                    .map(mobile_provider_settings)
                    .transpose()?;
                let usage_snapshot = listing_config.as_ref().and_then(|cfg| {
                    command_api::skill_usage::SkillUsageSnapshot::read(&cfg.lingxi_home).ok()
                });
                let full_catalog = reg
                    .native_source_ordered_commands()
                    .into_iter()
                    // Native `Sge`/`sso` visibility gates run before both
                    // full-catalog alias resolution and prompt listing.
                    .filter(|command| command.is_enabled)
                    .map(
                        |command| orchestrator::prompt::skill_listing::SkillListingCatalogName {
                            name: command.name.clone(),
                            user_facing_name: command.user_facing_name.clone(),
                            aliases: command.aliases.clone(),
                        },
                    )
                    .collect::<Vec<_>>();
                let touched = read_file_state.as_ref().map_or_else(Vec::new, |m| {
                    m.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .keys()
                });
                let root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                let entries = reg
                    .model_invocable_commands() // !disable_model_invocation (registry.rs)
                    .into_iter()
                    .filter(|c| c.is_enabled)
                    // Native `cmd.type === 'prompt'`; dispatch kind alone does
                    // not classify prompt commands, and builtin prompts use
                    // the separate `Xwr` bundled-skill gate below.
                    .filter(|c| c.is_prompt())
                    // A CONDITIONAL skill (`paths:`) stays out of the listing
                    // until the session has touched a matching file
                    // (claude-code `lhr`).
                    .filter(|c| match c.paths.as_deref() {
                        None => true,
                        Some(patterns) => conditional
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_available_named(&c.name, patterns, &touched, &root),
                    })
                    .filter(|c| command_visible_in_session_mode(c, session_mode))
                    // Native Gee eligibility after mobile mode/workspace filtering.
                    .filter(|c| {
                        c.source == CommandSource::Builtin
                            || matches!(
                                c.loaded_from.as_deref(),
                                Some("bundled" | "skills" | "syncedSkills" | "commands_DEPRECATED")
                            )
                            || c.has_user_specified_description
                            || c.when_to_use.is_some()
                    })
                    .filter_map(|c| {
                        use orchestrator::prompt::skill_listing::{
                            native_builtin_skill_gate_active, native_display_name,
                            native_skill_listing_mode, native_skill_override, SkillListingEntry,
                        };
                        let mut entry = SkillListingEntry {
                            name: c.name.clone(),
                            display_name: c.name.clone(),
                            user_facing_name: c.user_facing_name.clone(),
                            unqualified_name: c.unqualified_name.clone(),
                            is_synced_skill: c.loaded_from.as_deref() == Some("syncedSkills"),
                            bare_name_reserved: c.bare_name_reserved,
                            aliases: c.aliases.clone(),
                            source: c.native_source.clone(),
                            plugin_name: c.plugin_name.clone(),
                            description: c.description.clone(),
                            when_to_use: c.when_to_use.clone().map(Into::into),
                            is_bundled_prompt: c.source == CommandSource::Bundled,
                            is_name_only: false,
                            priority: usage_snapshot
                                .as_ref()
                                .map_or(0.0, |snapshot| snapshot.priority_for_command(&c.name)),
                        };
                        let is_builtin_prompt = c.source == CommandSource::Builtin;
                        let disable_bundled_skills =
                            crate::bundled_skills_policy::bundled_skills_disabled(
                                effective_settings.as_ref().is_some_and(|settings| {
                                    settings.disable_bundled_skills == Some(true)
                                }),
                            );
                        let builtin_gate_active = is_builtin_prompt
                            && native_builtin_skill_gate_active(
                                &c.name,
                                disable_bundled_skills,
                                crate::bundled_skills_policy::local_policy_allows_bundled_skills(
                                    None,
                                ),
                                Some(&[]),
                            );
                        let mode = native_skill_listing_mode(
                            native_skill_override(
                                &entry,
                                effective_settings
                                    .as_ref()
                                    .and_then(|settings| settings.skill_overrides.as_ref()),
                                None,
                                None,
                            ),
                            c.source == CommandSource::Plugin,
                            builtin_gate_active,
                        );
                        if matches!(
                            mode,
                            lingxi_core::settings::SkillOverride::Off
                                | lingxi_core::settings::SkillOverride::UserInvocableOnly
                        ) {
                            return None;
                        }
                        entry.is_name_only = mode == lingxi_core::settings::SkillOverride::NameOnly;
                        entry.display_name = native_display_name(&entry, &full_catalog);
                        Some(entry)
                    })
                    .collect();
                let budget_settings = effective_settings.as_ref().map_or_else(
                    orchestrator::prompt::skill_listing::SkillListingBudgetSettings::default,
                    |settings| orchestrator::prompt::skill_listing::SkillListingBudgetSettings {
                        budget_fraction: settings.skill_listing_budget_fraction,
                        max_desc_chars: settings.skill_listing_max_desc_chars,
                    },
                );
                Ok(orchestrator::prompt::skill_listing::SkillListingSnapshot {
                    entries,
                    budget_settings,
                })
            }
        })
        .with_bash_precommit_skills(move |entries| {
            let registry = precommit_registry.clone();
            let include_code_review_suggestion = include_code_review_suggestion.clone();
            let command_snapshot_provider = precommit_snapshot_provider.clone();
            async move {
                if let Some(provider) = &command_snapshot_provider {
                    provider().await;
                }
                let include_code_review_suggestion = include_code_review_suggestion()?;
                let registry_guard = registry.read().await;
                Ok(crate::skill_prompt::bash_precommit_skills(
                    &registry_guard,
                    &entries,
                    include_code_review_suggestion,
                ))
            }
        }),
    )
}

fn mobile_code_review_suggestion_provider(
    cfg: MobileConfig,
) -> Arc<dyn Fn() -> Result<bool, lingxi_core::settings::SettingsError> + Send + Sync> {
    Arc::new(move || {
        // cfg.cwd owns the HOST settings root; SessionCwd may name a guest mount.
        let settings = mobile_provider_settings(&cfg)?;
        Ok(settings.include_code_review_suggestion == Some(true))
    })
}

async fn mobile_live_plugin_skill_count(dispatcher: &RegistrySlashDispatcher) -> usize {
    dispatcher.refresh_command_snapshot().await;
    let registry = dispatcher.registry();
    let registry = registry.read().await;
    registry
        .list_all()
        .into_iter()
        .filter(|command| command.source == command_api::CommandSource::Plugin)
        .count()
}

fn mobile_reload_skills_handler(
    registry: Arc<RwLock<command_api::CommandRegistry>>,
    cwd: std::path::PathBuf,
    lingxi_home: std::path::PathBuf,
    home: std::path::PathBuf,
    device_skill_tools: Vec<String>,
    media_tools: Arc<ToolRegistry>,
    workflow_tool: Arc<tool_workflow::WorkflowTool>,
    workflow_command_snapshot: Arc<crate::workflow_catalog::WorkflowCommandSnapshot>,
    additional_roots: Vec<std::path::PathBuf>,
) -> command_api::builtins::reload_skills::ReloadSkillsHandler {
    let workflow_finalizer = workflow_command_snapshot.locked_post_reload_finalizer(workflow_tool);
    command_api::builtins::reload_skills::ReloadSkillsHandler::with_all_roots(
        registry,
        cwd,
        lingxi_home,
        None,
        home,
        additional_roots
            .iter()
            .map(|root| root.join(branding::DOT_DIR).join("skills"))
            .collect(),
        false,
    )
    .with_additional_command_dirs(
        additional_roots
            .iter()
            .map(|root| root.join(branding::DOT_DIR).join("commands"))
            .collect(),
    )
    .with_locked_post_reload_finalizer(
        true,
        Arc::new(move |reg| {
            crate::mobile::register_mobile_bundled_prompt_commands(reg);
            crate::mobile::device_skills::register_mobile_device_skills_with_tools(
                reg,
                &device_skill_tools,
                Some(media_tools.clone()),
            );
            workflow_finalizer(reg);
        }),
    )
}

/// Apply the mobile MCP dial preflight consistently on boot and reload.
/// OAuth-configured remote entries are only dialable when the platform supplied
/// an encrypted credential store; this check runs before `connect_all`, so it
/// cannot issue OAuth discovery, remote HTTP, or plaintext-storage writes.
fn mobile_mcp_preflight(
    mut configs: Vec<McpServerConfig>,
    oauth_supported: bool,
) -> Vec<McpServerConfig> {
    if oauth_supported {
        return configs;
    }
    for config in &mut configs {
        if config.config_error.is_none()
            && matches!(
                config.spec,
                lingxi_core::host::McpTransportSpec::Sse { oauth: Some(_), .. }
                    | lingxi_core::host::McpTransportSpec::Http { oauth: Some(_), .. }
            )
        {
            config.config_error =
                Some("MCP OAuth requires an encrypted secure credential store".to_string());
        }
    }
    configs
}

/// Serialize the complete MCP config for reload identity checks. `McpHeaders`
/// intentionally preserves insertion order because it is part of the server
/// key, so the serialized form also distinguishes a meaningful header-order
/// change. Returning `None` is fail-closed for equality: an unrepresentable
/// config is treated as changed and is never silently retained.
fn mobile_mcp_config_snapshot(config: &McpServerConfig) -> Option<String> {
    serde_json::to_string(config).ok()
}

fn mobile_mcp_config_unchanged(current: &McpServerConfig, desired: &McpServerConfig) -> bool {
    match (
        mobile_mcp_config_snapshot(current),
        mobile_mcp_config_snapshot(desired),
    ) {
        (Some(current), Some(desired)) => current == desired,
        _ => false,
    }
}

fn mobile_mcp_state_is_transitional(state: &mcp::connection::McpConnectionState) -> bool {
    matches!(
        state,
        mcp::connection::McpConnectionState::Connecting { .. }
            | mcp::connection::McpConnectionState::AwaitingOAuth { .. }
    )
}

fn mobile_mcp_reload_requires_replacement(
    current: &mcp::connection::McpConnectionState,
    desired: &McpServerConfig,
    intent_changed: bool,
) -> bool {
    if mobile_mcp_config_unchanged(current.config(), desired) {
        return mobile_mcp_state_is_transitional(current) && intent_changed;
    }
    true
}

/// Latest disk intent observed by mobile MCP reconciliation. This is kept
/// separately from the registry's visible state because a previous
/// background job may still be waiting on a lifecycle lock or a network/OAuth
/// operation while settings have already changed again.
struct MobileMcpReloadIntent {
    generation: u64,
    desired: Option<McpServerConfig>,
}

enum MobileMcpReloadJob {
    Remove {
        name: String,
        generation: u64,
        expected: McpServerConfig,
    },
    Connect {
        name: String,
        generation: u64,
        desired: McpServerConfig,
        previous: Option<McpServerConfig>,
    },
}

fn mobile_mcp_record_reload_intent(
    generations: &Arc<StdMutex<HashMap<String, MobileMcpReloadIntent>>>,
    name: &str,
    desired: Option<&McpServerConfig>,
) -> (u64, bool) {
    let mut generations = generations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let unchanged =
        generations
            .get(name)
            .is_some_and(|intent| match (intent.desired.as_ref(), desired) {
                (Some(current), Some(desired)) => mobile_mcp_config_unchanged(current, desired),
                (None, None) => true,
                _ => false,
            });
    let intent = generations
        .entry(name.to_string())
        .or_insert_with(|| MobileMcpReloadIntent {
            generation: 0,
            desired: None,
        });
    if !unchanged {
        intent.generation = intent.generation.saturating_add(1);
        intent.desired = desired.cloned();
    }
    (intent.generation, !unchanged)
}

fn mobile_mcp_reload_generation_is_current(
    generations: &Arc<StdMutex<HashMap<String, MobileMcpReloadIntent>>>,
    name: &str,
    generation: u64,
) -> bool {
    generations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .map(|intent| intent.generation)
        == Some(generation)
}

fn is_plugin_owned_mcp_config(config: &McpServerConfig) -> bool {
    config.metadata.agent_source == Some(mcp::McpAgentSource::Plugin)
}

fn mobile_mcp_reload_guard(
    generations: &Arc<StdMutex<HashMap<String, MobileMcpReloadIntent>>>,
    name: &str,
    generation: u64,
) -> Arc<mcp::registry::McpOperationGuard> {
    let generations = generations.clone();
    let name = name.to_string();
    Arc::new(move || mobile_mcp_reload_generation_is_current(&generations, &name, generation))
}

async fn mobile_mcp_run_reload_job(
    job: MobileMcpReloadJob,
    registry: Arc<McpRegistry>,
    generations: Arc<StdMutex<HashMap<String, MobileMcpReloadIntent>>>,
) {
    match job {
        MobileMcpReloadJob::Remove {
            name,
            generation,
            expected,
        } => {
            let guard = mobile_mcp_reload_guard(&generations, &name, generation);
            let _ = registry
                .remove_without_revoking_auth_if_config_guarded(&name, &expected, guard)
                .await;
        }
        MobileMcpReloadJob::Connect {
            name,
            generation,
            desired,
            previous,
        } => {
            let guard = mobile_mcp_reload_guard(&generations, &name, generation);
            if let Some(previous) = previous.as_ref() {
                if let Err(error) = registry
                    .remove_without_revoking_auth_if_config_guarded(&name, previous, guard.clone())
                    .await
                {
                    tracing::debug!(server = %name, %error, "mobile MCP reload removal failed");
                    return;
                }
            }

            loop {
                if !mobile_mcp_reload_generation_is_current(&generations, &name, generation) {
                    return;
                }
                let current = registry
                    .connections
                    .read()
                    .await
                    .get(&name)
                    .map(|state| state.config().clone());
                if current
                    .as_ref()
                    .is_some_and(|config| mobile_mcp_config_unchanged(config, &desired))
                {
                    return;
                }
                if let Some(current) = current {
                    // A prior generation may have completed after this job's
                    // initial conditional removal. Retire only the exact
                    // state observed here; the registry rechecks it under its
                    // lifecycle lock before touching clients/catalog/cache.
                    if let Err(error) = registry
                        .remove_without_revoking_auth_if_config_guarded(
                            &name,
                            &current,
                            guard.clone(),
                        )
                        .await
                    {
                        tracing::debug!(server = %name, %error, "mobile MCP reload removal failed");
                        return;
                    }
                    continue;
                }
                match registry
                    .connect_if_current(desired.clone(), guard.clone())
                    .await
                {
                    Ok(Some(_)) => return,
                    Ok(None) => {
                        // The registry either observed a newer generation or
                        // another config won the lifecycle race. Re-read the
                        // state and let the guarded conditional removal above
                        // retire only that exact state if this generation is
                        // still current.
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(server = %name, %error, "mobile MCP reload connect failed");
                        return;
                    }
                }
            }
        }
    }
}

fn mobile_mcp_oauth_authorization_callback(
    slot: Arc<StdMutex<Option<String>>>,
    opener: Option<Arc<dyn lingxi_core::host::DeepLinkOpener>>,
) -> mcp::oauth::OnAuthorizationUrl {
    Arc::new(move |url| {
        // Record before attempting the native opener. A successful open is
        // still observable as a copyable fallback, and an opener rejection or
        // missing opener cannot lose the authorization URL.
        if let Ok(mut current) = slot.lock() {
            *current = Some(url.to_string());
        }
        if let Some(opener) = opener.as_ref() {
            let opener = opener.clone();
            let url = url.to_string();
            tokio::spawn(async move {
                if let Err(error) = opener.open(url).await {
                    tracing::warn!(%error, "MCP OAuth authorization URL opener failed; copy the recorded URL");
                }
            });
        } else {
            tracing::info!(url = %url, "MCP OAuth authorization URL is ready to copy");
        }
    })
}

/// Errors surfaced to the foreign (Swift / Kotlin) host across the FFI boundary.
///
/// This is the SINGLE shared error vocabulary both FFI packager crates re-export
/// (F3-04), folded from the legacy per-crate `MobileEngineError`: keeping it in
/// the shared host crate is what prevents iOS and Android from drifting. Under
/// the `uniffi` feature (F3-01) this becomes `#[derive(uniffi::Error)]`-able; it
/// is intentionally flat (no embedded engine types) so it marshals across the
/// boundary unchanged.
#[derive(Debug, Clone, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
pub enum MobileEngineError {
    /// The requested session id is not registered with this engine handle.
    #[error("session not found")]
    NotFound,
    /// The engine is in a state that does not allow the requested operation.
    #[error("invalid state")]
    InvalidState,
    /// `build_mobile_engine` was called off-device. The real device `Platform`
    /// (`platform-ios` / `platform-android`) is `cfg(target_os)`-gated, so the
    /// FFI constructor returns this on the host — but the shared session host
    /// itself is fully exercised off-device via the test shim.
    #[error("platform unavailable on this target")]
    PlatformUnavailable,
    /// Catch-all for engine-internal failures (the message is log-safe).
    #[error("internal: {0}")]
    Internal(String),
}

/// The real mobile session host (plan F3-04): the opaque handle the foreign
/// (Swift / Kotlin) side holds for the lifetime of one engine connection.
///
/// This is the grown-up form of the M8 stub (which held only an `Arc<dyn
/// Platform>` + a `skill_count` and whose `create_session` returned an
/// `Internal` error). It now OWNS, per governing decision §0.5 (one connection ⇒
/// one engine host):
///
/// - the **handle-owned tokio runtime** (`rt-multi-thread`) every turn / FFI
///   `submit` (F3-05) is driven on — so the engine never blocks the foreign UI
///   thread, and F3-07 registers this same runtime as `UniFFI`'s foreign async
///   executor;
/// - the fully-wired [`MobileRuntime`] from [`build_mobile`] (F3-03): the
///   orchestrator bound to the [`AdapterOutputStream`] + the id-keyed
///   [`AdapterPermissionGate`], the slash dispatcher, the auth handle;
/// - the registered foreign [`ClientEventListener`] (re-surfaced via
///   [`MobileRuntime::listener`]) that the adapter feeds every translated
///   [`client::protocol::events::ClientEvent`].
///
/// Both FFI packager crates (`ios-framework` / `android-aar`) RE-EXPORT this
/// shared host rather than each re-deriving it — that is what keeps iOS and
/// Android from drifting (plan F3-04). Under the `uniffi` feature this becomes
/// `#[derive(uniffi::Object)]`.
///
/// The connection-scoped adapter sinks (output stream / permission gate /
/// listener) survive an in-place orchestrator swap on New / Resume (§0.5); F3-05
/// adds the async `submit` that resolves the parked permission gate from inbound
/// commands and drives the turn on the owned runtime.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct MobileEngineHandle {
    self_weak: std::sync::Weak<Self>,
    /// This Host generation owns all side state; no durable store is involved.
    side_chats: Arc<crate::side_chat::SideChatManager>,
    session_cron: Option<Arc<cron::CronScheduler>>,
    scheduled_reload: std::sync::atomic::AtomicBool,
    settings: Option<::configuration_admin::settings_bridge::SettingsContext>,
    task_notification_watcher: tokio::task::AbortHandle,
    /// The handle-owned multi-thread tokio runtime. Owned (not borrowed) so the
    /// engine outlives any single FFI call and F3-05's `submit(SendPrompt)` can
    /// `spawn` a streaming turn that returns promptly while results stream to the
    /// listener. F3-07 registers this runtime as the foreign async executor.
    runtime: Option<tokio::runtime::Runtime>,
    /// The fully-wired mobile runtime: orchestrator + dispatcher + auth + the
    /// connection-scoped [`AdapterPermissionGate`] + the registered listener.
    inner: MobileRuntime,
    /// The connection's event sink (a [`ListenerSink`] over the registered
    /// listener). Held so [`Self::submit`] can synthesize boundary events
    /// (`TurnStarted` / `MessageComplete`) and push listing replies to the SAME
    /// outbound channel the streamed turn events ride.
    event_sink: Arc<dyn client::adapter::ClientEventSink>,
    /// The same outbound channel as [`Self::event_sink`], taken BEFORE the
    /// [`TurnLifecycleListener`] wrap.
    ///
    /// `SystemNotice` is live-turn payload (`TurnLifecycleListener::
    /// is_live_turn_payload`), so the turn gate drops one emitted while no turn
    /// is in flight. That is right for stray turn output arriving after the
    /// active-turn slot is cleared, and wrong for a connection-scoped
    /// acknowledgement a command handler owes the user right now — the gate
    /// cannot tell the two apart, because both ride the same variant.
    ///
    /// This is mobile's counterpart to bridge-server's private
    /// `unscoped_event_sink` (`apps/bridge-server/src/server.rs`), which is why
    /// the desktop host does not have this bug: `server.rs` already routes every
    /// `ClientCommand` through the unscoped sink, so a command handler's reply
    /// never meets the turn filter there.
    ///
    /// Use ONLY for events that belong to the connection rather than to a turn;
    /// anything a turn produces must keep going through [`Self::event_sink`] so
    /// it stays gated, sequenced and journaled.
    connection_sink: Arc<dyn client::adapter::ClientEventSink>,
    /// The cancellation token for the IN-FLIGHT turn, armed by
    /// `submit(SendPrompt)` and fired by `submit(Cancel)`. `None` when no turn is
    /// active. One connection ⇒ one in-flight turn (§0.5), so a single slot.
    active_cancel: Arc<Mutex<Option<Arc<ActiveTurn>>>>,
    /// The same priority-aware message queue used by desktop/CLI. Running
    /// prompts enter at `Next` and are consumed inside the existing turn loop.
    message_queue: Arc<msgqueue::MessageQueueManager>,
    loop_transition: Arc<Mutex<()>>,
    pending_submissions: Arc<StdMutex<Vec<Arc<PendingSubmission>>>>,
    cancel_reason: orchestrator::prompt::mid_turn_input::CancelReasonFlag,
    /// Correlates interactive `AskUserQuestion` events with inbound answers.
    ask_user_question_broker: Arc<client::adapter::AskUserQuestionBroker>,
    /// Crash-safe input, event cursor, and recovery policy for client-addressed
    /// turns. This is deliberately independent from Activity/View ownership.
    durable_turns: Arc<DurableTurnStore>,
    /// Producer side of the tool-to-broker bridge. Retaining it with the
    /// handle keeps the broker's input lifetime explicit and lets host tests
    /// exercise ownership-scoped pause cancellation through the real path.
    ask_user_question_tx: tokio::sync::mpsc::Sender<tool_ui::AskUserQuestionExchange>,
    /// Number of builtin mobile skills assembled (the M8 smoke signal, retained
    /// so the existing Swift/Kotlin smoke test keeps working).
    skill_count: usize,
    /// The `~/.claude`-equivalent root the session enumerator walks
    /// (`<lingxi_home>/projects/<sanitized cwd>/*.jsonl`). Captured from the
    /// `MobileConfig` so `submit(ListSessions)` can read the on-disk catalog
    /// without re-deriving it (SESSIONS/HISTORY).
    lingxi_home: std::path::PathBuf,
    /// Process-wide per-settings-file transaction lock shared by every mobile
    /// engine handle in this process. Plugin toggles and reasoning selection
    /// both read-modify-write the same document.
    settings_write_lock: Arc<Mutex<()>>,
    /// The session enumerator's `cwd` key (its sanitized form selects the project
    /// subdir under `lingxi_home/projects/`). Captured from the `MobileConfig`.
    /// Lifecycle watermark used by the child-agent pump. Session switches
    /// publish this only after their SessionStarted/SessionResumed (or
    /// SessionEnded for ClearSession) event has entered the shared sink.
    session_lifecycle_tx: tokio::sync::watch::Sender<String>,
    /// The platform filesystem handle the JSONL reader reads each session file
    /// through (`list_recent_sessions`' `Arc<dyn FileSystem>` argument). The SAME
    /// `fs` the orchestrator's tools use — captured from the `Platform` so the
    /// session listing reads through the device's real backend.
    fs: Arc<dyn lingxi_core::host::FileSystem>,
    /// The deterministic build recipe, captured so the cron firing path
    /// (the per-task `run_cron_task_if_due`) can rebuild a FRESH, throwaway
    /// [`MobileRuntime`] per fired job (an isolated session that never pollutes
    /// the user's live conversation). Also carries `cwd`, which resolves the
    /// `<cwd>/.lingxi/scheduled_tasks.json` the cron FFI reads/writes.
    firer_cfg: MobileConfig,
    /// The aggregate device `Platform`, captured alongside `firer_cfg` so the
    /// cron firing path can call `build_mobile_inner` (and reach `filesystem()` /
    /// `clock()`) without re-deriving the device handles.
    firer_platform: Arc<dyn Platform>,
}

impl MobileRuntime {
    /// Keep every session-scoped tool and transcript consumer on one identity.
    /// The caller owns the foreground/background session lease while switching.
    async fn retarget_session_context(
        &self,
        home: &std::path::Path,
        session_id: lingxi_core::types::SessionId,
        cwd: &str,
    ) {
        let next_session_id = session_id.as_uuid().to_string();
        let previous_session_id = self
            .active_session_uuid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if previous_session_id != next_session_id {
            if let Some(pending) = self
                .media_agent_handoffs
                .clear_for_session(&previous_session_id)
            {
                if let Some(service) = self.media_service.as_ref() {
                    crate::media_handoff::stop_media_session(
                        service,
                        &pending.request.session,
                        media_api::MediaOwner::Session {
                            session_id: previous_session_id.clone(),
                        },
                    )
                    .await;
                } else {
                    tracing::warn!(
                        session_id = %previous_session_id,
                        "discarded pending media conversation handoff without a media service for cleanup"
                    );
                }
            }
            if let Some(service) = self.media_service.as_ref() {
                let stopped = crate::media_handoff::stop_media_owner_resources(
                    service,
                    media_api::MediaOwner::Session {
                        session_id: previous_session_id.clone(),
                    },
                )
                .await;
                if stopped > 0 {
                    tracing::debug!(
                        session_id = %previous_session_id,
                        stopped,
                        "stopped active media resources for the retired mobile session"
                    );
                }
            }
            if let Some(audio) = self.mcp_tool_context.audio.as_ref() {
                let owner = AudioOwner::Session {
                    session_id: previous_session_id.clone(),
                };
                if let Err(error) = end_mobile_audio_owner(
                    audio,
                    &self.mcp_tool_context.audio_recording_handles,
                    owner,
                )
                .await
                {
                    tracing::warn!(
                        kind = %error.kind,
                        %error,
                        "mobile audio owner teardown failed during session switch"
                    );
                }
            }
        }
        if let Some(scheduler) = self.wakeup_scheduler.get() {
            tool_cron::stop_dynamic_loop(Some(scheduler)).await;
            if let Some(state) = scheduler.loop_runtime() {
                state.reset();
            }
        }
        let active = self.session_writer.active_path();
        let path = if active.file_stem().and_then(|value| value.to_str())
            == Some(&session_id.as_uuid().to_string())
        {
            active
        } else {
            orchestrator::transcript_paths::main_transcript_path(
                home,
                cwd,
                &session_id.as_uuid().to_string(),
            )
        };
        // The owned orchestrator switch has already prepared and installed this
        // session's durable lock. Publish the same target to host consumers;
        // retargeting only the display path would leave durable appends behind.
        if let Err(error) = self.session_writer.activate_session_target(
            session_id,
            path,
            std::path::PathBuf::from(cwd),
        ) {
            tracing::error!(%error, %session_id, "mobile session transcript authority was not prepared");
        }
        // Keep the live session uuid in lockstep with the session on every
        // retarget (New/Resume/Clear).
        let session_uuid = next_session_id;
        {
            let mut guard = self
                .active_session_uuid
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = session_uuid.clone();
            // Re-point the plan-file carve-out at the new session, with a fresh
            // slug — a retarget is a new plan file, not a rename of the old one.
            if let Some(identity) = self.plan_files.identity() {
                let plans_dir = identity.plans_dir.clone();
                self.plan_files
                    .publish(permission::plan_files::PlanFileIdentity {
                        slug: lingxi_core::host::plan_slug::generate_slug(None, &|candidate| {
                            lingxi_core::host::plan_slug::slug_taken_in(&plans_dir, candidate)
                        }),
                        ..identity
                    });
            }
            self.permission_gate
                .set_session_id(Some(session_uuid.clone()));
            self.task_registry
                .set_workflow_session_filter(Some(session_uuid.clone()));
        }
    }
}

impl Drop for MobileEngineHandle {
    fn drop(&mut self) {
        self.task_notification_watcher.abort();
        // Native audio callbacks may need the platform's UI executor. In
        // particular, Swift can release this handle on MainActor while an
        // EndOwner callback is awaiting MainActor; Drop must return before that
        // callback can finish. The process-lifetime automation runtime survives
        // this handle's disposal, so use it for bounded owner cleanup and the
        // remaining graceful shutdown work without joining from a foreign
        // executor or relying on the runtime being dropped here.
        let audio_service = self.inner.mcp_tool_context.audio.clone();
        let audio_recording_handles = self.inner.mcp_tool_context.audio_recording_handles.clone();
        let current_session_id = self
            .inner
            .active_session_uuid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let active_turn = self.active_cancel.try_lock().ok().and_then(|active| {
            active.as_ref().map(|turn| {
                if matches!(
                    turn.try_request_lifecycle_quiesce(&self.cancel_reason),
                    Some(false)
                ) {
                    turn.cancel.cancel();
                }
                turn.clone()
            })
        });
        let active_cancel = self.active_cancel.clone();
        let cancel_reason = self.cancel_reason.clone();
        let media_service = self.inner.media_service.clone();
        let media_agent_handoffs = self.inner.media_agent_handoffs.clone();
        let lsp_registry = self.inner.lsp_registry.clone();
        let session_cron = self.session_cron.clone();
        let side_chats = self.side_chats.clone();
        let orchestrator = self.inner.orchestrator.clone();
        orchestrator.cancel_idle_compaction();
        mobile_automation_runtime().spawn(async move {
            // Cancel an in-flight model/tool request before the owner sweep.
            // The process-lifetime runtime remains available after this
            // handle's runtime begins shutting down, so waiting here never
            // blocks the native thread that called Drop.
            let active_turn = match active_turn {
                Some(turn) => Some(turn),
                None => tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    let active = active_cancel.lock().await;
                    active.as_ref().map(|turn| {
                        if !turn.request_lifecycle_quiesce(&cancel_reason) {
                            turn.cancel.cancel();
                        }
                        turn.clone()
                    })
                })
                .await
                .ok()
                .flatten(),
            };
            if let Some(turn) = active_turn {
                if !turn.request_lifecycle_quiesce(&cancel_reason) {
                    turn.cancel.cancel();
                }
                if tokio::time::timeout(crate::media_handoff::CLEANUP_BUDGET, turn.wait_completed())
                    .await
                    .is_err()
                {
                    if let Some(task) = turn.take_task_handle() {
                        task.abort();
                    }
                }
            }
            orchestrator.shutdown_idle_compaction().await;
            if let Err(error) = side_chats.shutdown().await {
                tracing::warn!(%error, "mobile side chat teardown did not finish");
            }
            if let Some(pending) = media_agent_handoffs.clear_for_session(&current_session_id) {
                if let Some(service) = media_service.as_ref() {
                    crate::media_handoff::stop_media_session(
                        service,
                        &pending.request.session,
                        media_api::MediaOwner::Session {
                            session_id: current_session_id.clone(),
                        },
                    )
                    .await;
                }
            }
            if let Some(service) = media_service.as_ref() {
                let stopped = crate::media_handoff::stop_media_owner_resources(
                    service,
                    media_api::MediaOwner::Session {
                        session_id: current_session_id.clone(),
                    },
                )
                .await;
                if stopped > 0 {
                    tracing::debug!(
                        session_id = %current_session_id,
                        stopped,
                        "stopped active media resources during mobile engine disposal"
                    );
                }
            }
            if let Some(audio_service) = audio_service {
                let mut audio_owners = vec![AudioOwner::Session {
                    session_id: current_session_id,
                }];
                for owner in audio_recording_handles.lock().await.keys() {
                    if !audio_owners.contains(owner) {
                        audio_owners.push(owner.clone());
                    }
                }
                for owner in audio_owners {
                    if let Err(error) =
                        end_mobile_audio_owner(&audio_service, &audio_recording_handles, owner)
                            .await
                    {
                        tracing::warn!(
                            kind = %error.kind,
                            %error,
                            "mobile audio owner teardown failed during engine disposal"
                        );
                    }
                }
            }
            if let Some(scheduler) = session_cron {
                let _ = scheduler.stop().await;
            }
            lsp_registry.shutdown_all().await;
        });
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Count valid append-only JSONL message records in a transcript prefix. This
/// monotonic raw watermark includes hidden compact-summary and lifecycle
/// records, so replacing a summary cannot be mistaken for an equal visible
/// message snapshot.
fn session_agent_transcript_revision(raw: &[u8]) -> u64 {
    raw.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| std::str::from_utf8(line).ok())
        .filter_map(|line| lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line).ok())
        .filter_map(|projection| session_agent_message_from_projection(&projection).ok())
        .count() as u64
}

fn session_agent_transcript_event(
    requested_session_id: lingxi_core::types::SessionId,
    current_session_id: lingxi_core::types::SessionId,
    agent_id: String,
    messages: Vec<client::protocol::listings::SessionAgentMessageRowDto>,
    next_message_index: u64,
    revision: u64,
) -> Option<ClientEvent> {
    if requested_session_id != current_session_id {
        return None;
    }
    Some(ClientEvent::SessionAgentTranscript {
        session_id: requested_session_id.as_uuid().to_string(),
        agent_id,
        messages,
        next_message_index,
        revision,
    })
}

fn session_agent_message_from_projection(
    row: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<lingxi_core::types::ConversationMessage, String> {
    let message = row
        .subprojection("/message")
        .map_err(|error| format!("session-agent transcript message: {error}"))?;
    let raw = message
        .to_json_string()
        .map_err(|error| format!("invalid session-agent message: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("invalid session-agent message: {error}"))
}

#[cfg(test)]
mod session_identity_import_tests {
    use super::session_identity_snapshot_for_path;
    use session::jsonl::{JsonlWriter, SessionMessageIdentitySnapshot};
    use std::sync::Arc;

    #[tokio::test]
    async fn metadata_only_new_session_bootstraps_before_first_message_without_rewriting_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let transcript_path = directory.path().join("session.jsonl");
        let filesystem: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer = JsonlWriter::new(transcript_path.clone(), filesystem);
        let anchor = concat!(
            "{\"type\":\"custom-title\",\"customTitle\":\"New\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"mobileEmptySession\":1}\n",
            "{\"type\":\"session-mode\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"sessionMode\":\"code\"}\n"
        );
        std::fs::write(&transcript_path, anchor).unwrap();

        let imported = session_identity_snapshot_for_path(&writer, &transcript_path)
            .await
            .unwrap();
        assert_eq!(imported, SessionMessageIdentitySnapshot::default());
        assert_eq!(std::fs::read_to_string(&transcript_path).unwrap(), anchor);

        let row = session::jsonl::reader::route_lines(
            r#"{"type":"user","uuid":"first-message","parentUuid":null,"sessionId":"11111111-2222-3333-4444-555555555555","timestamp":"2026-10-04T12:00:00.000Z","cwd":"/home/lingxi/lingxi","version":"test","isSidechain":false,"userType":"external","message":{"role":"user","content":"hello"}}"#,
        )
        .messages_in_order
        .into_iter()
        .next()
        .unwrap();
        writer.append(&row).await.unwrap();
        let after_first_message = session_identity_snapshot_for_path(&writer, &transcript_path)
            .await
            .unwrap();
        assert_eq!(after_first_message.by_uuid["first-message"], 0);
        assert_eq!(after_first_message.next_message_index, 1);
    }

    #[tokio::test]
    async fn resumed_native_transcript_keeps_bytes_and_exposes_original_indices() {
        let directory = tempfile::tempdir().unwrap();
        let transcript_path = directory.path().join("session.jsonl");
        let filesystem: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer = JsonlWriter::new(transcript_path.clone(), filesystem);
        let native = concat!(
            "{\"type\":\"user\",\"uuid\":\"resume-user\",\"parentUuid\":null,\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"timestamp\":\"2026-10-04T12:00:00.000Z\",\"cwd\":\"/home/lingxi/lingxi\",\"version\":\"test\",\"isSidechain\":false,\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"resume-assistant\",\"parentUuid\":\"resume-user\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"timestamp\":\"2026-10-04T12:00:01.000Z\",\"cwd\":\"/home/lingxi/lingxi\",\"version\":\"test\",\"isSidechain\":false,\"message\":{\"role\":\"assistant\",\"content\":\"world\"}}\n"
        );
        std::fs::write(&transcript_path, native).unwrap();

        let imported = session_identity_snapshot_for_path(&writer, &transcript_path)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&transcript_path).unwrap(), native);
        assert_eq!(imported.by_uuid["resume-user"], 0);
        assert_eq!(imported.by_uuid["resume-assistant"], 1);
        assert_eq!(imported.next_message_index, 2);
    }

    #[tokio::test]
    async fn missing_and_empty_paths_do_not_create_identity_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let transcript_path = directory.path().join("session.jsonl");
        let filesystem: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer = JsonlWriter::new(transcript_path.clone(), filesystem);
        let empty = session_identity_snapshot_for_path(&writer, &transcript_path)
            .await
            .unwrap();
        assert_eq!(empty, SessionMessageIdentitySnapshot::default());

        std::fs::write(&transcript_path, "").unwrap();
        let empty = session_identity_snapshot_for_path(&writer, &transcript_path)
            .await
            .unwrap();
        assert_eq!(empty, SessionMessageIdentitySnapshot::default());
        let sidecar = transcript_path.with_file_name(format!(
            "session.jsonl{}",
            branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX
        ));
        assert!(!sidecar.exists());
    }
}

#[cfg(test)]
mod session_agent_exact_utf16_tests {
    use super::*;

    #[test]
    fn hidden_exact_tool_context_rows_are_valid_transcript_entries() {
        let mut row = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
            r#"{
                "message_index":0,
                "uuid":"agent-message-1",
                "message":{
                    "role":"user",
                    "id":"11111111-2222-4333-8444-555555555555",
                    "content":[{"type":"text","text":"ctx�"}],
                    "is_meta":true,
                    "is_compact_summary":false,
                    "is_visible_in_transcript_only":false
                },
                "source_attachment":{"content":["ctx�"]}
            }"#,
        )
        .unwrap();
        let exact_context = [0x0063, 0x0074, 0x0078, 0xd800];
        row.set_string_units("/message/content/0/text", &exact_context)
            .unwrap();
        row.set_string_units("/source_attachment/content/0", &exact_context)
            .unwrap();
        let raw = format!("{}\n", row.to_json_string().unwrap());

        assert!(parse_session_agent_message_rows(raw.as_bytes())
            .unwrap()
            .is_empty());
        assert_eq!(session_agent_transcript_revision(raw.as_bytes()), 1);
    }
}

fn session_agent_id_from_path(path: &std::path::Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("agent-"))
        .and_then(|id| id.strip_suffix(".jsonl"))
        .and_then(lingxi_core::types::AgentId::parse_prefixed)
        .map(|id| id.to_string())
}

async fn collect_session_agent_transcript_paths(
    root: &std::path::Path,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut dirs = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(dir) = dirs.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let meta = tokio::fs::symlink_metadata(&path).await?;
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                dirs.push(path);
                continue;
            }
            if file_type.is_file() && session_agent_id_from_path(&path).is_some() {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

async fn find_session_agent_transcript_path(
    root: &std::path::Path,
    agent_id: &str,
) -> std::io::Result<Option<std::path::PathBuf>> {
    Ok(collect_session_agent_transcript_paths(root)
        .await?
        .into_iter()
        .find(|path| session_agent_id_from_path(path).as_deref() == Some(agent_id)))
}

/// Lower a complete JSONL prefix into the same snapshot DTOs used by the
/// explicit transcript-load command. This is intentionally prefix-scoped: a
/// compact-summary mutation can trigger a replacement snapshot before later
/// visible live rows in the same filesystem read are emitted.
fn lower_session_agent_snapshot(raw: &[u8]) -> Vec<client::protocol::message::MessageDto> {
    client::adapter::lowering::lower_transcript(&parse_session_agent_messages(raw))
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Read the user-tier global TypeScript LSP policy. Project/local settings are
/// intentionally ignored so repository content cannot promote execution.
fn mobile_typescript_lsp_mode(
    settings_path: &std::path::Path,
) -> Result<lsp::LspActivationMode, String> {
    let raw = match std::fs::read_to_string(settings_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(lsp::LspActivationMode::Auto);
        }
        Err(error) => return Err(error.to_string()),
    };
    let root: serde_json::Value =
        serde_json::from_str(&raw).map_err(|error| format!("invalid settings JSON: {error}"))?;
    let Some(value) = root
        .get("lsp")
        .and_then(|value| value.get("typescript"))
        .and_then(|value| value.get("mode"))
    else {
        return Ok(lsp::LspActivationMode::Auto);
    };
    let mode = value
        .as_str()
        .and_then(lsp::LspActivationMode::from_wire)
        .ok_or_else(|| "settings lsp.typescript.mode must be auto, off, or on".to_string())?;
    Ok(mode)
}

fn persist_mobile_typescript_lsp_mode(
    settings_path: &std::path::Path,
    mode: lsp::LspActivationMode,
) -> Result<(), String> {
    let mut root = match std::fs::read_to_string(settings_path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|error| format!("invalid settings JSON: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.to_string()),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| "settings JSON root must be an object".to_string())?;
    let lsp = object.entry("lsp").or_insert_with(|| serde_json::json!({}));
    let lsp = lsp
        .as_object_mut()
        .ok_or_else(|| "settings lsp must be an object".to_string())?;
    let typescript = lsp
        .entry("typescript")
        .or_insert_with(|| serde_json::json!({}));
    let typescript = typescript
        .as_object_mut()
        .ok_or_else(|| "settings lsp.typescript must be an object".to_string())?;
    typescript.insert(
        "mode".to_string(),
        serde_json::Value::String(mode.wire_str().to_string()),
    );
    persist_mobile_settings_root(settings_path, &root)
}

fn persist_mobile_settings_root(
    settings_path: &std::path::Path,
    root: &serde_json::Value,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(root)
        .map_err(|error| format!("serialize settings JSON: {error}"))?;
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file_name = settings_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid settings path {}", settings_path.display()))?;
    static SETTINGS_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp_path = settings_path.with_file_name(format!(
        ".{file_name}.tmp-{}-{nanos}-{sequence}",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let write_result = (|| -> Result<(), String> {
        let mut file = options.open(&tmp_path).map_err(|error| error.to_string())?;

        {
            use std::io::Write as _;
            file.write_all(&bytes).map_err(|error| error.to_string())?;
        }

        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        std::fs::rename(&tmp_path, settings_path).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Some(parent) = settings_path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
    Ok(())
}

fn mobile_settings_write_lock(settings_path: &std::path::Path) -> Arc<Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        StdMutex<HashMap<std::path::PathBuf, std::sync::Weak<Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = locks.get(settings_path).and_then(std::sync::Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(settings_path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn live_session_agent_activity(
    message: &lingxi_core::types::ConversationMessage,
) -> Option<ClientTextDto> {
    let summarize_utf8 = |text: &str| {
        let code_units = text.encode_utf16().collect::<Vec<_>>();
        client::adapter::activity::summarize_activity_line(&code_units)
    };
    let summarize_projection = |text: &Utf16JsonProjection| {
        let code_units = text.string_units("")?;
        client::adapter::activity::summarize_activity_line(&code_units)
    };
    match message {
        lingxi_core::types::ConversationMessage::Assistant { content, .. }
        | lingxi_core::types::ConversationMessage::User { content, .. } => {
            content.iter().find_map(|block| match block {
                lingxi_core::types::ContentBlock::Text { text, .. } => summarize_utf8(text),
                lingxi_core::types::ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => client::adapter::activity::summarize_activity_line(utf16_code_units),
                lingxi_core::types::ContentBlock::ToolUse { name, .. } => summarize_utf8(name),
                lingxi_core::types::ContentBlock::ToolResult { content, .. } => {
                    summarize_projection(content)
                }
                _ => None,
            })
        }
        lingxi_core::types::ConversationMessage::System { content, .. } => {
            summarize_projection(content)
        }
    }
}

#[derive(Clone)]
struct BoundSessionAgentMeta {
    session_id: String,
    name: String,
    agent_type: String,
    model: String,
    model_profile: Option<String>,
    persistent: bool,
}

#[derive(Clone, Default)]
pub(super) struct SessionAgentTranscriptRows {
    rows: Vec<client::protocol::listings::SessionAgentMessageRowDto>,
    next_message_index: u64,
    revision: u64,
}

pub(super) type SessionAgentTranscriptCache =
    Arc<tokio::sync::Mutex<HashMap<String, SessionAgentTranscriptRows>>>;

struct MobileSessionAgentObserver {
    event_sink: Arc<dyn client::adapter::ClientEventSink>,
    session_uuid: Arc<std::sync::Mutex<String>>,
    bound_agents: tokio::sync::Mutex<HashMap<String, BoundSessionAgentMeta>>,
    tool_indexes: tokio::sync::Mutex<HashMap<String, client::adapter::turn::ToolUseIndex>>,
    message_indexes: tokio::sync::Mutex<HashMap<String, u64>>,
    transcript_cache: SessionAgentTranscriptCache,
}

impl MobileSessionAgentObserver {
    fn new(
        event_sink: Arc<dyn client::adapter::ClientEventSink>,
        session_uuid: Arc<std::sync::Mutex<String>>,
    ) -> Self {
        Self {
            event_sink,
            session_uuid,
            bound_agents: tokio::sync::Mutex::new(HashMap::new()),
            tool_indexes: tokio::sync::Mutex::new(HashMap::new()),
            message_indexes: tokio::sync::Mutex::new(HashMap::new()),
            transcript_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    fn allocated_session_id(&self) -> String {
        if let Some(session_id) = agent::workflow_transcript_subdir_override()
            .and_then(|path| path.ancestors().nth(3).map(std::path::Path::to_path_buf))
            .and_then(|path| path.file_name().map(|name| name.to_owned()))
            .and_then(|name| name.to_str().map(str::to_string))
        {
            return session_id;
        }
        self.session_uuid
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    async fn clear_agent_state(&self, agent_id: &str) {
        self.bound_agents.lock().await.remove(agent_id);
        self.tool_indexes.lock().await.remove(agent_id);
        self.message_indexes.lock().await.remove(agent_id);
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::subagent_spawn::SubagentSpawnObserver for MobileSessionAgentObserver {
    async fn on_event(&self, event: lingxi_core::host::subagent_spawn::SubagentObservation) {
        match event {
            lingxi_core::host::subagent_spawn::SubagentObservation::Allocated {
                agent_id,
                agent_type,
                name,
                model,
                model_profile,
                persistent,
                initial_message_index,
                ..
            } => {
                let session_id = self.allocated_session_id();
                let name = name.unwrap_or_else(|| agent_type.clone());
                self.bound_agents.lock().await.insert(
                    agent_id.to_string(),
                    BoundSessionAgentMeta {
                        session_id: session_id.clone(),
                        name: name.clone(),
                        agent_type: agent_type.clone(),
                        model: model.clone(),
                        model_profile: model_profile.clone(),
                        persistent,
                    },
                );
                self.message_indexes
                    .lock()
                    .await
                    .insert(agent_id.to_string(), initial_message_index);
                {
                    let mut cache = self.transcript_cache.lock().await;
                    let rows = cache.entry(agent_id.to_string()).or_default();
                    rows.next_message_index = rows.next_message_index.max(initial_message_index);
                }
                self.event_sink
                    .emit(ClientEvent::SessionAgentUpdated {
                        session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: agent_id.to_string(),
                            name: name.into(),
                            agent_type,
                            model: Some(model),
                            model_profile,
                            status: "running".to_string(),
                            latest_activity: None,
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    })
                    .await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::MessageRow {
                agent_id,
                message,
                message_index,
            } => {
                let agent_key = agent_id.to_string();
                let mut indexes = self.message_indexes.lock().await;
                let next = indexes.entry(agent_key).or_default();
                *next = (*next).max(message_index);
                drop(indexes);
                Box::pin(self.on_event(
                    lingxi_core::host::subagent_spawn::SubagentObservation::Message {
                        agent_id,
                        message,
                    },
                ))
                .await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::Message {
                agent_id,
                message,
            } => {
                let parked = matches!(
                    &message,
                    lingxi_core::types::ConversationMessage::System { subtype: Some(subtype), .. } if subtype == "agent_idle"
                );
                let hidden_wake = matches!(
                    &message,
                    lingxi_core::types::ConversationMessage::User { is_meta: true, .. }
                );
                if parked || hidden_wake {
                    let agent_key = agent_id.to_string();
                    let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned()
                    else {
                        return;
                    };
                    // Foreground agents park between turns and can wake again,
                    // including agents that were allocated as nonpersistent.
                    // Keep their binding and visible-message indexes intact.
                    self.event_sink
                        .emit(ClientEvent::SessionAgentUpdated {
                            session_id: bound.session_id,
                            agent: SessionAgentSummaryDto {
                                agent_id: agent_key,
                                name: bound.name.into(),
                                agent_type: bound.agent_type,
                                model: Some(bound.model),
                                model_profile: bound.model_profile,
                                status: if parked { "completed" } else { "running" }.to_string(),
                                latest_activity: None,
                                updated_at_ms: Some(unix_time_ms()),
                            },
                        })
                        .await;
                    return;
                }

                if !session_agent_conversation_is_visible(&message) {
                    return;
                }
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                let dto = {
                    let mut indexes = self.tool_indexes.lock().await;
                    let index = indexes.entry(agent_key.clone()).or_default();
                    client::adapter::lowering::lower_conversation_message_with(&message, index)
                };
                let message_index = {
                    let mut indexes = self.message_indexes.lock().await;
                    let next = indexes.entry(agent_key.clone()).or_default();
                    let current = *next;
                    *next = next.saturating_add(1);
                    current
                };
                let row = client::protocol::listings::SessionAgentMessageRowDto {
                    message_index,
                    message_uuid: message.id().as_uuid().to_string(),
                    message: dto,
                    api_error_json: None,
                };
                {
                    let mut cache = self.transcript_cache.lock().await;
                    let rows = cache.entry(agent_key.clone()).or_default();
                    rows.next_message_index =
                        rows.next_message_index.max(message_index.saturating_add(1));
                    rows.revision = rows.revision.saturating_add(1);
                    rows.rows.push(row.clone());
                }
                self.event_sink
                    .emit(ClientEvent::SessionAgentMessage {
                        session_id: bound.session_id.clone(),
                        agent_id: agent_key.clone(),
                        row,
                    })
                    .await;
                self.event_sink
                    .emit(ClientEvent::SessionAgentUpdated {
                        session_id: bound.session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: agent_key.clone(),
                            name: bound.name.into(),
                            agent_type: bound.agent_type,
                            model: Some(bound.model),
                            model_profile: bound.model_profile,
                            status: "running".to_string(),
                            latest_activity: live_session_agent_activity(&message),
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    })
                    .await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::ServerFallbackTombstone {
                agent_id,
                message,
                display_only,
            } => {
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                {
                    let mut cache = self.transcript_cache.lock().await;
                    if let Some(rows) = cache.get_mut(&agent_key) {
                        rows.rows
                            .retain(|row| row.message_uuid != message.uuid.as_uuid().to_string());
                        rows.revision = rows.revision.saturating_add(1);
                    }
                }
                self.event_sink
                    .emit(ClientEvent::SessionAgentTombstone {
                        session_id: bound.session_id,
                        agent_id: agent_key,
                        message_uuid: message.uuid.as_uuid().to_string(),
                        display_only,
                    })
                    .await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::ServerFallbackApiErrorRow {
                agent_id,
                row,
                message_index,
            } => {
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                let message = row.query_message();
                let dto = {
                    let mut indexes = self.tool_indexes.lock().await;
                    let index = indexes.entry(agent_key.clone()).or_default();
                    client::adapter::lowering::lower_conversation_message_with(&message, index)
                };
                let message_index = {
                    let mut indexes = self.message_indexes.lock().await;
                    let next = indexes.entry(agent_key.clone()).or_default();
                    *next = (*next).max(message_index.saturating_add(1));
                    message_index
                };
                let row_dto = client::protocol::listings::SessionAgentMessageRowDto {
                    message_index,
                    message_uuid: row.uuid.as_uuid().to_string(),
                    message: dto,
                    api_error_json: Some(
                        serde_json::to_string(&row).expect("fallback API-error row serializes"),
                    ),
                };
                {
                    let mut cache = self.transcript_cache.lock().await;
                    let rows = cache.entry(agent_key.clone()).or_default();
                    rows.next_message_index =
                        rows.next_message_index.max(message_index.saturating_add(1));
                    rows.revision = rows.revision.saturating_add(1);
                    rows.rows.push(row_dto.clone());
                }
                self.event_sink
                    .emit(ClientEvent::SessionAgentMessage {
                        session_id: bound.session_id,
                        agent_id: agent_key,
                        row: row_dto,
                    })
                    .await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::Completed {
                agent_id, ..
            } => {
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                let terminal = !bound.persistent;
                self.event_sink
                    .emit(ClientEvent::SessionAgentUpdated {
                        session_id: bound.session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: agent_key.clone(),
                            name: bound.name.into(),
                            agent_type: bound.agent_type,
                            model: Some(bound.model),
                            model_profile: bound.model_profile,
                            // `completed` either way: claude-code labels a
                            // finished background agent `(done)` whether or not
                            // it can still be resumed, and reserves `idle` for
                            // the footer group and for teammates. Resumability
                            // stays in `terminal` below, which decides whether
                            // the agent's state is cleared.
                            status: "completed".to_string(),
                            latest_activity: None,
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    })
                    .await;
                if terminal {
                    self.clear_agent_state(&agent_key).await;
                }
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::Failed { agent_id, error } => {
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                self.event_sink
                    .emit(ClientEvent::SessionAgentUpdated {
                        session_id: bound.session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: agent_key.clone(),
                            name: bound.name.into(),
                            agent_type: bound.agent_type,
                            model: Some(bound.model),
                            model_profile: bound.model_profile,
                            status: "failed".to_string(),
                            latest_activity: {
                                let code_units = error.encode_utf16().collect::<Vec<_>>();
                                client::adapter::activity::summarize_activity_line(&code_units)
                            },
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    })
                    .await;
                self.clear_agent_state(&agent_key).await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::Killed { agent_id } => {
                let agent_key = agent_id.to_string();
                let Some(bound) = self.bound_agents.lock().await.get(&agent_key).cloned() else {
                    return;
                };
                self.event_sink
                    .emit(ClientEvent::SessionAgentUpdated {
                        session_id: bound.session_id,
                        agent: SessionAgentSummaryDto {
                            agent_id: agent_key.clone(),
                            name: bound.name.into(),
                            agent_type: bound.agent_type,
                            model: Some(bound.model),
                            model_profile: bound.model_profile,
                            status: "killed".to_string(),
                            latest_activity: None,
                            updated_at_ms: Some(unix_time_ms()),
                        },
                    })
                    .await;
                self.clear_agent_state(&agent_key).await;
            }
            lingxi_core::host::subagent_spawn::SubagentObservation::Progress { .. } => {}
            lingxi_core::host::subagent_spawn::SubagentObservation::Retry { .. } => {}
        }
    }
}

fn parse_session_agent_messages(raw: &[u8]) -> Vec<lingxi_core::types::ConversationMessage> {
    let mut messages = Vec::new();
    for line in raw.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = value.get("message") else {
            continue;
        };
        let Ok(conversation) =
            serde_json::from_value::<lingxi_core::types::ConversationMessage>(message.clone())
        else {
            continue;
        };
        if matches!(
            &conversation,
            lingxi_core::types::ConversationMessage::System {
                subtype: Some(subtype),
                ..
            } if subtype.starts_with("agent_")
        ) {
            continue;
        }
        if !session_agent_conversation_is_visible(&conversation) {
            continue;
        }
        messages.push(conversation);
    }
    messages
}

#[async_trait]
impl cron::scheduler::SessionCronDelivery for MobileWakeupDelivery {
    async fn clear_queued(&self) {
        let ids: Vec<_> = self
            .queue
            .snapshot()
            .await
            .into_iter()
            .filter(|command| {
                command.source == msgqueue::QueueSource::Cron
                    && command.uuid.starts_with("cron-fire-")
            })
            .map(|command| command.uuid)
            .collect();
        self.queue.remove(&ids, "session changed").await;
    }
    async fn is_loading(&self) -> bool {
        self.queue.has_active_turn().await
    }

    async fn enqueue(&self, fire: cron::scheduler::SessionCronFire) -> Result<(), String> {
        if fire.cron.is_empty() {
            self.queue
                .enqueue(msgqueue::QueuedCommand {
                    scheduled_task_id: None,
                    scheduled_fire_id: None,
                    uuid: format!("cron-fire-{}", fire.id),
                    content: msgqueue::QueuedCommandContent::UserInput { text: fire.prompt },
                    priority: msgqueue::QueuePriority::Later,
                    queued_at: std::time::SystemTime::now(),
                    source: msgqueue::QueueSource::Cron,
                    agent_id: None,
                    skip_slash_commands: true,
                    is_meta: true,
                })
                .await;
            return Ok(());
        }
        let task = tool_cron::WakeupTask::scheduled(&fire);
        let id = task.command_id();
        tool_cron::WakeupDelivery::deliver(self, &id, fire.prompt, String::new(), task).await;
        Ok(())
    }
}

/// Default `ListSessions` row cap when the command omits an explicit `limit`
/// (SESSIONS/HISTORY). Mirrors the CLI `/resume` default (`apps/cli/src/run.rs`
/// passes `5`).
const DEFAULT_SESSION_LIST_LIMIT: usize = 5;

/// One received command awaiting its admission result. The registry contains
/// only real submissions, so an unknown Cancel cannot create a tombstone.
pub(crate) struct PendingSubmission {
    session_id: String,
    turn_id: u64,
    submitting_handle: std::sync::Weak<MobileEngineHandle>,
    shared_lease: StdMutex<Option<crate::mobile::turn_durability::PendingAdmissionLease>>,
    decision: StdMutex<PendingAdmissionDecision>,
    cancel_requested: AtomicBool,
    suppressed: AtomicBool,
    cancel_without_checkpoint: AtomicBool,
    owner: StdMutex<Option<std::sync::Weak<ActiveTurn>>>,
    result: StdMutex<Option<Result<(), ClientError>>>,
    finished: Notify,
    #[cfg(test)]
    test_stage: std::sync::atomic::AtomicU8,
    #[cfg(test)]
    cancel_requested_notify: Notify,
    #[cfg(test)]
    owner_barrier: StdMutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

#[derive(Default)]
struct PendingAdmissionDecision {
    completed: bool,
    local_command_started: bool,
}

enum PendingCancelDecision {
    Requested,
    LocalCommandStarted,
    Completed,
}

impl PendingSubmission {
    fn new(
        session_id: String,
        turn_id: u64,
        shared_lease: crate::mobile::turn_durability::PendingAdmissionLease,
        submitting_handle: std::sync::Weak<MobileEngineHandle>,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            submitting_handle,
            shared_lease: StdMutex::new(Some(shared_lease)),
            decision: StdMutex::new(PendingAdmissionDecision::default()),
            cancel_requested: AtomicBool::new(false),
            suppressed: AtomicBool::new(false),
            cancel_without_checkpoint: AtomicBool::new(false),
            owner: StdMutex::new(None),
            result: StdMutex::new(None),
            finished: Notify::new(),
            #[cfg(test)]
            test_stage: std::sync::atomic::AtomicU8::new(0),
            #[cfg(test)]
            cancel_requested_notify: Notify::new(),
            #[cfg(test)]
            owner_barrier: StdMutex::new(None),
        }
    }

    fn request_cancel(&self) -> PendingCancelDecision {
        let decision = self.decision.lock().unwrap_or_else(|e| e.into_inner());
        if decision.local_command_started {
            return PendingCancelDecision::LocalCommandStarted;
        }
        if decision.completed {
            return PendingCancelDecision::Completed;
        }
        self.cancel_requested.store(true, Ordering::Release);
        #[cfg(test)]
        self.cancel_requested_notify.notify_waiters();
        PendingCancelDecision::Requested
    }

    fn try_start_local_command(&self) -> bool {
        let mut decision = self.decision.lock().unwrap_or_else(|e| e.into_inner());
        if decision.completed || self.cancel_requested.load(Ordering::Acquire) {
            return false;
        }
        decision.local_command_started = true;
        true
    }

    #[cfg(test)]
    async fn wait_for_cancel_request(&self) {
        loop {
            let notified = self.cancel_requested_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancel_requested.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn bind_owner(&self, owner: &Arc<ActiveTurn>) {
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(owner));
    }

    fn bound_owner(&self) -> Option<Arc<ActiveTurn>> {
        self.owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    fn complete(&self, result: Result<(), ClientError>) {
        let mut decision = self.decision.lock().unwrap_or_else(|e| e.into_inner());
        self.complete_locked(&mut decision, result);
        drop(decision);
        self.finished.notify_waiters();
    }

    // Atomically choose between a received Stop and finishing an otherwise
    // successful queue admission. Queue consumers stay fenced by the lease
    // until this decision is made.
    fn complete_if_not_cancelled(&self) -> bool {
        let mut decision = self.decision.lock().unwrap_or_else(|e| e.into_inner());
        if decision.completed || self.cancel_requested.load(Ordering::Acquire) {
            return false;
        }
        self.complete_locked(&mut decision, Ok(()));
        drop(decision);
        self.finished.notify_waiters();
        true
    }

    fn complete_locked(
        &self,
        decision: &mut PendingAdmissionDecision,
        result: Result<(), ClientError>,
    ) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            // An error after Stop can mean that a checkpoint or its
            // cancellation was only partly published. Do not let another
            // handle claim this identity merely because this task finished.
            if result.is_err() && self.cancel_requested.load(Ordering::Acquire) {
                if let Some(lease) = self
                    .shared_lease
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    lease.mark_unresolved();
                }
            }
            *slot = Some(result);
            decision.completed = true;
            // Once admission and its cleanup are finished, observers may retain
            // this object while waiting for the result without fencing the store.
            self.shared_lease
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
    }

    async fn wait(&self) -> Result<(), ClientError> {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

struct PendingSubmissionGuard {
    registry: Arc<StdMutex<Vec<Arc<PendingSubmission>>>>,
    pending: Arc<PendingSubmission>,
}

impl Drop for PendingSubmissionGuard {
    fn drop(&mut self) {
        self.pending.complete(Err(ClientError::Internal {
            message: "pending mobile submission terminated before admission".into(),
        }));
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|item| !Arc::ptr_eq(item, &self.pending));
    }
}

/// Connection-scoped ownership record for one streamed turn.
struct ActiveTurn {
    /// Optional client correlator supplied by `SendPrompt`. `Cancel(Some(id))`
    /// may only affect the owner carrying the same id; Android's legacy
    /// `Cancel(None)` intentionally targets whichever turn is current.
    turn_id: Option<u64>,
    /// Stable session captured when the owner slot was reserved.
    session_id: String,
    /// Shared across handles; clones travel with tool work that can outlive
    /// the primary turn task.
    live_owner: Option<crate::mobile::turn_durability::LiveTurnOwnerLease>,
    permission_owner_id: Option<u64>,
    cancel: CancellationToken,
    /// Linearizes synchronous side-effect admission with lifecycle stop.
    admission: StdMutex<()>,
    queue_owner: Arc<()>,
    /// Members actually claimed by this live owner during its mid-turn drain.
    /// Durable batch metadata alone does not identify the active parent.
    mid_turn_members: StdMutex<std::collections::HashSet<u64>>,
    task: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    completed: AtomicBool,
    /// Set before asking the orchestrator to unwind for a platform pause.
    /// Unlike an explicit cancel, a quiesced turn must not surface the
    /// orchestrator's `Cancelled` outcome or any late live payloads.
    quiescing: AtomicBool,
    /// An explicit Cancel still terminalizes a turn whose lifecycle pause won
    /// the executor's cancellation reason first.
    explicit_cancel_requested: AtomicBool,
    /// Set before the terminal event is forwarded to the foreign listener.
    /// Any subsequent live-turn event is stale and must be discarded.
    terminal_emitted: AtomicBool,
    /// Emit at most one reconnect notice if ordered delivery fails closed.
    delivery_failed: AtomicBool,
    /// Journal persistence failed ambiguously; keep live delivery but never
    /// advance this turn's recovery cursor again in this execution.
    journal_degraded: AtomicBool,
    /// A durable unsafe boundary precedes every material event delivered to
    /// the client, including appends which have not yet reached an fsync.
    output_recovery_fenced: AtomicBool,
    /// Ordinary terminal event held until a requested media continuation has
    /// either started or been rejected by the current turn owner.
    deferred_terminal: StdMutex<Option<ClientEvent>>,
    completion: Notify,
}

tokio::task_local! {
    /// Captured by the actual turn task, never inferred from a later slot.
    static MOBILE_TURN_ORIGIN: std::sync::Weak<ActiveTurn>;
}

struct MobileWakeupDelivery {
    transition: Arc<Mutex<()>>,
    queue: Arc<msgqueue::MessageQueueManager>,
    orchestrator: std::sync::Weak<ConversationOrchestrator>,
    events: Arc<dyn client::adapter::ClientEventSink>,
    state: Arc<tool_cron::LoopRuntime>,
}

#[async_trait]
impl tool_cron::WakeupDelivery for MobileWakeupDelivery {
    async fn deliver(
        &self,

        command_id: &str,
        prompt: Utf16JsonProjection,
        _reason: String,
        task: tool_cron::WakeupTask,
    ) {
        let _transition = loop {
            let guard = self.transition.lock().await;
            if !self.queue.has_active_turn().await {
                break guard;
            }
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        let Some(orchestrator) = self.orchestrator.upgrade() else {
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64);
        let streak = task
            .task_kind_loop
            .then(|| self.state.noop_streak())
            .flatten();
        let (message, companion) = task.lines(now_ms, streak);
        let since_ms = streak.map_or(0, |(_, since)| {
            since
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis() as u64)
        });
        let count = streak.map_or(0, |(count, _)| count);
        if let Err(error) = orchestrator
            .append_scheduled_loop_wakeup(
                message.clone(),
                companion.clone(),
                count,
                since_ms,
                orchestrator::ScheduledLoopFire {
                    fire_id: task.fire_id,
                    task_id: task.task_id.clone(),
                    cron: task.cron.clone(),
                    prompt: task.display_prompt.clone(),
                    task_kind_loop: task.task_kind_loop,
                },
            )
            .await
        {
            tracing::warn!(%error, "mobile: could not persist /loop wakeup boundary");
        }
        self.events
            .emit(if task.task_kind_loop {
                ClientEvent::LoopWakeup {
                    message,
                    companion,
                    streak: count,
                    since_ms,
                }
            } else {
                ClientEvent::ScheduledTaskFire { message }
            })
            .await;
        self.queue
            .enqueue(msgqueue::QueuedCommand {
                scheduled_task_id: Some(task.task_id.clone()),
                scheduled_fire_id: Some(task.fire_id.as_uuid().to_string()),
                uuid: command_id.into(),
                content: msgqueue::QueuedCommandContent::UserInput { text: prompt },
                priority: msgqueue::QueuePriority::Later,
                queued_at: std::time::SystemTime::now(),
                source: msgqueue::QueueSource::Cron,
                agent_id: None,
                skip_slash_commands: true,
                is_meta: true,
            })
            .await;
    }

    async fn cancel_queued(&self) -> Vec<Utf16JsonProjection> {
        let commands = self
            .queue
            .get_by_max_priority(msgqueue::QueuePriority::Later, |command| {
                command.source == msgqueue::QueueSource::Cron
                    && command.uuid.starts_with("loop-wakeup-")
                    && command.is_main_thread()
            })
            .await;
        let ids = commands
            .iter()
            .map(|command| command.uuid.clone())
            .collect::<Vec<_>>();
        self.queue.remove(&ids, "dynamic loop cancelled").await;
        commands
            .iter()
            .filter_map(|command| command.text_projection().cloned())
            .collect()
    }
}

async fn settle_mobile_loop_turn(
    orchestrator: &ConversationOrchestrator,
    scheduler: &Arc<dyn tool_cron::WakeupScheduler>,
    cancel: &CancellationToken,
    reason: &orchestrator::prompt::mid_turn_input::CancelReasonFlag,
    human: bool,
) {
    let Some(state) = scheduler.loop_runtime() else {
        return;
    };
    let span = orchestrator.turn_span().snapshot();
    let user_aborted = cancel.is_cancelled()
        && reason.get() == orchestrator::prompt::mid_turn_input::CancelReason::UserInterrupt;
    if span.compactions > 0 {
        state.veto_tick(tool_cron::LoopFoldVeto::BlockingSystemInSpan);
        state.reset_autonomous_loop_delivered();
    }
    if user_aborted || span.aborts > 0 {
        state.veto_tick(tool_cron::LoopFoldVeto::ToolAbort);
    }
    if span.denials > 0 {
        state.veto_tick(tool_cron::LoopFoldVeto::ToolDenial);
    }
    if cancel.is_cancelled() && !user_aborted {
        state.veto_tick(tool_cron::LoopFoldVeto::QueuedCommand);
    }
    if state.in_flight_prompt().is_none()
        && (human || span.compactions > 0 || span.denials > 0 || span.aborts > 0)
    {
        state.invalidate_noop_streak();
    }
    tool_cron::settle_loop_tick(
        &state,
        tool_cron::LoopSpanCounts {
            tool_uses: span.tool_uses,
            span_len: span.messages,
        },
    );
    if user_aborted {
        tool_cron::cancel_dynamic_loop_on_user_abort(scheduler).await;
    } else {
        tool_cron::maybe_arm_keepalive_with_runtime(scheduler, &state).await;
    }
}

pub(super) async fn ensure_mobile_session_identity(
    session_writer: &session::jsonl::writer::JsonlWriter,
    lingxi_home: &std::path::Path,
    session_cwd: &str,
    session_id: &str,
) -> Result<(), ClientError> {
    let expected_path =
        orchestrator::transcript_paths::main_transcript_path(lingxi_home, session_cwd, session_id);
    let transcript_path = session_writer.active_path();
    if transcript_path != expected_path {
        return Err(ClientError::Internal {
            message: format!(
                "session transcript identity import failed: writer is not active for reserved session {session_id}"
            ),
        });
    }
    match tokio::fs::symlink_metadata(&transcript_path).await {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(ClientError::Internal {
                message: format!(
                    "session transcript identity import failed: transcript is not a regular file: {}",
                    transcript_path.display()
                ),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ClientError::Internal {
                message: format!(
                    "session transcript identity import failed: could not inspect transcript {}: {error}",
                    transcript_path.display()
                ),
            });
        }
    }
    let Some(filename) = transcript_path.file_name() else {
        return Err(ClientError::Internal {
            message: "session transcript identity import failed: transcript has no filename".into(),
        });
    };
    let mut identity_filename = filename.to_os_string();
    identity_filename.push(branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX);
    let identity_path = transcript_path.with_file_name(identity_filename);
    match tokio::fs::symlink_metadata(&identity_path).await {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(ClientError::Internal {
            message: format!(
                "session transcript identity import failed: identity sidecar is not a regular file: {}",
                identity_path.display()
            ),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            session_identity_snapshot_for_path(session_writer, &transcript_path)
                .await
                .map(|_| ())
                .map_err(|error| ClientError::Internal {
                    message: format!("session transcript identity import failed: {error}"),
                })
        }
        Err(error) => Err(ClientError::Internal {
            message: format!(
                "session transcript identity import failed: could not inspect identity sidecar {}: {error}",
                identity_path.display()
            ),
        }),
    }
}

struct MobileMsgQueueInput {
    queue: Arc<msgqueue::MessageQueueManager>,
    loop_state: Arc<tool_cron::LoopRuntime>,
    durable_turns: Arc<DurableTurnStore>,
    active_session_uuid: Arc<std::sync::Mutex<String>>,
    active_turn: Option<Arc<Mutex<Option<Arc<ActiveTurn>>>>>,
}

#[async_trait]
impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for MobileMsgQueueInput {
    fn supports_goal_retries(&self) -> bool {
        true
    }
    async fn has_queued_goal_work(&self) -> bool {
        self.queue.has_main_thread_commands().await
    }
    async fn enqueue_goal_retry(
        &self,
        id: String,
        body: Utf16JsonProjection,
        cancel: CancellationToken,
    ) {
        self.queue.enqueue_goal_retry(id, body, cancel).await;
    }

    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        let commands = self
            .queue
            .take_mid_turn_commands_until(|command| !is_mobile_dispatch_command(&command.uuid))
            .await?;
        let prompt = msgqueue::join_prompt_values(&commands).map(|(text, _)| text);
        if prompt.is_some() {
            self.loop_state
                .veto_tick(tool_cron::LoopFoldVeto::ForeignUserInput);
        }
        prompt
    }

    async fn take_mid_turn_batch(
        &self,
    ) -> Result<Option<Vec<orchestrator::prompt::mid_turn_input::MidTurnInput>>, String> {
        use orchestrator::prompt::mid_turn_input::{MidTurnInput, PendingMidTurnPromptIdentity};

        let session_id = self
            .active_session_uuid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mobile_prefix = loop {
            let Some(commands) = self
                .queue
                .peek_mid_turn_commands_until(|command| {
                    command
                        .uuid
                        .starts_with(&format!("mobile-prompt:{session_id}:"))
                })
                .await
            else {
                break None;
            };
            let Some(first_command) = commands.first() else {
                break None;
            };
            let owner_id = parse_mobile_prompt_command(&first_command.uuid, &session_id)
                .ok_or_else(|| "mobile prompt dispatch identity is malformed".to_string())?;
            let owner = match self.durable_turns.load(&session_id, owner_id) {
                Ok(owner) => owner,
                Err(DurableTurnStoreError::NotFound { .. })
                | Err(DurableTurnStoreError::Terminal { .. }) => {
                    self.queue
                        .consume(
                            std::slice::from_ref(&first_command.uuid),
                            "remove stale durable mobile prompt dispatch",
                        )
                        .await;
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            };
            let pending = match self.durable_turns.scan_pending(&session_id) {
                Ok(pending) => pending,
                Err(DurableTurnStoreError::UncertainQueuedTurn { .. }) => return Ok(None),
                Err(error) => return Err(error.to_string()),
            };
            let is_dispatch_pending = pending.iter().any(|checkpoint| checkpoint.turn_id == owner_id);
            if is_dispatch_pending
                && owner.state.is_none()
                && matches!(
                    owner.queue_delivery,
                    QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                )
            {
                break Some((commands, owner));
            }
            let stale_ids = owner
                .coalesced_batch
                .as_ref()
                .map(|batch| batch.member_turn_ids.clone())
                .unwrap_or_else(|| vec![owner_id]);
            let command_ids = stale_ids
                .iter()
                .map(|turn_id| mobile_prompt_command_uuid(&session_id, *turn_id))
                .collect::<Vec<_>>();
            self.queue
                .consume(&command_ids, "remove acknowledged mobile prompt dispatch")
                .await;
        };
        if let Some((mobile_commands, owner)) = mobile_prefix {
            // This mid-turn source only knows how to inject normal user text.
            // Leave Bash at the head of the durable dispatch queue for the
            // between-turn direct executor; returning it here would silently
            // turn a side-effecting command back into model prompt text.
            if owner.prompt_mode == Some(PromptModeDto::Bash) {
                return Ok(None);
            }
            // An already persisted batch is an append-once boundary. A later
            // arrival remains queued for the next drain pass.
            let member_turn_ids = if let Some(batch) = owner.coalesced_batch.as_ref() {
                batch.member_turn_ids.clone()
            } else {
                // Bash is a direct side-effecting command, not ordinary prompt
                // text. It is always its own claim boundary. Normal prompts
                // coalesce only across the contiguous Normal prefix; a later
                // Bash or other mode remains at the front of the next drain.
                let mut member_turn_ids = Vec::new();
                for command in &mobile_commands {
                    let turn_id = parse_mobile_prompt_command(&command.uuid, &session_id)
                        .ok_or_else(|| {
                            "mobile prompt dispatch identity is malformed".to_string()
                        })?;
                    let checkpoint = match self.durable_turns.load(&session_id, turn_id) {
                        Ok(checkpoint) => checkpoint,
                        Err(DurableTurnStoreError::NotFound { .. })
                        | Err(DurableTurnStoreError::Terminal { .. }) => break,
                        Err(error) => return Err(error.to_string()),
                    };
                    let is_bash = checkpoint.prompt_mode == Some(PromptModeDto::Bash);
                    if is_bash {
                        break;
                    }
                    member_turn_ids.push(turn_id);
                }
                member_turn_ids
            };
            let active_guard = match &self.active_turn {
                Some(active) => Some(active.lock().await),
                None => None,
            };
            let origin = MOBILE_TURN_ORIGIN
                .try_with(std::sync::Weak::upgrade)
                .ok()
                .flatten();
            let live_owner = active_guard
                .as_ref()
                .and_then(|guard| guard.as_ref())
                .filter(|owner| {
                    origin
                        .as_ref()
                        .is_some_and(|origin| Arc::ptr_eq(owner, origin))
                        && owner.session_id == session_id
                        && !owner.is_quiescing()
                        && !owner.cancel.is_cancelled()
                });
            if self.active_turn.is_some() && live_owner.is_none() {
                return Ok(None);
            }
            // Pin the prospective members before persistence. An atomic
            // rename may succeed even when its sync reports failure; in that
            // case a subsequent Stop must still find this exact live owner.
            // The route additionally checks the durable batch, so a failed
            // write that never claimed anything cannot misroute a Stop.
            if let Some(owner) = live_owner {
                owner
                    .mid_turn_members
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(member_turn_ids.iter().copied());
            }
            let batch_result = self.durable_turns.with_no_pending_admissions(
                &session_id,
                &member_turn_ids,
                || {
                    self.durable_turns
                        .prepare_mid_turn_batch(&session_id, &member_turn_ids)
                },
            );
            drop(active_guard);
            let batch = match batch_result {
                Ok(batch) => batch,
                Err(DurableTurnStoreError::PendingAdmission { .. }) => return Ok(None),
                Err(DurableTurnStoreError::Terminal { .. })
                | Err(DurableTurnStoreError::NotFound { .. }) => {
                    // A cancellation can win before the owner envelope is
                    // committed. Retire only those stale dispatch markers;
                    // survivors remain queued and form the next ordered batch.
                    let pending = match self.durable_turns.scan_pending(&session_id) {
                        Ok(pending) => pending,
                        Err(DurableTurnStoreError::UncertainQueuedTurn { .. }) => return Ok(None),
                        Err(error) => return Err(error.to_string()),
                    };
                    let pending_member_ids = pending
                        .into_iter()
                        .flat_map(|checkpoint| {
                            let mut members = vec![checkpoint.turn_id];
                            if let Some(batch) = checkpoint.coalesced_batch {
                                members.extend(batch.member_turn_ids);
                            }
                            members
                        })
                        .collect::<Vec<_>>();
                    let command_ids = member_turn_ids
                        .iter()
                        .filter(|turn_id| !pending_member_ids.contains(*turn_id))
                        .map(|turn_id| mobile_prompt_command_uuid(&session_id, *turn_id))
                        .collect::<Vec<_>>();
                    if !command_ids.is_empty() {
                        self.queue
                            .consume(
                                &command_ids,
                                "retire prompt canceled before mid-turn admission",
                            )
                            .await;
                    }
                    return Ok(None);
                }
                Err(error) => return Err(error.to_string()),
            };
            let mut inputs = Vec::with_capacity(batch.member_turn_ids.len());
            let mut client_turn_ids = Vec::with_capacity(batch.member_turn_ids.len());
            for turn_id in &batch.member_turn_ids {
                let checkpoint = self
                    .durable_turns
                    .load(&session_id, *turn_id)
                    .map_err(|error| error.to_string())?;
                client_turn_ids.push(
                    checkpoint
                        .original_client_turn_id
                        .unwrap_or(checkpoint.turn_id)
                        .to_string(),
                );
                inputs.push(MidTurnInput {
                    text: checkpoint.prompt,
                    images: checkpoint
                        .images
                        .into_iter()
                        .map(|image| lingxi_core::types::ImageSource::Base64 {
                            media_type: image.media_type,
                            data: image.base64,
                        })
                        .collect(),
                    origin_kind: None,
                    projected_content: None,
                    source_message_uuid: None,
                    queue_delivery: None,
                    pending_prompt_identity: None,
                });
            }
            let first = inputs
                .first_mut()
                .ok_or_else(|| "mobile prompt batch contains no entries".to_string())?;
            first.pending_prompt_identity = Some(PendingMidTurnPromptIdentity {
                session_id,
                owner_turn_id: batch.owner_turn_id,
                message_id: batch.aggregate_identity.message_id,
                prompt_id: batch.aggregate_identity.prompt_id,
                transcript_row_token: Some(format!("client-turn:{}", client_turn_ids.join(","))),
                member_turn_ids: batch.member_turn_ids,
            });
            self.loop_state
                .veto_tick(tool_cron::LoopFoldVeto::ForeignUserInput);
            return Ok(Some(inputs));
        }

        // Other Next-priority producers keep the existing volatile behavior,
        // but stop at a durable Mobile prompt so the two source classes are
        // never merged into one unrepeatable transcript row.
        let Some(commands) = self
            .queue
            .take_mid_turn_commands_until(|command| !is_mobile_dispatch_command(&command.uuid))
            .await
        else {
            return Ok(None);
        };
        let inputs = commands
            .into_iter()
            .filter_map(|command| {
                command.text_projection().cloned().map(|text| MidTurnInput {
                    text,
                    images: Vec::new(),
                    origin_kind: None,
                    projected_content: None,
                    source_message_uuid: None,
                    queue_delivery: None,
                    pending_prompt_identity: None,
                })
            })
            .collect::<Vec<_>>();
        if !inputs.is_empty() {
            self.loop_state
                .veto_tick(tool_cron::LoopFoldVeto::ForeignUserInput);
        }
        Ok(Some(inputs))
    }

    async fn input_consumed(
        &self,
        inputs: &[orchestrator::prompt::mid_turn_input::MidTurnInput],
    ) -> Result<(), orchestrator::prompt::mid_turn_input::MidTurnConsumptionError> {
        let Some(identity) = inputs
            .iter()
            .find_map(|input| input.pending_prompt_identity.as_ref())
        else {
            return Ok(());
        };
        #[cfg(test)]
        if let Some((entered, release)) = self.durable_turns.take_midturn_ack_barrier() {
            entered.notify_one();
            release.notified().await;
        }
        // Serialize the durable acknowledgement with cancellation of this
        // exact producer. A stopped owner must not turn an uncertain batch
        // back into a replayable one after the cancellation fence is written.
        let active = match &self.active_turn {
            Some(active) => Some(active.lock().await),
            None => None,
        };
        if let Some(active) = &active {
            let origin = MOBILE_TURN_ORIGIN
                .try_with(std::sync::Weak::upgrade)
                .ok()
                .flatten();
            let matches = active.as_ref().is_some_and(|owner| {
                origin
                    .as_ref()
                    .is_some_and(|origin| Arc::ptr_eq(owner, origin))
                    && owner.session_id == identity.session_id
                    && !owner.is_quiescing()
                    && !owner.cancel.is_cancelled()
            });
            if !matches {
                return Err(
                    orchestrator::prompt::mid_turn_input::MidTurnConsumptionError::Interrupted,
                );
            }
        }
        self.durable_turns
            .acknowledge_mid_turn_batch(
                &identity.session_id,
                identity.owner_turn_id,
                &identity.member_turn_ids,
            )
            .map_err(|error| {
                orchestrator::prompt::mid_turn_input::MidTurnConsumptionError::Failure(
                    error.to_string(),
                )
            })?;
        drop(active);
        let command_ids = identity
            .member_turn_ids
            .iter()
            .map(|turn_id| mobile_prompt_command_uuid(&identity.session_id, *turn_id))
            .collect::<Vec<_>>();
        self.queue
            .consume(&command_ids, "durable mobile prompt appended")
            .await;
        Ok(())
    }
}

struct MobileBashExecution {
    result: DurableBashResult,
    interrupted: bool,
}

async fn execute_mobile_bash_tool(
    orchestrator: &ConversationOrchestrator,
    command: &Utf16JsonProjection,
    cancellation: CancellationToken,
) -> MobileBashExecution {
    let result = orchestrator
        .call_direct_bash_from_host(command.clone(), cancellation)
        .await;
    let result = match result {
        Ok(result) => result,
        Err(tool_api::ToolError::Aborted) => {
            return MobileBashExecution {
                result: DurableBashResult {
                    stdout: String::new(),
                    stderr: "The shell command was interrupted before a result was available."
                        .into(),
                    exit_code: 130,
                    interrupted: false,
                },
                interrupted: true,
            };
        }
        Err(error) => {
            let message = error.model_facing_message();
            return MobileBashExecution {
                result: DurableBashResult {
                    stdout: String::new(),
                    stderr: message.string_at("").unwrap_or_default().to_owned(),
                    exit_code: 1,
                    interrupted: false,
                },
                interrupted: false,
            };
        }
    };
    let payload = result.data;
    let stdout = payload.string_at("/stdout").unwrap_or_default().to_owned();
    let mut stderr = payload.string_at("/stderr").unwrap_or_default().to_owned();
    let interrupted = payload
        .at_pointer("/interrupted")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let is_error = result.is_error
        || payload
            .at_pointer("/is_error")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
    let exit_code = payload
        .number_at("/exit_code")
        .filter(|number| number.is_finite() && number.fract() == 0.0)
        .map(|number| number as i32)
        .unwrap_or_else(|| i32::from(is_error));
    if is_error && stderr.is_empty() {
        stderr = "mobile Shell command failed".into();
    }
    MobileBashExecution {
        result: DurableBashResult {
            stdout,
            stderr,
            exit_code,
            // Only an explicit user cancellation is projected as the Native
            // user-interruption row. A Shell timeout also sets its payload's
            // interrupted bit, but must retain its partial output instead.
            interrupted: false,
        },
        interrupted,
    }
}

fn mobile_prompt_command(
    session_id: &str,
    checkpoint: &crate::mobile::turn_durability::DurableTurnCheckpoint,
) -> msgqueue::QueuedCommand {
    msgqueue::QueuedCommand {
        scheduled_task_id: None,
        scheduled_fire_id: None,
        uuid: mobile_prompt_command_uuid(session_id, checkpoint.turn_id),
        content: msgqueue::QueuedCommandContent::UserInput {
            text: checkpoint.prompt.clone(),
        },
        priority: msgqueue::QueuePriority::Next,
        queued_at: std::time::SystemTime::now(),
        source: msgqueue::QueueSource::PromptInput,
        agent_id: None,
        skip_slash_commands: false,
        is_meta: false,
    }
}

pub(super) async fn enqueue_mobile_prompt_dispatch(
    queue: &msgqueue::MessageQueueManager,
    session_id: &str,
    checkpoint: &crate::mobile::turn_durability::DurableTurnCheckpoint,
) {
    let uuid = mobile_prompt_command_uuid(session_id, checkpoint.turn_id);
    if queue
        .snapshot()
        .await
        .iter()
        .any(|command| command.uuid == uuid)
    {
        return;
    }
    queue
        .enqueue(mobile_prompt_command(session_id, checkpoint))
        .await;
}

async fn enqueue_mobile_slash_dispatch(
    queue: &msgqueue::MessageQueueManager,
    session_id: &str,
    checkpoint: &crate::mobile::turn_durability::DurableTurnCheckpoint,
) {
    let uuid = mobile_slash_command_uuid(session_id, checkpoint.turn_id);
    if queue
        .snapshot()
        .await
        .iter()
        .any(|command| command.uuid == uuid)
    {
        return;
    }
    queue
        .enqueue(msgqueue::QueuedCommand {
            scheduled_task_id: None,
            scheduled_fire_id: None,
            uuid,
            content: msgqueue::QueuedCommandContent::UserInput {
                text: checkpoint.prompt.clone(),
            },
            priority: msgqueue::QueuePriority::Next,
            queued_at: std::time::SystemTime::now(),
            source: msgqueue::QueueSource::PromptInput,
            agent_id: None,
            skip_slash_commands: false,
            is_meta: false,
        })
        .await;
}

pub(super) async fn remove_mobile_prompt_dispatches(
    queue: &msgqueue::MessageQueueManager,
    session_id: &str,
    turn_ids: &[u64],
) {
    let ids = turn_ids
        .iter()
        .flat_map(|turn_id| {
            [
                mobile_prompt_command_uuid(session_id, *turn_id),
                mobile_slash_command_uuid(session_id, *turn_id),
            ]
        })
        .collect::<Vec<_>>();
    queue.remove(&ids, "cancel durable mobile prompt").await;
}

pub(super) async fn rebuild_mobile_prompt_dispatch_cache(
    queue: &msgqueue::MessageQueueManager,
    durable_turns: &DurableTurnStore,
    session_id: &str,
) -> Result<(), DurableTurnStoreError> {
    let stale_ids = queue
        .snapshot()
        .await
        .into_iter()
        .filter(|command| is_mobile_dispatch_command(&command.uuid))
        .map(|command| command.uuid)
        .collect::<Vec<_>>();
    queue
        .remove(&stale_ids, "rebuild durable mobile prompt dispatch cache")
        .await;
    let pending = match durable_turns.scan_pending(session_id) {
        Ok(pending) => pending,
        // Restore the session, but do not silently replay a queue whose Stop
        // outcome was lost. Query/attach exposes the uncertain turn.
        Err(DurableTurnStoreError::UncertainQueuedTurn { .. }) => return Ok(()),
        Err(error) => return Err(error),
    };
    for checkpoint in pending {
        match checkpoint.queue_operation {
            QueueOperationKind::Prompt => {
                enqueue_mobile_prompt_dispatch(queue, session_id, &checkpoint).await
            }
            QueueOperationKind::Slash => {
                enqueue_mobile_slash_dispatch(queue, session_id, &checkpoint).await
            }
        }
    }
    Ok(())
}

fn mobile_prompt_command_uuid(session_id: &str, turn_id: u64) -> String {
    format!("mobile-prompt:{session_id}:{turn_id}")
}

fn is_mobile_prompt_command(uuid: &str) -> bool {
    uuid.starts_with("mobile-prompt:")
}

fn mobile_slash_command_uuid(session_id: &str, turn_id: u64) -> String {
    format!("mobile-slash:{session_id}:{turn_id}")
}

fn is_mobile_slash_command(uuid: &str) -> bool {
    uuid.starts_with("mobile-slash:")
}

fn is_mobile_dispatch_command(uuid: &str) -> bool {
    is_mobile_prompt_command(uuid) || is_mobile_slash_command(uuid)
}

fn parse_mobile_slash_command(uuid: &str, session_id: &str) -> Option<u64> {
    let suffix = uuid.strip_prefix("mobile-slash:")?;
    let (queued_session, turn_id) = suffix.rsplit_once(':')?;
    (queued_session == session_id)
        .then(|| turn_id.parse().ok())
        .flatten()
}

fn parse_mobile_prompt_command(uuid: &str, session_id: &str) -> Option<u64> {
    let suffix = uuid.strip_prefix("mobile-prompt:")?;
    let (queued_session, turn_id) = suffix.rsplit_once(':')?;
    (queued_session == session_id)
        .then(|| turn_id.parse().ok())
        .flatten()
}

impl ActiveTurn {
    fn new(turn_id: Option<u64>) -> Self {
        Self {
            turn_id,
            session_id: String::new(),
            live_owner: None,
            permission_owner_id: None,
            cancel: CancellationToken::new(),
            admission: StdMutex::new(()),
            queue_owner: Arc::new(()),
            mid_turn_members: StdMutex::new(std::collections::HashSet::new()),
            task: StdMutex::new(None),
            completed: AtomicBool::new(false),
            quiescing: AtomicBool::new(false),
            explicit_cancel_requested: AtomicBool::new(false),
            terminal_emitted: AtomicBool::new(false),
            delivery_failed: AtomicBool::new(false),
            journal_degraded: AtomicBool::new(false),
            output_recovery_fenced: AtomicBool::new(false),
            deferred_terminal: StdMutex::new(None),
            completion: Notify::new(),
        }
    }

    fn new_owned(turn_id: Option<u64>, session_id: String, permission_owner_id: u64) -> Self {
        let mut turn = Self::new(turn_id);
        turn.session_id = session_id;
        turn.permission_owner_id = Some(permission_owner_id);
        turn
    }

    fn with_live_owner(
        turn_id: Option<u64>,
        session_id: String,
        permission_owner_id: u64,
        live_owner: crate::mobile::turn_durability::LiveTurnOwnerLease,
    ) -> Self {
        let mut turn = Self::new_owned(turn_id, session_id, permission_owner_id);
        turn.live_owner = Some(live_owner);
        turn
    }

    fn tool_recovery_guard(
        self: &Arc<Self>,
        store: Arc<DurableTurnStore>,
    ) -> orchestrator::conversation::ToolDispatchRecoveryGuard {
        let session_id = self.session_id.clone();
        let turn_id = self.turn_id;
        let weak_owner = Arc::downgrade(self);
        Arc::new(move || {
            let owner = weak_owner
                .upgrade()
                .ok_or(hooks::RecoveryAdmissionError::Interrupted)?;
            let _admission = owner
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let is_interrupted =
                || owner.is_quiescing() || owner.explicit_cancel_requested.load(Ordering::Acquire);
            if is_interrupted() {
                return Err(hooks::RecoveryAdmissionError::Interrupted);
            }
            let lease = owner.live_owner.clone().ok_or_else(|| {
                hooks::RecoveryAdmissionError::Failure("missing mobile turn owner".into())
            })?;
            store
                .with_live_owner(&session_id, &lease, turn_id, || {
                    if let Some(turn_id) = turn_id {
                        store.mark_tool_dispatch_unsafe(&session_id, turn_id)?;
                    }
                    Ok(())
                })
                .map_err(|error| {
                    if is_interrupted() {
                        hooks::RecoveryAdmissionError::Interrupted
                    } else {
                        hooks::RecoveryAdmissionError::Failure(format!(
                            "could not persist tool recovery boundary: {error}"
                        ))
                    }
                })?;
            let work = lease.detached_work_lease();
            Ok(Some(
                orchestrator::conversation::ToolDispatchRecoveryLeases {
                    execution: Arc::new(lease),
                    turn_work: Arc::new(work),
                },
            ))
        })
    }

    fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>) {
        let mut task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *task = Some(handle);
    }

    fn take_task_handle(&self) -> Option<tokio::task::JoinHandle<()>> {
        let task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        task
    }

    async fn wait_completed(&self) {
        loop {
            if self.completed.load(Ordering::Acquire) {
                return;
            }
            let notified = self.completion.notified();
            tokio::pin!(notified);
            // Register before the second state check so completion cannot be
            // lost between checking and awaiting.
            notified.as_mut().enable();
            if self.completed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
        self.completion.notify_waiters();
    }

    fn matches_cancel(&self, requested_turn_id: Option<u64>) -> bool {
        requested_turn_id.is_none() || self.turn_id == requested_turn_id
    }

    fn request_quiesce(&self) {
        // Publish the delivery gate before firing cancellation. The
        // orchestrator may synchronously emit a final `TurnEnded` while it
        // observes the token, and that event must not be rewritten to
        // `Cancelled` or reach the client after the pause boundary.
        self.quiescing.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    /// Called while holding the active-owner lock. The first cancellation
    /// source determines whether a later lifecycle request may quiesce it.
    fn request_lifecycle_quiesce(
        &self,
        reason: &orchestrator::prompt::mid_turn_input::CancelReasonFlag,
    ) -> bool {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.request_lifecycle_quiesce_locked(reason)
    }

    fn try_request_lifecycle_quiesce(
        &self,
        reason: &orchestrator::prompt::mid_turn_input::CancelReasonFlag,
    ) -> Option<bool> {
        let _admission = match self.admission.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(self.request_lifecycle_quiesce_locked(reason))
    }

    fn request_lifecycle_quiesce_locked(
        &self,
        reason: &orchestrator::prompt::mid_turn_input::CancelReasonFlag,
    ) -> bool {
        reason.set(orchestrator::prompt::mid_turn_input::CancelReason::LifecyclePause);
        if reason.get() == orchestrator::prompt::mid_turn_input::CancelReason::UserInterrupt {
            return false;
        }
        self.request_quiesce();
        true
    }

    fn is_quiescing(&self) -> bool {
        self.quiescing.load(Ordering::Acquire)
    }

    fn defer_terminal(&self, event: ClientEvent) {
        let mut deferred = self
            .deferred_terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if deferred.is_none() {
            *deferred = Some(event);
        }
    }

    fn take_deferred_terminal(&self) -> Option<ClientEvent> {
        self.deferred_terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Release the queue registration and owner slot as one synchronous decision.
/// The caller holds the active-owner lock, never across a callback or await.
fn release_active_turn(
    active: &mut Option<Arc<ActiveTurn>>,
    queue: &msgqueue::MessageQueueManager,
    turn: &Arc<ActiveTurn>,
) {
    if active
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, turn))
    {
        queue.clear_owned_active_turn(&turn.queue_owner);
        *active = None;
    }
}

async fn report_owned_turn_error(
    orchestrator: &ConversationOrchestrator,
    turn: &Arc<ActiveTurn>,
    sink: &Arc<dyn client::adapter::ClientEventSink>,
    message_output: &client::adapter::AdapterOutputStream,
    error: &orchestrator::OrchestratorError,
) {
    message_output.reset_message_buffer().await;
    if matches!(error, orchestrator::OrchestratorError::RecoveryInterrupted)
        && (turn.is_quiescing() || turn.explicit_cancel_requested.load(Ordering::Acquire))
    {
        if !turn.is_quiescing() && !turn.terminal_emitted.load(Ordering::Acquire) {
            sink.emit(ClientEvent::TurnEnded {
                outcome: TurnOutcomeDto::Cancelled,
                stop_reason: Some("cancelled".into()),
                cost: client::adapter::lowering::lower_cost_snapshot(
                    &orchestrator.snapshot_cost_real().await,
                ),
            })
            .await;
        }
        return;
    }
    sink.emit(client::adapter::map_orchestrator_error(error))
        .await;
}

type PendingMediaAgentHandoff = crate::media_handoff::MediaHandoffInvocation;

/// One pending main-thread media conversation takeover. It is bound to the
/// active session and exact tool-use row returned by a trusted host callback.
#[derive(Default)]
pub(crate) struct MobileMediaAgentHandoffQueue {
    session_id: StdMutex<Option<Arc<StdMutex<String>>>>,
    state: StdMutex<MediaAgentHandoffQueueState>,
    next_claim_generation: AtomicU64,
    attachment: StdMutex<Option<Arc<dyn MediaConversationAttachment>>>,
}

#[derive(Default)]
struct MediaAgentHandoffQueueState {
    pending: Option<PendingMediaAgentHandoff>,
    in_progress_session: Option<(String, u64)>,
}

impl MobileMediaAgentHandoffQueue {
    fn set_attachment(&self, attachment: Option<Arc<dyn MediaConversationAttachment>>) {
        *self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = attachment;
    }

    fn supported_modes(&self) -> Vec<media_api::MediaSessionMode> {
        let attachment = self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        attachment.map_or_else(Vec::new, |attachment| {
            attachment
                .supported_modes()
                .into_iter()
                .filter(|mode| *mode != media_api::MediaSessionMode::Video)
                .collect()
        })
    }

    fn supported_modes_for_context(
        &self,
        capability_context: &orchestrator::MediaAgentCapabilityContext,
    ) -> Vec<media_api::MediaSessionMode> {
        if self.current_session_id().ok().as_deref() != Some(capability_context.session_id.as_str())
        {
            return Vec::new();
        }
        let attachment = self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        attachment.map_or_else(Vec::new, |attachment| {
            attachment
                .supported_modes_for_session(capability_context)
                .into_iter()
                .filter(|mode| {
                    *mode != media_api::MediaSessionMode::Video
                        && attachment.supports_mode(capability_context, *mode)
                })
                .collect()
        })
    }

    fn supports_mode(
        &self,
        capability_context: &orchestrator::MediaAgentCapabilityContext,
        mode: media_api::MediaSessionMode,
    ) -> bool {
        if mode == media_api::MediaSessionMode::Video {
            return false;
        }
        if self.current_session_id().ok().as_deref() != Some(capability_context.session_id.as_str())
        {
            return false;
        }
        let attachment = self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        attachment.is_some_and(|attachment| attachment.supports_mode(&capability_context, mode))
    }

    fn bind_session(&self, session_id: Arc<StdMutex<String>>) {
        *self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
    }

    fn current_session_id(&self) -> Result<String, String> {
        let session_cell = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "media handoff has no active session owner".to_owned())?;
        let session_id = session_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if session_id.is_empty() {
            return Err("media handoff has no active session owner".to_owned());
        }
        Ok(session_id)
    }

    fn has_pending_for_session(&self, session_id: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.session_id == session_id)
            || state
                .in_progress_session
                .as_ref()
                .is_some_and(|(active, _)| active == session_id)
    }

    fn take_for_turn(&self, session_id: &str) -> Option<(PendingMediaAgentHandoff, u64)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.pending.as_ref() {
            Some(request) if request.session_id == session_id => {
                let pending = state.pending.take();
                let generation = self
                    .next_claim_generation
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1)
                    .max(1);
                state.in_progress_session = Some((session_id.to_owned(), generation));
                pending.map(|pending| (pending, generation))
            }
            // The queue has one slot and one current-session binding. A stale
            // request can never be consumed by another session, so discard it
            // rather than blocking a future Conversation request forever.
            Some(_) => {
                state.pending = None;
                None
            }
            None => None,
        }
    }

    fn clear_for_session(&self, session_id: &str) -> Option<PendingMediaAgentHandoff> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = if state
            .pending
            .as_ref()
            .is_some_and(|request| request.session_id == session_id)
        {
            state.pending.take()
        } else {
            None
        };
        if state
            .in_progress_session
            .as_ref()
            .is_some_and(|(active, _)| active == session_id)
        {
            state.in_progress_session = None;
        }
        pending
    }

    fn finish_for_session(&self, session_id: &str, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_progress_session.as_ref() == Some(&(session_id.to_owned(), generation)) {
            state.in_progress_session = None;
        }
    }
}

struct MediaAgentHandoffClaim {
    queue: Arc<MobileMediaAgentHandoffQueue>,
    session_id: String,
    generation: u64,
}

impl Drop for MediaAgentHandoffClaim {
    fn drop(&mut self) {
        self.queue
            .finish_for_session(&self.session_id, self.generation);
    }
}

impl tool_api::MediaAgentHandoffHandle for MobileMediaAgentHandoffQueue {
    fn supported_modes(&self) -> Vec<media_api::MediaSessionMode> {
        MobileMediaAgentHandoffQueue::supported_modes(self)
    }

    fn supported_modes_for_context(
        &self,
        capability_context: &orchestrator::MediaAgentCapabilityContext,
    ) -> Vec<media_api::MediaSessionMode> {
        MobileMediaAgentHandoffQueue::supported_modes_for_context(self, capability_context)
    }

    fn supports_mode(
        &self,
        capability_context: &orchestrator::MediaAgentCapabilityContext,
        mode: media_api::MediaSessionMode,
    ) -> bool {
        MobileMediaAgentHandoffQueue::supports_mode(self, capability_context, mode)
    }

    fn request(
        &self,
        invocation: &tool_api::ToolUseContext,
        capability_context: &orchestrator::MediaAgentCapabilityContext,
        request: tool_api::MediaAgentHandoffRequest,
    ) -> Result<(), String> {
        if invocation.is_scheduled_turn {
            return Err("media conversation handoff is unavailable during scheduled turns".into());
        }
        if !self.supports_mode(capability_context, request.mode) {
            return Err(format!(
                "media conversation mode {:?} is not supported by this host attachment",
                request.mode
            ));
        }
        if invocation.agent_id.is_some()
            || invocation.origin_session_id.is_some()
            || invocation.depth != 0
        {
            return Err("media conversation handoff is available only to the main Agent".into());
        }
        if invocation.session.is_none() {
            return Err("media conversation handoff has no trusted session context".into());
        }
        let assistant_message_id = invocation
            .assistant_message_id
            .clone()
            .ok_or_else(|| "media handoff is not attached to an active Agent turn".to_owned())?;
        let tool_use_id = invocation
            .tool_use_id
            .clone()
            .ok_or_else(|| "media handoff is not attached to a tool call".to_owned())?;
        let assistant_message = crate::media_handoff::capture_assistant_message(
            invocation,
            &assistant_message_id,
            &tool_use_id,
        )?;
        let session_id = self.current_session_id()?;
        if capability_context.session_id != session_id {
            return Err("media conversation handoff belongs to a different active session".into());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.is_some() || state.in_progress_session.is_some() {
            return Err("another media conversation handoff is already pending".into());
        }
        state.pending = Some(PendingMediaAgentHandoff {
            session_id,
            capability_context: capability_context.clone(),
            assistant_message_id,
            assistant_message,
            tool_use_id,
            request,
        });
        Ok(())
    }
}

async fn continue_media_agent_handoff(
    orchestrator: Arc<ConversationOrchestrator>,
    turn: Arc<ActiveTurn>,
    handoffs: Arc<MobileMediaAgentHandoffQueue>,
    service: Option<Arc<dyn media_api::MediaService>>,
    attachment: Option<Arc<dyn MediaConversationAttachment>>,
    ordinary_turn_succeeded: bool,
) -> Result<Option<orchestrator::RealtimeAgentEnd>, orchestrator::OrchestratorError> {
    let pending = handoffs.take_for_turn(&turn.session_id);
    let Some((pending, claim_generation)) = pending else {
        return Ok(None);
    };
    let _claim = MediaAgentHandoffClaim {
        queue: handoffs,
        session_id: turn.session_id.clone(),
        generation: claim_generation,
    };
    crate::media_handoff::continue_media_handoff(
        orchestrator,
        pending,
        &turn.session_id,
        turn.cancel.clone(),
        service,
        attachment,
        ordinary_turn_succeeded,
    )
    .await
}

/// Mobile-only lifecycle guard around the foreign listener. The core protocol
/// intentionally keeps its frozen event shapes, so the host enforces the
/// single-owner invariant at the delivery boundary: one terminal event closes
/// the turn, cancellation rewrites that terminal outcome, and live-turn events
/// emitted after terminal/slot release are discarded.
struct TurnLifecycleListener {
    inner: Arc<dyn ClientEventListener>,
    active_turn: Arc<Mutex<Option<Arc<ActiveTurn>>>>,
    durable_turns: Option<Arc<DurableTurnStore>>,
    media_agent_handoffs: Option<Arc<MobileMediaAgentHandoffQueue>>,
    cancel_reason: Option<orchestrator::prompt::mid_turn_input::CancelReasonFlag>,
    #[cfg(test)]
    fail_next_serialization: AtomicBool,
}

impl TurnLifecycleListener {
    fn new(
        inner: Arc<dyn ClientEventListener>,
        active_turn: Arc<Mutex<Option<Arc<ActiveTurn>>>>,
    ) -> Self {
        Self {
            inner,
            active_turn,
            durable_turns: None,
            media_agent_handoffs: None,
            cancel_reason: None,
            #[cfg(test)]
            fail_next_serialization: AtomicBool::new(false),
        }
    }

    fn new_durable(
        inner: Arc<dyn ClientEventListener>,
        active_turn: Arc<Mutex<Option<Arc<ActiveTurn>>>>,
        durable_turns: Arc<DurableTurnStore>,
    ) -> Self {
        Self {
            inner,
            active_turn,
            durable_turns: Some(durable_turns),
            media_agent_handoffs: None,
            cancel_reason: None,
            #[cfg(test)]
            fail_next_serialization: AtomicBool::new(false),
        }
    }

    fn new_durable_with_media_handoffs(
        inner: Arc<dyn ClientEventListener>,
        active_turn: Arc<Mutex<Option<Arc<ActiveTurn>>>>,
        durable_turns: Arc<DurableTurnStore>,
        media_agent_handoffs: Arc<MobileMediaAgentHandoffQueue>,
        cancel_reason: Option<orchestrator::prompt::mid_turn_input::CancelReasonFlag>,
    ) -> Self {
        Self {
            inner,
            active_turn,
            durable_turns: Some(durable_turns),
            media_agent_handoffs: Some(media_agent_handoffs),
            cancel_reason,
            #[cfg(test)]
            fail_next_serialization: AtomicBool::new(false),
        }
    }

    fn serialize_journal_event(&self, event: &ClientEvent) -> Result<String, String> {
        #[cfg(test)]
        if self.fail_next_serialization.swap(false, Ordering::AcqRel) {
            return Err("injected event serialization failure".into());
        }
        serde_json::to_string(event).map_err(|error| error.to_string())
    }

    fn is_live_turn_payload(event: &ClientEvent) -> bool {
        // AskUserQuestion is connection-scoped: a background workflow can park
        // on it after the launching conversation turn has already ended.
        matches!(
            event,
            ClientEvent::SystemNotice { .. }
                | ClientEvent::UiLog { .. }
                | ClientEvent::UiToast { .. }
                | ClientEvent::UiStatus { .. }
                | ClientEvent::TextDelta { .. }
                | ClientEvent::ToolUseStarted { .. }
                | ClientEvent::ToolHeartbeat { .. }
                | ClientEvent::ToolUseResult { .. }
                | ClientEvent::MessageComplete { .. }
                | ClientEvent::MessageIdentity { .. }
                | ClientEvent::MessageRetracted { .. }
                | ClientEvent::CostUpdate { .. }
                | ClientEvent::CompactionCompleted { .. }
                | ClientEvent::CoordinatorStatus { .. }
                | ClientEvent::CoordinatorWorker { .. }
                | ClientEvent::ThinkingDelta { .. }
                | ClientEvent::UsageUpdate { .. }
                | ClientEvent::Attachment { .. }
                | ClientEvent::ApiRetry { .. }
        )
    }

    /// Events that belong to one conversation turn's replayable transcript.
    ///
    /// The turn journal is the attach/replay source of truth for a single
    /// turn, and it is bounded (`MAX_RETAINED_EVENTS`). Connection-scoped
    /// listing/session/app/task/settings events are neither owned by the turn
    /// nor decodable by the clients' retained-event decoders — they replay as
    /// envelopes that map to null — so journaling them only evicts real turn
    /// output from the retention window and makes
    /// `DurableTurnCheckpoint::can_resume_without_user` false for a turn that
    /// has produced nothing of its own. The boundary set here is the same one
    /// `turn_durability::event_requires_durable_flush` already names, plus the
    /// live-turn payloads.
    fn is_turn_journal_event(event: &ClientEvent) -> bool {
        // Permission requests and their resolutions are connection-scoped:
        // the request arrives through a separate sink and neither native
        // retained-event decoder consumes the resolution. Deliver it live
        // without taking the turn gate so a listener can resolve a request.
        Self::is_live_turn_payload(event)
            || matches!(
                event,
                ClientEvent::TurnStarted { .. }
                    | ClientEvent::TurnEnded { .. }
                    | ClientEvent::Error { .. }
                    | ClientEvent::PlanUpdated { .. }
            )
    }
}

#[async_trait]
impl ClientEventListener for TurnLifecycleListener {
    async fn on_event(&self, mut event: ClientEvent) {
        // Acquire the shared turn gate before reserving a journal sequence.
        // Waiting never holds the owner lock or an external callback.
        let expected_owner = if Self::is_turn_journal_event(&event) {
            self.active_turn.lock().await.clone()
        } else {
            None
        };
        let mut delivery_guard = if let (Some(turn), Some(store)) =
            (expected_owner.as_ref(), self.durable_turns.as_ref())
        {
            match turn.turn_id.filter(|_| !turn.session_id.is_empty()) {
                Some(turn_id) => match store.delivery_gate(&turn.session_id, turn_id) {
                    Ok(gate) => match gate.lock().await {
                        Ok(guard) => Some(guard),
                        Err(error) => {
                            tracing::error!(%error, "mobile turn event delivery failed closed");
                            if !turn.delivery_failed.swap(true, Ordering::AcqRel) {
                                self.inner.on_event(ClientEvent::Error {
                                    kind: ErrorKindDto::Internal,
                                    message: "Turn event delivery was interrupted; close the engine and reconnect to replay this turn.".into(),
                                }).await;
                            }
                            return;
                        }
                    },
                    Err(error) => {
                        tracing::error!(%error, "invalid mobile turn delivery identity");
                        return;
                    }
                },
                None => None,
            }
        } else {
            None
        };
        // Decide event ownership and commit its durable state while holding
        // the same short-lived lock used by pause. This makes terminal
        // admission and lifecycle quiescence mutually exclusive; the lock is
        // released before awaiting any foreign listener callback.
        let active_guard = self.active_turn.lock().await;
        let active = active_guard.clone();
        if let Some(expected) = expected_owner.as_ref() {
            if !active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected))
            {
                return;
            }
        }
        if let (Some(turn), ClientEvent::TurnEnded { .. }) = (active.as_ref(), &event) {
            let handoff_pending = self
                .media_agent_handoffs
                .as_ref()
                .is_some_and(|handoffs| handoffs.has_pending_for_session(&turn.session_id));
            if handoff_pending
                && !turn.is_quiescing()
                && !turn.terminal_emitted.load(Ordering::Acquire)
            {
                let mut deferred = event.clone();
                if turn.cancel.is_cancelled() {
                    if let ClientEvent::TurnEnded {
                        outcome,
                        stop_reason,
                        ..
                    } = &mut deferred
                    {
                        *outcome = TurnOutcomeDto::Cancelled;
                        *stop_reason = Some("cancelled".to_string());
                    }
                }
                turn.defer_terminal(deferred);
                return;
            }
        }
        let is_live_turn_payload = Self::is_live_turn_payload(&event);
        let should_forward = match &mut event {
            ClientEvent::TurnEnded {
                outcome,
                stop_reason,
                ..
            } => {
                if let Some(turn) = active.as_ref() {
                    if turn.is_quiescing() {
                        return;
                    }
                    if turn.terminal_emitted.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    if turn.cancel.is_cancelled() {
                        *outcome = TurnOutcomeDto::Cancelled;
                        *stop_reason = Some("cancelled".to_string());
                    }
                    true
                } else {
                    false
                }
            }
            ClientEvent::Error { .. } => {
                if let Some(turn) = active.as_ref() {
                    !turn.is_quiescing() && !turn.terminal_emitted.swap(true, Ordering::AcqRel)
                } else {
                    true
                }
            }
            ClientEvent::TurnStarted { turn_id } => active.as_ref().is_some_and(|turn| {
                turn.turn_id == *turn_id
                    && !turn.is_quiescing()
                    && !turn.terminal_emitted.load(Ordering::Acquire)
            }),
            _ if is_live_turn_payload => active.as_ref().is_some_and(|turn| {
                !turn.is_quiescing() && !turn.terminal_emitted.load(Ordering::Acquire)
            }),
            // Listing, session, app, task and explicit resolution events are
            // connection-scoped rather than owned by a live conversation turn.
            _ => true,
        };

        if should_forward {
            if let Some(guard) = delivery_guard.as_mut() {
                guard.arm();
            }
            let durable_identity = active
                .as_ref()
                .filter(|turn| !turn.is_quiescing() && self.durable_turns.is_some())
                .and_then(|turn| {
                    turn.turn_id
                        .filter(|_| !turn.session_id.is_empty())
                        .map(|turn_id| (turn.session_id.as_str(), turn_id))
                });
            let is_recovery_event = matches!(
                event,
                ClientEvent::TurnRecoveryState { .. }
                    | ClientEvent::TurnEventReplay { .. }
                    | ClientEvent::TurnEventLiveUnretained { .. }
                    | ClientEvent::TurnAttachStarted { .. }
                    | ClientEvent::TurnAttachFinished { .. }
                    | ClientEvent::QueuedTurnStatus { .. }
            );
            let mut delivery_failure = None;
            let mut recovery_notice = false;
            let outbound = if is_recovery_event {
                None
            } else if let Some((session_id, turn_id)) =
                durable_identity.filter(|_| Self::is_turn_journal_event(&event))
            {
                let store = self
                    .durable_turns
                    .as_ref()
                    .expect("durable identity has a store");
                let serialized = self.serialize_journal_event(&event).and_then(|json| {
                    let turn = active.as_ref().expect("durable identity has an owner");
                    // The existing safe-recovery rule treats TurnStarted as
                    // the sole non-material turn event. All other output must
                    // cross a durable unsafe fence before publication: normal
                    // journal appends can be acknowledged before their fsync.
                    if !matches!(event, ClientEvent::TurnStarted { .. })
                        && !turn.output_recovery_fenced.load(Ordering::Acquire)
                    {
                        store
                            .fence_observed_turn_output(session_id, turn_id)
                            .map_err(|error| format!("turn output recovery boundary: {error}"))?;
                        turn.output_recovery_fenced.store(true, Ordering::Release);
                    }
                    Ok(json)
                });
                match serialized {
                    Ok(event_json) => {
                        let turn = active.as_ref().expect("durable identity has an owner");
                        let retained = if turn.journal_degraded.load(Ordering::Acquire) {
                            None
                        } else {
                            match store.append_event(session_id, turn_id, event_json.clone()) {
                                Ok(retained) => Some(retained),
                                Err(error) => {
                                    tracing::warn!(%error, session_id, turn_id, "mobile: turn event not acknowledged by journal");
                                    if matches!(error, DurableTurnStoreError::Storage(_)) {
                                        recovery_notice =
                                            !turn.journal_degraded.swap(true, Ordering::AcqRel);
                                    }
                                    None
                                }
                            }
                        };
                        Some(match retained {
                            Some(retained) => ClientEvent::TurnEventReplay {
                                session_id: session_id.to_owned(),
                                turn_id,
                                sequence: retained.sequence,
                                event_json: retained.event_json,
                            },
                            None => ClientEvent::TurnEventLiveUnretained {
                                session_id: session_id.to_owned(),
                                turn_id,
                                event_json,
                            },
                        })
                    }
                    Err(error) => {
                        tracing::error!(%error, session_id, turn_id, "mobile: turn event delivery failed closed");
                        if let Some(turn) = active.as_ref() {
                            if let Some(reason) = &self.cancel_reason {
                                turn.request_lifecycle_quiesce(reason);
                            } else {
                                turn.request_quiesce();
                            }
                            turn.terminal_emitted.store(true, Ordering::Release);
                            turn.delivery_failed.store(true, Ordering::Release);
                        }
                        delivery_failure = Some(error);
                        Some(ClientEvent::Error {
                            kind: ErrorKindDto::Internal,
                            message: "Turn output could not be saved safely; reconnect to recover the conversation.".into(),
                        })
                    }
                }
            } else {
                None
            };

            let recovery_snapshot = durable_identity.and_then(|(session_id, turn_id)| {
                let (state, safe_to_resume, reason) = if let Some(error) = &delivery_failure {
                    (TurnRecoveryStateDto::Failed, false, Some(format!("turn_event_delivery_failed: {error}")))
                } else { match &event {
                    ClientEvent::ToolUseStarted { .. } => (
                        TurnRecoveryStateDto::Running,
                        false,
                        Some("tool_boundary_requires_confirmation".to_string()),
                    ),
                    ClientEvent::TurnEnded { outcome, .. } => match outcome {
                        TurnOutcomeDto::EndTurn => {
                            (TurnRecoveryStateDto::Completed, false, None)
                        }
                        TurnOutcomeDto::MaxTurns => (
                            TurnRecoveryStateDto::Failed,
                            false,
                            Some("max_turns".to_string()),
                        ),
                        TurnOutcomeDto::Cancelled => return None,
                        _ => return None,
                    },
                    ClientEvent::Error { message, .. } => (
                        TurnRecoveryStateDto::Failed,
                        false,
                        Some(message.display()),
                    ),
                    _ => return None,
                }};
                match self.durable_turns.as_ref()?.transition_if_live(
                    session_id,
                    turn_id,
                    state,
                    safe_to_resume,
                    reason,
                ) {
                    Ok(snapshot) => snapshot,
                    Err(DurableTurnStoreError::Terminal { .. }) => None,
                    Err(error) => {
                        tracing::warn!(%error, session_id, turn_id, "mobile: failed to transition durable turn");
                        None
                    }
                }
            });

            drop(active_guard);
            let inner = self.inner.clone();
            let fail_closed = delivery_failure.is_some();
            let deliver = async move {
                inner.on_event(outbound.unwrap_or(event)).await;
                if recovery_notice {
                    inner.on_event(ClientEvent::SystemNotice {
                        message: "Some live turn events could not be saved; reconnect to rebuild the conversation.".into(),
                        is_error: true,
                    }).await;
                }
                if let Some(snapshot) = recovery_snapshot {
                    inner
                        .on_event(ClientEvent::TurnRecoveryState { snapshot })
                        .await;
                }
            };
            if let Some(guard) = delivery_guard {
                // There is no await between durable commit and spawning this
                // completion task. A dropped producer cannot release its gate
                // before the raw event and its replay envelope are delivered.
                let delivery = tokio::spawn(async move {
                    deliver.await;
                    if !fail_closed {
                        guard.complete();
                    }
                });
                if let Err(error) = delivery.await {
                    tracing::error!(%error, "mobile turn event callback terminated");
                }
            } else {
                deliver.await;
            }
        } else {
            drop(active_guard);
            tracing::debug!("mobile: dropped stale live-turn event");
        }
    }

    async fn on_workflow_progress(
        &self,
        origin_session_id: String,
        task_id: String,
        run_id: String,
        progress: client::protocol::listings::WorkflowProgressDto,
    ) {
        self.inner
            .on_workflow_progress(origin_session_id, task_id, run_id, progress)
            .await;
    }
}

impl MobileEngineHandle {
    /// The same live logical cwd used by file tools, Bash and the orchestrator.
    fn session_cwd(&self) -> String {
        self.inner.session_cwd.cwd().to_string_lossy().into_owned()
    }

    async fn emit_controls_snapshot(&self) {
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        let Some(controls) = handle.conversation_controls().await else {
            return;
        };
        let requested_permission = self
            .inner
            .requested_permission_mode
            .lock()
            .map(|mode| mode.clone())
            .unwrap_or_else(|_| controls.permission.requested.clone());
        self.event_sink
            .emit(ClientEvent::ConversationControlsChanged {
                controls: lower_controls(
                    controls,
                    requested_permission,
                    handle.native_proactivity_snapshot(),
                ),
            })
            .await;
        self.event_sink
            .emit(ClientEvent::FastModeChanged {
                enabled: handle.fast_mode().await,
            })
            .await;
    }

    async fn emit_typescript_lsp_mode(&self) {
        let requested = self.inner.lsp_registry.activation_mode();
        let effective = if self.inner.typescript_lsp_runtime_available {
            requested
        } else {
            lsp::LspActivationMode::Off
        };
        self.event_sink
            .emit(ClientEvent::TypescriptLspModeChanged {
                requested: requested.wire_str().to_string(),
                effective: effective.wire_str().to_string(),
                available: self.inner.typescript_lsp_runtime_available,
            })
            .await;
    }

    /// Return a credential-free view over this handle's validated cron store.
    pub async fn cron_store(&self) -> Result<Arc<MobileCronStoreHandle>, ClientError> {
        let cfg = self.current_mobile_config()?;
        Ok(Arc::new(MobileCronStoreHandle::new(
            cfg.cwd,
            self.firer_platform.filesystem(),
            self.firer_platform.clock(),
        )))
    }

    async fn fire_automation_task(
        &self,
        task_id: String,
        scheduled_at: Option<u64>,
        manual_at: Option<u64>,
    ) -> Option<FiredCronJobDto> {
        let cfg = self.current_mobile_config().ok()?;
        let firer = MobileTurnFirer {
            cfg: cfg.clone(),
            platform: self.firer_platform.clone(),
        };
        let fs = self.firer_platform.filesystem();
        let cwd = cfg.cwd.clone();
        let clock = self.firer_platform.clock();
        let claim_fs = fs.clone();
        let claim_cwd = cwd.clone();
        let claim_clock = clock.clone();
        supervise_mobile_automation(
            fs,
            cwd,
            clock,
            async move {
                match (scheduled_at, manual_at) {
                    (Some(at), _) => {
                        cron::claim_automation_run(
                            claim_fs.as_ref(),
                            &claim_cwd,
                            &task_id,
                            claim_clock.now(),
                            Some(at),
                        )
                        .await
                    }
                    (None, Some(at)) => {
                        cron::claim_automation_run_now_at(
                            claim_fs.as_ref(),
                            &claim_cwd,
                            &task_id,
                            claim_clock.now(),
                            at,
                        )
                        .await
                    }
                    (None, None) => {
                        cron::claim_automation_run_now(
                            claim_fs.as_ref(),
                            &claim_cwd,
                            &task_id,
                            claim_clock.now(),
                        )
                        .await
                    }
                }
            },
            move |request| async move { firer.fire_automation(&request).await },
        )
        .await
    }

    /// Number of currently-live builtin mobile Plugin skills. This is read
    /// from the same registry used by listing and invocation, so a runtime
    /// disable immediately reports zero rather than a boot-time constant.
    /// (Under `uniffi`: `#[uniffi::export]`.)
    #[must_use]
    pub fn skill_count(&self) -> u32 {
        let count = self
            .runtime()
            .block_on(mobile_live_plugin_skill_count(&self.inner.dispatcher));
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Create a conversation session for `model`.
    ///
    /// No longer stubbed (F3-04): the handle now owns a real, fully-wired
    /// [`MobileRuntime`], so a session is a live attribute of THIS connection
    /// (§0.5 — `session_id` is a connection attribute, not a per-command param).
    /// Returns the connection's session ref. The full New/Resume orchestrator
    /// swap lands with the command path (F3-05); here we confirm the host is no
    /// longer a stub by returning the live session ref instead of an error.
    ///
    /// # Errors
    ///
    /// Returns [`MobileEngineError::InvalidState`] only if the handle were ever
    /// torn down mid-call (cannot happen with `&self`); kept typed so the FFI
    /// signature is stable for the F3-05 command path.
    // `_model` is a deliberately-unused, by-value FFI-shape placeholder (see the
    // doc above): the F3-05 command path keeps the owned `String` signature, so
    // it is not narrowed to `&str` just to satisfy the lint.
    #[allow(clippy::needless_pass_by_value)]
    pub fn create_session(&self, _model: String) -> Result<u64, MobileEngineError> {
        // One connection ⇒ one engine host ⇒ a single session ref (1). The
        // orchestrator is already constructed and bound; New/Resume swap it in
        // place (F3-05) without minting a new handle.
        Ok(1)
    }

    /// Borrow the owned tokio runtime (F3-05 spawns the streaming turn on it;
    /// F3-07 registers it as the foreign async executor).
    #[must_use]
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime
            .as_ref()
            .expect("engine runtime is present while the handle is live")
    }

    /// Return the app-scoped media facade used by tools and native UI callers.
    #[must_use]
    pub fn media_service(&self) -> Option<Arc<dyn media_api::MediaService>> {
        self.inner.media_service.clone()
    }

    /// The identity of the handle-owned tokio runtime (F3-07), as a stable
    /// string token.
    ///
    /// UniFFI's `#[uniffi::export(async_runtime = "tokio")]` drives every async
    /// export (`submit`, the async inspection helpers) on a tokio runtime via the
    /// `tokio` feature's foreign-executor scaffolding; the host registers THIS
    /// owned `rt-multi-thread` runtime as that executor (decision §0.5 — one
    /// connection ⇒ one engine host owning one runtime). The
    /// `async_submit_resolves_on_handle_runtime` test compares this token against
    /// the one an async export observes via [`Self::observed_runtime_id`] to PROVE
    /// the export awaits on this runtime — not a transient ambient one.
    /// (`tokio::runtime::Id` is not UniFFI-representable, so the token is its
    /// `Debug` form, which is stable for the lifetime of the runtime.)
    #[doc(hidden)]
    #[must_use]
    pub fn runtime_id(&self) -> String {
        format!("{:?}", self.runtime().handle().id())
    }

    /// Borrow the wired [`MobileRuntime`] (orchestrator / dispatcher / auth /
    /// gate / listener) the F3-05 command path drives.
    #[must_use]
    pub fn inner(&self) -> &MobileRuntime {
        &self.inner
    }

    /// The connection-scoped [`AdapterPermissionGate`] — F3-05's
    /// `submit(ApprovePermission/DenyPermission)` calls `resolve` on it to
    /// satisfy a parked `check()`.
    #[must_use]
    pub fn permission_gate(&self) -> Arc<AdapterPermissionGate> {
        self.inner.permission_gate.clone()
    }

    /// The registered foreign event listener the adapter feeds.
    #[must_use]
    pub fn listener(&self) -> Arc<dyn ClientEventListener> {
        self.inner.listener.clone()
    }

    /// Test/inspection helper: `true` iff the in-flight turn's cancellation token
    /// has been fired.
    #[doc(hidden)]
    pub async fn active_turn_is_cancelled(&self) -> bool {
        self.active_cancel
            .lock()
            .await
            .as_ref()
            .is_some_and(|turn| turn.cancel.is_cancelled())
    }

    async fn ensure_reserved_session_identity(&self, session_id: &str) -> Result<(), ClientError> {
        ensure_mobile_session_identity(
            &self.inner.session_writer,
            &self.lingxi_home,
            &self.session_cwd(),
            session_id,
        )
        .await
    }

    async fn retarget_session_writer(
        &self,
        session_id: lingxi_core::types::SessionId,
        cwd: &str,
    ) -> Result<(), ClientError> {
        self.inner
            .retarget_session_context(&self.lingxi_home, session_id, cwd)
            .await;
        let transcript_path = self.inner.session_writer.active_path();
        if transcript_path.file_stem().and_then(|name| name.to_str())
            != Some(&session_id.as_uuid().to_string())
        {
            return Err(ClientError::Internal {
                message: "session transcript identity does not match the active UUID".into(),
            });
        }
        session_identity_snapshot_for_path(&self.inner.session_writer, &transcript_path)
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("session transcript identity import failed: {error}"),
            })?;
        if let Some(scheduler) = &self.session_cron {
            scheduler
                .set_tasks_file(cron::tasks_file::session_scheduled_tasks_path(
                    &self.current_mobile_config()?.cwd,
                ))
                .await
                .map_err(|error| ClientError::Internal {
                    message: error.to_string(),
                })?;
            if let Err(error) = scheduler
                .set_session_id(session_id.as_uuid().to_string())
                .await
            {
                tracing::warn!(%error, "mobile: could not restart session cron");
            }
        }
        self.reload_workspace_catalog(None).await?;
        self.reload_configured_mcp().await;
        let session_uuid = session_id.as_uuid().to_string();
        rebuild_mobile_prompt_dispatch_cache(
            &self.message_queue,
            &self.durable_turns,
            &session_uuid,
        )
        .await
        .map_err(Self::map_durable_turn_error)?;
        if let Err(error) = self.inner.orchestrator.recover_main_reports().await {
            tracing::warn!(%error, "could not recover reports for the resumed mobile session");
        }
        let subagents_dir =
            orchestrator::transcript_paths::subagents_dir(&self.lingxi_home, cwd, &session_uuid);
        for (agent_id, outcome) in crate::parked_agent_restore::restore_parked_agents_in_registry(
            &subagents_dir,
            self.inner.task_registry.clone(),
            self.inner.agent_resume_gate.as_ref(),
            &self.inner.parked_agent_restore_inheritance,
            self.inner.orchestrator.permission_mode(),
        )
        .await
        {
            if let crate::parked_agent_restore::RestoreOutcome::Failed(error) = outcome {
                tracing::warn!(%agent_id, %error, "mobile parked agent restore failed");
            }
        }
        self.emit_workspace_state().await;
        Ok(())
    }

    async fn restore_preferred_permission_mode(
        &self,
        fallback: &str,
    ) -> Result<String, ClientError> {
        if let Some(preference) = self
            .inner
            .interactive_launch
            .then(|| permission_preference::load(&self.lingxi_home))
            .flatten()
        {
            match self
                .restore_session_permission_mode(preference.wire_str())
                .await
            {
                Ok(active) => return Ok(active),
                Err(ClientError::Rejected { message }) => {
                    // A policy change can invalidate a saved choice. Keep the engine's
                    // validated current mode rather than preventing all session changes.
                    tracing::warn!(%message, "saved mobile permission mode rejected by current policy");
                    let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                    let current = handle
                        .permission_mode()
                        .await
                        .unwrap_or_else(|| self.inner.session_default_permission_mode.clone());
                    return self.restore_session_permission_mode(&current).await;
                }
                Err(error) => return Err(error),
            }
        }
        self.restore_session_permission_mode(fallback).await
    }

    async fn restore_preferred_reasoning_selection(&self) {
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        let saved = command_api::builtins::effort::load_reasoning_default_selection_at(
            &self.lingxi_home.join("settings.json"),
        );
        if let Some(selection) = saved {
            if let Err(error) = handle.set_reasoning_selection(selection).await {
                tracing::warn!(%error, "saved mobile reasoning selection rejected by current model");
                let _ = handle
                    .set_reasoning_selection(lingxi_core::host::ReasoningSelection::Automatic)
                    .await;
            }
            return;
        }
        if let Some(controls) = handle.conversation_controls().await {
            if controls.requested_reasoning_selection != controls.effective_reasoning_selection {
                let _ = handle
                    .set_reasoning_selection(lingxi_core::host::ReasoningSelection::Automatic)
                    .await;
            }
        }
    }

    async fn restore_preferred_fast_mode(&self) {
        if !self.inner.interactive_launch {
            return;
        }
        let Some(enabled) = fast_mode_preference::load(&self.lingxi_home) else {
            return;
        };
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        if let Err(error) = handle.set_fast_mode(enabled).await {
            tracing::warn!(%error, "saved mobile Fast mode rejected by current runtime");
        }
    }

    async fn restore_session_permission_mode(&self, mode: &str) -> Result<String, ClientError> {
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        let result = if mode == "bypassPermissions" {
            self.inner
                .permission_policy_gate
                .restore_session_permission_mode(mode)
                .await
        } else {
            handle
                .set_permission_mode(mode)
                .await
                .map_err(|error| error.to_string())
        };
        result.map_err(|error| ClientError::Rejected {
            message: format!("restore session permission mode failed: {error}"),
        })?;
        let active = handle
            .permission_mode()
            .await
            .unwrap_or_else(|| mode.to_string());
        handle
            .set_plan_mode(active == "plan")
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("restore plan state failed: {error}"),
            })?;
        if let Ok(mut requested) = self.inner.requested_permission_mode.lock() {
            *requested = active.clone();
        }
        Ok(active)
    }

    async fn persist_session_permission_mode(&self, mode: &str) -> Result<(), ClientError> {
        let path = self.inner.session_writer.active_path();
        session_identity_snapshot_for_path(&self.inner.session_writer, &path)
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("session transcript identity import failed: {error}"),
            })?;
        if !path.exists() {
            let session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| ClientError::Internal {
                    message: "persist session permission mode failed: invalid transcript path"
                        .into(),
                })?;
            self.inner
                .session_writer
                .append_mobile_empty_session(session_id, "新对话", &self.session_cwd())
                .await
                .map_err(|error| ClientError::Internal {
                    message: format!("persist session permission anchor failed: {error}"),
                })?;
            self.inner
                .session_writer
                .append_session_mode(self.inner.session_mode.as_str())
                .await
                .map_err(|error| ClientError::Internal {
                    message: format!("persist session mode failed: {error}"),
                })?;
        }
        session_identity_snapshot_for_path(&self.inner.session_writer, &path)
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("session transcript identity import failed: {error}"),
            })?;
        self.inner
            .session_writer
            .append_permission_mode(mode)
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("persist session permission mode failed: {error}"),
            })?;
        session_identity_snapshot_for_path(&self.inner.session_writer, &path)
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("session transcript identity import failed: {error}"),
            })?;
        Ok(())
    }

    async fn persist_mobile_empty_session_anchor(
        &self,
        session_id: uuid::Uuid,
        cwd: &str,
        title: &str,
    ) -> Result<(), ClientError> {
        let path = session::jsonl::session_path(&self.lingxi_home, cwd, &session_id.to_string());
        let writer = session::jsonl::JsonlWriter::new(path, self.fs.clone());
        writer
            .append_mobile_empty_session(
                &session_id.to_string(),
                if title.is_empty() { "新对话" } else { title },
                cwd,
            )
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("persist empty session failed: {error}"),
            })?;
        writer
            .append_session_mode(self.inner.session_mode.as_str())
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("persist empty session mode failed: {error}"),
            })
    }

    async fn resume_session_impl(
        &self,
        session_id: String,
        cwd: Option<String>,
        empty_bootstrap_title: Option<String>,
        selected_path: Option<std::path::PathBuf>,
    ) -> Result<(), ClientError> {
        if self.active_cancel.lock().await.is_some() {
            return Err(ClientError::Rejected {
                message: "cannot resume while a turn is in flight".into(),
            });
        }

        let canonical_session_id = session_id.strip_prefix("sess:").unwrap_or(&session_id);
        let uuid =
            uuid::Uuid::parse_str(canonical_session_id).map_err(|error| ClientError::Rejected {
                message: format!("resume: malformed session id {session_id:?}: {error}"),
            })?;
        // Resolve the catalog key the same way the ResumeSession gate
        // compared it: CANONICAL spelling, and an empty string treated as
        // "unset" (the gate's own `filter(|c| !c.is_empty())` skips it, so a
        // literal `""` must not become the project-dir key here either).
        let cwd = match cwd.as_deref().filter(|c| !c.is_empty()) {
            Some(requested) => canonical_cwd_string(std::path::Path::new(requested)),
            None => self.session_cwd().clone(),
        };
        // Capture exactly one source for history, directory, permissions and
        // mode. Picker resume never resolves its UUID a second time.
        let mut captured = match selected_path.as_deref() {
            Some(path) => {
                session::jsonl::load_routed_session_from_path_with_source(
                    path,
                    uuid,
                    self.fs.clone(),
                )
                .await
            }
            None => {
                session::jsonl::load_routed_session_with_source(
                    &self.lingxi_home,
                    &cwd,
                    uuid,
                    self.fs.clone(),
                )
                .await
            }
        };
        // CLI discovery deliberately omits empty files. A current mobile/cron
        // anchor is an explicit durable owner, so admit only that exact typed
        // record and leave ordinary missing/corrupt transcripts rejected.
        if captured.is_err() {
            let path = selected_path.clone().unwrap_or_else(|| {
                session::jsonl::session_path(&self.lingxi_home, &cwd, &uuid.to_string())
            });
            if let Ok(file) = self.fs.read_file(&path.to_string_lossy(), None, None).await {
                let loaded = session::jsonl::route_lines(&file.content);
                if loaded.mobile_empty_sessions.contains(&uuid.to_string()) {
                    captured = Ok((path, file.content, loaded));
                }
            }
        }
        // Resolve a recorded directory before changing any active-session state.
        // A missing or escaping workspace is a failed restore, never permission
        // to run the recovered conversation in the previous session's cwd.
        let restored_directory = captured
            .as_ref()
            .ok()
            .and_then(|(path, _, loaded)| {
                session::jsonl::loader::transcript_workspace_path(path, loaded)
            })
            .map(|hint| self.workspace_target(&hint).map(|(logical, _)| logical))
            .transpose()?;
        let captured_empty_source = captured
            .as_ref()
            .ok()
            .filter(|(_, _, loaded)| {
                loaded.messages_in_order.is_empty()
                    && loaded.mobile_empty_sessions.contains(&uuid.to_string())
            })
            .map(|(path, _, _)| path.clone());
        let recorded_permission_mode = captured
            .as_ref()
            .ok()
            .and_then(|(_, _, loaded)| loaded.permission_modes.get(&uuid.to_string()).cloned());
        let effective_session_mode = captured
            .as_ref()
            .ok()
            .and_then(|(_, _, loaded)| {
                loaded
                    .session_modes
                    .get(&uuid.to_string())
                    .and_then(|value| session::jsonl::SessionMode::from_str(value))
            })
            .unwrap_or(session::jsonl::SessionMode::Code);
        let mut restored_project_root = None;
        let replay_result = captured
            .map_err(orchestrator::resume::ResumeError::from)
            .and_then(|(path, source, loaded)| {
                restored_project_root = loaded
                    .mobile_empty_session_cwds
                    .get(&uuid.to_string())
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        loaded
                            .messages_in_order
                            .first()
                            .map(|message| std::path::PathBuf::from(&message.cwd))
                    })
                    .or_else(|| restored_directory.clone());
                let mut replayed =
                    orchestrator::resume::replay_captured_session(path, source, loaded, uuid)?;
                replayed.working_directory = restored_directory.clone();
                Ok(replayed)
            });
        if effective_session_mode != self.inner.session_mode {
            return Err(ClientError::Rejected {
                message: format!(
                    "resume: session {session_id} belongs to {} mode, but this source runs {} mode",
                    effective_session_mode.as_str(),
                    self.inner.session_mode.as_str()
                ),
            });
        }

        match replay_result {
            Ok(replayed) => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                let resume_plan_mode = replayed.state.plan_mode;
                let previous_permission_mode = handle
                    .permission_mode()
                    .await
                    .unwrap_or_else(|| self.inner.session_default_permission_mode.clone());
                let fallback_permission_mode = if resume_plan_mode {
                    "plan".to_string()
                } else {
                    recorded_permission_mode
                        .clone()
                        .unwrap_or_else(|| self.inner.session_default_permission_mode.clone())
                };
                let target_permission_mode = self
                    .restore_preferred_permission_mode(&fallback_permission_mode)
                    .await?;
                let resume_runtime = self
                    .resume_runtime_with_model_preference(replayed.handle_runtime_snapshot())
                    .await;
                if let Err(error) = handle
                    .resume_session(
                        lingxi_core::types::SessionId::from_uuid(uuid),
                        replayed.state.history.clone(),
                        replayed.last_message_uuid.map(|id| id.to_string()),
                        replayed.state.active_goal.clone().map(|goal| {
                            lingxi_core::host::ActiveGoalSnapshot {
                                condition: goal.condition,
                                set_at: goal.set_at,
                                last_reason: goal.last_reason,
                                iterations: goal.iterations,
                                tokens_at_start: goal.tokens_at_start,
                            }
                        }),
                        resume_runtime,
                    )
                    .await
                {
                    let _ = self
                        .restore_session_permission_mode(&previous_permission_mode)
                        .await;
                    return Err(ClientError::Internal {
                        message: format!("resume_session failed: {error}"),
                    });
                }
                if let Some(root) = restored_project_root {
                    if self.inner.mobile_linux.is_none()
                        || root.starts_with(mobile_linux_api::guest_paths::HOME)
                    {
                        self.inner.session_cwd.restore_project_root(root);
                    }
                }
                let cwd = self.session_cwd();
                self.restore_preferred_reasoning_selection().await;
                self.restore_preferred_fast_mode().await;
                {
                    if let Err(error) = handle.set_plan_mode(target_permission_mode == "plan").await
                    {
                        let _ = self
                            .restore_session_permission_mode(&previous_permission_mode)
                            .await;
                        return Err(ClientError::Internal {
                            message: format!("resume plan mode failed: {error}"),
                        });
                    }
                }
                self.retarget_session_writer(lingxi_core::types::SessionId::from_uuid(uuid), &cwd)
                    .await?;
                self.inner
                    .workflow_checkpoints
                    .adopt_session(&uuid.to_string(), self.inner.task_registry.as_ref())
                    .await;
                let messages = client::adapter::lowering::lower_replayed_transcript(&replayed);
                self.event_sink
                    .emit(ClientEvent::SessionResumed {
                        session_id: uuid.to_string(),
                        mode: lower_session_mode(self.inner.session_mode),
                        messages,
                    })
                    .await;
                if let Some(usage) = replayed.runtime_metadata.current_usage {
                    self.event_sink
                        .emit(client::adapter::lowering::lower_current_usage(usage))
                        .await;
                }
                self.event_sink
                    .emit(ClientEvent::StatusSnapshot {
                        snapshot: lower_status_snapshot(&handle.get_status_snapshot().await),
                    })
                    .await;
                self.emit_controls_snapshot().await;
                let _ = self.session_lifecycle_tx.send(uuid.to_string());
                Ok(())
            }
            Err(error) => {
                let is_empty = matches!(
                    &error,
                    orchestrator::resume::ResumeError::Loader(
                        session::jsonl::LoaderError::EmptyDirectory
                    )
                );
                let is_missing = matches!(
                    &error,
                    orchestrator::resume::ResumeError::Loader(
                        session::jsonl::LoaderError::SessionNotFound { .. }
                    )
                );
                let cwd = restored_directory
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or(cwd);
                let has_anchor = is_empty && captured_empty_source.is_some();
                let may_bootstrap = empty_bootstrap_title.is_some() && (is_empty || is_missing);
                if !has_anchor && !may_bootstrap {
                    return Err(ClientError::Rejected {
                        message: format!("resume: session {session_id} not resumable: {error}"),
                    });
                }

                if !has_anchor {
                    self.persist_mobile_empty_session_anchor(
                        uuid,
                        &cwd,
                        empty_bootstrap_title.as_deref().unwrap_or("新对话"),
                    )
                    .await?;
                }

                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                let previous_permission_mode = handle
                    .permission_mode()
                    .await
                    .unwrap_or_else(|| self.inner.session_default_permission_mode.clone());
                let fallback_permission_mode = recorded_permission_mode
                    .clone()
                    .unwrap_or_else(|| self.inner.session_default_permission_mode.clone());
                let target_permission_mode = self
                    .restore_preferred_permission_mode(&fallback_permission_mode)
                    .await?;
                if let Err(resume_error) = handle
                    .resume_session(
                        lingxi_core::types::SessionId::from_uuid(uuid),
                        Vec::new(),
                        None,
                        None,
                        self.resume_runtime_with_model_preference(
                            lingxi_core::host::ResumeRuntimeSnapshot {
                                transcript_path: captured_empty_source,
                                working_directory: restored_directory,
                                ..Default::default()
                            },
                        )
                        .await,
                    )
                    .await
                {
                    let _ = self
                        .restore_session_permission_mode(&previous_permission_mode)
                        .await;
                    return Err(ClientError::Internal {
                        message: format!("resume empty session failed: {resume_error}"),
                    });
                }
                let root = restored_project_root
                    .filter(|root| {
                        self.inner.mobile_linux.is_none()
                            || root.starts_with(mobile_linux_api::guest_paths::HOME)
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from(&cwd));
                self.inner.session_cwd.restore_project_root(root);
                self.restore_preferred_reasoning_selection().await;
                self.restore_preferred_fast_mode().await;
                handle
                    .set_plan_mode(target_permission_mode == "plan")
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: format!("resume empty session plan mode failed: {error}"),
                    })?;
                self.retarget_session_writer(lingxi_core::types::SessionId::from_uuid(uuid), &cwd)
                    .await?;
                self.inner
                    .workflow_checkpoints
                    .adopt_session(&uuid.to_string(), self.inner.task_registry.as_ref())
                    .await;
                self.event_sink
                    .emit(ClientEvent::SessionResumed {
                        session_id: uuid.to_string(),
                        mode: lower_session_mode(self.inner.session_mode),
                        messages: Vec::new(),
                    })
                    .await;
                self.emit_controls_snapshot().await;
                let _ = self.session_lifecycle_tx.send(uuid.to_string());
                Ok(())
            }
        }
    }

    /// Reserve the connection's single turn slot without replacing its owner.
    async fn reserve_turn(&self, turn_id: Option<u64>) -> Result<Arc<ActiveTurn>, ClientError> {
        self.reserve_turn_for_session(turn_id, None).await
    }

    async fn lock_turn_delivery(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<crate::mobile::turn_delivery::TurnDeliveryGuard, ClientError> {
        let gate = self
            .durable_turns
            .delivery_gate(session_id, turn_id)
            .map_err(Self::map_durable_turn_error)?;
        gate.lock().await.map_err(|error| ClientError::Rejected {
            message: error.to_string(),
        })
    }

    /// Wait until callbacks already admitted for this turn have returned.
    /// This does not fence a producer which begins after this barrier.
    async fn drain_turn_delivery(&self, turn: &ActiveTurn) -> Result<(), ClientError> {
        if let Some(turn_id) = turn.turn_id.filter(|_| !turn.session_id.is_empty()) {
            self.lock_turn_delivery(&turn.session_id, turn_id)
                .await?
                .complete();
        }
        Ok(())
    }

    fn spawn_recovery_delivery(
        &self,
        guard: crate::mobile::turn_delivery::TurnDeliveryGuard,
        snapshot: client::protocol::events::TurnRecoverySnapshotDto,
    ) -> tokio::task::JoinHandle<()> {
        let sink = self.connection_sink.clone();
        tokio::spawn(async move {
            sink.emit(ClientEvent::TurnRecoveryState { snapshot }).await;
            guard.complete();
        })
    }

    async fn await_recovery_delivery(task: tokio::task::JoinHandle<()>) -> Result<(), ClientError> {
        task.await.map_err(|error| ClientError::Internal {
            message: format!("turn recovery event delivery failed: {error}"),
        })
    }

    async fn reserve_turn_for_session(
        &self,
        turn_id: Option<u64>,
        expected_session_id: Option<&str>,
    ) -> Result<Arc<ActiveTurn>, ClientError> {
        let mut active = self.active_cancel.lock().await;
        if active.is_some() {
            return Err(ClientError::Rejected {
                message: "a turn is already in flight".into(),
            });
        }
        let session_id = self
            .inner
            .active_session_uuid
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        if expected_session_id.is_some_and(|expected| expected != session_id) {
            return Err(ClientError::Rejected {
                message: "queued mobile slash belongs to a session that is no longer active".into(),
            });
        }
        let live_owner = self
            .durable_turns
            .reserve_live_owner(&session_id, turn_id)
            .map_err(Self::map_durable_turn_error)?;
        let permission_owner_id = self
            .inner
            .permission_gate
            .begin_main_turn(Some(session_id.clone()), turn_id);
        let turn = Arc::new(ActiveTurn::with_live_owner(
            turn_id,
            session_id,
            permission_owner_id,
            live_owner,
        ));
        // Bind the queue abort callback to this generation before publishing
        // the owner; a late callback from an older turn cannot relabel it.
        let reason = self.cancel_reason.reset_and_bind();
        self.message_queue.register_owned_active_turn(
            turn.queue_owner.clone(),
            turn.cancel.clone(),
            Arc::new(move || {
                reason.set(orchestrator::prompt::mid_turn_input::CancelReason::QueueNowCommand);
            }),
        );
        *active = Some(turn.clone());
        Ok(turn)
    }

    /// Called while the active-owner lock is held. Batch membership must be
    /// bound to this exact live owner, not inferred from an old checkpoint.
    fn live_owner_matches_cancel(&self, turn: &ActiveTurn, requested_turn_id: Option<u64>) -> bool {
        if turn.matches_cancel(requested_turn_id) {
            return true;
        }
        let Some(requested_id) = requested_turn_id else {
            return false;
        };
        let owner = self
            .durable_turns
            .coalesced_owner_for_member(&turn.session_id, requested_id)
            .ok()
            .flatten()
            .and_then(|owner_id| self.durable_turns.load(&turn.session_id, owner_id).ok());
        owner.is_some_and(|owner| {
            owner
                .coalesced_batch
                .as_ref()
                .is_some_and(|batch| batch.member_turn_ids.contains(&requested_id))
                && ((matches!(
                    owner.queue_delivery,
                    QueueDeliveryState::CoalescedClaimed | QueueDeliveryState::Delivered
                ) && turn
                    .mid_turn_members
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&requested_id))
                    || (owner.queue_delivery == QueueDeliveryState::Claimed
                        && owner.state == Some(TurnRecoveryStateDto::Running)
                        && turn.turn_id == Some(owner.turn_id)))
        })
    }

    async fn cancel_inactive_turn(
        &self,
        session_id: &str,
        turn_id: u64,
        original_owner: Option<&Arc<ActiveTurn>>,
    ) -> Result<(), ClientError> {
        let mut delivery = self.lock_turn_delivery(session_id, turn_id).await?;
        let active = self.active_cancel.lock().await;
        let reroute_owner = {
            let current_session = self
                .inner
                .active_session_uuid
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if current_session.as_str() != session_id {
                return Err(ClientError::Rejected {
                    message: "cancel request belongs to a different session".into(),
                });
            }
            if active.as_ref().is_some_and(|owner| {
                owner.session_id == session_id
                    && self.live_owner_matches_cancel(owner, Some(turn_id))
            }) {
                if original_owner.is_some_and(|original| {
                    !active
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, original))
                }) {
                    return Err(ClientError::Rejected {
                        message: "the original turn owner has changed".into(),
                    });
                }
                Some(active.as_ref().expect("checked active owner").clone())
            } else {
                None
            }
        };
        if let Some(expected) = reroute_owner {
            // A claim may have won after the first routing check. Release both
            // gates before rechecking this exact owner and awaiting callbacks.
            drop(active);
            drop(delivery);
            return Box::pin(self.cancel_active_turn_for_owner(
                session_id,
                Some(turn_id),
                Some(expected),
            ))
            .await;
        }
        let (snapshot, member_ids) = {
            // Recheck after reacquiring the session guard, and keep it through
            // the synchronous checkpoint change. No std guard crosses await.
            let current_session = self
                .inner
                .active_session_uuid
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if current_session.as_str() != session_id {
                return Err(ClientError::Rejected {
                    message: "cancel request belongs to a different session".into(),
                });
            }
            let cancel = || -> Result<_, DurableTurnStoreError> {
                // A failed first checkpoint read cannot leave this queue item
                // dispatchable in the current store after a received Stop.
                self.durable_turns.fence_queued_stop(session_id, turn_id);
                if let Some(owner_id) = self
                    .durable_turns
                    .coalesced_owner_for_member(session_id, turn_id)?
                {
                    let checkpoint = self.durable_turns.load(session_id, owner_id)?;
                    let own_paused = owner_id == turn_id
                        && matches!(
                            checkpoint.state,
                            Some(
                                TurnRecoveryStateDto::PausedRecoverable
                                    | TurnRecoveryStateDto::WaitingForUser
                            )
                        );
                    let unconfirmed_private = checkpoint.reason.as_deref()
                        == Some("queued_cancellation_unconfirmed_after_recovery")
                        && matches!(checkpoint.queue_delivery, QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending);
                    if !own_paused && !unconfirmed_private
                        && (checkpoint.state.is_some()
                            || !matches!(
                                checkpoint.queue_delivery,
                                QueueDeliveryState::Pending | QueueDeliveryState::CoalescedPending
                            ))
                    {
                        return Err(DurableTurnStoreError::BatchAlreadyClaimed {
                            session_id: session_id.to_string(),
                            turn_id,
                        });
                    }
                }
                Ok(
                    match self.durable_turns.cancel_pending(session_id, turn_id) {
                        Ok(Some(ids)) => (
                            Some(
                                self.durable_turns
                                    .expose_cancelled_pending(session_id, turn_id)?,
                            ),
                            ids,
                        ),
                        Ok(None) => match self.durable_turns.load(session_id, turn_id) {
                            Ok(checkpoint)
                                if matches!(
                                    checkpoint.state,
                                    Some(
                                        TurnRecoveryStateDto::PausedRecoverable
                                            | TurnRecoveryStateDto::WaitingForUser
                                    )
                                ) =>
                            {
                                (
                                    self.durable_turns.cancel(session_id, turn_id)?,
                                    vec![turn_id],
                                )
                            }
                            Ok(_) | Err(DurableTurnStoreError::NotFound { .. }) => {
                                (None, Vec::new())
                            }
                            Err(error) => return Err(error),
                        },
                        Err(DurableTurnStoreError::NotFound { .. })
                        | Err(DurableTurnStoreError::Terminal { .. }) => (None, Vec::new()),
                        Err(error) => return Err(error),
                    },
                )
            };
            let owned_lease = original_owner
                .and_then(|turn| turn.live_owner.as_ref())
                .or_else(|| {
                    active
                        .as_ref()
                        .filter(|turn| turn.session_id == session_id)
                        .and_then(|turn| turn.live_owner.as_ref())
                });
            delivery.arm();
            let cancelled =
                self.durable_turns
                    .with_no_pending_admission(session_id, turn_id, || match owned_lease {
                        Some(lease) => self.durable_turns.with_live_owner(
                            session_id,
                            lease,
                            Some(turn_id),
                            cancel,
                        ),
                        None => {
                            self.durable_turns
                                .with_no_live_owner(session_id, Some(turn_id), cancel)
                        }
                    });
            match cancelled {
                Ok(result) => result,
                Err(error) => {
                    if !matches!(error, DurableTurnStoreError::Storage(_)) {
                        delivery.complete();
                    }
                    return Err(Self::map_durable_turn_error(error));
                }
            }
        };
        drop(active);
        let delivery_task = if let Some(snapshot) = snapshot {
            Some(self.spawn_recovery_delivery(delivery, snapshot))
        } else {
            delivery.complete();
            None
        };
        remove_mobile_prompt_dispatches(&self.message_queue, session_id, &member_ids).await;
        if let Some(task) = delivery_task {
            Self::await_recovery_delivery(task).await?;
        }
        // A persisted private batch may contain more than the requested turn.
        // Each member needs its own correlated terminal callback and gate.
        for member_id in member_ids.into_iter().filter(|id| *id != turn_id) {
            let mut member_delivery = self.lock_turn_delivery(session_id, member_id).await?;
            member_delivery.arm();
            match self
                .durable_turns
                .expose_cancelled_pending(session_id, member_id)
            {
                Ok(snapshot) => {
                    let task = self.spawn_recovery_delivery(member_delivery, snapshot);
                    Self::await_recovery_delivery(task).await?;
                }
                Err(error) => {
                    if !matches!(error, DurableTurnStoreError::Storage(_)) {
                        member_delivery.complete();
                    }
                    return Err(Self::map_durable_turn_error(error));
                }
            }
        }
        Ok(())
    }

    /// Cooperatively cancel exactly the requested turn and wait until every
    /// owned tool/event producer has unwound. `Block` tools deliberately finish
    /// naturally; force-aborting the outer task would violate their mutation
    /// safety contract. A stale specific id is a no-op and cannot drain or
    /// otherwise disturb the current turn.
    async fn cancel_active_turn(
        &self,
        requested_session_id: &str,
        requested_turn_id: Option<u64>,
    ) -> Result<(), ClientError> {
        self.cancel_active_turn_for_owner(requested_session_id, requested_turn_id, None)
            .await
    }

    async fn cancel_active_turn_for_owner(
        &self,
        requested_session_id: &str,
        requested_turn_id: Option<u64>,
        expected_owner: Option<Arc<ActiveTurn>>,
    ) -> Result<(), ClientError> {
        let active = {
            let active = self.active_cancel.lock().await;
            let session = self
                .inner
                .active_session_uuid
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if requested_session_id.is_empty() || session.as_str() != requested_session_id {
                return Err(ClientError::Rejected {
                    message: "cancel request belongs to a different session".into(),
                });
            }
            active.clone()
        };
        if let Some(expected) = expected_owner.as_ref() {
            if !active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected))
            {
                if let Some(turn_id) = requested_turn_id.or(expected.turn_id) {
                    return self
                        .cancel_inactive_turn(requested_session_id, turn_id, Some(expected))
                        .await;
                }
                return Err(ClientError::Rejected {
                    message: "the original turn owner has changed".into(),
                });
            }
        }
        let Some(turn) = active else {
            if requested_turn_id.is_none() {
                // An idle cancel affects the shared scheduler. Exclude session
                // retargeting while it executes, without waiting on a long
                // transition that may itself depend on cancellation.
                let _transition =
                    self.loop_transition
                        .try_lock()
                        .map_err(|_| ClientError::Rejected {
                            message: "session transition is in progress; retry cancellation".into(),
                        })?;
                if self.active_session_id() != requested_session_id {
                    return Err(ClientError::Rejected {
                        message: "cancel request belongs to a different session".into(),
                    });
                }
                if self.active_cancel.lock().await.is_some() {
                    return Err(ClientError::Rejected {
                        message: "a turn became active; retry cancellation".into(),
                    });
                }
                if let Some(scheduler) = self.inner.wakeup_scheduler.get() {
                    tool_cron::cancel_dynamic_loop_on_user_abort(scheduler).await;
                }
            }

            if let Some(turn_id) = requested_turn_id {
                return self
                    .cancel_inactive_turn(requested_session_id, turn_id, None)
                    .await;
            }
            tracing::debug!(
                requested_turn_id,
                "mobile: ignored cancel without an active turn"
            );
            return Ok(());
        };
        if turn.session_id != requested_session_id {
            return match requested_turn_id {
                Some(turn_id) => {
                    self.cancel_inactive_turn(requested_session_id, turn_id, None)
                        .await
                }
                None => Ok(()),
            };
        }
        let mut delivery = match turn.turn_id {
            Some(turn_id) => Some(self.lock_turn_delivery(&turn.session_id, turn_id).await?),
            None => None,
        };
        // Persist the cancellation boundary and fire the token while this
        // exact owner still holds the slot, without an await between them.
        // A cancelled command future cannot leave an alleged user interrupt
        // with a still-running owner, nor relabel a later owner's reason.
        let (finalize_claimed_bash_cancel, cancel_result, background_snapshot) = {
            let active = self.active_cancel.lock().await;
            if self.active_session_id() != requested_session_id {
                return Err(ClientError::Rejected {
                    message: "cancel request belongs to a different session".into(),
                });
            }
            if !active
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &turn))
            {
                // The owner may have finished a lifecycle pause after this
                // command observed it. Cancel that exact durable turn without
                // touching a different live owner or its cancellation token.
                let Some(turn_id) = requested_turn_id.or(turn.turn_id) else {
                    return Ok(());
                };
                drop(active);
                drop(delivery);
                return self
                    .cancel_inactive_turn(&turn.session_id, turn_id, Some(&turn))
                    .await;
            }
            if !self.live_owner_matches_cancel(&turn, requested_turn_id) {
                drop(active);
                drop(delivery);
                if let Some(turn_id) = requested_turn_id {
                    return self
                        .cancel_inactive_turn(requested_session_id, turn_id, Some(&turn))
                        .await;
                }
                return Ok(());
            }
            if turn.terminal_emitted.load(Ordering::Acquire) {
                // The outcome already won, but its callback and owner cleanup
                // may still be running. Preserve Cancel's release guarantee
                // without relabeling or interrupting the completed turn.
                drop(active);
                drop(delivery);
                self.wait_for_turn_release(&turn, false).await;
                return self.drain_turn_delivery(&turn).await;
            }
            let (finalize, result) = {
                let current_session = self
                    .inner
                    .active_session_uuid
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if current_session.as_str() != requested_session_id {
                    return Err(ClientError::Rejected {
                        message: "cancel request belongs to a different session".into(),
                    });
                }
                if let Some(guard) = delivery.as_mut() {
                    guard.arm();
                }
                let _admission = turn
                    .admission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                turn.explicit_cancel_requested
                    .store(true, Ordering::Release);
                let finalize = turn.turn_id.is_some_and(|turn_id| {
                    self.durable_turns
                        .load(&turn.session_id, turn_id)
                        .ok()
                        .is_some_and(|checkpoint| {
                            checkpoint.queue_operation
                                == crate::mobile::turn_durability::QueueOperationKind::Prompt
                                && checkpoint.prompt_mode == Some(PromptModeDto::Bash)
                                && checkpoint.state == Some(TurnRecoveryStateDto::Running)
                                && (checkpoint.queue_delivery == QueueDeliveryState::BashClaimed
                                    || (checkpoint.queue_delivery == QueueDeliveryState::Claimed
                                        && checkpoint.bash_result.is_some()))
                        })
                });
                let result = turn.turn_id.map(|turn_id| {
                    let member_ids = self
                        .durable_turns
                        .load(&turn.session_id, turn_id)
                        .ok()
                        .and_then(|checkpoint| checkpoint.coalesced_batch)
                        .map(|batch| batch.member_turn_ids)
                        .unwrap_or_else(|| vec![turn_id]);
                    let result = if finalize {
                        self.durable_turns
                            .mark_claimed_bash_cancel_pending(&turn.session_id, turn_id)
                            .map(Some)
                    } else {
                        self.durable_turns.cancel(&turn.session_id, turn_id)
                    };
                    (turn_id, member_ids, result)
                });
                self.cancel_reason
                    .set(orchestrator::prompt::mid_turn_input::CancelReason::UserInterrupt);
                turn.cancel.cancel();
                (finalize, result)
            };
            // Prevent the owner slot from being replaced while the registry
            // captures its current concrete task/requester instances. This
            // awaits only registry locks, never a task or listener callback.
            let snapshot = if lingxi_core::host::env::background_tasks_disabled() {
                None
            } else {
                Some(
                    self.inner
                        .task_registry
                        .snapshot_backgroundable_tasks()
                        .await,
                )
            };
            (finalize, result, snapshot)
        };
        let mut persistence_error = None;
        if let Some((turn_id, queued_member_ids, cancel_result)) = cancel_result {
            match cancel_result {
                Ok(Some(snapshot)) => {
                    let task = self.spawn_recovery_delivery(
                        delivery.take().expect("durable owner has delivery gate"),
                        snapshot,
                    );
                    remove_mobile_prompt_dispatches(
                        &self.message_queue,
                        &turn.session_id,
                        &queued_member_ids,
                    )
                    .await;
                    if let Err(error) = Self::await_recovery_delivery(task).await {
                        persistence_error = Some(error);
                    }
                }
                Ok(None) => {
                    if let Some(guard) = delivery.take() {
                        guard.complete();
                    }
                    remove_mobile_prompt_dispatches(
                        &self.message_queue,
                        &turn.session_id,
                        &queued_member_ids,
                    )
                    .await;
                }
                Err(DurableTurnStoreError::NotFound { .. }) => {
                    if let Some(guard) = delivery.take() {
                        guard.complete();
                    }
                }
                Err(error) => {
                    if !matches!(error, DurableTurnStoreError::Storage(_)) {
                        if let Some(guard) = delivery.take() {
                            guard.complete();
                        }
                    } else {
                        drop(delivery.take());
                    }
                    tracing::warn!(%error, turn_id, "mobile: failed to persist explicit cancel");
                    persistence_error = Some(Self::map_durable_turn_error(error));
                }
            }
        }

        let cancelled_permissions = if let Some(owner_id) = turn.permission_owner_id {
            self.inner.permission_gate.cancel_owner(owner_id).await
        } else {
            Vec::new()
        };
        let permission_count = cancelled_permissions.len();
        drop(cancelled_permissions);
        if let Some(snapshot) = background_snapshot {
            let background = self
                .inner
                .task_registry
                .background_snapshot_with_outcome(
                    snapshot,
                    lingxi_core::host::task_registry::TaskBackgroundReason::TurnAbort,
                )
                .await;
            if background.capture_unknown || background.unknown != 0 {
                persistence_error.get_or_insert(ClientError::Internal {
                    message: if background.capture_unknown {
                        "could not determine the full set of background tasks".into()
                    } else {
                        format!(
                            "could not confirm background transition for {} task(s)",
                            background.unknown
                        )
                    },
                });
            }
        }
        self.wait_for_turn_release(&turn, true).await;
        if finalize_claimed_bash_cancel {
            if let Some(turn_id) = turn.turn_id {
                let mut final_delivery = self.lock_turn_delivery(&turn.session_id, turn_id).await?;
                final_delivery.arm();
                match self.durable_turns.cancel(&turn.session_id, turn_id) {
                    Ok(Some(snapshot)) => {
                        let task = self.spawn_recovery_delivery(final_delivery, snapshot);
                        if let Err(error) = Self::await_recovery_delivery(task).await {
                            persistence_error.get_or_insert(error);
                        }
                    }
                    Ok(None)
                    | Err(DurableTurnStoreError::NotFound { .. })
                    | Err(DurableTurnStoreError::Terminal { .. }) => final_delivery.complete(),
                    Err(error) => {
                        if !matches!(error, DurableTurnStoreError::Storage(_)) {
                            final_delivery.complete();
                        }
                        tracing::warn!(
                            %error,
                            turn_id,
                            "mobile: failed to finalize interrupted Bash checkpoint"
                        );
                        persistence_error
                            .get_or_insert_with(|| Self::map_durable_turn_error(error));
                    }
                }
            }
        }
        // AskUserQuestion's Block resolver observes the turn cancellation
        // token and drops only its own response receiver. Remove exactly
        // those closed requests after the owner has unwound; unrelated
        // workflow questions remain parked until connection/session teardown.
        let question_count = self.ask_user_question_broker.cancel_closed().await;
        if let Err(error) = self.drain_turn_delivery(&turn).await {
            persistence_error.get_or_insert(error);
        }
        tracing::debug!(
            requested_turn_id,
            active_turn_id = turn.turn_id,
            permission_count,
            question_count,
            "mobile: waiting for cancelled turn to release its owner slot"
        );
        tracing::debug!(active_turn_id = turn.turn_id, "mobile: cancel completed");
        if let Some(error) = persistence_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// Ask a live turn to unwind and wait until its owner slot is released.
    /// `emit_task_error` is used only for an explicit cancel; a paused turn is
    /// intentionally silent because its recovery snapshot is the authoritative
    /// boundary event.
    async fn wait_for_turn_release(&self, turn: &Arc<ActiveTurn>, emit_task_error: bool) {
        if let Some(task) = turn.take_task_handle() {
            if task.await.is_err() {
                let mut active = self.active_cancel.lock().await;
                release_active_turn(&mut active, &self.message_queue, turn);
                drop(active);
                turn.mark_completed();
                if let Some(owner_id) = turn.permission_owner_id {
                    self.inner.permission_gate.end_main_turn(owner_id);
                }
                if emit_task_error {
                    self.event_sink
                        .emit(ClientEvent::Error {
                            kind: ErrorKindDto::Internal,
                            message: "turn task terminated unexpectedly".into(),
                        })
                        .await;
                }
            }
        } else {
            // Another concurrent Cancel/Pause may own the JoinHandle. The
            // completion notification still gives every caller the same
            // release guarantee.
            turn.wait_completed().await;
        }
    }

    /// Quiesce the matching live turn before publishing its recoverable pause.
    /// The cancellation token is still used to stop the orchestrator, but the
    /// lifecycle listener observes `quiescing` and drops the resulting
    /// cancellation/late live events rather than exposing `Cancelled`.
    async fn pause_active_turn(
        &self,
        requested_session_id: String,
        turn_id: u64,
        reason: String,
    ) -> Result<(), ClientError> {
        let active_guard = self.active_cancel.lock().await;
        let active = active_guard.clone();
        let owns_live_slot = active.as_ref().is_some_and(|turn| {
            turn.session_id == requested_session_id && turn.turn_id == Some(turn_id)
        });
        if let Some(turn) = active.as_ref().filter(|_| owns_live_slot) {
            if turn.terminal_emitted.load(Ordering::Acquire) {
                return Err(ClientError::Rejected {
                    message: "the turn has already ended".into(),
                });
            }
            // If an explicit user cancel won first, do not suppress its
            // terminal delivery or relabel its transcript as a pause.
            turn.request_lifecycle_quiesce(&self.cancel_reason);
        }
        drop(active_guard);
        let session_id = if let Some(turn) = active.clone().filter(|_| owns_live_slot) {
            let session_id = turn.session_id.clone();
            if let Some(owner_id) = turn.permission_owner_id {
                let _ = self.inner.permission_gate.cancel_owner(owner_id).await;
            }
            self.wait_for_turn_release(&turn, false).await;
            // AskUserQuestion's Block resolver observes the turn cancellation
            // token and drops only its own response receiver. Remove exactly
            // those closed requests after the owner has unwound; unrelated
            // workflow questions still have an open receiver and remain
            // parked in the connection-scoped broker.
            let question_count = self.ask_user_question_broker.cancel_closed().await;
            tracing::debug!(
                turn_id,
                question_count,
                "mobile: cancelled closed main-turn AskUserQuestion requests before pause"
            );
            session_id
        } else {
            // A pause for a turn that does not own the live executor slot must
            // still be ANSWERED. The previous `return Ok(())` completed the
            // command while emitting nothing at all, and a client that awaits
            // the acknowledging `TurnRecoveryState` (iOS parks an untimed
            // continuation in `pauseAcknowledgements`) then waited forever.
            // Fall through to the same durable path a background pause takes:
            // it publishes the requested turn's snapshot, or maps a
            // missing/terminal checkpoint to a `ClientError` the caller can
            // observe. Nothing here touches the unrelated live turn.
            tracing::debug!(
                requested_turn_id = turn_id,
                active_turn_id = ?active.as_ref().and_then(|turn| turn.turn_id),
                "mobile: pausing a turn that does not own the live executor slot"
            );
            requested_session_id
        };

        // Read only after the owner has unwound. Serialize the final decision
        // with an explicit Cancel, including one arriving while pause waits.
        let mut delivery = self.lock_turn_delivery(&session_id, turn_id).await?;
        let active_guard = self.active_cancel.lock().await;
        if active
            .as_ref()
            .filter(|_| owns_live_slot)
            .is_some_and(|turn| turn.explicit_cancel_requested.load(Ordering::Acquire))
        {
            return Err(ClientError::Rejected {
                message: "the turn was explicitly cancelled".into(),
            });
        }
        let pause = || {
            let checkpoint = self.durable_turns.load(&session_id, turn_id)?;
            self.durable_turns.transition(
                &session_id,
                turn_id,
                TurnRecoveryStateDto::PausedRecoverable,
                checkpoint.safe_to_resume,
                Some(reason),
            )
        };
        let live_owner = active
            .as_ref()
            .filter(|turn| turn.session_id == session_id)
            .and_then(|turn| turn.live_owner.as_ref());
        delivery.arm();
        let pause_result = match live_owner {
            Some(lease) => {
                self.durable_turns
                    .with_live_owner(&session_id, lease, Some(turn_id), pause)
            }
            None => self
                .durable_turns
                .with_no_live_owner(&session_id, Some(turn_id), pause),
        };
        let snapshot = match pause_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if !matches!(error, DurableTurnStoreError::Storage(_)) {
                    delivery.complete();
                }
                return Err(Self::map_durable_turn_error(error));
            }
        };
        drop(active_guard);
        let task = self.spawn_recovery_delivery(delivery, snapshot);
        Self::await_recovery_delivery(task).await
    }

    async fn submit_slash_command(
        &self,
        raw: ClientTextDto,
        images: Vec<ImageRefDto>,
        client_turn_id: Option<u64>,
        transition: tokio::sync::MutexGuard<'_, ()>,
        pending: Option<Arc<PendingSubmission>>,
    ) -> Result<(), ClientError> {
        let raw_source = raw.into_projection();
        // Workspace commands are explicit host navigation. They must never be
        // queued as model turns or accepted while another turn owns the session.
        if let Some(parsed) = command_api::parse_slash_command(&raw_source) {
            let name = parsed.name.string_at("").unwrap_or("");
            if matches!(name, "cd" | "add-dir") {
                if let Some(raw_text) = raw_source.string_at("") {
                    let _ = self
                        .inner
                        .orchestrator
                        .record_prompt_suggestion_submission(raw_text)
                        .await;
                }
                let result = match parsed.raw_args_utf8().filter(|input| !input.is_empty()) {
                    Some(input) if name == "cd" => self.change_workspace_inner(input).await,
                    Some(input) => self.add_workspace_inner(input).await,
                    None => Err(ClientError::Rejected {
                        message: format!("Usage: /{name} <path>"),
                    }),
                };
                let (display, is_error) = match result {
                    Ok(()) => (
                        format!(
                            "{} {}",
                            if name == "cd" {
                                "Moved to"
                            } else {
                                "Added directory to"
                            },
                            self.session_cwd()
                        ),
                        false,
                    ),
                    Err(error) => (error.to_string(), true),
                };
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display: display.into(),
                        is_error,
                    })
                    .await;
                return Ok(());
            }
        }
        self.inner.dispatcher.refresh_command_snapshot().await;
        if self.inner.session_mode == session::jsonl::SessionMode::Chat {
            let allowed = if let Some(parsed) = command_api::parse_slash_command(&raw_source) {
                let registry = self.inner.slash_registry.read().await;
                registry.resolve(&parsed.name).is_some_and(|command| {
                    command_visible_in_session_mode(command, self.inner.session_mode)
                })
            } else {
                true
            };
            if !allowed {
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display: "command is unavailable in Chat mode".into(),
                        is_error: true,
                    })
                    .await;
                return Ok(());
            }
        }

        // A queued item always remains attached to the active owner's session.
        // The loop transition guard prevents a concurrent New/Resume retarget
        // between choosing that session and durably admitting the operation.
        let active_session = {
            self.active_cancel
                .lock()
                .await
                .as_ref()
                .map(|turn| turn.session_id.clone())
        };
        if let Some(active_session) = active_session {
            self.persist_and_enqueue_mobile_slash(
                &active_session,
                client_turn_id,
                raw_source,
                images.clone(),
            )
            .await?;
            return Ok(());
        }
        let session_id = self.active_session_id();
        if !self
            .durable_turns
            .scan_pending(&session_id)
            .map_err(Self::map_durable_turn_error)?
            .is_empty()
        {
            self.persist_and_enqueue_mobile_slash(
                &session_id,
                client_turn_id,
                raw_source,
                images.clone(),
            )
            .await?;
            return Ok(());
        }

        // Allocate the durable id before reserving, including when the client
        // omitted a correlator. Detached work is fenced by that actual id.
        let checkpoint = self
            .durable_turns
            .admit_queued_slash(
                &session_id,
                client_turn_id,
                client_turn_id,
                raw_source.clone(),
                images.clone(),
            )
            .map_err(Self::map_durable_turn_error)?;
        let owner = match self
            .reserve_turn_for_session(Some(checkpoint.turn_id), Some(&session_id))
            .await
        {
            Ok(owner) => owner,
            Err(ClientError::Rejected { .. }) => {
                if self.active_session_id() == session_id {
                    enqueue_mobile_slash_dispatch(&self.message_queue, &session_id, &checkpoint)
                        .await;
                    return Ok(());
                }
                return Err(ClientError::Rejected {
                    message: "a turn is already in flight".into(),
                });
            }
            Err(error) => return Err(error),
        };
        if let Some(pending) = &pending {
            pending.bind_owner(&owner);
        }
        let mut delivery = self
            .lock_turn_delivery(&owner.session_id, checkpoint.turn_id)
            .await?;
        let mut active = self.active_cancel.lock().await;
        if !active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &owner))
        {
            delivery.complete();
            return Err(ClientError::Rejected {
                message: "slash owner changed before admission".into(),
            });
        }
        if pending
            .as_ref()
            .is_some_and(|item| item.cancel_requested.load(Ordering::Acquire))
        {
            delivery.arm();
            let result = self
                .durable_turns
                .cancel_pending(&owner.session_id, checkpoint.turn_id)
                .and_then(|ids| {
                    if ids.is_some() {
                        self.durable_turns
                            .expose_cancelled_pending(&owner.session_id, checkpoint.turn_id)
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                });
            let snapshot = match result {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => {
                    delivery.complete();
                    return Err(ClientError::Rejected {
                        message: "slash cancellation could not be confirmed".into(),
                    });
                }
                Err(error) => {
                    if !matches!(error, DurableTurnStoreError::Storage(_)) {
                        delivery.complete();
                    }
                    return Err(Self::map_durable_turn_error(error));
                }
            };
            release_active_turn(&mut active, &self.message_queue, &owner);
            drop(active);
            if let Some(owner_id) = owner.permission_owner_id {
                self.inner.permission_gate.end_main_turn(owner_id);
            }
            if let Some(pending) = &pending {
                pending.suppressed.store(true, Ordering::Release);
            }
            let task = self.spawn_recovery_delivery(delivery, snapshot);
            owner.mark_completed();
            drop(transition);
            Self::await_recovery_delivery(task).await?;
            return Ok(());
        }
        let claimed = match self
            .durable_turns
            .claim_queued_slash(&owner.session_id, checkpoint.turn_id)
        {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                delivery.complete();
                drop(active);
                self.release_unspawned_turn(&owner).await;
                return Err(Self::map_durable_turn_error(error));
            }
        };
        delivery.complete();
        drop(active);
        drop(transition);
        #[cfg(test)]
        if let Some(pending) = &pending {
            let barrier = pending.owner_barrier.lock().unwrap().clone();
            if let Some((entered, release)) = barrier {
                entered.notify_one();
                release.notified().await;
            }
        }
        if !owner.cancel.is_cancelled() {
            if let Some(raw_text) = raw_source.string_at("") {
                let _ = self
                    .inner
                    .orchestrator
                    .record_prompt_suggestion_submission(raw_text)
                    .await;
            }
        }
        let execution_images = claimed.images.clone();
        self.execute_claimed_slash(raw_source, client_turn_id, owner, claimed, execution_images)
            .await
    }

    async fn persist_and_enqueue_mobile_slash(
        &self,
        session_id: &str,
        client_turn_id: Option<u64>,
        raw: Utf16JsonProjection,
        images: Vec<ImageRefDto>,
    ) -> Result<(), ClientError> {
        let checkpoint = match self.durable_turns.admit_queued_slash(
            session_id,
            client_turn_id,
            client_turn_id,
            raw.clone(),
            images.clone(),
        ) {
            Ok(checkpoint) => checkpoint,
            Err(DurableTurnStoreError::AlreadyExists { .. }) => {
                let Some(turn_id) = client_turn_id else {
                    return Err(ClientError::Internal {
                        message: "mobile slash operation id collided without a client turn id"
                            .into(),
                    });
                };
                let existing = self
                    .durable_turns
                    .load(session_id, turn_id)
                    .map_err(Self::map_durable_turn_error)?;
                if existing.queue_operation != QueueOperationKind::Slash
                    || existing.prompt != raw
                    || existing.images != images
                    || existing.original_client_turn_id != client_turn_id
                {
                    return Err(ClientError::Internal {
                        message: format!(
                            "mobile slash turn id {turn_id} was reused for different input"
                        ),
                    });
                }
                existing
            }
            Err(error) => return Err(Self::map_durable_turn_error(error)),
        };
        if checkpoint.queue_delivery == QueueDeliveryState::Pending && checkpoint.state.is_none() {
            enqueue_mobile_slash_dispatch(&self.message_queue, session_id, &checkpoint).await;
        }
        Ok(())
    }

    async fn run_queued_mobile_slash(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(), ClientError> {
        let transition = self.loop_transition.lock().await;
        let checkpoint = match self.durable_turns.load(session_id, turn_id) {
            Ok(checkpoint)
                if checkpoint.queue_operation == QueueOperationKind::Slash
                    && checkpoint.queue_delivery == QueueDeliveryState::Pending
                    && checkpoint.state.is_none() =>
            {
                checkpoint
            }
            Ok(_)
            | Err(DurableTurnStoreError::NotFound { .. })
            | Err(DurableTurnStoreError::Terminal { .. }) => {
                remove_mobile_prompt_dispatches(&self.message_queue, session_id, &[turn_id]).await;
                return Ok(());
            }
            Err(error) => return Err(Self::map_durable_turn_error(error)),
        };
        let owner = match self
            .reserve_turn_for_session(Some(turn_id), Some(session_id))
            .await
        {
            Ok(owner) => owner,
            Err(ClientError::Rejected { .. }) => {
                if self.active_session_id() == session_id {
                    enqueue_mobile_slash_dispatch(&self.message_queue, session_id, &checkpoint)
                        .await;
                } else {
                    remove_mobile_prompt_dispatches(&self.message_queue, session_id, &[turn_id])
                        .await;
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let claimed =
            match self
                .durable_turns
                .with_no_pending_admission(session_id, turn_id, || {
                    self.durable_turns.claim_queued_slash(session_id, turn_id)
                }) {
                Ok(checkpoint) => checkpoint,
                Err(DurableTurnStoreError::PendingAdmission { .. }) => {
                    self.release_unspawned_turn(&owner).await;
                    enqueue_mobile_slash_dispatch(&self.message_queue, session_id, &checkpoint)
                        .await;
                    return Ok(());
                }
                Err(DurableTurnStoreError::Terminal { .. })
                | Err(DurableTurnStoreError::NotFound { .. }) => {
                    self.release_unspawned_turn(&owner).await;
                    return Ok(());
                }
                Err(error) => {
                    self.release_unspawned_turn(&owner).await;
                    return Err(Self::map_durable_turn_error(error));
                }
            };
        drop(transition);
        let execution_images = claimed.images.clone();
        self.execute_claimed_slash(
            checkpoint.prompt,
            checkpoint.original_client_turn_id,
            owner,
            claimed,
            execution_images,
        )
        .await
    }

    async fn run_queued_mobile_bash(
        &self,
        session_id: &str,
        turn_id: u64,
    ) -> Result<(), ClientError> {
        let transition = self.loop_transition.lock().await;
        let checkpoint = match self.durable_turns.load(session_id, turn_id) {
            Ok(checkpoint) if checkpoint.is_pending_mobile_bash() => checkpoint,
            Ok(_)
            | Err(DurableTurnStoreError::NotFound { .. })
            | Err(DurableTurnStoreError::Terminal { .. }) => {
                remove_mobile_prompt_dispatches(&self.message_queue, session_id, &[turn_id]).await;
                return Ok(());
            }
            Err(error) => return Err(Self::map_durable_turn_error(error)),
        };
        let owner = match self
            .reserve_turn_for_session(Some(turn_id), Some(session_id))
            .await
        {
            Ok(owner) => owner,
            Err(ClientError::Rejected { .. }) => {
                if self.active_session_id() == session_id {
                    enqueue_mobile_prompt_dispatch(&self.message_queue, session_id, &checkpoint)
                        .await;
                } else {
                    // The checkpoint belongs to a session that was retargeted
                    // while this marker was in the volatile queue. Keep the
                    // durable pending record for a later resume, but do not
                    // dispatch it under the new session owner.
                    remove_mobile_prompt_dispatches(&self.message_queue, session_id, &[turn_id])
                        .await;
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = self
            .ensure_reserved_session_identity(&owner.session_id)
            .await
        {
            self.release_unspawned_turn(&owner).await;
            enqueue_mobile_prompt_dispatch(&self.message_queue, session_id, &checkpoint).await;
            return Err(error);
        }
        let should_query = self.mobile_bash_respond_to_command();
        let claimed =
            match self
                .durable_turns
                .with_no_pending_admission(session_id, turn_id, || {
                    self.durable_turns
                        .claim_queued_bash(session_id, turn_id, should_query)
                }) {
                Ok(checkpoint) => checkpoint,
                Err(DurableTurnStoreError::PendingAdmission { .. }) => {
                    self.release_unspawned_turn(&owner).await;
                    enqueue_mobile_prompt_dispatch(&self.message_queue, session_id, &checkpoint)
                        .await;
                    return Ok(());
                }
                Err(DurableTurnStoreError::Terminal { .. })
                | Err(DurableTurnStoreError::NotFound { .. }) => {
                    self.release_unspawned_turn(&owner).await;
                    remove_mobile_prompt_dispatches(&self.message_queue, session_id, &[turn_id])
                        .await;
                    return Ok(());
                }
                Err(error) => {
                    self.release_unspawned_turn(&owner).await;
                    return Err(Self::map_durable_turn_error(error));
                }
            };
        self.prepare_mobile_bash_owner(&claimed).await;
        drop(transition);
        self.spawn_mobile_bash_turn(owner, claimed);
        Ok(())
    }

    async fn execute_claimed_slash(
        &self,
        raw: Utf16JsonProjection,
        client_turn_id: Option<u64>,
        owner: Arc<ActiveTurn>,
        checkpoint: crate::mobile::turn_durability::DurableTurnCheckpoint,
        images: Vec<ImageRefDto>,
    ) -> Result<(), ClientError> {
        if owner.cancel.is_cancelled() {
            let _ = self.durable_turns.complete_claimed_slash(
                &owner.session_id,
                checkpoint.turn_id,
                Some("explicit_cancel".into()),
            );
            self.release_unspawned_turn(&owner).await;
            return Ok(());
        }
        let before = self.capture_slash_authority().await;
        let builtin_model =
            command_api::parse_slash_command(&raw).and_then(|parsed| {
                let model_selection = trim_model_source_text(&parsed.raw_args);
                let raw_args = model_selection.string_at("").unwrap_or_default().to_owned();
                (parsed.name.string_at("") == Some("model") && !raw_args.trim().is_empty())
                    .then_some((parsed, raw_args, model_selection))
            });
        if let Some((parsed, raw_model_args, model_selection)) = builtin_model {
            let is_builtin = self
                .inner
                .slash_registry
                .read()
                .await
                .resolve(&parsed.name)
                .is_some_and(|command| {
                    command.source == command_api::model::CommandSource::Builtin
                });
            if is_builtin {
                let result = async {
                    let listings = self.inner.orchestrator.list_model_listings().await;
                    let (source, route_hint) =
                        split_model_selection_source(model_selection.clone(), &listings);
                    let model = source.string_at("").unwrap_or(&raw_model_args).to_owned();
                    let selected = self
                        .switch_model_and_remember(
                            lingxi_core::host::ModelSelectionRequest::Select {
                                source,
                                route_hint,
                                origin: lingxi_core::host::ModelSelectionOrigin::Command,
                                validation:
                                    lingxi_core::host::ModelValidationMode::SlashOuterRaceDetached,
                            },
                        )
                        .await?;
                    Ok::<_, ClientError>((model, selected))
                }
                .await;
                match result {
                    Ok((model, selected)) => {
                        self.restore_preferred_reasoning_selection().await;
                        self.event_sink
                            .emit(ClientEvent::SlashCommandResult {
                                turn_id: client_turn_id,
                                display: format!("Switched to model: {model}").into(),
                                is_error: false,
                            })
                            .await;
                        self.event_sink
                            .emit(ClientEvent::ModelChanged {
                                model: selected,
                                model_source: lower_model_source(
                                    self.inner
                                        .orchestrator
                                        .get_status_snapshot()
                                        .await
                                        .model_source
                                        .as_ref(),
                                ),
                            })
                            .await;
                        self.emit_controls_snapshot().await;
                    }
                    Err(error) => {
                        self.event_sink
                            .emit(ClientEvent::SlashCommandResult {
                                turn_id: client_turn_id,
                                display: format!("Could not switch model: {error}").into(),
                                is_error: true,
                            })
                            .await;
                    }
                }
                self.finish_local_slash(&owner, checkpoint.turn_id, &before)
                    .await?;
                return Ok(());
            }
        }

        let result = self
            .inner
            .dispatcher
            .dispatch(raw.clone(), owner.cancel.clone())
            .await;
        if owner.cancel.is_cancelled() {
            let _ = self.durable_turns.complete_claimed_slash(
                &owner.session_id,
                checkpoint.turn_id,
                Some("explicit_cancel".into()),
            );
            self.release_unspawned_turn(&owner).await;
            return Ok(());
        }
        match result {
            lingxi_core::host::SlashDispatchResult::RunAsTurn { prompt } => {
                let checkpoint = match self.durable_turns.prepare_claimed_slash_turn(
                    &owner.session_id,
                    checkpoint.turn_id,
                    prompt.clone(),
                    None,
                ) {
                    Ok(checkpoint) => checkpoint,
                    Err(error) => {
                        self.release_unspawned_turn(&owner).await;
                        return Err(Self::map_durable_turn_error(error));
                    }
                };
                self.event_sink
                    .emit(ClientEvent::TurnRecoveryState {
                        snapshot: checkpoint.snapshot(),
                    })
                    .await;
                let after = self.capture_slash_authority().await;
                if let Err(error) = self.emit_slash_authority_changes(&before, &after).await {
                    self.release_unspawned_turn(&owner).await;
                    return Err(error);
                }
                let queued_inputs = match checkpoint.initial_prompt_inputs() {
                    Ok(inputs) => inputs,
                    Err(error) => {
                        self.release_unspawned_turn(&owner).await;
                        return Err(Self::map_durable_turn_error(error));
                    }
                };
                self.start_streaming_turn_inner(
                    prompt,
                    None,
                    images,
                    Some(checkpoint.turn_id),
                    checkpoint.original_client_turn_id,
                    false,
                    false,
                    Some(queued_inputs),
                    None,
                    Some(owner),
                    None,
                )
                .await
            }
            lingxi_core::host::SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                let skill_prompt = DurableSkillPrompt::new(command.clone(), body, hook_messages);
                let checkpoint = match self.durable_turns.prepare_claimed_slash_turn(
                    &owner.session_id,
                    checkpoint.turn_id,
                    command.clone(),
                    Some(skill_prompt.clone()),
                ) {
                    Ok(checkpoint) => checkpoint,
                    Err(error) => {
                        self.release_unspawned_turn(&owner).await;
                        return Err(Self::map_durable_turn_error(error));
                    }
                };
                self.event_sink
                    .emit(ClientEvent::TurnRecoveryState {
                        snapshot: checkpoint.snapshot(),
                    })
                    .await;
                let after = self.capture_slash_authority().await;
                if let Err(error) = self.emit_slash_authority_changes(&before, &after).await {
                    self.release_unspawned_turn(&owner).await;
                    return Err(error);
                }
                let queued_inputs = match checkpoint.initial_prompt_inputs() {
                    Ok(inputs) => inputs,
                    Err(error) => {
                        self.release_unspawned_turn(&owner).await;
                        return Err(Self::map_durable_turn_error(error));
                    }
                };
                self.start_streaming_turn_inner(
                    command,
                    None,
                    Vec::new(),
                    Some(checkpoint.turn_id),
                    checkpoint.original_client_turn_id,
                    false,
                    false,
                    Some(queued_inputs),
                    Some(skill_prompt),
                    Some(owner),
                    None,
                )
                .await
            }
            lingxi_core::host::SlashDispatchResult::Handled { display } => {
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display: ClientTextDto::from_projection(&display)
                            .expect("slash command output is a text source"),
                        is_error: false,
                    })
                    .await;
                self.finish_local_slash(&owner, checkpoint.turn_id, &before)
                    .await
            }
            lingxi_core::host::SlashDispatchResult::Unknown { display, .. } => {
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display: ClientTextDto::from_projection(&display)
                            .expect("slash command output is a text source"),
                        is_error: true,
                    })
                    .await;
                self.finish_local_slash(&owner, checkpoint.turn_id, &before)
                    .await
            }
            lingxi_core::host::SlashDispatchResult::Failed { diagnostic } => {
                let display = diagnostic
                    .string_units("/message")
                    .or_else(|| diagnostic.string_units(""))
                    .map(ClientTextDto::from_code_units)
                    .unwrap_or_else(|| "command settings failed".into());
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display,
                        is_error: true,
                    })
                    .await;
                self.finish_local_slash(&owner, checkpoint.turn_id, &before)
                    .await
            }
            lingxi_core::host::SlashDispatchResult::ParseFailure { input: _ } => {
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display: "Commands are in the form `/command [args]`".into(),
                        is_error: false,
                    })
                    .await;
                self.finish_local_slash(&owner, checkpoint.turn_id, &before)
                    .await
            }
            lingxi_core::host::SlashDispatchResult::NotASlashCommand => {
                self.event_sink
                    .emit(ClientEvent::SlashCommandResult {
                        turn_id: client_turn_id,
                        display: ClientTextDto::from_code_units(
                            "not a slash command: "
                                .encode_utf16()
                                .chain(raw.string_units("").expect("slash input is text"))
                                .collect(),
                        ),
                        is_error: true,
                    })
                    .await;
                self.finish_local_slash(&owner, checkpoint.turn_id, &before)
                    .await
            }
        }
    }

    async fn finish_local_slash(
        &self,
        owner: &Arc<ActiveTurn>,
        checkpoint_turn_id: u64,
        before: &SlashAuthoritySnapshot,
    ) -> Result<(), ClientError> {
        let after = self.capture_slash_authority().await;
        let authority_result = self.emit_slash_authority_changes(before, &after).await;
        let reason = owner
            .cancel
            .is_cancelled()
            .then(|| "explicit_cancel".to_string());
        let completion_result = self
            .durable_turns
            .complete_claimed_slash(&owner.session_id, checkpoint_turn_id, reason)
            .map_err(Self::map_durable_turn_error);
        self.release_unspawned_turn(owner).await;
        authority_result?;
        completion_result
    }

    async fn release_unspawned_turn(&self, owner: &Arc<ActiveTurn>) {
        let mut active = self.active_cancel.lock().await;
        release_active_turn(&mut active, &self.message_queue, owner);
        drop(active);
        if let Some(permission_owner_id) = owner.permission_owner_id {
            self.inner
                .permission_gate
                .end_main_turn(permission_owner_id);
        }
        owner.mark_completed();
    }

    /// Start one streamed turn and release its slot on every normal return path
    /// (success, orchestrator error, or cancellation). Pointer ownership keeps a
    /// finishing task from clearing a newer reservation.
    async fn start_streaming_turn(
        &self,
        text: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
        turn_id: Option<u64>,
        queue_if_busy: bool,
        pending: Option<Arc<PendingSubmission>>,
    ) -> Result<(), ClientError> {
        self.start_streaming_turn_inner(
            text,
            prompt_mode,
            images,
            turn_id,
            turn_id,
            true,
            queue_if_busy,
            None,
            None,
            None,
            pending,
        )
        .await
    }

    async fn start_mobile_bash_turn(
        &self,
        command: Utf16JsonProjection,
        images: Vec<ImageRefDto>,
        client_turn_id: Option<u64>,
        transition: tokio::sync::MutexGuard<'_, ()>,
        pending: Option<Arc<PendingSubmission>>,
    ) -> Result<(), ClientError> {
        let active_session = {
            self.active_cancel
                .lock()
                .await
                .as_ref()
                .map(|turn| turn.session_id.clone())
        };
        if let Some(active_session) = active_session {
            self.persist_and_enqueue_mobile_prompt(
                &active_session,
                client_turn_id,
                client_turn_id,
                command,
                Some(PromptModeDto::Bash),
                images,
            )
            .await?;
            drop(transition);
            return Ok(());
        }

        let session_id = self.active_session_id();
        if !self
            .durable_turns
            .scan_pending(&session_id)
            .map_err(Self::map_durable_turn_error)?
            .is_empty()
        {
            self.persist_and_enqueue_mobile_prompt(
                &session_id,
                client_turn_id,
                client_turn_id,
                command,
                Some(PromptModeDto::Bash),
                images,
            )
            .await?;
            drop(transition);
            return Ok(());
        }

        let admitted = self
            .durable_turns
            .admit_queued_prompt(
                &session_id,
                client_turn_id,
                client_turn_id,
                command,
                Some(PromptModeDto::Bash),
                images,
            )
            .map_err(Self::map_durable_turn_error)?;
        let owner = match self
            .reserve_turn_for_session(Some(admitted.turn_id), Some(&session_id))
            .await
        {
            Ok(owner) => owner,
            Err(error) => {
                let active_session = {
                    self.active_cancel
                        .lock()
                        .await
                        .as_ref()
                        .map(|turn| turn.session_id.clone())
                };
                if active_session.as_deref() == Some(session_id.as_str()) {
                    enqueue_mobile_prompt_dispatch(&self.message_queue, &session_id, &admitted)
                        .await;
                    drop(transition);
                    return Ok(());
                }
                let _ = self
                    .durable_turns
                    .cancel_pending(&session_id, admitted.turn_id);
                return Err(error);
            }
        };
        if let Some(pending) = &pending {
            pending.bind_owner(&owner);
        }
        if let Err(error) = self
            .ensure_reserved_session_identity(&owner.session_id)
            .await
        {
            let _ = self
                .durable_turns
                .cancel_pending(&owner.session_id, admitted.turn_id);
            self.release_unspawned_turn(&owner).await;
            return Err(error);
        }
        let should_query = self.mobile_bash_respond_to_command();
        let claimed = match self.durable_turns.claim_queued_bash(
            &owner.session_id,
            admitted.turn_id,
            should_query,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                self.release_unspawned_turn(&owner).await;
                return Err(Self::map_durable_turn_error(error));
            }
        };
        self.prepare_mobile_bash_owner(&claimed).await;
        let admission_guard = if let Some(pending) = pending.as_ref() {
            let mut delivery = self
                .lock_turn_delivery(&owner.session_id, pending.turn_id)
                .await?;
            let active = self.active_cancel.lock().await;
            if !active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &owner))
            {
                delivery.complete();
                return Err(ClientError::Rejected {
                    message: "Bash owner changed before startup".into(),
                });
            }
            if pending.cancel_requested.load(Ordering::Acquire) {
                delivery.arm();
                let result = {
                    let _admission = owner.admission.lock().unwrap_or_else(|e| e.into_inner());
                    owner
                        .explicit_cancel_requested
                        .store(true, Ordering::Release);
                    self.cancel_reason
                        .set(orchestrator::prompt::mid_turn_input::CancelReason::UserInterrupt);
                    owner.cancel.cancel();
                    self.durable_turns
                        .mark_claimed_bash_cancel_pending(&owner.session_id, pending.turn_id)
                };
                match result {
                    Ok(_) | Err(DurableTurnStoreError::Terminal { .. }) => delivery.complete(),
                    Err(error) => {
                        if !matches!(error, DurableTurnStoreError::Storage(_)) {
                            delivery.complete();
                        }
                        return Err(Self::map_durable_turn_error(error));
                    }
                }
            } else {
                delivery.complete();
            }
            Some(active)
        } else {
            None
        };
        drop(transition);
        self.spawn_mobile_bash_turn(owner, claimed);
        drop(admission_guard);
        Ok(())
    }

    fn mobile_bash_respond_to_command(&self) -> bool {
        self.settings
            .as_ref()
            .and_then(|settings| settings.snapshot().ok())
            .and_then(|snapshot| {
                snapshot
                    .effective_source
                    .at_pointer("/respondToBashCommands")
                    .and_then(|value| value.as_bool())
            })
            .unwrap_or(true)
    }

    async fn prepare_mobile_bash_owner(
        &self,
        checkpoint: &crate::mobile::turn_durability::DurableTurnCheckpoint,
    ) {
        if let Some(state) = self
            .inner
            .wakeup_scheduler
            .get()
            .and_then(|scheduler| scheduler.loop_runtime())
        {
            state.take_in_flight_prompt();
            state.invalidate_noop_streak();
        }
        self.inner.orchestrator.turn_span().reset();
        self.inner.message_output.reset_message_buffer().await;
        self.event_sink
            .emit(ClientEvent::TurnRecoveryState {
                snapshot: checkpoint.snapshot(),
            })
            .await;
        TurnEventEmitter::new(self.event_sink.clone())
            .emit_turn_started(Some(checkpoint.turn_id))
            .await;
    }

    fn spawn_mobile_bash_turn(
        &self,
        owner: Arc<ActiveTurn>,
        claimed: crate::mobile::turn_durability::DurableTurnCheckpoint,
    ) {
        let orchestrator = self.inner.orchestrator.clone();
        let durable_turns = self.durable_turns.clone();
        let sink = self.event_sink.clone();
        let active_cancel = self.active_cancel.clone();
        let message_output = self.inner.message_output.clone();
        let permission_gate = self.inner.permission_gate.clone();
        let message_queue = self.message_queue.clone();
        let task_turn = owner.clone();
        let loop_scheduler = self.inner.wakeup_scheduler.get().cloned();
        let cancel_reason = self.cancel_reason.clone();
        let recovery_guard = task_turn.tool_recovery_guard(durable_turns.clone());
        let scope_orch = orchestrator.clone();
        let origin = Arc::downgrade(&task_turn);
        let task = self.runtime().spawn(async move {
            MOBILE_TURN_ORIGIN.scope(origin, async move {
            scope_orch.with_tool_dispatch_recovery_guard(recovery_guard, async move {
            let execution = if task_turn.cancel.is_cancelled() {
                MobileBashExecution {
                    result: DurableBashResult {
                        stdout: String::new(),
                        stderr: "The shell command was interrupted before a result was available.".into(),
                        exit_code: 130,
                        interrupted: false,
                    },
                    interrupted: true,
                }
            } else {
                execute_mobile_bash_tool(&orchestrator, &claimed.prompt, task_turn.cancel.clone())
                    .await
            };
            let paused_with_uncertain_result = task_turn.is_quiescing() && execution.interrupted;
            let mut shell_result = execution.result;
            shell_result.interrupted = execution.interrupted
                && task_turn.explicit_cancel_requested.load(Ordering::Acquire)
                && !task_turn.is_quiescing();
            let still_should_query = claimed.bash_should_query.unwrap_or(true)
                && (!task_turn.cancel.is_cancelled() || task_turn.is_quiescing())
                && !shell_result.interrupted;
            // A lifecycle pause that interrupted Shell (including before the
            // task started) cannot establish a completed result. Keep the
            // unsafe claim so cold recovery asks the user and never reruns a
            // possibly side-effectful command. A known result can be stored
            // for recovery without executing the command a second time.
            let prepared = (!paused_with_uncertain_result).then(|| {
                durable_turns.prepare_claimed_bash_turn(
                    &task_turn.session_id,
                    claimed.turn_id,
                    shell_result,
                    still_should_query,
                )
            });
            match prepared {
                Some(Ok(checkpoint)) if !task_turn.is_quiescing() => {
                    sink.emit(ClientEvent::TurnRecoveryState {
                        snapshot: checkpoint.snapshot(),
                    })
                    .await;
                    let checkpoint = durable_turns
                        .load(&task_turn.session_id, checkpoint.turn_id)
                        .unwrap_or(checkpoint);
                    match checkpoint.queued_prompt_inputs(&durable_turns) {
                        Ok(mut inputs) => {
                            if task_turn.cancel.is_cancelled() {
                                for input in &mut inputs {
                                    if input.prompt_ingress_kind
                                        == Some(orchestrator::PromptIngressKind::DirectBash)
                                    {
                                        input.skip_model_query = true;
                                    }
                                }
                            }
                            let append_only_direct_bash =
                                inputs.iter().any(|input| input.skip_model_query)
                                    && inputs.iter().any(|input| {
                                        input.prompt_ingress_kind
                                            == Some(orchestrator::PromptIngressKind::DirectBash)
                                    })
                                    && inputs.iter().filter(|input| !input.is_meta).all(|input| {
                                        input.skip_model_query
                                            && input.prompt_ingress_kind
                                                == Some(orchestrator::PromptIngressKind::DirectBash)
                                    });
                            let append_cancel = if task_turn.cancel.is_cancelled() {
                                append_only_direct_bash.then(CancellationToken::new)
                            } else {
                                Some(task_turn.cancel.clone())
                            };
                            if let Some(append_cancel) = append_cancel {
                                if let Err(error) = orchestrator
                                    .run_queued_prompt_batch(inputs, append_cancel)
                                    .await
                                {
                                    report_owned_turn_error(&orchestrator, &task_turn, &sink, &message_output, &error).await;
                                }
                            }
                        }
                        Err(error) => {
                            sink.emit(ClientEvent::Error {
                                kind: ErrorKindDto::Internal,
                                message: format!(
                                    "could not restore mobile Bash history rows: {error}"
                                )
                                .into(),
                            })
                            .await;
                        }
                    }
                }
                Some(Ok(_)) | None => {}
                Some(Err(error)) if !task_turn.cancel.is_cancelled() => {
                    sink.emit(ClientEvent::Error {
                        kind: ErrorKindDto::Internal,
                        message: format!("could not persist mobile Bash result: {error}").into(),
                    })
                    .await;
                }
                Some(Err(_)) => {}
            }
            if let Some(scheduler) = &loop_scheduler {
                settle_mobile_loop_turn(
                    &orchestrator,
                    scheduler,
                    &task_turn.cancel,
                    &cancel_reason,
                    true,
                )
                .await;
            }
            let mut active = active_cancel.lock().await;
            release_active_turn(&mut active, &message_queue, &task_turn);
            drop(active);
            if let Some(owner_id) = task_turn.permission_owner_id {
                permission_gate.end_main_turn(owner_id);
            }
            task_turn.mark_completed();
            }).await;
            }).await;
        });
        owner.set_task_handle(task);
    }

    async fn start_streaming_turn_inner(
        &self,
        text: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
        mut turn_id: Option<u64>,
        original_client_turn_id: Option<u64>,
        create_checkpoint: bool,
        queue_if_busy: bool,
        mut queued_inputs: Option<Vec<orchestrator::QueuedPromptInput>>,
        skill_prompt: Option<DurableSkillPrompt>,
        reserved_owner: Option<Arc<ActiveTurn>>,
        pending: Option<Arc<PendingSubmission>>,
    ) -> Result<(), ClientError> {
        if queue_if_busy && queued_inputs.is_none() {
            let session_id = self.active_session_id();
            if !self
                .durable_turns
                .scan_pending(&session_id)
                .map_err(Self::map_durable_turn_error)?
                .is_empty()
            {
                self.persist_and_enqueue_mobile_prompt(
                    &session_id,
                    turn_id,
                    original_client_turn_id,
                    text,
                    prompt_mode,
                    images,
                )
                .await?;
                return Ok(());
            }
        }
        // A direct SendPrompt without a client correlator still needs a stable
        // durable identity. Admit it through the same private checkpoint path
        // as a busy prompt, then reserve and claim it before any Running event.
        let direct_queued_checkpoint =
            if create_checkpoint && queue_if_busy && queued_inputs.is_none() && turn_id.is_none() {
                let session_id = self.active_session_id();
                let checkpoint = self
                    .durable_turns
                    .admit_queued_prompt(
                        &session_id,
                        None,
                        original_client_turn_id,
                        text.clone(),
                        prompt_mode,
                        images.clone(),
                    )
                    .map_err(Self::map_durable_turn_error)?;
                turn_id = Some(checkpoint.turn_id);
                Some(checkpoint)
            } else {
                None
            };
        let turn = if let Some(turn) = reserved_owner {
            let current = self.active_cancel.lock().await.clone();
            let active_session = self.active_session_id();
            if turn.completed.load(Ordering::Acquire)
                || turn.session_id != active_session
                || !current
                    .as_ref()
                    .is_some_and(|owner| Arc::ptr_eq(owner, &turn))
            {
                return Err(ClientError::Rejected {
                    message: "the reserved slash turn is no longer current".into(),
                });
            }
            turn
        } else {
            match self.reserve_turn(turn_id).await {
                Ok(turn) => turn,
                Err(ClientError::Rejected { .. }) if queue_if_busy && queued_inputs.is_none() => {
                    let session_id = self.active_session_id();
                    self.persist_and_enqueue_mobile_prompt(
                        &session_id,
                        turn_id,
                        original_client_turn_id,
                        text,
                        prompt_mode,
                        images,
                    )
                    .await?;
                    return Ok(());
                }
                Err(error) => {
                    if let Some(checkpoint) = direct_queued_checkpoint.as_ref() {
                        let _ = self
                            .durable_turns
                            .cancel_pending(&checkpoint.session_id, checkpoint.turn_id);
                    }
                    return Err(error);
                }
            }
        };
        if let Some(pending) = &pending {
            pending.bind_owner(&turn);
        }
        #[cfg(test)]
        if let Some(pending) = &pending {
            let barrier = pending.owner_barrier.lock().unwrap().clone();
            if let Some((entered, release)) = barrier {
                entered.notify_one();
                release.notified().await;
            }
        }
        // Pending prompt rows append through the same Host-only identity ledger
        // as ordinary durable rows. Import an existing Native transcript before
        // checkpointing or starting the producer; session retargets do this at
        // their lifecycle boundary, while the initially active session may
        // first encounter a nonempty transcript here.
        if let Err(error) = self
            .ensure_reserved_session_identity(&turn.session_id)
            .await
        {
            if let Some(checkpoint) = direct_queued_checkpoint.as_ref() {
                let _ = self
                    .durable_turns
                    .cancel_pending(&checkpoint.session_id, checkpoint.turn_id);
            }
            let mut active = self.active_cancel.lock().await;
            release_active_turn(&mut active, &self.message_queue, &turn);
            drop(active);
            if let Some(owner_id) = turn.permission_owner_id {
                self.inner.permission_gate.end_main_turn(owner_id);
            }
            turn.mark_completed();
            return Err(error);
        }
        if let Some(state) = self
            .inner
            .wakeup_scheduler
            .get()
            .and_then(|scheduler| scheduler.loop_runtime())
        {
            state.take_in_flight_prompt();
            state.invalidate_noop_streak();
        }
        self.inner.orchestrator.turn_span().reset();
        // Resume-only non-checkpointed paths can still carry inline image
        // sources directly. New direct prompts always have a stable identity.
        let direct_image_sources =
            if queued_inputs.is_none() && (!create_checkpoint || turn_id.is_none()) {
                images
                    .iter()
                    .map(|image| lingxi_core::types::ImageSource::Base64 {
                        media_type: image.media_type.clone(),
                        data: image.base64.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            };

        if create_checkpoint {
            let checkpoint_result = if let Some(admitted) = direct_queued_checkpoint.as_ref() {
                Some(
                    self.durable_turns
                        .claim_pending(&turn.session_id, admitted.turn_id)
                        .map_err(Self::map_durable_turn_error),
                )
            } else {
                turn_id.map(|turn_id| {
                    self.durable_turns
                        .begin_direct_with_skill_prompt(
                            &turn.session_id,
                            turn_id,
                            original_client_turn_id,
                            text.clone(),
                            prompt_mode,
                            images,
                            skill_prompt.clone(),
                        )
                        .map_err(Self::map_durable_turn_error)
                })
            };
            if let Some(checkpoint) = checkpoint_result {
                match checkpoint.and_then(|checkpoint| {
                    if queued_inputs.is_none() {
                        queued_inputs = Some(
                            checkpoint
                                .initial_prompt_inputs()
                                .map_err(Self::map_durable_turn_error)?,
                        );
                    }
                    Ok(checkpoint)
                }) {
                    Ok(checkpoint) => {
                        if let Some(inputs) = queued_inputs.as_mut() {
                            checkpoint.correlate_prompt_inputs(inputs);
                        }
                        tracing::debug!(
                            session_id = %checkpoint.session_id,
                            turn_id,
                            checkpoint_revision = checkpoint.revision,
                            "mobile: durable turn checkpoint created"
                        );
                        self.event_sink
                            .emit(ClientEvent::TurnRecoveryState {
                                snapshot: checkpoint.snapshot(),
                            })
                            .await;
                    }
                    Err(error) => {
                        if pending.is_some() {
                            // A failed write may have reached disk. Keep this
                            // exact unspawned owner visible to the outer
                            // admission cleanup so it can conservatively
                            // reconcile the checkpoint before releasing it.
                            return Err(error);
                        }
                        let mut active = self.active_cancel.lock().await;
                        release_active_turn(&mut active, &self.message_queue, &turn);
                        drop(active);
                        if let Some(owner_id) = turn.permission_owner_id {
                            self.inner.permission_gate.end_main_turn(owner_id);
                        }
                        turn.mark_completed();
                        return Err(error);
                    }
                }
            }
        }

        let wrapper = TurnEventEmitter::new(self.event_sink.clone());
        self.inner.message_output.reset_message_buffer().await;
        wrapper.emit_turn_started(turn_id).await;

        // The owner lock linearizes a pending Stop with producer startup.
        // Stop recorded during any earlier await wins before model work starts;
        // after this lock is held, installing the task is synchronous.
        let admission_guard = if let Some(pending) = pending.as_ref() {
            let mut delivery = self
                .lock_turn_delivery(&turn.session_id, pending.turn_id)
                .await?;
            let mut active = self.active_cancel.lock().await;
            if !active
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &turn))
            {
                delivery.complete();
                return Err(ClientError::Rejected {
                    message: "submission owner changed before startup".into(),
                });
            }
            if pending.cancel_requested.load(Ordering::Acquire) {
                delivery.arm();
                let result = {
                    let _admission = turn.admission.lock().unwrap_or_else(|e| e.into_inner());
                    turn.explicit_cancel_requested
                        .store(true, Ordering::Release);
                    self.cancel_reason
                        .set(orchestrator::prompt::mid_turn_input::CancelReason::UserInterrupt);
                    turn.cancel.cancel();
                    self.durable_turns.cancel(&turn.session_id, pending.turn_id)
                };
                release_active_turn(&mut active, &self.message_queue, &turn);
                drop(active);
                if let Some(owner_id) = turn.permission_owner_id {
                    self.inner.permission_gate.end_main_turn(owner_id);
                }
                turn.mark_completed();
                match result {
                    Ok(snapshot) => {
                        pending.suppressed.store(true, Ordering::Release);
                        if let Some(snapshot) = snapshot {
                            drop(self.spawn_recovery_delivery(delivery, snapshot));
                        } else {
                            delivery.complete();
                        }
                        return Ok(());
                    }
                    Err(DurableTurnStoreError::Terminal { .. }) => {
                        pending.suppressed.store(true, Ordering::Release);
                        delivery.complete();
                        return Ok(());
                    }
                    Err(error) => {
                        if !matches!(error, DurableTurnStoreError::Storage(_)) {
                            delivery.complete();
                        }
                        return Err(Self::map_durable_turn_error(error));
                    }
                }
            }
            delivery.complete();
            Some(active)
        } else {
            None
        };

        let orch = self.inner.orchestrator.clone();
        let sink = self.event_sink.clone();
        let active_cancel = self.active_cancel.clone();
        let message_output = self.inner.message_output.clone();
        let permission_gate = self.inner.permission_gate.clone();
        let message_queue = self.message_queue.clone();
        let task_turn = turn.clone();
        let loop_scheduler = self.inner.wakeup_scheduler.get().cloned();
        let cancel_reason = self.cancel_reason.clone();
        let media_agent_handoffs = self.inner.media_agent_handoffs.clone();
        let media_service = self.inner.media_service.clone();
        let media_conversation_attachment = self.inner.media_conversation_attachment.clone();
        let recovery_guard = task_turn.tool_recovery_guard(self.durable_turns.clone());
        let scope_orch = orch.clone();
        let origin = Arc::downgrade(&task_turn);
        let task =
            self.runtime().spawn(async move {
                MOBILE_TURN_ORIGIN
                    .scope(origin, async move {
                        scope_orch
                .with_tool_dispatch_recovery_guard(recovery_guard, async move {
                    let result = match queued_inputs {
                        Some(inputs) => {
                            orch.run_queued_prompt_batch(inputs, task_turn.cancel.clone())
                                .await
                        }
                        None => {
                            orch.run_turn_streaming_with_cancel_image_sources(
                                &text,
                                direct_image_sources,
                                task_turn.cancel.clone(),
                            )
                            .await
                        }
                    };
                    if let Err(err) = &result {
                        report_owned_turn_error(&orch, &task_turn, &sink, &message_output, err)
                            .await;
                    }

                    let (media_handoff_end, media_handoff_error) =
                        match continue_media_agent_handoff(
                            orch.clone(),
                            task_turn.clone(),
                            media_agent_handoffs,
                            media_service,
                            media_conversation_attachment,
                            result.is_ok(),
                        )
                        .await
                        {
                            Ok(end) => (end, None),
                            Err(error) => {
                                report_owned_turn_error(
                                    &orch,
                                    &task_turn,
                                    &sink,
                                    &message_output,
                                    &error,
                                )
                                .await;
                                (None, Some(error))
                            }
                        };
                    let deferred_terminal = task_turn.take_deferred_terminal();

                    if let Some(scheduler) = &loop_scheduler {
                        settle_mobile_loop_turn(
                            &orch,
                            scheduler,
                            &task_turn.cancel,
                            &cancel_reason,
                            true,
                        )
                        .await;
                    }
                    if let Some(end) = media_handoff_end {
                        let (outcome, stop_reason) = match end {
                            orchestrator::RealtimeAgentEnd::Cancelled => (
                                client::protocol::events::TurnOutcomeDto::Cancelled,
                                "cancelled",
                            ),
                            orchestrator::RealtimeAgentEnd::Closed => (
                                client::protocol::events::TurnOutcomeDto::EndTurn,
                                "media_realtime_closed",
                            ),
                            orchestrator::RealtimeAgentEnd::Deadline => (
                                client::protocol::events::TurnOutcomeDto::EndTurn,
                                "media_realtime_deadline",
                            ),
                            orchestrator::RealtimeAgentEnd::Idle => (
                                client::protocol::events::TurnOutcomeDto::EndTurn,
                                "media_realtime_idle",
                            ),
                            orchestrator::RealtimeAgentEnd::ToolStopped => (
                                client::protocol::events::TurnOutcomeDto::EndTurn,
                                "media_realtime_tool_stopped",
                            ),
                        };
                        sink.emit(ClientEvent::TurnEnded {
                            outcome,
                            stop_reason: Some(stop_reason.into()),
                            cost: client::adapter::lowering::lower_cost_snapshot(
                                &orch.snapshot_cost_real().await,
                            ),
                        })
                        .await;
                    } else if !task_turn.terminal_emitted.load(Ordering::Acquire) {
                        // A requested handoff can be cleaned up without entering
                        // realtime (ordinary turn failure/cancellation). In that
                        // case restore the terminal event held by the lifecycle gate
                        // so the client still leaves its running state exactly once.
                        if let Some(mut event @ ClientEvent::TurnEnded { .. }) = deferred_terminal {
                            let terminal_cost = client::adapter::lowering::lower_cost_snapshot(
                                &orch.snapshot_cost_real().await,
                            );
                            if let ClientEvent::TurnEnded {
                                outcome,
                                stop_reason,
                                cost,
                            } = &mut event
                            {
                                if task_turn.cancel.is_cancelled() {
                                    *outcome = client::protocol::events::TurnOutcomeDto::Cancelled;
                                    *stop_reason = Some("cancelled".into());
                                } else if media_handoff_error.is_some() {
                                    *outcome = client::protocol::events::TurnOutcomeDto::EndTurn;
                                    *stop_reason = Some("media_handoff_error".into());
                                } else if result.is_err() {
                                    *outcome = client::protocol::events::TurnOutcomeDto::EndTurn;
                                    *stop_reason = Some("error".into());
                                }
                                *cost = terminal_cost;
                            }
                            sink.emit(event).await;
                        }
                    }
                    let mut active = active_cancel.lock().await;
                    release_active_turn(&mut active, &message_queue, &task_turn);
                    drop(active);
                    if let Some(owner_id) = task_turn.permission_owner_id {
                        permission_gate.end_main_turn(owner_id);
                    }
                    // Notify only after the slot is released: Cancel returning is the
                    // guarantee that New/Resume/Clear can no longer observe this turn.
                    task_turn.mark_completed();
                })
                .await;
                    })
                    .await;
            });
        turn.set_task_handle(task);
        drop(admission_guard);
        Ok(())
    }

    async fn persist_and_enqueue_mobile_prompt(
        &self,
        session_id: &str,
        requested_turn_id: Option<u64>,
        original_client_turn_id: Option<u64>,
        text: Utf16JsonProjection,
        prompt_mode: Option<PromptModeDto>,
        images: Vec<ImageRefDto>,
    ) -> Result<(), ClientError> {
        let checkpoint = self
            .durable_turns
            .admit_queued_prompt(
                session_id,
                requested_turn_id,
                original_client_turn_id,
                text,
                prompt_mode,
                images,
            )
            .map_err(Self::map_durable_turn_error)?;
        enqueue_mobile_prompt_dispatch(&self.message_queue, session_id, &checkpoint).await;
        tracing::debug!(
            %session_id,
            turn_id = checkpoint.turn_id,
            queue_sequence = checkpoint.queue_sequence,
            "mobile: durable queued prompt accepted"
        );
        if !lingxi_core::host::env::background_tasks_disabled() {
            let background = self
                .inner
                .task_registry
                .background_all_tasks_with_outcome(
                    lingxi_core::host::task_registry::TaskBackgroundReason::DeliverMessage,
                )
                .await;
            if background.capture_unknown || background.unknown != 0 {
                return Err(ClientError::Internal {
                    message: if background.capture_unknown {
                        "prompt was queued, but the full set of background tasks could not be determined".into()
                    } else {
                        format!("prompt was queued, but the background transition for {} task(s) could not be confirmed", background.unknown)
                    },
                });
            }
        }
        Ok(())
    }

    fn map_durable_turn_error(error: DurableTurnStoreError) -> ClientError {
        match error {
            DurableTurnStoreError::Storage(_) => ClientError::Internal {
                message: "durable turn storage failed".to_string(),
            },
            other => ClientError::Rejected {
                message: other.to_string(),
            },
        }
    }

    fn active_session_id(&self) -> String {
        self.inner
            .active_session_uuid
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    async fn read_mobile_linux_status(
        runtime: &dyn MobileLinuxRuntime,
    ) -> Result<MobileLinuxStatusDto, MobileEngineError> {
        let capability = runtime.probe_capability().await;
        let status = runtime
            .rootfs_status()
            .await
            .map_err(|e| MobileEngineError::Internal(format!("mobile_linux_status failed: {e}")))?;
        Ok(lower_mobile_linux_status(capability, status))
    }
}

// F3-05: the inbound command path — the async FFI entry point. Under the
// `uniffi` feature this impl block is a `#[uniffi::export(async_runtime =
// "tokio")]` so `submit` is exported as an async foreign method that resolves on
// the handle-owned tokio runtime (the runtime F3-07 registers as the foreign
// executor). Plain (non-FFI) on the host build so `cargo test` exercises the
// SAME method body.
#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileEngineHandle {
    /// Return the most recent MCP OAuth authorization URL. This is a
    /// copyable fallback for mobile hosts when no deep-link opener exists or
    /// the native opener rejects the URL; the slot never contains credentials
    /// or a PKCE verifier.
    #[must_use]
    pub fn mcp_oauth_authorization_url(&self) -> Option<String> {
        self.inner
            .mcp_oauth_authorization_url
            .lock()
            .ok()
            .and_then(|url| url.clone())
    }

    /// Compatibility spelling for mobile clients that prefix host-owned
    /// MCP surfaces with `mobile_`. Both accessors read the same slot.
    #[must_use]
    pub fn mobile_mcp_oauth_authorization_url(&self) -> Option<String> {
        self.mcp_oauth_authorization_url()
    }

    /// Return the built-in provider catalog without credentials or runtime
    /// secrets. The catalog is assembled from the same vendored models.dev
    /// snapshots used to build the live LLM registry.
    #[must_use]
    pub fn builtin_provider_catalog(&self) -> Vec<ProviderCatalogEntryDto> {
        self.inner.provider_catalog.clone()
    }

    /// Start a native OAuth authorization-code flow. PKCE verifier/state stay
    /// in the Rust-owned coordinator; the foreign host receives only the URL.
    pub async fn begin_o_auth(
        &self,
        provider: String,
        redirect_uri: String,
    ) -> Result<MobileOAuthSessionDto, MobileEngineError> {
        if !self.inner.oauth_supported {
            return Err(MobileEngineError::Internal(
                "OAuth requires an encrypted secure credential store".to_string(),
            ));
        }
        self.inner.oauth.begin(provider, redirect_uri).await
    }

    /// Complete a native OAuth callback. The callback URL is validated and the
    /// resulting tokens are persisted inside Rust; no token crosses FFI.
    pub async fn complete_o_auth(
        &self,
        flow_id: String,
        callback_url: String,
    ) -> Result<MobileOAuthStateDto, MobileEngineError> {
        self.inner.oauth.complete(flow_id, callback_url).await
    }

    /// Cancel one pending native OAuth flow.
    pub async fn cancel_o_auth(&self, flow_id: String) {
        self.inner.oauth.cancel(flow_id).await;
    }

    /// Remove persisted OAuth credentials for one provider.
    pub async fn logout_o_auth(&self, provider: String) -> Result<(), MobileEngineError> {
        self.inner.oauth.logout(provider).await
    }

    /// Return non-secret OAuth account state restored from the secure store.
    pub async fn auth_state(
        &self,
        provider: String,
    ) -> Result<MobileOAuthStateDto, MobileEngineError> {
        self.inner.oauth.state(provider).await
    }

    /// Probe provider metadata with an OAuth token, without an inference call.
    pub async fn test_o_auth_connection(
        &self,
        provider: String,
        api_base: String,
        model: String,
    ) -> ProviderConnectionTestDto {
        self.inner.oauth.test(provider, api_base, model).await
    }

    /// Resume a confirmed zero-message mobile session without changing its UUID.
    ///
    /// Android persists `SessionStarted` immediately in its Project index. Older
    /// versions did so before the engine wrote any JSONL file, so this explicit
    /// entrypoint is the migration-safe proof that a missing transcript is an
    /// intended empty session rather than lost conversation data. If a valid
    /// transcript now exists, it is replayed normally instead of being cleared.
    pub async fn resume_empty_session(
        &self,
        session_id: String,
        title: String,
    ) -> Result<(), ClientError> {
        let _transition = self.loop_transition.lock().await;
        if self.active_cancel.lock().await.is_none() {
            if let Some(scheduler) = self.inner.wakeup_scheduler.get() {
                tool_cron::stop_dynamic_loop(Some(scheduler)).await;
            }
        }
        let gate = mobile_cron_session_gate(
            &self.session_cwd(),
            session_id.strip_prefix("sess:").unwrap_or(&session_id),
        );
        let _scheduled_guard = gate.lock().await;
        self.resume_session_impl(session_id, None, Some(title), None)
            .await
    }

    /// Record the iOS risk acknowledgement that permits a subsequent live
    /// `bypassPermissions` mode transition for this session.
    pub async fn confirm_bypass_permissions(&self) -> Result<(), ClientError> {
        self.inner
            .permission_policy_gate
            .confirm_bypass_permissions()
            .map_err(|message| ClientError::Rejected { message })
    }

    /// Submit one [`ClientCommand`] to the engine (plan F3-05).
    ///
    /// This is the mobile analog of the bridge-server's inbound frame dispatch
    /// (`apps/bridge-server/src/server.rs`): it never blocks the foreign UI
    /// thread for a whole turn. `SendPrompt` SPAWNS the streaming turn on the
    /// handle-owned runtime and returns promptly (results stream via the
    /// listener); the other commands resolve their engine entry and return.
    ///
    /// Command routing (decision §0.2 — the same lowering surface both transports
    /// share):
    /// - `SendPrompt` → arm a fresh [`CancellationToken`], synthesize
    ///   `TurnStarted`, SPAWN `run_turn_streaming_with_cancel` on the owned
    ///   runtime, return immediately. The orchestrator's [`AdapterOutputStream`]
    ///   streams `TextDelta` / `ToolUse*` / `CostUpdate` / `TurnEnded` to the
    ///   listener as side effects. Inline `images` are DEFERRED on mobile
    ///   (§0.8 / §5.12) — carried in the DTO, not fed to the engine.
    /// - `Cancel` → fire the in-flight cancellation token.
    /// - `ApprovePermission` / `DenyPermission` → resolve the parked oneshot on
    ///   the connection-scoped [`AdapterPermissionGate`] (F1-14), looking the
    ///   recorded tool name back up for an `AllowAlways` rule append.
    /// - `SetModel` → [`OrchestratorHandle::switch_model`], confirmed by a
    ///   `ModelChanged` event.
    /// - `RunSlashCommand` → the mobile slash dispatcher; local results use
    ///   `SlashCommandResult`, while prompt commands enter the normal turn stream.
    /// - `RefreshListings` / `ListModels` → the `list_*` handle reads, lowered to
    ///   their listing events through the shared `client::adapter::lowering` fns.
    /// - `ForceCompact` / `ClearSession` / `RequestExit` / `Login` / `Logout` →
    ///   their `OrchestratorHandle` / `AuthHandle` entries.
    ///
    /// - `ListSessions` → enumerate the on-disk JSONL catalog via
    ///   `session::jsonl::list_recent_sessions`, lower each row through the shared
    ///   `client::adapter::lower_session_metadata`, reply with `SessionList`.
    /// - `NewSession` → `clear_session` (mints a fresh `SessionId`) + optional
    ///   `switch_model`, confirmed by `SessionStarted` (SESSIONS/HISTORY).
    /// - `ResumeSession` → LIVE hot-restore (SESSIONS/HISTORY): reject mid-turn,
    ///   parse the `session_id` as a `Uuid`, load + validate the on-disk JSONL via
    ///   `orchestrator::replay_session_state`, adopt it into the running
    ///   orchestrator with `OrchestratorHandle::resume_session`, and confirm with a
    ///   `SessionResumed { session_id, messages }` carrying the full restored
    ///   transcript (lowered via `client::adapter::lowering::lower_transcript`).
    ///   An explicitly anchored mobile zero-message session restores with an empty
    ///   transcript and the same UUID. Other missing, corrupt, or malformed
    ///   sessions are honestly `Rejected` — we never emit a false
    ///   `SessionResumed`.
    ///
    /// Remaining host-driven / reserved commands (the task commands — mobile binds
    /// no `TaskRegistry`) are accepted and no-op'd (the `#[non_exhaustive]` enum
    /// also requires a catch-all); lighting them up is additive and does not
    /// change this seam.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when a command's engine entry fails synchronously
    /// (e.g. `SetModel` on an unknown model → [`ClientError::Internal`]). A turn
    /// failure is NOT surfaced here — it streams as a `ClientEvent::Error` to the
    /// listener (the turn is spawned, so `submit` has already returned `Ok`).
    // One match over the full command surface — the one-place-routes-everything
    // map this entry exists to be (same convention as `EngineCommandRouter::route`
    // and `harness_runtime::desktop::build`).
    #[allow(clippy::too_many_lines)]
    /// Poll engine commands on the owned runtime. Foreign executors, including
    /// Swift's cooperative pool, can have much smaller stacks than our workers.
    pub async fn submit(&self, command: ClientCommand) -> Result<(), ClientError> {
        let handle = self
            .self_weak
            .upgrade()
            .ok_or_else(|| ClientError::Internal {
                message: "mobile engine is shutting down".into(),
            })?;
        // Turn-owning commands and lifecycle changes must finish their
        // reservation or cleanup even if the foreign caller drops its future.
        let finish_lifecycle = matches!(
            &command,
            ClientCommand::SendPrompt { .. }
                | ClientCommand::RunSlashCommand { .. }
                | ClientCommand::ResumeTurn { .. }
                | ClientCommand::Cancel { .. }
                | ClientCommand::PauseTurn { .. }
        );
        let pending = match &command {
            ClientCommand::SendPrompt {
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                ..
            }
            | ClientCommand::RunSlashCommand {
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                ..
            } if !session_id.is_empty() => {
                let mut registry = self
                    .pending_submissions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if registry
                    .iter()
                    .any(|item| item.session_id == *session_id && item.turn_id == *turn_id)
                {
                    return Err(ClientError::Rejected {
                        message: "a submission with this turn id is already pending".into(),
                    });
                }
                let pending = self
                    .durable_turns
                    .register_pending_admission(session_id, *turn_id, |lease| {
                        Arc::new(PendingSubmission::new(
                            session_id.clone(),
                            *turn_id,
                            lease,
                            self.self_weak.clone(),
                        ))
                    })
                    .map_err(Self::map_durable_turn_error)?;
                registry.push(pending.clone());
                Some(pending)
            }
            _ => None,
        };
        let registry = self.pending_submissions.clone();
        let task = self.runtime().spawn(async move {
            use futures_util::FutureExt as _;
            let guard = pending.as_ref().map(|pending| PendingSubmissionGuard {
                registry,
                pending: pending.clone(),
            });
            let mut result = std::panic::AssertUnwindSafe(handle.submit_on_runtime(command, pending.clone()))
                .catch_unwind().await.unwrap_or_else(|_| Err(ClientError::Internal { message: "mobile submission panicked during admission".into() }));
            if result.is_ok() {
                if let Some(pending) = &pending {
                    let owner_is_unbound = pending.owner.lock().unwrap_or_else(|e| e.into_inner()).is_none();
                    if !pending.suppressed.load(Ordering::Acquire) && owner_is_unbound {
                        result = handle.finish_received_queue_admission(pending).await;
                    }
                }
            }
            if result.is_err() {
                if let Some(pending) = &pending {
                    if let Err(error) = handle.cleanup_failed_pending_submission(pending).await {
                        tracing::warn!(%error, "mobile: failed to clean up a failed pending submission");
                    }
                }
            }
            if let Some(pending) = &pending {
                pending.complete(result.clone());
            }
            drop(guard);
            result
        });
        // Preserve caller cancellation for other commands.
        struct AbortCommand(tokio::task::AbortHandle);
        impl Drop for AbortCommand {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _abort = (!finish_lifecycle).then(|| AbortCommand(task.abort_handle()));
        task.await.map_err(|error| ClientError::Internal {
            message: format!("mobile command task failed: {error}"),
        })?
    }
}

// These helpers carry internal admission state and must not be part of the
// UniFFI surface. Only the public wrapper above crosses the foreign boundary.
impl MobileEngineHandle {
    async fn resolve_ask_user_question(
        &self,
        request_id: u64,
        answers: Option<client::protocol::ask_user_question::AskUserQuestionAnswersDto>,
    ) -> Result<(), ClientError> {
        let resolved = match answers {
            Some(answers) => {
                self.ask_user_question_broker
                    .resolve(request_id, answers.into_projections())
                    .await
            }
            None => self.ask_user_question_broker.cancel(request_id).await,
        };
        if !resolved {
            tracing::debug!(
                request_id,
                "mobile: resolve for unknown / already-resolved AskUserQuestion id"
            );
        }
        Ok(())
    }

    async fn cancel_received_submission(
        &self,
        command: &ClientCommand,
        pending: &Arc<PendingSubmission>,
    ) -> Result<(), ClientError> {
        let session_id = &pending.session_id;
        let turn_id = pending.turn_id;
        let mut delivery = self.lock_turn_delivery(session_id, turn_id).await?;
        let active = self.active_cancel.lock().await;
        let operation = || -> Result<Option<TurnRecoverySnapshotDto>, DurableTurnStoreError> {
            match command {
                ClientCommand::SendPrompt {
                    text,
                    prompt_mode,
                    images,
                    ..
                } if active.is_none() => {
                    self.durable_turns.begin_direct_with_skill_prompt(
                        session_id,
                        turn_id,
                        Some(turn_id),
                        text.clone().into_projection(),
                        *prompt_mode,
                        images.clone(),
                        None,
                    )?;
                    self.durable_turns.cancel(session_id, turn_id)
                }
                ClientCommand::SendPrompt {
                    text,
                    prompt_mode,
                    images,
                    ..
                } => {
                    self.durable_turns.admit_queued_prompt(
                        session_id,
                        Some(turn_id),
                        Some(turn_id),
                        text.clone().into_projection(),
                        *prompt_mode,
                        images.clone(),
                    )?;
                    self.durable_turns
                        .cancel_pending(session_id, turn_id)?
                        .ok_or_else(|| DurableTurnStoreError::Terminal {
                            session_id: session_id.clone(),
                            turn_id,
                        })?;
                    self.durable_turns
                        .expose_cancelled_pending(session_id, turn_id)
                        .map(Some)
                }
                ClientCommand::RunSlashCommand { raw, images, .. } => {
                    self.durable_turns.admit_queued_slash(
                        session_id,
                        Some(turn_id),
                        Some(turn_id),
                        raw.clone().into_projection(),
                        images.clone(),
                    )?;
                    self.durable_turns
                        .cancel_pending(session_id, turn_id)?
                        .ok_or_else(|| DurableTurnStoreError::Terminal {
                            session_id: session_id.clone(),
                            turn_id,
                        })?;
                    self.durable_turns
                        .expose_cancelled_pending(session_id, turn_id)
                        .map(Some)
                }
                _ => unreachable!("only turn submissions have pending admission"),
            }
        };
        delivery.arm();
        let result = match active.as_ref() {
            Some(owner) if owner.session_id == *session_id => match owner.live_owner.as_ref() {
                Some(lease) => {
                    self.durable_turns
                        .with_live_owner(session_id, lease, Some(turn_id), operation)
                }
                None => Err(DurableTurnStoreError::Storage(
                    "missing active turn lease".into(),
                )),
            },
            Some(_) => {
                delivery.complete();
                return Err(ClientError::Rejected {
                    message: "submission owner changed".into(),
                });
            }
            None => self
                .durable_turns
                .with_no_live_owner(session_id, Some(turn_id), operation),
        };
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if !matches!(error, DurableTurnStoreError::Storage(_)) {
                    delivery.complete();
                }
                return Err(Self::map_durable_turn_error(error));
            }
        };
        pending.suppressed.store(true, Ordering::Release);
        drop(active);
        if let Some(snapshot) = snapshot {
            // A submitted cancellation is durable before this delivery task is
            // scheduled; callback execution never holds the owner lock.
            drop(self.spawn_recovery_delivery(delivery, snapshot));
        } else {
            delivery.complete();
        }
        Ok(())
    }

    async fn cleanup_failed_pending_submission(
        &self,
        pending: &Arc<PendingSubmission>,
    ) -> Result<(), ClientError> {
        let owner = pending
            .owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let Some(owner) = owner else {
            return Ok(());
        };
        // Even an already-poisoned delivery gate must not strand an unspawned
        // owner. Preserve the gate error while still releasing that owner.
        let (mut delivery, delivery_error) = match self
            .lock_turn_delivery(&owner.session_id, pending.turn_id)
            .await
        {
            Ok(guard) => (Some(guard), None),
            Err(error) => (None, Some(error)),
        };
        let mut active = self.active_cancel.lock().await;
        if !active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &owner))
            || owner
                .task
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            if let Some(guard) = delivery {
                guard.complete();
            }
            return Ok(());
        }
        let mut snapshot = None;
        let recovery = if pending.cancel_requested.load(Ordering::Acquire) {
            if let Some(guard) = delivery.as_mut() {
                guard.arm();
            }
            match self
                .durable_turns
                .cancel(&owner.session_id, pending.turn_id)
            {
                Ok(value) => {
                    snapshot = value;
                    pending.suppressed.store(true, Ordering::Release);
                    Ok(())
                }
                Err(DurableTurnStoreError::Terminal { .. }) => Ok(()),
                Err(error) => Err(Self::map_durable_turn_error(error)),
            }
        } else {
            match self.durable_turns.load(&owner.session_id, pending.turn_id) {
                Ok(checkpoint)
                    if checkpoint
                        .state
                        .as_ref()
                        .is_some_and(|state| !state.is_terminal()) =>
                {
                    self.durable_turns
                        .transition(
                            &owner.session_id,
                            pending.turn_id,
                            TurnRecoveryStateDto::WaitingForUser,
                            false,
                            Some("admission_failed".into()),
                        )
                        .map(|_| ())
                        .map_err(Self::map_durable_turn_error)
                }
                Ok(_) | Err(DurableTurnStoreError::NotFound { .. }) => Ok(()),
                Err(error) => Err(Self::map_durable_turn_error(error)),
            }
        };
        owner.request_lifecycle_quiesce(&self.cancel_reason);
        release_active_turn(&mut active, &self.message_queue, &owner);
        drop(active);
        if let Some(owner_id) = owner.permission_owner_id {
            self.inner.permission_gate.end_main_turn(owner_id);
        }
        let delivery_task = match (delivery.take(), snapshot) {
            (Some(guard), Some(snapshot)) => Some(self.spawn_recovery_delivery(guard, snapshot)),
            (Some(guard), None) if recovery.is_ok() => {
                guard.complete();
                None
            }
            (Some(guard), None) => {
                drop(guard);
                None
            }
            (None, _) => None,
        };
        owner.mark_completed();
        if let Some(task) = delivery_task {
            Self::await_recovery_delivery(task).await?;
        }
        recovery?;
        if let Some(error) = delivery_error {
            return Err(error);
        }
        Ok(())
    }

    /// Complete a received queued admission. The exact shared admission lease
    /// fences claim until its cancellation decision is durably committed.
    async fn finish_received_queue_admission(
        &self,
        pending: &Arc<PendingSubmission>,
    ) -> Result<(), ClientError> {
        let mut delivery = self
            .lock_turn_delivery(&pending.session_id, pending.turn_id)
            .await?;
        if pending.complete_if_not_cancelled() {
            delivery.complete();
            return Ok(());
        }
        delivery.arm();
        let result = {
            let lease = pending
                .shared_lease
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match lease.as_ref() {
                Some(lease) => self.durable_turns.cancel_unclaimed_admission(
                    &pending.session_id,
                    pending.turn_id,
                    lease,
                    pending,
                ),
                None => Err(DurableTurnStoreError::PendingAdmission {
                    session_id: pending.session_id.clone(),
                    turn_id: pending.turn_id,
                }),
            }
        };
        let snapshot = match result {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) | Err(DurableTurnStoreError::NotFound { .. }) => {
                pending
                    .cancel_without_checkpoint
                    .store(true, Ordering::Release);
                pending.complete(Ok(()));
                delivery.complete();
                return Ok(());
            }
            Err(error) => {
                if !matches!(error, DurableTurnStoreError::Storage(_)) {
                    delivery.complete();
                }
                return Err(Self::map_durable_turn_error(error));
            }
        };
        pending.suppressed.store(true, Ordering::Release);
        remove_mobile_prompt_dispatches(
            &self.message_queue,
            &pending.session_id,
            &[pending.turn_id],
        )
        .await;
        let task = self.spawn_recovery_delivery(delivery, snapshot);
        pending.complete(Ok(()));
        Self::await_recovery_delivery(task).await
    }

    async fn cancel_received_or_active(
        &self,
        session_id: &str,
        turn_id: Option<u64>,
    ) -> Result<(), ClientError> {
        let Some(turn_id) = turn_id else {
            return self.cancel_active_turn(session_id, None).await;
        };
        if session_id.is_empty() || self.active_session_id() != session_id {
            return Err(ClientError::Rejected {
                message: "cancel request belongs to a different session".into(),
            });
        }

        // Once a turn-specific Stop is received, revoke this store's positive
        // queue-dispatch evidence before any fallible gate or owner lookup.
        self.durable_turns.fence_queued_stop(session_id, turn_id);

        // A store-scoped admission can belong to another handle. Record Stop
        // before waiting for the submitting handle's owner or delivery gate:
        // those can be held up by foreign callbacks. The pending decision
        // itself is a short synchronous boundary shared by all handles.
        let Some(pending) = self.durable_turns.pending_admission(session_id, turn_id) else {
            return self.cancel_active_turn(session_id, Some(turn_id)).await;
        };
        if matches!(pending.request_cancel(), PendingCancelDecision::LocalCommandStarted) {
            return Err(ClientError::Rejected {
                message: "the local command has already started and cannot be cancelled".into(),
            });
        }
        let handle = pending.submitting_handle.upgrade().ok_or_else(|| ClientError::Rejected {
            message: "the submitting engine is no longer available".into(),
        })?;
        handle
            .cancel_registered_submission(session_id, turn_id, pending)
            .await
    }

    async fn cancel_registered_submission(
        &self,
        session_id: &str,
        turn_id: u64,
        pending: Arc<PendingSubmission>,
    ) -> Result<(), ClientError> {
        let bound_owner = pending.bound_owner();
        let live_owner = {
            let active = self.active_cancel.lock().await;
            let current = self
                .inner
                .active_session_uuid
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if session_id.is_empty() || current.as_str() != session_id {
                return Err(ClientError::Rejected {
                    message: "cancel request belongs to a different session".into(),
                });
            }
            active
                .as_ref()
                .filter(|owner| {
                    owner.session_id == session_id
                        && owner.turn_id == Some(turn_id)
                        && bound_owner
                            .as_ref()
                            .is_some_and(|bound| Arc::ptr_eq(owner, bound))
                })
                .cloned()
        };
        if let Some(owner) = live_owner {
            return self
                .cancel_active_turn_for_owner(session_id, Some(turn_id), Some(owner))
                .await;
        }
        if let Err(error) = pending.wait().await {
            if !pending.suppressed.load(Ordering::Acquire) {
                return Err(error);
            }
        }
        if pending.cancel_without_checkpoint.load(Ordering::Acquire) {
            return Err(ClientError::Rejected {
                message: "the local command has already crossed its cancellation boundary".into(),
            });
        }
        if pending.suppressed.load(Ordering::Acquire) {
            // Submission queued its terminal callback without awaiting foreign
            // code; Cancel acknowledges only after that delivery has returned.
            let delivery = self.lock_turn_delivery(session_id, turn_id).await?;
            delivery.complete();
            return Ok(());
        }
        let owner = pending.bound_owner().ok_or_else(|| ClientError::Rejected {
            message: "the original admission finished before cancellation was established; retry".into(),
        })?;
        self.cancel_active_turn_for_owner(session_id, Some(turn_id), Some(owner))
            .await
    }

    async fn submit_impl(
        &self,
        command: ClientCommand,
        pending: Option<Arc<PendingSubmission>>,
    ) -> Result<(), ClientError> {
        // Serialize session mutation and turn reservation with wakeup publication.
        let _transition = self.loop_transition.lock().await;
        if let ClientCommand::SendPrompt { session_id, .. }
        | ClientCommand::RunSlashCommand { session_id, .. } = &command
        {
            if session_id.as_deref().filter(|id| !id.is_empty())
                != Some(self.active_session_id().as_str())
            {
                return Err(ClientError::SubmissionNotAdmitted {
                    message: "prompt request belongs to a different session".into(),
                });
            }
        }
        if let ClientCommand::ResumeTurn { session_id, .. }
        | ClientCommand::PauseTurn { session_id, .. } = &command
        {
            if session_id.is_empty() || session_id != &self.active_session_id() {
                return Err(ClientError::Rejected {
                    message: "turn request belongs to a different session".into(),
                });
            }
        }
        if let ClientCommand::AttachTurn {
            session_id,
            request_id,
            ..
        }
        | ClientCommand::QueryQueuedTurn {
            session_id,
            request_id,
            ..
        } = &command
        {
            if session_id != &self.active_session_id() {
                return Err(ClientError::Rejected {
                    message: "attach request belongs to a different session".into(),
                });
            }
            if request_id.is_empty() || request_id.len() > 128 {
                return Err(ClientError::Rejected {
                    message: "attach request id must contain 1 to 128 UTF-8 bytes".into(),
                });
            }
        }
        if let Some(pending) = pending.as_ref() {
            if pending.cancel_requested.load(Ordering::Acquire) {
                return self.cancel_received_submission(&command, pending).await;
            }
            #[cfg(test)]
            pending.test_stage.store(1, Ordering::Release);
        }
        if (matches!(
            &command,
            ClientCommand::ClearSession
                | ClientCommand::NewSession { .. }
                | ClientCommand::ResumeSession { .. }
                | ClientCommand::ForkSession { .. }
                | ClientCommand::RequestExit
        ) || self
            .scheduled_reload
            .load(std::sync::atomic::Ordering::Acquire))
            && self.active_cancel.lock().await.is_none()
        {
            if let Some(scheduler) = self.inner.wakeup_scheduler.get() {
                tool_cron::stop_dynamic_loop(Some(scheduler)).await;
            }
        }
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        let current_id = handle.current_session_id().await.as_uuid().to_string();
        let gate_id = match &command {
            ClientCommand::ResumeSession { session_id, .. } => session_id
                .strip_prefix("sess:")
                .unwrap_or(session_id)
                .to_string(),
            _ => current_id.clone(),
        };
        let gate = mobile_cron_session_gate(&self.session_cwd(), &gate_id);
        let _scheduled_guard = gate.lock().await;
        // Only reload while idle. `resume_session_impl` hard-rejects with a turn
        // in flight, and this block runs before the command `match` — so failing
        // here would reject EVERY command (`Cancel` included) for the life of
        // that turn, leaving the user unable to stop it. Leave the flag armed
        // and let the next idle `submit` do the reload.
        if self.active_cancel.lock().await.is_none()
            && self
                .scheduled_reload
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            if let Err(error) = self.resume_session_impl(current_id, None, None, None).await {
                self.scheduled_reload
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(error);
            }
        }

        // A scheduled reload can replace session-owned state. Recheck the
        // original command owner before any prompt admission or side effect.
        if let ClientCommand::SendPrompt { session_id, .. }
        | ClientCommand::RunSlashCommand { session_id, .. } = &command
        {
            if session_id.as_deref() != Some(self.active_session_id().as_str()) {
                return Err(ClientError::Rejected {
                    message: "prompt request belongs to a different session".into(),
                });
            }
        }
        if let ClientCommand::QueryQueuedTurn { session_id, .. } = &command {
            if session_id != &self.active_session_id() {
                return Err(ClientError::Rejected {
                    message: "queued turn request belongs to a different session".into(),
                });
            }
        }
        if let Some(pending) = pending.as_ref() {
            if pending.cancel_requested.load(Ordering::Acquire) {
                return self.cancel_received_submission(&command, pending).await;
            }
        }

        // Workspace navigation is an immediate, non-cancellable host action.
        // Arbitrate its start against an already received Stop under the same
        // owner lock; a later Stop gets an explicit rejection instead of a
        // false successful cancellation receipt.
        if let (Some(pending), ClientCommand::RunSlashCommand { raw, .. }) =
            (pending.as_ref(), &command)
        {
            let source = raw.clone().into_projection();
            let is_local = command_api::parse_slash_command(&source)
                .is_some_and(|parsed| matches!(parsed.name.string_at(""), Some("cd" | "add-dir")));
            if is_local {
                if !pending.try_start_local_command() {
                    return self.cancel_received_submission(&command, pending).await;
                }
            }
        }

        if matches!(
            &command,
            ClientCommand::SendPrompt { .. }
                | ClientCommand::RunSlashCommand { .. }
                | ClientCommand::TaskMessage { .. }
        ) {
            let busy = self.active_cancel.lock().await.is_some();
            self.inner
                .task_registry
                .update_shell_session_activity(true, busy, true);
        }
        match command {
            // ── Turn driving (SPAWN + return promptly) ─────────────────────
            ClientCommand::GetAudioSessionContext => {
                match self.inner.orchestrator.current_audio_binding().await {
                    Ok((session_id, profile)) => {
                        self.connection_sink
                            .emit(ClientEvent::AudioSessionContext {
                                session_id,
                                account_scope: format!("profile:{profile}"),
                                profile_id: profile,
                            })
                            .await;
                    }
                    Err(_) => {
                        self.connection_sink
                            .emit(ClientEvent::Error {
                                kind: ErrorKindDto::Rejected,
                                message: "audio requires an exact session provider profile".into(),
                            })
                            .await;
                    }
                }
                Ok(())
            }
            ClientCommand::SendPrompt {
                session_id: _,
                text,
                prompt_mode,
                images,
                turn_id,
                visualization_context,
            } => {
                if let Ok(text) = String::from_utf16(&text.code_units) {
                    let _ = self
                        .inner
                        .orchestrator
                        .record_prompt_suggestion_submission(&text)
                        .await;
                }
                let root_session = self
                    .inner
                    .active_session_uuid
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let source_text = text.into_projection();
                if prompt_mode == Some(PromptModeDto::Bash) {
                    return self
                        .start_mobile_bash_turn(source_text, images, turn_id, _transition, pending)
                        .await;
                }
                let text = crate::inline_visualization::followup_text(
                    self.fs.clone(),
                    &self.lingxi_home,
                    &root_session,
                    visualization_context.as_ref(),
                    source_text,
                )
                .await;
                self.start_streaming_turn(text, prompt_mode, images, turn_id, true, pending)
                    .await
            }

            ClientCommand::Cancel {
                session_id,
                turn_id,
            } => {
                let session_id = session_id.filter(|id| !id.is_empty()).ok_or_else(|| {
                    ClientError::Rejected {
                        message: "mobile cancel requires a session id".into(),
                    }
                })?;
                self.cancel_received_or_active(&session_id, turn_id).await
            }

            ClientCommand::QueryQueuedTurn {
                session_id,
                turn_id,
                request_id,
            } => {
                let status =
                    match self
                        .durable_turns
                        .with_no_pending_admission(&session_id, turn_id, || {
                            self.durable_turns.queued_turn_status(&session_id, turn_id)
                        }) {
                        Ok(status) => status,
                        Err(DurableTurnStoreError::NotFound { .. }) => {
                            return Err(ClientError::NotFound {
                                message: "queued turn checkpoint not found".into(),
                            })
                        }
                        Err(error) => return Err(Self::map_durable_turn_error(error)),
                    };
                let sink = self.connection_sink.clone();
                // Never hold session or store locks while invoking a foreign
                // callback. The request identity lets a later source ignore a
                // response from an abandoned query.
                drop(self.runtime().spawn(async move {
                    sink.emit(ClientEvent::QueuedTurnStatus {
                        session_id,
                        turn_id,
                        request_id,
                        status,
                    })
                    .await;
                }));
                Ok(())
            }

            ClientCommand::AttachTurn {
                session_id,
                turn_id,
                after_sequence,
                request_id,
            } => {
                let mut delivery = self.lock_turn_delivery(&session_id, turn_id).await?;
                // Another handle does not subscribe to this handle's live
                // event stream. Do not return a Running snapshot that would
                // otherwise leave the attaching client waiting indefinitely.
                let active = self.active_cancel.lock().await;
                let lease = active
                    .as_ref()
                    .filter(|turn| turn.session_id == session_id)
                    .and_then(|turn| turn.live_owner.as_ref());
                let mut checkpoint = {
                    // The store is shared across handles. Registration and
                    // this authoritative load use the same short boundary,
                    // so an earlier received submission cannot be reported
                    // absent merely because it belongs to another handle.
                    let load = || {
                        let read = || self.durable_turns.load(&session_id, turn_id);
                        match lease {
                            Some(lease) => self.durable_turns.with_live_owner(
                                &session_id,
                                lease,
                                Some(turn_id),
                                read,
                            ),
                            None => self.durable_turns.with_no_live_owner(
                                &session_id,
                                Some(turn_id),
                                read,
                            ),
                        }
                    };
                    match self
                        .durable_turns
                        .with_no_pending_admission(&session_id, turn_id, load)
                    {
                        Ok(checkpoint) => checkpoint,
                        Err(DurableTurnStoreError::NotFound { .. }) => {
                            return Err(ClientError::NotFound {
                                message: "turn checkpoint not found".into(),
                            })
                        }
                        Err(error) => return Err(Self::map_durable_turn_error(error)),
                    }
                };
                drop(active);
                if checkpoint.state.is_none() {
                    return Err(ClientError::Rejected {
                        message: "queued prompt has not been claimed for execution".into(),
                    });
                }
                let snapshot =
                    if let Some(gap_reason) = checkpoint.replay_gap_reason(after_sequence) {
                        tracing::warn!(
                            %session_id,
                            turn_id,
                            after_sequence,
                            first_sequence = checkpoint.first_sequence,
                            last_sequence = checkpoint.last_sequence,
                            gap_reason,
                            "mobile: attach cursor fell behind retained durable turn history"
                        );
                        if checkpoint
                            .state
                            .as_ref()
                            .is_some_and(TurnRecoveryStateDto::is_terminal)
                        {
                            // A replay gap is not permission to resurrect a
                            // completed/failed/cancelled turn. Preserve the
                            // terminal state and explain why no suffix was sent.
                            checkpoint.snapshot_with_reason(Some(gap_reason.to_string()))
                        } else {
                            let active = self.active_cancel.lock().await;
                            let lease = active
                                .as_ref()
                                .filter(|turn| turn.session_id == session_id)
                                .and_then(|turn| turn.live_owner.as_ref());
                            let update = || {
                                let latest = self.durable_turns.load(&session_id, turn_id)?;
                                let snapshot = if let Some(gap_reason) =
                                    latest.replay_gap_reason(after_sequence)
                                {
                                    if latest
                                        .state
                                        .as_ref()
                                        .is_some_and(TurnRecoveryStateDto::is_terminal)
                                    {
                                        latest.snapshot_with_reason(Some(gap_reason.to_string()))
                                    } else {
                                        self.durable_turns.transition(
                                            &session_id,
                                            turn_id,
                                            TurnRecoveryStateDto::WaitingForUser,
                                            false,
                                            Some(gap_reason.to_string()),
                                        )?
                                    }
                                } else {
                                    latest.snapshot()
                                };
                                Ok((latest, snapshot))
                            };
                            delivery.arm();
                            let update_result = match lease {
                                Some(lease) => self.durable_turns.with_live_owner(
                                    &session_id,
                                    lease,
                                    Some(turn_id),
                                    update,
                                ),
                                None => self.durable_turns.with_no_live_owner(
                                    &session_id,
                                    Some(turn_id),
                                    update,
                                ),
                            };
                            let (latest, snapshot) = match update_result {
                                Ok(result) => result,
                                Err(error) => {
                                    if !matches!(error, DurableTurnStoreError::Storage(_)) {
                                        delivery.complete();
                                    }
                                    return Err(Self::map_durable_turn_error(error));
                                }
                            };
                            checkpoint = latest;
                            snapshot
                        }
                    } else {
                        checkpoint.snapshot()
                    };
                if !checkpoint.has_replay_gap(after_sequence) {
                    // Validate the entire batch before announcing it. A
                    // malformed or unsupported record cannot be skipped while
                    // claiming that the cursor is continuous.
                    for event in checkpoint.replay_after(after_sequence) {
                        client::turn_journal::decode_turn_journal_event(event.event_json.clone())
                            .map_err(|error| ClientError::Internal {
                            message: format!("turn replay contains an invalid event: {error}"),
                        })?;
                    }
                }
                tracing::debug!(
                    %session_id,
                    turn_id,
                    after_sequence,
                    replayed_event_count = if checkpoint.has_replay_gap(after_sequence) {
                        0
                    } else {
                        checkpoint.replay_after(after_sequence).len()
                    },
                    checkpoint_revision = checkpoint.revision,
                    "mobile: attached durable turn"
                );
                delivery.arm();
                let sink = self.connection_sink.clone();
                let task = tokio::spawn(async move {
                    sink.emit(ClientEvent::TurnAttachStarted {
                        request_id: request_id.clone(),
                        snapshot,
                    })
                    .await;
                    if !checkpoint.has_replay_gap(after_sequence) {
                        for retained in checkpoint.replay_after(after_sequence) {
                            sink.emit(ClientEvent::TurnEventReplay {
                                session_id: session_id.clone(),
                                turn_id,
                                sequence: retained.sequence,
                                event_json: retained.event_json.clone(),
                            })
                            .await;
                        }
                    }
                    sink.emit(ClientEvent::TurnAttachFinished {
                        request_id,
                        session_id,
                        turn_id,
                    })
                    .await;
                    delivery.complete();
                });
                Self::await_recovery_delivery(task).await
            }

            ClientCommand::ResumeTurn {
                session_id,
                turn_id,
            } => {
                let mut delivery = self.lock_turn_delivery(&session_id, turn_id).await?;
                // Reserve before changing the checkpoint so another handle or
                // a detached tool cannot start the same session in between.
                let owner = self
                    .reserve_turn_for_session(Some(turn_id), Some(&session_id))
                    .await?;
                delivery.arm();
                let resumed =
                    self.durable_turns
                        .load(&session_id, turn_id)
                        .and_then(|checkpoint| {
                            self.durable_turns
                                .resume(&session_id, turn_id)
                                .map(|result| (checkpoint, result))
                        });
                let (checkpoint, (disposition, snapshot)) = match resumed {
                    Ok(result) => result,
                    Err(error) => {
                        if !matches!(error, DurableTurnStoreError::Storage(_)) {
                            delivery.complete();
                        } else {
                            drop(delivery);
                        }
                        self.release_unspawned_turn(&owner).await;
                        return Err(Self::map_durable_turn_error(error));
                    }
                };
                if let Some(snapshot) = snapshot {
                    let task = self.spawn_recovery_delivery(delivery, snapshot);
                    if let Err(error) = Self::await_recovery_delivery(task).await {
                        self.release_unspawned_turn(&owner).await;
                        return Err(error);
                    }
                } else {
                    delivery.complete();
                }
                match disposition {
                    ResumeDisposition::Queued => {
                        self.release_unspawned_turn(&owner).await;
                        Ok(())
                    }
                    ResumeDisposition::Ready => {
                        tracing::info!(
                            %session_id,
                            turn_id,
                            recovery_source = "client_resume",
                            "mobile: resuming turn from pre-execution checkpoint"
                        );
                        let skill_prompt = checkpoint.skill_prompt.clone();
                        let queued_result = if checkpoint.prompt_mode == Some(PromptModeDto::Bash) {
                            checkpoint.queued_prompt_inputs(&self.durable_turns)
                        } else {
                            checkpoint.initial_prompt_inputs()
                        };
                        let mut queued_inputs = match queued_result {
                            Ok(inputs) => Some(inputs),
                            Err(error) => {
                                self.release_unspawned_turn(&owner).await;
                                return Err(Self::map_durable_turn_error(error));
                            }
                        };
                        if let Some(inputs) = queued_inputs.as_mut() {
                            checkpoint.correlate_prompt_inputs(inputs);
                        }
                        self.start_streaming_turn_inner(
                            checkpoint.prompt,
                            checkpoint.prompt_mode,
                            checkpoint.images,
                            Some(turn_id),
                            checkpoint.original_client_turn_id,
                            false,
                            false,
                            queued_inputs,
                            skill_prompt,
                            Some(owner),
                            None,
                        )
                        .await
                    }
                    ResumeDisposition::WaitingForUser | ResumeDisposition::Terminal => {
                        self.release_unspawned_turn(&owner).await;
                        Ok(())
                    }
                }
            }

            ClientCommand::PauseTurn {
                session_id,
                turn_id,
                reason,
            } => self.pause_active_turn(session_id, turn_id, reason).await,

            // ── Permission resolution (resolve the parked oneshot, F1-14) ───
            ClientCommand::ApprovePermission {
                request_id,
                response,
            } => self.resolve_permission(request_id, response).await,
            ClientCommand::DenyPermission { request_id } => {
                self.resolve_permission(request_id, PermissionResponseDto::Deny)
                    .await
            }
            ClientCommand::AnswerAskUserQuestion {
                request_id,
                answers,
            } => {
                self.resolve_ask_user_question(request_id, Some(answers))
                    .await
            }
            ClientCommand::CancelAskUserQuestion { request_id } => {
                self.resolve_ask_user_question(request_id, None).await
            }

            ClientCommand::SetPermissionMode { mode } => {
                let requested_mode = mode.clone();
                let previous_requested = self
                    .inner
                    .requested_permission_mode
                    .lock()
                    .ok()
                    .map(|value| value.clone());
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                let previous_mode = handle
                    .permission_mode()
                    .await
                    .unwrap_or_else(|| self.inner.session_default_permission_mode.clone());
                handle
                    .set_permission_mode(&mode)
                    .await
                    .map_err(|e| ClientError::Rejected {
                        message: format!("set_permission_mode failed: {e}"),
                    })?;
                let active = handle.permission_mode().await.unwrap_or(mode);
                if let Err(error) = self.persist_session_permission_mode(&active).await {
                    let _ = self.restore_session_permission_mode(&previous_mode).await;
                    if let (Some(previous), Ok(mut requested)) = (
                        previous_requested.as_ref(),
                        self.inner.requested_permission_mode.lock(),
                    ) {
                        *requested = previous.clone();
                    }
                    return Err(error);
                }
                if let Err(error) = if self.inner.interactive_launch {
                    permission_preference::save(&self.lingxi_home, &active)
                } else {
                    Ok(())
                } {
                    let _ = self.restore_session_permission_mode(&previous_mode).await;
                    if let (Some(previous), Ok(mut requested)) = (
                        previous_requested.as_ref(),
                        self.inner.requested_permission_mode.lock(),
                    ) {
                        *requested = previous.clone();
                    }
                    let _ = self.persist_session_permission_mode(&previous_mode).await;
                    return Err(ClientError::Internal {
                        message: format!("save permission preference failed: {error}"),
                    });
                }
                if let Ok(mut requested) = self.inner.requested_permission_mode.lock() {
                    *requested = requested_mode;
                }
                self.event_sink
                    .emit(ClientEvent::PermissionModeChanged { mode: active })
                    .await;
                self.emit_controls_snapshot().await;
                Ok(())
            }

            ClientCommand::SetTypescriptLspMode { mode } => {
                let requested = lsp::LspActivationMode::from_wire(&mode).ok_or_else(|| {
                    ClientError::Rejected {
                        message: "TypeScript LSP mode must be auto, off, or on".to_string(),
                    }
                })?;
                let _settings_guard = self.settings_write_lock.lock().await;
                persist_mobile_typescript_lsp_mode(
                    &self.lingxi_home.join("settings.json"),
                    requested,
                )
                .map_err(|error| ClientError::Rejected {
                    message: format!("persist TypeScript LSP mode failed: {error}"),
                })?;
                let previous = self.inner.lsp_registry.activation_mode();
                self.inner.lsp_registry.set_activation_mode(requested);
                if previous != requested {
                    self.inner.lsp_registry.shutdown_instances().await;
                }
                self.emit_typescript_lsp_mode().await;
                Ok(())
            }

            ClientCommand::GetConversationControls => {
                self.emit_controls_snapshot().await;
                Ok(())
            }

            ClientCommand::InitializeProactivity {
                proactivity,
                reinitialize,
            } => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                handle
                    .apply_native_proactivity_initialize(proactivity, reinitialize.unwrap_or(false))
                    .await
                    .map_err(|error| ClientError::Rejected {
                        message: format!("initialize_proactivity failed: {error}"),
                    })?;
                self.emit_controls_snapshot().await;
                Ok(())
            }

            ClientCommand::SetProactivityLevel {
                level,
                offer_bypass,
            } => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                match handle
                    .set_native_proactivity_level(&level, offer_bypass)
                    .await
                {
                    Ok(response) => {
                        if let Ok(mut requested) = self.inner.requested_permission_mode.lock() {
                            *requested = response.permission_mode.clone();
                        }
                        self.event_sink
                            .emit(ClientEvent::PermissionModeChanged {
                                mode: response.permission_mode,
                            })
                            .await;
                        self.emit_controls_snapshot().await;
                        Ok(())
                    }
                    Err(error) => Err(ClientError::Rejected {
                        message: format!("set_proactivity_level failed: {error}"),
                    }),
                }
            }

            ClientCommand::SetReasoningSelection { selection } => {
                let _settings_guard = self.settings_write_lock.lock().await;
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                let requested = decode_reasoning_selection(selection);
                let previous = handle.conversation_controls().await.map(|controls| {
                    (
                        controls.requested_reasoning_selection,
                        controls.effective_reasoning_selection,
                        controls.reasoning_spec.selections_persistable,
                    )
                });
                let settings_path = self.lingxi_home.join("settings.json");
                if let Err(error) = handle.set_reasoning_selection(requested).await {
                    return Err(ClientError::Rejected {
                        message: format!("set_reasoning_selection failed: {error}"),
                    });
                }

                // The engine is authoritative: an unsupported selection is
                // reset to Auto rather than nearest-mapped. Persist only the
                // validated/effective value so an invalid request cannot
                // poison the next session's default.
                let (effective, persistable) = handle
                    .conversation_controls()
                    .await
                    .map(|controls| {
                        (
                            controls.effective_reasoning_selection,
                            controls.reasoning_spec.selections_persistable,
                        )
                    })
                    .unwrap_or((lingxi_core::host::ReasoningSelection::Automatic, true));
                let persisted_default = if persistable {
                    effective
                } else {
                    lingxi_core::host::ReasoningSelection::Automatic
                };
                if let Err(error) =
                    command_api::builtins::effort::persist_reasoning_default_selection_at(
                        &settings_path,
                        Some(&persisted_default),
                    )
                {
                    let rollback = previous
                        .as_ref()
                        .map(|(requested, _, _)| requested.clone())
                        .unwrap_or(lingxi_core::host::ReasoningSelection::Automatic);
                    let _ = handle.set_reasoning_selection(rollback.clone()).await;
                    let previous_default = previous.as_ref().map_or(
                        lingxi_core::host::ReasoningSelection::Automatic,
                        |(_, effective, persistable)| {
                            persistable
                                .then_some(effective.clone())
                                .unwrap_or(lingxi_core::host::ReasoningSelection::Automatic)
                        },
                    );
                    let _ = command_api::builtins::effort::persist_reasoning_default_selection_at(
                        &settings_path,
                        Some(&previous_default),
                    );
                    return Err(ClientError::Rejected {
                        message: format!("persist reasoning selection failed: {error}"),
                    });
                }
                self.emit_controls_snapshot().await;
                Ok(())
            }

            ClientCommand::SetFastMode { enabled } => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                let previous = handle.fast_mode().await;
                handle
                    .set_fast_mode(enabled)
                    .await
                    .map_err(|e| ClientError::Rejected {
                        message: format!("set_fast_mode failed: {e}"),
                    })?;
                let active = handle.fast_mode().await;
                if self.inner.interactive_launch {
                    if let Err(error) = fast_mode_preference::save(&self.lingxi_home, active) {
                        let _ = handle.set_fast_mode(previous).await;
                        return Err(ClientError::Internal {
                            message: format!("save Fast mode preference failed: {error}"),
                        });
                    }
                }
                self.event_sink
                    .emit(ClientEvent::FastModeChanged { enabled: active })
                    .await;
                Ok(())
            }

            // ── Provider credentials ────────────────────────────────────────
            // Mobile uses the same CredentialManager instance as the live
            // multi-provider client. Settings writes therefore become visible
            // to the next request immediately; secret values are never echoed.
            ClientCommand::ListProviderCredentials {
                operation_id,
                provider_ids,
                preview_provider_ids,
            } => {
                let validation_error = if provider_ids.len() > 32
                    || provider_ids.iter().any(|id| !provider_id_is_valid(id))
                    || preview_provider_ids
                        .iter()
                        .any(|id| !provider_ids.contains(id))
                {
                    Some("invalid provider credential query".to_string())
                } else {
                    None
                };
                self.emit_provider_credential_status(
                    operation_id,
                    &provider_ids,
                    &preview_provider_ids,
                    validation_error,
                )
                .await;
                Ok(())
            }
            ClientCommand::SetProviderCredential {
                operation_id,
                provider_id,
                credential,
            } => {
                let credential_preview =
                    secret::masked_credential_preview(credential.expose_secret());
                let error = if !provider_id_is_valid(&provider_id)
                    || credential.expose_secret().is_empty()
                    || credential.expose_secret().len() > 16_384
                    || credential.expose_secret().contains('\0')
                {
                    Some("invalid provider credential".to_string())
                } else {
                    self.inner
                        .credentials
                        .set_provider_key(&provider_id, credential.expose_secret())
                        .await
                        .err()
                        .map(|failure| format!("failed to store provider credential: {failure}"))
                };
                let applied = error.is_none();
                let credential_previews = if applied {
                    HashMap::from([(provider_id.clone(), credential_preview)])
                } else {
                    HashMap::new()
                };
                self.event_sink
                    .emit(ClientEvent::ProviderCredentialStatus {
                        operation_id,
                        configured_provider_ids: applied
                            .then_some(provider_id.clone())
                            .into_iter()
                            .collect(),
                        unavailable_provider_ids: (!applied)
                            .then_some(provider_id)
                            .into_iter()
                            .collect(),
                        storage_encrypted: self
                            .inner
                            .credentials
                            .provider_key_storage_is_encrypted(),
                        credential_previews,
                        error,
                    })
                    .await;
                Ok(())
            }
            ClientCommand::DeleteProviderCredential {
                operation_id,
                provider_id,
            } => {
                let error = if !provider_id_is_valid(&provider_id) {
                    Some("invalid provider id".to_string())
                } else {
                    self.inner
                        .credentials
                        .delete_provider_key(&provider_id)
                        .await
                        .err()
                        .map(|failure| format!("failed to delete provider credential: {failure}"))
                };
                let applied = error.is_none();
                self.event_sink
                    .emit(ClientEvent::ProviderCredentialStatus {
                        operation_id,
                        configured_provider_ids: Vec::new(),
                        unavailable_provider_ids: (!applied)
                            .then_some(provider_id)
                            .into_iter()
                            .collect(),
                        storage_encrypted: self
                            .inner
                            .credentials
                            .provider_key_storage_is_encrypted(),
                        credential_previews: HashMap::new(),
                        error,
                    })
                    .await;
                Ok(())
            }

            // ── Model ──────────────────────────────────────────────────────
            ClientCommand::SetModel { model } => {
                let model_selection = model.into_projection();
                let listings = self.inner.orchestrator.list_model_listings().await;
                let (source, route_hint) = split_model_selection_source(model_selection, &listings);
                let selected = self
                    .switch_model_and_remember(lingxi_core::host::ModelSelectionRequest::Select {
                        source,
                        route_hint,
                        origin: lingxi_core::host::ModelSelectionOrigin::Sdk,
                        validation:
                            lingxi_core::host::ModelValidationMode::GzrAbortAfterFiveSeconds,
                    })
                    .await?;
                self.restore_preferred_reasoning_selection().await;
                self.event_sink
                    .emit(ClientEvent::ModelChanged {
                        model: selected,
                        model_source: lower_model_source(
                            self.inner
                                .orchestrator
                                .get_status_snapshot()
                                .await
                                .model_source
                                .as_ref(),
                        ),
                    })
                    .await;
                self.emit_controls_snapshot().await;
                Ok(())
            }
            ClientCommand::ListModels => {
                self.emit_listing(ProtocolListingKind::Models).await;
                Ok(())
            }

            // ── Slash commands ──────────────────────────────────────────────
            // Display-only (`type: "local"`) commands surface as a TextDelta; a
            // `type: "prompt"` command (`/loop`, Markdown/Plugin) runs its
            // expanded prompt AS a turn through the SAME streaming path as
            // `SendPrompt` (claude-code injects the expanded prompt as the user
            // message), so a typed `/loop` actually schedules + executes.
            ClientCommand::RunSlashCommand {
                session_id: _,
                raw,
                images,
                turn_id,
            } => {
                self.submit_slash_command(raw, images, turn_id, _transition, pending)
                    .await
            }

            ClientCommand::UpdateSettings {
                destination,
                patch_json,
            } => {
                self.apply_settings_patch(destination, &patch_json, self.connection_sink.as_ref())
                    .await;
                Ok(())
            }
            ClientCommand::UpdatePermissionRules {
                destination,
                behavior,
                add,
                remove,
            } => {
                self.apply_permission_rule_update(
                    destination,
                    behavior,
                    add,
                    remove,
                    self.connection_sink.as_ref(),
                )
                .await;
                Ok(())
            }
            ClientCommand::SetDefaultPermissionMode { destination, mode } => {
                self.apply_default_permission_mode(
                    destination,
                    mode,
                    self.connection_sink.as_ref(),
                )
                .await;
                Ok(())
            }
            ClientCommand::UpdateWorkspaceDirectories {
                destination,
                add,
                remove,
            } => {
                self.apply_workspace_directories_update(
                    destination,
                    add,
                    remove,
                    self.connection_sink.as_ref(),
                )
                .await;
                Ok(())
            }
            ClientCommand::UpsertMcpServer {
                scope,
                name,
                config_json,
            } => {
                self.apply_mobile_mcp_write(scope, &name, Some(&config_json))
                    .await;
                Ok(())
            }
            ClientCommand::RemoveMcpServer { scope, name } => {
                self.apply_mobile_mcp_write(scope, &name, None).await;
                Ok(())
            }
            ClientCommand::SkillAdmin { command } => {
                self.apply_skill_admin(command, self.connection_sink.as_ref())
                    .await;
                Ok(())
            }
            ClientCommand::McpAdmin { command } => {
                self.apply_mcp_admin(command, self.connection_sink.as_ref())
                    .await;
                Ok(())
            }
            ClientCommand::PluginAdmin { command } => {
                self.apply_plugin_admin(command, self.connection_sink.as_ref())
                    .await;
                Ok(())
            }
            ClientCommand::HookAdmin { command } => {
                self.apply_hook_admin(command, self.connection_sink.as_ref())
                    .await;
                Ok(())
            }

            // ── Listings ────────────────────────────────────────────────────
            ClientCommand::RefreshListings { which } => {
                for kind in which {
                    self.emit_listing(kind).await;
                }
                Ok(())
            }
            ClientCommand::ListSessionAgents => {
                self.emit_session_agent_list().await;
                Ok(())
            }
            ClientCommand::LoadSessionAgentTranscript { agent_id } => {
                self.emit_session_agent_transcript(agent_id).await
            }

            // ── Auth ─────────────────────────────────────────────────────────
            ClientCommand::Login => {
                // Audit (secure-storage): on a build with no persisting secure
                // store (mobile currently wires the PlainTextSecureStorage stub),
                // an OAuth exchange would authenticate but fail to persist its
                // tokens with a cryptic `BackendUnavailable`. Short-circuit with a
                // clear, actionable message instead. API-key auth needs no /login.
                // Lifts automatically once a native Keychain/Keystore store is
                // injected (then `oauth_supported` is true). §11 / Plan-17 follow-up.
                if !self.inner.oauth_supported {
                    self.event_sink
                        .emit(ClientEvent::Error {
                            kind: client::protocol::events::ErrorKindDto::Internal,
                            message: "OAuth login is not yet supported on this platform \
                                      (no secure credential store); configure an API key instead."
                                .into(),
                        })
                        .await;
                    self.event_sink
                        .emit(ClientEvent::AuthState {
                            state: lower_auth_state(self.inner.auth.current_user().await),
                        })
                        .await;
                    return Ok(());
                }
                let state = match self.inner.auth.login().await {
                    Ok(li) => lower_auth_state(Some(li)),
                    Err(e) => {
                        self.event_sink
                            .emit(ClientEvent::Error {
                                kind: client::protocol::events::ErrorKindDto::Internal,
                                message: format!("login failed: {e}").into(),
                            })
                            .await;
                        lower_auth_state(self.inner.auth.current_user().await)
                    }
                };
                self.event_sink.emit(ClientEvent::AuthState { state }).await;
                Ok(())
            }
            ClientCommand::Logout => {
                if let Err(e) = self.inner.auth.logout().await {
                    self.event_sink
                        .emit(ClientEvent::Error {
                            kind: client::protocol::events::ErrorKindDto::Internal,
                            message: format!("logout failed: {e}").into(),
                        })
                        .await;
                }
                self.event_sink
                    .emit(ClientEvent::AuthState {
                        state: lower_auth_state(self.inner.auth.current_user().await),
                    })
                    .await;
                Ok(())
            }

            // ── Compaction ─────────────────────────────────────────────────
            ClientCommand::ForceCompact => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                match handle.force_compact().await {
                    Ok(summary) => {
                        // Same gate as the task-message reply: `/compact` is
                        // issued BETWEEN turns by construction, and
                        // `CompactionCompleted` is live-turn payload, so the
                        // turn-scoped sink drops the confirmation in exactly the
                        // case the user asked for it.
                        self.connection_sink
                            .emit(ClientEvent::CompactionCompleted {
                                messages_before: summary.messages_before,
                                messages_after: summary.messages_after,
                                bytes_saved: summary.bytes_saved,
                                summary: client::protocol::text::ClientTextDto::from_projection(
                                    &summary.summary,
                                )
                                .expect("compaction summary is text"),
                            })
                            .await;
                        Ok(())
                    }
                    Err(e) => Err(ClientError::Internal {
                        message: format!("force_compact failed: {e}"),
                    }),
                }
            }

            // ── Session control ──────────────────────────────────────────────
            ClientCommand::ClearSession => {
                // Mid-turn semantics (plan §2): reject while a turn is in flight.
                let mid_turn = self.active_cancel.lock().await.is_some();
                if mid_turn {
                    return Err(ClientError::Rejected {
                        message: "cannot clear the session while a turn is in flight".into(),
                    });
                }
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                handle
                    .clear_session()
                    .await
                    .map_err(|e| ClientError::Internal {
                        message: format!("clear_session failed: {e}"),
                    })?;
                self.retarget_session_writer(
                    handle.current_session_id().await,
                    &self.session_cwd(),
                )
                .await?;
                self.event_sink.emit(ClientEvent::SessionEnded).await;
                let _ = self
                    .session_lifecycle_tx
                    .send(handle.current_session_id().await.as_uuid().to_string());
                Ok(())
            }
            ClientCommand::RequestExit => {
                if let Some(scheduler) = self.inner.wakeup_scheduler.get() {
                    tool_cron::stop_dynamic_loop(Some(scheduler)).await;
                }
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                handle.request_exit().await;
                Ok(())
            }

            // ── Sessions / history (SESSIONS/HISTORY) ────────────────────────
            //
            // `ListSessions` enumerates the on-disk JSONL catalog
            // (`<lingxi_home>/projects/<sanitized cwd>/*.jsonl`) via the shared
            // `session::jsonl::list_recent_sessions`, lowers each row through the
            // shared `client::adapter::lower_session_metadata`, and replies with a
            // `SessionList` event — the same listing surface the bridge-server
            // router uses (decision §0.2). An empty / missing catalog replies with
            // an empty list (the loader's `EmptyDirectory` is not an error here —
            // it is "no resumable sessions yet").
            ClientCommand::ListSessions { limit } => {
                let limit = limit.map_or(DEFAULT_SESSION_LIST_LIMIT, |l| l as usize);
                self.emit_session_list(limit).await;
                Ok(())
            }

            ClientCommand::ForkSession {
                session_id,
                target_mode,
            } => {
                if self.active_cancel.lock().await.is_some() {
                    return Err(ClientError::Rejected {
                        message: "cannot fork a session while a turn is in flight".into(),
                    });
                }
                let canonical = session_id.strip_prefix("sess:").unwrap_or(&session_id);
                let source_uuid =
                    uuid::Uuid::parse_str(canonical).map_err(|error| ClientError::Rejected {
                        message: format!("fork: malformed session id {session_id:?}: {error}"),
                    })?;
                let result = session::branch::create_branch(
                    &self.lingxi_home,
                    &self.session_cwd(),
                    source_uuid,
                    None,
                    self.fs.clone(),
                )
                .await
                .map_err(|error| ClientError::Rejected {
                    message: format!("fork: could not duplicate session {session_id}: {error}"),
                })?;
                let fork_path = session::jsonl::session_path(
                    &self.lingxi_home,
                    &self.session_cwd(),
                    &result.new_session_id.to_string(),
                );
                let writer = session::jsonl::JsonlWriter::new(fork_path, self.fs.clone());
                let target_mode = match target_mode {
                    SessionModeDto::Chat => session::jsonl::SessionMode::Chat,
                    SessionModeDto::Code => session::jsonl::SessionMode::Code,
                };
                writer
                    .append_session_mode(target_mode.as_str())
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: format!("fork: persist session mode failed: {error}"),
                    })?;
                self.event_sink
                    .emit(ClientEvent::SessionForked {
                        source_session_id: source_uuid.to_string(),
                        session_id: result.new_session_id.to_string(),
                        mode: lower_session_mode(target_mode),
                    })
                    .await;
                Ok(())
            }

            // `NewSession` swaps the connection's orchestrator to a fresh session
            // (decision §0.5 — `session_id` is a connection attribute). The
            // orchestrator handle's `clear_session` mints a brand-new `SessionId`
            // and resets the in-memory history + JSONL parent chain; we then read
            // the new id back and confirm with `SessionStarted`. An optional
            // `model` override is applied via `switch_model` (the only New-session
            // knob is validated before mutation). A cwd override moves only
            // the new draft through the shared owned directory transition.
            ClientCommand::NewSession { cwd, model } => {
                let requested_workspace = cwd
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .map(|path| self.workspace_target(path))
                    .transpose()?;
                // Reject mid-turn (same contract as `ClearSession`): a new session
                // must not race an in-flight turn.
                let mid_turn = self.active_cancel.lock().await.is_some();
                if mid_turn {
                    return Err(ClientError::Rejected {
                        message: "cannot start a new session while a turn is in flight".into(),
                    });
                }
                // Parse the model-source carrier before session mutation. The
                // orchestrator performs source-aware route and Native validation
                // when the fresh session applies the selection.
                let requested_model = match model {
                    Some(model) => {
                        let model_selection = model.into_projection();
                        let listings = self.inner.orchestrator.list_model_listings().await;
                        let (source, route_hint) =
                            split_model_selection_source(model_selection, &listings);
                        Some(lingxi_core::host::ModelSelectionRequest::Select {
                            source,
                            route_hint,
                            origin: lingxi_core::host::ModelSelectionOrigin::Sdk,
                            validation:
                                lingxi_core::host::ModelValidationMode::GzrAbortAfterFiveSeconds,
                        })
                    }
                    None => None,
                };
                if let Some(request) = requested_model.as_ref() {
                    self.inner
                        .orchestrator
                        .validate_model_selection(request)
                        .await
                        .map_err(|error| ClientError::Internal {
                            message: format!("new session model validation failed: {error}"),
                        })?;
                }
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                let previous_permission_mode = handle
                    .permission_mode()
                    .await
                    .unwrap_or_else(|| self.inner.session_default_permission_mode.clone());
                let new_session_permission_mode = self
                    .restore_preferred_permission_mode(&self.inner.session_default_permission_mode)
                    .await?;
                if let Err(error) = handle.clear_session().await {
                    let _ = self
                        .restore_session_permission_mode(&previous_permission_mode)
                        .await;
                    return Err(ClientError::Internal {
                        message: format!("new session (clear_session) failed: {error}"),
                    });
                }
                handle
                    .set_plan_mode(new_session_permission_mode == "plan")
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: error.to_string(),
                    })?;
                if let Some((logical, _)) = requested_workspace {
                    self.change_workspace_inner(&logical.to_string_lossy())
                        .await?;
                }
                self.inner
                    .session_cwd
                    .restore_project_root(self.inner.session_cwd.cwd());
                let new_session_id = handle.current_session_id().await;
                self.retarget_session_writer(new_session_id, &self.session_cwd())
                    .await?;
                self.inner
                    .session_writer
                    .append_mobile_empty_session(
                        &new_session_id.as_uuid().to_string(),
                        "新对话",
                        &self.session_cwd(),
                    )
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: format!("new session anchor failed: {error}"),
                    })?;
                self.inner
                    .session_writer
                    .append_session_mode(self.inner.session_mode.as_str())
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: format!("new session mode persist failed: {error}"),
                    })?;
                self.persist_session_permission_mode(&new_session_permission_mode)
                    .await?;
                if let Some(request) = requested_model {
                    self.switch_model_and_remember(request).await?;
                }
                self.restore_preferred_reasoning_selection().await;
                self.restore_preferred_fast_mode().await;
                // Mobile clients persist this value as the resumable catalog key.
                // `SessionId::Display` is presentation-oriented (`sess:<uuid>`),
                // while the JSONL filename and ResumeSession contract use the
                // bare UUID. Never leak the display prefix into persisted state.
                let session_id = new_session_id.as_uuid().to_string();
                self.event_sink
                    .emit(ClientEvent::SessionStarted {
                        session_id: session_id.clone(),
                        mode: lower_session_mode(self.inner.session_mode),
                    })
                    .await;
                self.emit_controls_snapshot().await;
                let _ = self.session_lifecycle_tx.send(session_id);
                Ok(())
            }

            // `ResumeSession` names a prior session to hot-restore onto the live
            // orchestrator (SESSIONS/HISTORY). The orchestrator now exposes a real
            // rehydrate seam (`OrchestratorHandle::resume_session`, the symmetric
            // twin of `clear_session`): we load + validate the on-disk JSONL, adopt
            // it into the RUNNING orchestrator IN PLACE (named id + replayed
            // history + JSONL parent-uuid chain pointer), and emit a
            // `SessionResumed` carrying the full restored transcript so the client
            // renders the rehydrated conversation atomically. An explicitly
            // anchored mobile zero-message session restores with an empty
            // transcript and the same UUID. Other missing, corrupt, or malformed
            // sessions are honestly `Rejected`, so we never emit a FALSE
            // `SessionResumed`.
            ClientCommand::ResumeSession { session_id, cwd } => {
                self.resume_session_impl(session_id, cwd, None, None).await
            }
            // ── Background tasks (v3 Phase 1: workflow-on-mobile) ───────────
            //
            // The Task command family routes to the real mobile `TaskRegistry`
            // now (it was a documented no-op while `task_registry: None`).
            // Replies ride the pre-existing (previously emitter-less) DTOs:
            // one `TaskRow` per record, one `TaskOutputChunk`, one
            // `TaskStatusChanged` after a stop.
            ClientCommand::TaskList { status_filter, .. } => {
                let filter = lingxi_core::host::task_registry::TaskListFilter {
                    status: status_filter.map(|s| {
                        match s {
                            client::protocol::listings::TaskStatusDto::Pending => "pending",
                            client::protocol::listings::TaskStatusDto::Running => "running",
                            client::protocol::listings::TaskStatusDto::Paused => "paused",
                            client::protocol::listings::TaskStatusDto::Completed => "completed",
                            client::protocol::listings::TaskStatusDto::Failed => "failed",
                            client::protocol::listings::TaskStatusDto::ExitedUnknown => {
                                "exited_unknown"
                            }
                            // The DTO's user-stop variant maps back to the
                            // engine's terminal "killed" wire status (the same
                            // reconciliation as `lower_task_status`).
                            _ => "killed",
                        }
                        .to_string()
                    }),
                };
                let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle =
                    &*self.inner.task_registry;
                let records = registry
                    .list(filter)
                    .await
                    .map_err(|e| ClientError::Internal {
                        message: format!("task list failed: {e}"),
                    })?;
                for record in &records {
                    let task = client::adapter::lowering::lower_task_record(record);
                    self.event_sink.emit(ClientEvent::TaskRow { task }).await;
                }
                Ok(())
            }
            ClientCommand::TaskOutput { task_id, offset } => {
                let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle =
                    &*self.inner.task_registry;
                let chunk = registry.output(&task_id, Some(offset)).await.map_err(|e| {
                    ClientError::Internal {
                        message: format!("task output failed: {e}"),
                    }
                })?;
                let (task_id, content, total_lines, truncated) =
                    client::adapter::lowering::lower_task_output_chunk(&chunk);
                self.event_sink
                    .emit(ClientEvent::TaskOutputChunk {
                        task_id,
                        content,
                        total_lines,
                        truncated,
                    })
                    .await;
                Ok(())
            }
            ClientCommand::TaskMessage { task_id, message } => {
                if !self.inner.orchestrator.workspace_trusted().await {
                    return Err(ClientError::Rejected {
                        message: "Trust this workspace before messaging a task".into(),
                    });
                }
                let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle =
                    &*self.inner.task_registry;
                let message_source = Utf16JsonProjection::from(message);
                registry
                    .send_human_task_message(&task_id, &message_source)
                    .await
                    .map_err(|error| ClientError::Rejected {
                        message: format!("task message failed: {error}"),
                    })?;
                // Connection-scoped: the user sent this outside any turn, so the
                // turn gate would drop it (see `connection_sink`).
                self.connection_sink
                    .emit(ClientEvent::SystemNotice {
                        message: format!("Message accepted for task {task_id}").into(),
                        is_error: false,
                    })
                    .await;
                Ok(())
            }
            ClientCommand::TaskStop { task_id } => {
                let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle =
                    &*self.inner.task_registry;
                let record = registry
                    .kill(&task_id)
                    .await
                    .map_err(|e| ClientError::Internal {
                        message: format!("task stop failed: {e}"),
                    })?;
                if record.task_type != "local_workflow" {
                    self.event_sink
                        .emit(ClientEvent::TaskStatusChanged {
                            task_id: record.task_id.clone(),
                            status: client::adapter::lowering::lower_task_status(&record.status),
                            origin_session_id: None,
                            // A user stop is `killed`, never `failed`.
                            error: None,
                        })
                        .await;
                }
                Ok(())
            }
            ClientCommand::ResumeWorkflow { task_id } => {
                let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle =
                    &*self.inner.task_registry;
                let resume_session = self
                    .inner
                    .active_session_uuid
                    .lock()
                    .ok()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                let mut workflow = registry
                    .list_workflows()
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: format!("workflow list failed: {error}"),
                    })?
                    .into_iter()
                    .find(|workflow| workflow.task_id == task_id);
                if workflow.is_none() && self.active_session_id() == resume_session {
                    // Terminal task GC can remove the live row while that
                    // workflow's validated recovery checkpoint remains durable.
                    self.inner
                        .workflow_checkpoints
                        .adopt_task(&resume_session, &task_id, self.inner.task_registry.as_ref())
                        .await;
                    workflow = registry
                        .list_workflows()
                        .await
                        .map_err(|error| ClientError::Internal {
                            message: format!("workflow list failed: {error}"),
                        })?
                        .into_iter()
                        .find(|workflow| workflow.task_id == task_id);
                }
                let workflow = workflow.ok_or_else(|| ClientError::NotFound {
                    message: format!("workflow task {task_id}"),
                })?;
                let still_active = self
                    .inner
                    .active_session_uuid
                    .lock()
                    .ok()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                if still_active != resume_session {
                    return Err(ClientError::Rejected {
                        message: "cannot resume workflow while the active session is changing"
                            .to_string(),
                    });
                }
                if workflow.status != "paused" {
                    return Err(ClientError::Rejected {
                        message: format!("workflow task {task_id} is not paused"),
                    });
                }
                let run_id = workflow
                    .run_id
                    .clone()
                    .ok_or_else(|| ClientError::Rejected {
                        message: format!("workflow task {task_id} has no resumable run id"),
                    })?;
                let script_path =
                    workflow
                        .script_path
                        .clone()
                        .ok_or_else(|| ClientError::Rejected {
                            message: format!("workflow task {task_id} has no persisted script"),
                        })?;
                let args = workflow
                    .args
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|error| ClientError::Rejected {
                        message: format!("workflow task {task_id} has invalid args: {error}"),
                    })?;
                let launch_spec = tool_workflow::WorkflowLaunchSpec {
                    script_path: Some(script_path),
                    args,
                    resume_from_run_id: Some(run_id.clone()),
                    session_uuid: Some(resume_session.clone()),
                    session_origin: self
                        .inner
                        .orchestrator
                        .capture_session_origin()
                        .await
                        .map_err(|error| ClientError::Rejected {
                            message: error.to_string(),
                        })?,
                    ..Default::default()
                };
                let launched = self
                    .inner
                    .workflow_launcher
                    .launch(launch_spec)
                    .await
                    .map_err(|error| ClientError::Rejected {
                        message: error.to_string(),
                    })?;
                let new_record = registry
                    .get(&launched.task_id)
                    .await
                    .map_err(|error| ClientError::Internal {
                        message: format!("resumed workflow lookup failed: {error}"),
                    })?
                    .ok_or_else(|| ClientError::Internal {
                        message: format!("resumed workflow task {} disappeared", launched.task_id),
                    })?;
                self.event_sink
                    .emit(ClientEvent::WorkflowResumed {
                        previous_task_id: task_id,
                        task: client::adapter::lowering::lower_task_record(&new_record),
                        run_id,
                        origin_session_id: Some(resume_session),
                    })
                    .await;
                Ok(())
            }

            // ── Host-driven / reserved in the foundation ────────────────────
            //
            // The `#[non_exhaustive]` enum requires a catch-all for commands
            // this host does not route.
            other => {
                tracing::debug!(
                    ?other,
                    "engine-mobile: command not routed by submit in the foundation"
                );
                Ok(())
            }
        }
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileEngineHandle {
    /// Test a provider endpoint without exposing a stored credential to the
    /// foreign host or mutating the live engine configuration.
    ///
    /// The request uses each provider's model-list endpoint because it verifies
    /// DNS/TLS, authentication, and the selected model without consuming
    /// inference tokens. An optional draft credential takes precedence over the
    /// secure-store value and is never persisted.
    pub async fn test_provider_connection(
        &self,
        provider_id: String,
        provider_preset: String,
        api_base: String,
        model: String,
        credential_override: Option<ProviderCredentialSecretDto>,
    ) -> ProviderConnectionTestDto {
        if !provider_id_is_valid(&provider_id) {
            return provider_connection_failure("Provider 标识无效", false, false, None, 0, false);
        }

        let draft = credential_override
            .as_ref()
            .map(ProviderCredentialSecretDto::expose_secret)
            .filter(|value| !value.trim().is_empty());
        let used_stored_credential = draft.is_none();
        let credential = if let Some(value) = draft {
            value.to_string()
        } else {
            match self.inner.credentials.get_provider_key(&provider_id).await {
                Ok(Some(secret)) => secret.expose_secret().clone(),
                Ok(None) => {
                    return provider_connection_failure(
                        "请先输入或保存 API Key",
                        false,
                        false,
                        None,
                        0,
                        true,
                    );
                }
                Err(_) => {
                    return provider_connection_failure(
                        "无法读取本机安全存储中的 API Key",
                        false,
                        false,
                        None,
                        0,
                        true,
                    );
                }
            }
        };
        if credential.len() > 16_384 || credential.contains('\0') {
            return provider_connection_failure(
                "API Key 格式无效",
                false,
                false,
                None,
                0,
                used_stored_credential,
            );
        }

        use llm_runtime::services::sdk::directory::probe::{ProbeCredential, ProbeProtocol};
        let kind = match provider_preset.as_str() {
            "anthropic" => ProbeProtocol::Anthropic,
            "google" => ProbeProtocol::Google,
            _ => ProbeProtocol::OpenAi,
        };
        let started = std::time::Instant::now();
        let response = probe_provider(
            &api_base,
            kind,
            ProbeCredential {
                token: credential.into(),
                bearer: false,
                account_id: None,
                fedramp: false,
            },
        )
        .await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        classify_provider_connection_response(
            response,
            model.trim(),
            latency_ms,
            used_stored_credential,
        )
    }

    /// The tokio runtime [`tokio::runtime::Id`] (as a string token) this async
    /// export RESOLVES ON (plan F3-07).
    ///
    /// This is a genuine async `#[uniffi::export(async_runtime = "tokio")]`
    /// method, so polling it travels the EXACT foreign-executor path `submit`
    /// does: `UniFFI` hands the returned future to the registered tokio runtime —
    /// the handle-owned `rt-multi-thread` runtime — which polls it to completion.
    /// It reads `tokio::runtime::Handle::current()` (which panics outside a tokio
    /// context) and returns that runtime's id token. The
    /// `async_submit_resolves_on_handle_runtime` test asserts the returned token
    /// equals [`Self::runtime_id`], proving the async export awaits on the
    /// handle-owned runtime rather than a transient ambient one — i.e. that the
    /// foreign async executor is registered to THIS runtime.
    pub async fn observed_runtime_id(&self) -> String {
        // `Handle::current()` resolves the runtime the polling thread is bound to.
        // Yield once so the value is read AFTER an actual await point — the future
        // is genuinely driven by the executor, not resolved eagerly on the caller.
        tokio::task::yield_now().await;
        format!("{:?}", tokio::runtime::Handle::current().id())
    }

    /// Inspect the mobile Linux runtime/rootfs status exposed by the current
    /// mobile platform backend.
    pub async fn mobile_linux_status(&self) -> Result<MobileLinuxStatusDto, MobileEngineError> {
        let Some(runtime) = self.inner.mobile_linux.as_ref() else {
            return Err(MobileEngineError::PlatformUnavailable);
        };

        Self::read_mobile_linux_status(runtime.as_ref()).await
    }

    /// Re-run rootfs verification and return the updated status.
    pub async fn verify_mobile_linux_rootfs(
        &self,
    ) -> Result<MobileLinuxStatusDto, MobileEngineError> {
        let Some(runtime) = self.inner.mobile_linux.as_ref() else {
            return Err(MobileEngineError::PlatformUnavailable);
        };
        let status = runtime.verify_rootfs().await.map_err(|e| {
            MobileEngineError::Internal(format!("verify_mobile_linux_rootfs failed: {e}"))
        })?;
        let capability = runtime.probe_capability().await;
        Ok(lower_mobile_linux_status(capability, status))
    }

    /// Attempt a non-destructive rootfs repair and return the updated status.
    pub async fn repair_mobile_linux_rootfs(
        &self,
    ) -> Result<MobileLinuxStatusDto, MobileEngineError> {
        let Some(runtime) = self.inner.mobile_linux.as_ref() else {
            return Err(MobileEngineError::PlatformUnavailable);
        };
        let status = runtime.repair_rootfs().await.map_err(|e| {
            MobileEngineError::Internal(format!("repair_mobile_linux_rootfs failed: {e}"))
        })?;
        let capability = runtime.probe_capability().await;
        Ok(lower_mobile_linux_status(capability, status))
    }

    /// Reset the managed rootfs state and return the updated status.
    pub async fn reset_mobile_linux_rootfs(
        &self,
    ) -> Result<MobileLinuxStatusDto, MobileEngineError> {
        let Some(runtime) = self.inner.mobile_linux.as_ref() else {
            return Err(MobileEngineError::PlatformUnavailable);
        };
        let status = runtime.reset_rootfs().await.map_err(|e| {
            MobileEngineError::Internal(format!("reset_mobile_linux_rootfs failed: {e}"))
        })?;
        let capability = runtime.probe_capability().await;
        Ok(lower_mobile_linux_status(capability, status))
    }
}

impl MobileEngineHandle {
    async fn submit_on_runtime(
        &self,
        command: ClientCommand,
        pending: Option<Arc<PendingSubmission>>,
    ) -> Result<(), ClientError> {
        // A questionnaire parks work that can itself be holding the transition
        // lock (for example, a long-running workflow). Its answer is
        // connection-scoped and only resolves the broker's oneshot, so waiting
        // behind that lock turns the native sheet's submit spinner into a
        // deadlock. Resolve these commands before serializing session state.
        match command {
            ClientCommand::Cancel { session_id, turn_id } => {
                let session_id = session_id.filter(|id| !id.is_empty()).ok_or_else(|| ClientError::Rejected { message: "mobile cancel requires a session id".into() })?;
                self.cancel_received_or_active(&session_id, turn_id).await
            },
            ClientCommand::ApprovePermission { request_id, response } => {
                self.resolve_permission(request_id, response).await
            }
            ClientCommand::DenyPermission { request_id } => {
                self.resolve_permission(request_id, PermissionResponseDto::Deny).await
            }
            ClientCommand::UpdatePromptSuggestionContext {
                session_id,
                focus,
                input_empty,
                pending_permission,
                elicitation_active,
            } => self
                .inner
                .orchestrator
                .update_prompt_suggestion_context(
                    lingxi_core::host::orchestrator::PromptSuggestionContext {
                        session_id,
                        focus,
                        input_empty,
                        pending_permission,
                        elicitation_active,
                    },
                )
                .await
                .map_err(|error| ClientError::Internal {
                    message: error.to_string(),
                }),
            ClientCommand::RecordPromptSuggestionSubmission { text } => self
                .inner
                .orchestrator
                .record_prompt_suggestion_submission(&text)
                .await
                .map_err(|error| ClientError::Internal {
                    message: error.to_string(),
                }),
            ClientCommand::PromptSuggestionFeedback {
                session_id,
                uuid,
                action,
            } => self
                .inner
                .orchestrator
                .prompt_suggestion_feedback(&session_id, &uuid, &action)
                .await
                .map_err(|error| ClientError::Internal {
                    message: error.to_string(),
                }),
            ClientCommand::AnswerAskUserQuestion {
                request_id,
                answers,
            } => {
                self.resolve_ask_user_question(request_id, Some(answers))
                    .await
            }
            ClientCommand::CancelAskUserQuestion { request_id } => {
                self.resolve_ask_user_question(request_id, None).await
            }
            ClientCommand::UiRender {
                request_id,
                request_json,
            } => {
                let request =
                    client::adapter::turn::parse_mod_ui_control_request(&request_json, "ui_render");
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiClientModule { request_id, plugin } => {
                self.emit_mod_ui_control_result(
                    request_id,
                    Ok(Utf16JsonProjection::object(vec![
                        ("subtype".into(), "ui_client_module".into()),
                        ("plugin".into(), plugin.into()),
                    ])
                    .expect("UI module request is a flat object")),
                )
                .await
            }
            ClientCommand::UiMessage {
                request_id,
                request_json,
            } => {
                let request = client::adapter::turn::parse_mod_ui_control_request(
                    &request_json,
                    "ui_message",
                );
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiClientFault {
                request_id,
                request_json,
            } => {
                let request = client::adapter::turn::parse_mod_ui_control_request(
                    &request_json,
                    "ui_client_fault",
                );
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiClientPress {
                request_id,
                request_json,
            } => {
                let request = client::adapter::turn::parse_mod_ui_control_request(
                    &request_json,
                    "ui_client_press",
                );
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiPress {
                request_id,
                request_json,
            } => {
                let request =
                    client::adapter::turn::parse_mod_ui_control_request(&request_json, "ui_press");
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiInput {
                request_id,
                request_json,
            } => {
                let request =
                    client::adapter::turn::parse_mod_ui_control_request(&request_json, "ui_input");
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiSelect {
                request_id,
                request_json,
            } => {
                let request =
                    client::adapter::turn::parse_mod_ui_control_request(&request_json, "ui_select");
                self.emit_mod_ui_control_result(request_id, request).await
            }
            ClientCommand::UiClientOperation {
                request_id,
                operation_json,
            } => {
                let operation =
                    client::adapter::turn::parse_mod_ui_client_operation(&operation_json);
                self.emit_mod_ui_client_operation_result(request_id, operation)
                    .await
            }
            ClientCommand::MediaServiceRequest { request }
                if request.validate().is_err() =>
            {
                Err(ClientError::Rejected {
                    message: "media service request failed validation".into(),
                })
            }
            ClientCommand::MediaServiceRequest { .. }
            | ClientCommand::MediaDeviceResponse { .. }
            | ClientCommand::MediaConnectionResponse { .. }
            | ClientCommand::MediaControlResponse { .. } => Err(ClientError::Rejected {
                message: "host-private media transport is not accepted by the mobile session command path".into(),
            }),
            command => Box::pin(self.submit_impl(command, pending)).await,
        }
    }

    /// Dispatch one non-turn Mod UI query directly to the currently mounted
    /// session handle. These commands bypass the live-turn/transition gate and
    /// report a response on the connection sink with their original correlator.
    async fn emit_mod_ui_control_result(
        &self,
        request_id: String,
        request: Result<lingxi_core::types::utf16_json::Utf16JsonProjection, String>,
    ) -> Result<(), ClientError> {
        let result = match request {
            Ok(request) => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                handle.mod_ui_control(request).await
            }
            Err(error) => Err(lingxi_core::host::HandleError::ActionFailed(error)),
        };
        self.connection_sink
            .emit(client::adapter::turn::mod_ui_control_result_event(
                request_id, result,
            ))
            .await;
        Ok(())
    }

    /// Route one host-local UI VM operation to the current session handle.
    async fn emit_mod_ui_client_operation_result(
        &self,
        request_id: String,
        operation: Result<lingxi_core::types::utf16_json::Utf16JsonProjection, String>,
    ) -> Result<(), ClientError> {
        let result = match operation {
            Ok(operation) => {
                let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
                handle.mod_ui_client_operation(operation).await
            }
            Err(error) => Err(lingxi_core::host::HandleError::ActionFailed(error)),
        };
        self.connection_sink
            .emit(client::adapter::turn::mod_ui_client_operation_result_event(
                request_id, result,
            ))
            .await;
        Ok(())
    }

    /// Resolve a parked permission request on the connection-scoped gate (the
    /// inbound side of the inverted handshake). The gate owns the original tool
    /// name and rejects stale/unknown ids rather than accepting a phantom tap.
    async fn resolve_permission(
        &self,
        request_id: u64,
        response: PermissionResponseDto,
    ) -> Result<(), ClientError> {
        let resolved = self
            .inner
            .permission_gate
            .resolve(request_id, response, "")
            .await;
        if resolved {
            Ok(())
        } else {
            Err(ClientError::NotFound {
                message: format!("permission request {request_id} is no longer pending"),
            })
        }
    }

    async fn emit_provider_credential_status(
        &self,
        operation_id: u64,
        provider_ids: &[String],
        preview_provider_ids: &[String],
        operation_error: Option<String>,
    ) {
        if let Some(error) = operation_error {
            self.event_sink
                .emit(ClientEvent::ProviderCredentialStatus {
                    operation_id,
                    configured_provider_ids: Vec::new(),
                    unavailable_provider_ids: provider_ids.to_vec(),
                    storage_encrypted: self.inner.credentials.provider_key_storage_is_encrypted(),
                    credential_previews: HashMap::new(),
                    error: Some(error),
                })
                .await;
            return;
        }

        let mut configured_provider_ids = Vec::new();
        let mut unavailable_provider_ids = Vec::new();
        let mut credential_previews = HashMap::new();
        let mut failures = Vec::new();
        for provider_id in provider_ids {
            match self.inner.credentials.has_provider_key(provider_id).await {
                Ok(true) => {
                    configured_provider_ids.push(provider_id.clone());
                    if preview_provider_ids.contains(provider_id) {
                        match self.inner.credentials.get_provider_key(provider_id).await {
                            Ok(Some(secret)) => {
                                credential_previews.insert(
                                    provider_id.clone(),
                                    secret::masked_credential_preview(secret.expose_secret()),
                                );
                            }
                            Ok(None) => {}
                            Err(failure) => {
                                failures.push(format!("{provider_id} preview: {failure}"))
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(failure) => {
                    unavailable_provider_ids.push(provider_id.clone());
                    failures.push(format!("{provider_id}: {failure}"));
                }
            }
        }
        let error = (!failures.is_empty()).then(|| {
            format!(
                "provider credential storage is unavailable ({})",
                failures.join("; ")
            )
        });
        self.event_sink
            .emit(ClientEvent::ProviderCredentialStatus {
                operation_id,
                configured_provider_ids,
                unavailable_provider_ids,
                storage_encrypted: self.inner.credentials.provider_key_storage_is_encrypted(),
                credential_previews,
                error,
            })
            .await;
    }

    /// Enumerate the on-disk resumable-session catalog and emit a `SessionList`
    /// event (SESSIONS/HISTORY).
    ///
    /// Reads `<lingxi_home>/projects/<sanitized cwd>/*.jsonl` via the shared
    /// `session::jsonl::list_recent_sessions` (the SAME enumerator the CLI
    /// `/resume` picker uses), capped at `limit`, then lowers each
    /// `SessionMetadata` row through the shared
    /// `client::adapter::lower_session_metadata` (decision §0.2). A missing /
    /// empty catalog (`LoaderError::EmptyDirectory`) is NOT an error here — it
    /// replies with an empty list ("no resumable sessions yet"); a real I/O
    /// failure is logged and also yields an empty list so the client always gets
    /// a reply.
    async fn emit_session_list(&self, limit: usize) {
        match self
            .workspace_sessions_inner(
                MobileSessionDiscoveryScope::Repository,
                limit.min(u32::MAX as usize) as u32,
            )
            .await
        {
            Ok(sessions) => {
                self.event_sink
                    .emit(ClientEvent::SessionList { sessions })
                    .await
            }
            Err(error) => {
                self.event_sink
                    .emit(ClientEvent::Error {
                        kind: ErrorKindDto::Internal,
                        message: format!("session catalog: {error}").into(),
                    })
                    .await
            }
        }
    }

    /// The catalog rows this connection can actually route to.
    ///
    /// The LIVE client config, not `OrchestratorHandle::list_model_listings` —
    /// see [`MobileRuntime::routable_listings`] for why the static catalog
    /// answers a different question than the one a picker is asking.
    ///
    /// The one exception is an EMPTY routable set. iOS always emits
    /// `routing.mobileEnabledProfiles`, so a fresh install with nothing
    /// configured sends `[]`, which `apply_mobile_profile_allowlist` treats as
    /// fail-closed and strips every profile. Nothing is routable in that state
    /// whatever we show, so fall back to the static catalog rather than hand the
    /// client an empty picker it can neither act on nor explain.
    async fn routable_model_listings(&self) -> Vec<lingxi_core::host::ModelListing> {
        if !self.inner.routable_listings.is_empty() {
            return self.inner.routable_listings.clone();
        }
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        let listings = handle.list_model_listings().await;
        if !listings.is_empty() {
            return listings;
        }
        // An explicit empty mobile profile allowlist intentionally strips all
        // live routes. Keep a shortlist of what can be configured in this
        // runtime's captured region, including when a region change filtered
        // out every enabled profile.
        let providers: Vec<_> = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .filter(|provider| provider.regions.contains(&self.inner.provider_region))
            .collect();
        model_listings(&providers)
    }

    async fn resume_runtime_with_model_preference(
        &self,
        mut runtime: lingxi_core::host::ResumeRuntimeSnapshot,
    ) -> lingxi_core::host::ResumeRuntimeSnapshot {
        if self.inner.interactive_launch {
            if let Some(saved) = model_preference::load(&self.lingxi_home) {
                // Picker listings can fall back to unavailable built-ins. Only
                // live, enabled routes may restore an implicit preference.
                let listings = &self.inner.routable_listings;
                let current = self.inner.orchestrator.get_status_snapshot().await;
                let (model, profile) = model_preference::resolve(&saved, listings)
                    .unwrap_or((current.model, current.model_profile));
                runtime.model = model;
                runtime.model_profile = profile;
            }
        }
        runtime
    }

    async fn switch_model_and_remember(
        &self,
        request: lingxi_core::host::ModelSelectionRequest,
    ) -> Result<String, ClientError> {
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        let previous = handle.get_status_snapshot().await;
        handle
            .select_model(request)
            .await
            .map_err(|error| ClientError::Internal {
                message: format!("switch_model failed: {error}"),
            })?;
        let current = handle.get_status_snapshot().await;
        let selected = lingxi_core::host::qualified_model_ref(
            &current.model,
            current.model_profile.as_deref(),
        );
        if self.inner.interactive_launch {
            if let Err(error) = model_preference::save(&self.lingxi_home, &selected) {
                let rollback = handle
                    .switch_model(&previous.model, previous.model_profile.as_deref())
                    .await;
                let message = match rollback {
                    Ok(()) => format!("save model preference failed: {error}"),
                    Err(rollback_error) => format!(
                        "save model preference failed: {error}; restore previous model failed: {rollback_error}"
                    ),
                };
                return Err(ClientError::Internal { message });
            }
        }
        Ok(selected)
    }

    /// Resolve a client-supplied model reference into the `(wire id, profile)`
    /// pair the orchestrator takes, REFUSING one no configured provider serves.
    ///
    /// [`lingxi_core::host::parse_model_ref`] falls back to treating an unresolvable
    /// reference as a bare wire id, so accepting one put `provider/model` —
    /// which is not a wire id at all — into `session.model`. Every turn of that
    /// session then 404'd, and because the transcript persists the session
    /// model, the failure outlived the session.
    async fn resolve_routable_model(
        &self,
        model: &str,
    ) -> Result<(String, Option<String>), ClientError> {
        let listings = &self.inner.routable_listings;
        let (model_id, profile) = lingxi_core::host::parse_model_ref(model, listings);
        let routable = listings.iter().any(|listing| {
            listing.request_model == model_id
                && profile
                    .as_deref()
                    .is_none_or(|wanted| listing.provider_id == wanted)
        });
        if routable {
            Ok((model_id, profile))
        } else {
            Err(ClientError::Rejected {
                message: format!(
                    "model {model:?} is not served by any configured provider; \
                     enable its provider in settings or pick another model"
                ),
            })
        }
    }

    /// Return the directory containing child-agent transcripts for the live
    /// connection session. The path is derived exclusively from engine-owned
    /// session state; callers never get to supply a filesystem path.
    async fn session_agent_dir(&self) -> (lingxi_core::types::SessionId, std::path::PathBuf) {
        let session_id = self.inner.orchestrator.current_session_id().await;
        let dir = orchestrator::transcript_paths::subagents_dir(
            &self.lingxi_home,
            &self.session_cwd(),
            &session_id.as_uuid().to_string(),
        );
        (session_id, dir)
    }

    fn agent_summary_activity(
        messages: &[client::protocol::message::MessageDto],
    ) -> Option<ClientTextDto> {
        messages
            .iter()
            .rev()
            .flat_map(|message| message.blocks.iter())
            .find_map(|block| match block {
                client::protocol::message::MessageBlockDto::Text { text } => {
                    client::adapter::activity::summarize_activity_line(&text.code_units)
                }
                client::protocol::message::MessageBlockDto::Thinking { thinking, .. } => {
                    let units = thinking.encode_utf16().collect::<Vec<_>>();
                    client::adapter::activity::summarize_activity_line(&units)
                }
                _ => None,
            })
    }

    fn hide_agent_lifecycle_messages(
        messages: Vec<lingxi_core::types::ConversationMessage>,
    ) -> Vec<lingxi_core::types::ConversationMessage> {
        messages
            .into_iter()
            .filter(|message| {
                !matches!(
                    message,
                    lingxi_core::types::ConversationMessage::System {
                        subtype: Some(subtype),
                        ..
                    } if subtype.starts_with("agent_")
                )
            })
            .collect()
    }

    async fn read_agent_summary(
        agent_id: String,
        path: &std::path::Path,
    ) -> Option<SessionAgentSummaryDto> {
        let messages = Self::hide_agent_lifecycle_messages(
            session::agent_rows::read_transcript_messages(path.parent()?, &agent_id).await,
        )
        .into_iter()
        .filter(session_agent_conversation_is_visible)
        .collect::<Vec<_>>();
        let raw = tokio::fs::read_to_string(path).await.ok();
        let mut status = "running".to_string();
        let mut metadata_name: Option<Utf16JsonProjection> = None;
        let mut metadata_type: Option<String> = None;
        let mut metadata_model: Option<String> = None;
        let mut metadata_model_profile: Option<String> = None;
        let mut latest_activity =
            Self::agent_summary_activity(&client::adapter::lowering::lower_transcript(&messages));
        if let Some(raw) = raw {
            for line in raw.lines().rev().filter(|line| !line.trim().is_empty()) {
                let Ok(source) = Utf16JsonProjection::parse(line) else {
                    continue;
                };
                metadata_name = source
                    .subprojection("/agent_name")
                    .ok()
                    .filter(|name| {
                        name.string_units("")
                            .is_some_and(|code_units| !code_units.is_empty())
                    })
                    .or(metadata_name);
                metadata_type = source
                    .string_at("/agent_type")
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .or(metadata_type);
                metadata_model = source
                    .string_at("/model")
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .or(metadata_model);
                metadata_model_profile = source
                    .string_at("/model_profile")
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .or(metadata_model_profile);
                if let Some(status_value) = source.string_at("/status") {
                    status = match status_value {
                        "completed" => "completed",
                        "failed" => "failed",
                        "killed" => "killed",
                        "cancelled" => "cancelled",
                        // The TRANSCRIPT's own rest marker (`agent_idle`,
                        // written by `agent::transcript::record_terminal`) is
                        // an engine-internal word. Translate it here rather
                        // than letting it reach a client: on the wire a parked
                        // agent is `completed`, exactly like claude-code's row.
                        "idle" => "completed",
                        "running" => "running",
                        _ => "unknown",
                    }
                    .to_string();
                    if let Some(error_units) = source
                        .string_units("/error")
                        .filter(|code_units| !code_units.is_empty())
                    {
                        latest_activity =
                            client::adapter::activity::summarize_activity_line(&error_units);
                    }
                    break;
                }
            }
        }
        let row = session::agent_rows::read_row(path.parent()?, &agent_id).await;
        let (row_name, row_type, row_model, row_model_profile, parked) = row
            .map(|row| {
                (
                    row.request
                        .name
                        .map(Utf16JsonProjection::from)
                        .or(row.request.description),
                    (!row.request.subagent_type.is_empty()).then_some(row.request.subagent_type),
                    row.request.model,
                    row.request.model_profile,
                    true,
                )
            })
            .unwrap_or((None, None, None, None, false));
        let agent_type = metadata_type
            .or(row_type)
            .unwrap_or_else(|| "unknown".to_string());
        let name = metadata_name
            .map(Utf16JsonProjection::from)
            .or(row_name)
            .or_else(|| {
                (agent_type != "unknown").then(|| Utf16JsonProjection::from(agent_type.clone()))
            })
            .unwrap_or_else(|| {
                agent_id
                    .strip_prefix("agent:")
                    .unwrap_or(&agent_id)
                    .chars()
                    .take(8)
                    .collect::<String>()
                    .into()
            });
        // A `.task.json` row beside the transcript means the agent PARKED:
        // it finished a turn-set and is resumable. claude-code renders that as
        // `(done)` under status `completed`; `idle` is the footer group's word,
        // not a row's status.
        if parked && status == "running" {
            status = "completed".to_string();
        }
        let updated_at_ms = tokio::fs::metadata(path)
            .await
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        Some(SessionAgentSummaryDto {
            agent_id,
            name: client::protocol::text::ClientTextDto::from_projection(&name)
                .expect("agent label is text"),
            agent_type,
            model: metadata_model.or(row_model),
            model_profile: metadata_model_profile.or(row_model_profile),
            status,
            latest_activity,
            updated_at_ms,
        })
    }

    async fn emit_session_agent_list(&self) {
        let (session_id, dir) = self.session_agent_dir().await;
        let snapshot = self.inner.orchestrator.get_status_snapshot().await;
        let status = if self.active_cancel.lock().await.is_some() {
            "running"
        } else {
            "idle"
        };
        let mut agents = vec![SessionAgentSummaryDto {
            agent_id: "main".to_string(),
            name: "Main agent".into(),
            agent_type: "main".to_string(),
            model: Some(snapshot.model.clone()),
            model_profile: snapshot.model_profile.clone(),
            status: status.to_string(),
            latest_activity: (snapshot.n_messages > 0)
                .then(|| format!("{} messages · {}", snapshot.n_messages, snapshot.model).into()),
            updated_at_ms: None,
        }];
        if let Ok(paths) = collect_session_agent_transcript_paths(&dir).await {
            for path in paths {
                let Some(agent_id) = session_agent_id_from_path(&path) else {
                    continue;
                };
                if let Some(summary) = Self::read_agent_summary(agent_id, &path).await {
                    agents.push(summary);
                }
            }
        }
        agents[1..].sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        self.event_sink
            .emit(ClientEvent::SessionAgentList {
                session_id: session_id.as_uuid().to_string(),
                agents,
            })
            .await;
    }

    async fn load_session_agent_transcript_for_session(
        &self,
        session_id: lingxi_core::types::SessionId,
        dir: &std::path::Path,
        agent_id: &str,
    ) -> Result<
        (
            Vec<client::protocol::listings::SessionAgentMessageRowDto>,
            u64,
            u64,
        ),
        ClientError,
    > {
        let (messages, revision, next_message_index) = if agent_id == "main" {
            let uuid = session_id.as_uuid();
            let replayed = orchestrator::replay_session_state(
                &self.lingxi_home,
                &self.session_cwd(),
                uuid,
                self.fs.clone(),
            )
            .await
            .map_err(|error| ClientError::Rejected {
                message: format!("load main transcript failed: {error}"),
            })?;
            let path = orchestrator::transcript_paths::main_transcript_path(
                &self.lingxi_home,
                &self.session_cwd(),
                &uuid.to_string(),
            );
            if self.inner.session_writer.active_path() != path {
                return Err(ClientError::Internal {
                    message: format!(
                        "load main transcript identity index failed: shared writer is not active for {}",
                        path.display()
                    ),
                });
            }
            let raw = match tokio::fs::read(&path).await {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => {
                    return Err(ClientError::Rejected {
                        message: format!("load main transcript failed: {error}"),
                    });
                }
            };
            let identity_snapshot =
                session_identity_snapshot_for_path(&self.inner.session_writer, &path)
                    .await
                    .map_err(|error| ClientError::Rejected {
                        message: format!("load main transcript identity import failed: {error}"),
                    })?;
            let rows = parse_main_session_agent_message_rows(&replayed, &identity_snapshot)
                .map_err(|message| ClientError::Rejected {
                    message: format!("load main transcript rows failed: {message}"),
                })?;
            (
                rows,
                session_agent_transcript_revision(&raw),
                identity_snapshot.next_message_index,
            )
        } else {
            let parsed =
                lingxi_core::types::AgentId::parse_prefixed(agent_id).ok_or_else(|| {
                    ClientError::Rejected {
                        message: format!("malformed session agent id: {agent_id:?}"),
                    }
                })?;
            let path = find_session_agent_transcript_path(dir, &parsed.to_string())
                .await
                .map_err(|error| ClientError::Rejected {
                    message: format!("load session agent transcript failed: {error}"),
                })?;
            if let Some(path) = path {
                let raw = tokio::fs::read(&path).await.unwrap_or_default();
                let messages = parse_session_agent_message_rows(&raw).map_err(|message| {
                    ClientError::Rejected {
                        message: format!("load session-agent transcript rows failed: {message}"),
                    }
                })?;
                let next_message_index =
                    read_session_agent_next_message_index(self.fs.as_ref(), &path).await?;
                (
                    messages,
                    session_agent_transcript_revision(&raw),
                    next_message_index,
                )
            } else if let Some(snapshot) = self
                .inner
                .session_agent_transcript_cache
                .lock()
                .await
                .get(agent_id)
                .cloned()
            {
                (
                    snapshot.rows,
                    snapshot.revision,
                    snapshot.next_message_index,
                )
            } else {
                (Vec::new(), 0, 0)
            }
        };
        Ok((messages, revision, next_message_index))
    }

    async fn emit_session_agent_transcript(&self, agent_id: String) -> Result<(), ClientError> {
        let (requested_session_id, dir) = self.session_agent_dir().await;
        let (messages, revision, next_message_index) = self
            .load_session_agent_transcript_for_session(requested_session_id, &dir, &agent_id)
            .await?;
        let current_session_id = self.inner.orchestrator.current_session_id().await;
        if let Some(event) = session_agent_transcript_event(
            requested_session_id,
            current_session_id,
            agent_id,
            messages,
            next_message_index,
            revision,
        ) {
            self.event_sink.emit(event).await;
        }
        Ok(())
    }

    fn slash_command_catalog_from_registry(
        reg: &command_api::CommandRegistry,
        session_mode: session::jsonl::SessionMode,
    ) -> Vec<SlashCommandDto> {
        let mut commands: Vec<_> = reg
            .palette_commands()
            .into_iter()
            .filter(|command| command_visible_in_session_mode(command, session_mode))
            .map(|command| SlashCommandDto {
                hidden: command_api::builtin_support::names::is_palette_hidden(
                    command.name.string_at("").expect("command name is text"),
                ),
                source: command_source_string(command.source).to_string(),
                name: client::protocol::text::ClientTextDto::from_projection(&command.name)
                    .expect("command name is text"),
                description: client::protocol::text::ClientTextDto::from_projection(
                    &command.description,
                )
                .expect("command description is text"),
                aliases: command
                    .aliases
                    .iter()
                    .map(|alias| {
                        client::protocol::text::ClientTextDto::from_projection(alias)
                            .expect("command alias is text")
                    })
                    .collect(),
                argument_hint: command.argument_hint.as_ref().map(|hint| {
                    client::protocol::text::ClientTextDto::from_projection(hint)
                        .expect("command hint is text")
                }),
                argument_names: if matches!(
                    &command.kind,
                    command_api::SlashCommandKind::Builtin { .. }
                ) {
                    Vec::new()
                } else {
                    command
                        .argument_names
                        .iter()
                        .map(|argument| {
                            client::protocol::text::ClientTextDto::from_projection(argument)
                                .expect("command argument name is text")
                        })
                        .collect()
                },
                menu_description: command.menu_description,
            })
            .collect();
        commands.sort_by(|a, b| a.name.code_units.cmp(&b.name.code_units));
        commands
    }

    async fn slash_command_catalog_snapshot(&self) -> Vec<SlashCommandDto> {
        self.inner.dispatcher.refresh_command_snapshot().await;
        let reg = self.inner.slash_registry.read().await;
        Self::slash_command_catalog_from_registry(&reg, self.inner.session_mode)
    }

    async fn capture_slash_authority(&self) -> SlashAuthoritySnapshot {
        let snapshot = self.inner.orchestrator.get_status_snapshot().await;
        SlashAuthoritySnapshot {
            session_id: self
                .inner
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string(),
            model: lingxi_core::host::qualified_model_ref(
                &snapshot.model,
                snapshot.model_profile.as_deref(),
            ),
            model_source: lower_model_source(snapshot.model_source.as_ref()),
            permission_mode: self
                .inner
                .orchestrator
                .permission_mode()
                // `capture_slash_authority` runs after construction and on
                // command dispatch; the boot-local resolved mode is not in
                // scope here. The orchestrator is authoritative once built,
                // while Auto is the built-in fallback for new runtimes.
                .unwrap_or_else(|| PermissionMode::Auto.wire_str().to_string()),
            auth: lower_auth_state(self.inner.auth.current_user().await),
            catalog: self.slash_command_catalog_snapshot().await,
        }
    }

    async fn emit_slash_authority_changes(
        &self,
        before: &SlashAuthoritySnapshot,
        after: &SlashAuthoritySnapshot,
    ) -> Result<(), ClientError> {
        if before.session_id != after.session_id {
            self.retarget_session_writer(
                self.inner.orchestrator.current_session_id().await,
                &self.session_cwd(),
            )
            .await?;
            self.event_sink.emit(ClientEvent::SessionEnded).await;
            let _ = self.session_lifecycle_tx.send(after.session_id.clone());
        }
        if before.model != after.model || before.model_source != after.model_source {
            self.event_sink
                .emit(ClientEvent::ModelChanged {
                    model: after.model.clone(),
                    model_source: after.model_source.clone(),
                })
                .await;
        }
        if before.permission_mode != after.permission_mode {
            self.event_sink
                .emit(ClientEvent::PermissionModeChanged {
                    mode: after.permission_mode.clone(),
                })
                .await;
        }
        if before.auth != after.auth {
            self.event_sink
                .emit(ClientEvent::AuthState {
                    state: after.auth.clone(),
                })
                .await;
        }
        if before.catalog != after.catalog {
            self.event_sink
                .emit(ClientEvent::CommandsChanged {
                    commands: after.catalog.clone(),
                })
                .await;
        }
        Ok(())
    }

    /// Pull a single listing kind and emit its listing event through the
    /// connection's event sink, reusing the shared `client::adapter::lowering`
    /// parity fns (decision §0.2). Listing kinds with no engine handle on mobile
    /// (`Sessions` / `Memory` / `Settings` / `Tasks`) are skipped. Slash
    /// commands are the engine's authoritative skill catalog on mobile.
    async fn emit_listing(&self, kind: ProtocolListingKind) {
        use client::adapter::lowering::{
            lower_agent_info, lower_doctor_report, lower_hook_info, lower_mcp_server_info,
            lower_status_snapshot,
        };
        let handle: Arc<dyn OrchestratorHandle> = self.inner.orchestrator.clone();
        match kind {
            ProtocolListingKind::Settings => {
                self.emit_settings_snapshot(self.connection_sink.as_ref())
                    .await
            }
            ProtocolListingKind::Models => {
                self.event_sink
                    .emit(ClientEvent::ProviderModelCatalog {
                        providers: self.inner.provider_model_catalog.clone(),
                    })
                    .await;
                // Curate to the "latest few" per provider instead of flooding the
                // client with the full assembled catalog (~hundreds of ids — every
                // preset is injected into the live config by `provider_config::assemble`).
                // Mobile lacks the TUI's availability maps, so this trims to the
                // shared `is_curated_model` whitelist (keeping the current model);
                // `[Connect]` gating + grouping stays a TUI/structured-DTO concern.
                let available = handle.list_available_models().await;
                let listings = self.routable_model_listings().await;
                let snapshot = handle.get_status_snapshot().await;
                let curated = lingxi_core::host::curated_model_listings(
                    &listings,
                    &snapshot.model,
                    snapshot.model_profile.as_deref(),
                );
                let models = lingxi_core::host::curated_model_refs(
                    &listings,
                    &available,
                    &snapshot.model,
                    snapshot.model_profile.as_deref(),
                );
                let details = curated.iter().map(lower_model_details).collect();
                let current = lingxi_core::host::qualified_model_ref(
                    &snapshot.model,
                    snapshot.model_profile.as_deref(),
                );
                self.event_sink
                    .emit(ClientEvent::ModelList {
                        models,
                        current,
                        current_model_source: lower_model_source(snapshot.model_source.as_ref()),
                        details,
                    })
                    .await;
            }
            ProtocolListingKind::Mcp => {
                if self.inner.session_mode == session::jsonl::SessionMode::Chat {
                    self.event_sink
                        .emit(ClientEvent::McpServers {
                            servers: Vec::new(),
                        })
                        .await;
                    return;
                }
                self.reload_configured_mcp().await;
                let servers = handle
                    .list_mcp_servers()
                    .await
                    .iter()
                    .map(lower_mcp_server_info)
                    .collect();
                self.event_sink
                    .emit(ClientEvent::McpServers { servers })
                    .await;
            }
            ProtocolListingKind::SlashCommands => {
                // `/reload-skills` reconciles project/user SKILL.md files into
                // the same command registry that dispatches them. Ignore the
                // display result; the following snapshot is the structured
                // source of truth for the settings UI.
                let _ = self
                    .inner
                    .dispatcher
                    .dispatch(
                        "/reload-skills".into(),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await;
                let commands = self.slash_command_catalog_snapshot().await;
                self.event_sink
                    .emit(ClientEvent::SlashCommandCatalog { commands })
                    .await;
            }
            ProtocolListingKind::Hooks => {
                let hooks = handle
                    .list_hooks()
                    .await
                    .iter()
                    .map(lower_hook_info)
                    .collect();
                self.event_sink.emit(ClientEvent::Hooks { hooks }).await;
            }
            ProtocolListingKind::Agents => {
                let agents = handle
                    .list_agents()
                    .await
                    .iter()
                    .map(lower_agent_info)
                    .collect();
                self.event_sink.emit(ClientEvent::Agents { agents }).await;
            }
            ProtocolListingKind::Status => {
                let snapshot = lower_status_snapshot(&handle.get_status_snapshot().await);
                self.event_sink
                    .emit(ClientEvent::StatusSnapshot { snapshot })
                    .await;
            }
            ProtocolListingKind::Doctor => {
                let report = lower_doctor_report(&handle.run_doctor_checks().await);
                self.event_sink
                    .emit(ClientEvent::DoctorReport { report })
                    .await;
            }
            ProtocolListingKind::Auth => {
                let state = lower_auth_state(self.inner.auth.current_user().await);
                self.event_sink.emit(ClientEvent::AuthState { state }).await;
            }
            // No engine handle on mobile in the foundation — additive to wire.
            _ => {
                tracing::debug!(
                    ?kind,
                    "engine-mobile: listing kind unhandled in the foundation"
                );
            }
        }
    }

    /// Re-read MCP config before a settings refresh so a save in the iOS UI
    /// becomes visible without rebuilding the whole conversation engine.
    /// The live catalog refresher below the initial tool registration observes
    /// the registry changes and updates the model-facing tools asynchronously.
    /// The listing and the next turn therefore use the same live registry.
    async fn reload_configured_mcp(&self) {
        if self.inner.session_mode == session::jsonl::SessionMode::Chat {
            return;
        }
        let Ok(cfg) = self.current_mobile_config() else {
            return;
        };
        let cwd = cfg.cwd;
        let configured = mobile_mcp_preflight(
            mcp::load_mcp_servers(
                &cwd.join(".mcp.json"),
                &self.lingxi_home.join("mcp-config.json"),
                &cwd,
            ),
            self.inner.oauth_supported,
        );
        let desired: std::collections::HashMap<String, McpServerConfig> = configured
            .iter()
            .cloned()
            .map(|config| (config.name.clone(), config))
            .collect();
        let generations = self.inner.mcp_reload_generations.clone();
        let current_states: std::collections::HashMap<_, _> = self
            .inner
            .mcp_registry
            .connections
            .read()
            .await
            .iter()
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect();
        let current_names: std::collections::HashSet<String> =
            current_states.keys().cloned().collect();
        let retained_plugin_names: std::collections::HashSet<String> = current_states
            .iter()
            // `reconcileMcpServers` retains only plugin-sourced CURRENT configs
            // that disappeared from desired state. A desired name collision is
            // an overlap/replacement, not a retained plugin.
            .filter(|(name, state)| {
                !desired.contains_key(*name) && is_plugin_owned_mcp_config(state.config())
            })
            .map(|(name, _)| name.clone())
            .collect();
        let desired_count = desired.len();
        let current_count = current_names.len();
        let tracked_names: Vec<String> = generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        let mut reconcile_names = std::collections::HashSet::new();
        reconcile_names.extend(
            current_names
                .into_iter()
                .filter(|name| !retained_plugin_names.contains(name)),
        );
        reconcile_names.extend(
            tracked_names
                .into_iter()
                .filter(|name| !retained_plugin_names.contains(name)),
        );
        reconcile_names.extend(desired.keys().cloned());
        let mut to_add_count = 0_usize;
        let mut to_remove_count = 0_usize;
        let mut to_replace_count = 0_usize;
        let mut jobs = Vec::new();
        for name in reconcile_names {
            let current_state = current_states.get(&name).cloned();
            let mut desired_config = desired.get(&name).cloned();
            if let (Some(current), Some(desired)) =
                (current_state.as_ref(), desired_config.as_mut())
            {
                // Oracle retention carries plugin provenance across a desired
                // collision even though the transport config is replaced.
                if is_plugin_owned_mcp_config(current.config()) {
                    desired.metadata.agent_source = Some(mcp::McpAgentSource::Plugin);
                }
            }
            let desired_config_ref = desired_config.as_ref();
            let Some(current_state) = current_state else {
                let (generation, _) =
                    mobile_mcp_record_reload_intent(&generations, &name, desired_config_ref);
                let Some(config) = desired_config else {
                    continue;
                };
                to_add_count += 1;
                jobs.push(MobileMcpReloadJob::Connect {
                    name,
                    generation,
                    desired: config,
                    previous: None,
                });
                continue;
            };
            let expected = current_state.config().clone();
            let (generation, intent_changed) =
                mobile_mcp_record_reload_intent(&generations, &name, desired_config_ref);
            if let Some(config) = desired_config {
                if !mobile_mcp_reload_requires_replacement(&current_state, &config, intent_changed)
                {
                    // Preserve live/cached/failed state exactly as-is. In
                    // particular, do not redial, purge discovery, or touch
                    // OAuth storage merely because the listing was refreshed.
                    continue;
                }
                to_replace_count += 1;
                jobs.push(MobileMcpReloadJob::Connect {
                    name,
                    generation,
                    desired: config,
                    previous: Some(expected),
                });
            } else {
                to_remove_count += 1;
                jobs.push(MobileMcpReloadJob::Remove {
                    name,
                    generation,
                    expected,
                });
            }
        }
        telemetry::emit_mcp_reconcile(&telemetry::tengu::mcp::ReconcilePayload {
            // This mobile listing-triggered refresh has no Claude Code caller
            // equivalent. `reconcileMcpServers` defaults an omitted caller to
            // the exact low-cardinality value `unknown`.
            caller: telemetry::Verified::assert_safe("unknown".to_string()),
            desired_count: u32::try_from(desired_count).expect("desired MCP count fits in u32"),
            current_count: u32::try_from(current_count).expect("current MCP count fits in u32"),
            to_remove_count: u32::try_from(to_remove_count).expect("remove MCP count fits in u32"),
            to_add_count: u32::try_from(to_add_count).expect("add MCP count fits in u32"),
            to_replace_count: u32::try_from(to_replace_count)
                .expect("replace MCP count fits in u32"),
            retained_plugin_count: u32::try_from(retained_plugin_names.len())
                .expect("retained plugin MCP count fits in u32"),
        });
        if !jobs.is_empty() {
            // A settings listing must never wait for a server's transport
            // teardown, network handshake, or interactive OAuth. Every job
            // carries a per-server generation; stale jobs stop before touching
            // state and stale connect completions are disconnected by CAS.
            let registry = self.inner.mcp_registry.clone();
            tokio::spawn(async move {
                futures_util::future::join_all(jobs.into_iter().map(|job| {
                    mobile_mcp_run_reload_job(job, registry.clone(), generations.clone())
                }))
                .await;
            });
        }
    }
}

fn command_source_string(source: command_api::model::CommandSource) -> &'static str {
    match source {
        command_api::model::CommandSource::Builtin => "builtin",
        command_api::model::CommandSource::Settings(lingxi_core::types::SettingsScope::User) => {
            "user"
        }
        command_api::model::CommandSource::Settings(lingxi_core::types::SettingsScope::Project) => {
            "project"
        }
        command_api::model::CommandSource::Settings(lingxi_core::types::SettingsScope::Local) => {
            "local"
        }
        command_api::model::CommandSource::Plugin => "plugin",
        command_api::model::CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => {
            "managed"
        }
        command_api::model::CommandSource::Mcp => "mcp",
        command_api::model::CommandSource::Bundled => "bundled",
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Cron firing — the Android background-scheduler bridge.
//
// The desktop `cron::CronScheduler` 60s tick loop is unavailable on mobile (no
// long-lived daemon, and the mobile engine binds no `TaskRegistry` / subagent
// spawner). Android WorkManager calls the single-occurrence methods below after
// AlarmManager or the 15-minute watchdog wakes it. Due-detection and
// bookkeeping remain in the shared cron store; firing is a fresh, throwaway
// orchestrator turn. Durable allow rules still apply, while any permission that
// would require foreground interaction is denied immediately.
// ───────────────────────────────────────────────────────────────────────────

// Headless turns share session gates with foreground command dispatch. They
// write persistent JSONL through an isolated listener, then invalidate any
// idle foreground reader so its next command reloads the updated transcript.

// The cron FFI surface — async UniFFI exports driven on the handle-owned runtime
// (same `async_runtime = "tokio"` contract as `submit`). A separate impl block so
// the cron methods read as one unit; UniFFI supports multiple exported blocks.
#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileEngineHandle {
    /// Run one scheduled occurrence exactly once across duplicate alarm/worker
    /// deliveries. A retryable transport failure intentionally leaves the
    /// occurrence unacknowledged so WorkManager can retry it.
    pub async fn run_cron_task_if_due(
        &self,
        task_id: String,
        scheduled_at_ms: u64,
    ) -> Option<FiredCronJobDto> {
        let _transition = self.loop_transition.lock().await;
        let cfg = self.current_mobile_config().ok()?;
        let fs = self.firer_platform.filesystem();
        let existing = read_cron_tasks(fs.as_ref(), &cfg.cwd)
            .await
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)?;
        if existing.automation.is_some() {
            if mobile_cron_schedule_error(&existing.cron, existing.recurring.unwrap_or(false))
                .is_some()
            {
                return None;
            }
            return self
                .fire_automation_task(task_id, Some(scheduled_at_ms), None)
                .await;
        }
        let _process_guard = cron::lock_cron_file().await;
        let _file_guard = cron::tasks_file::lock_automation_tasks(fs.as_ref(), &cfg.cwd)
            .await
            .ok()?;
        let mut document = read_cron_tasks(fs.as_ref(), &cfg.cwd).await;
        let now = self.firer_platform.clock().now();
        let now_ms = now
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let task = document
            .tasks
            .iter()
            .find(|task| task.id == task_id)?
            .clone();
        let recurring = task.recurring.unwrap_or(false);
        if !cron_task_active(&task) || mobile_cron_schedule_error(&task.cron, recurring).is_some() {
            return None;
        }
        let expected = cron::next_fire_epoch_ms_for_persisted_task(&task, now)?;
        if expected != scheduled_at_ms || scheduled_at_ms > now_ms {
            return None;
        }

        let firer = MobileTurnFirer {
            cfg: cfg.clone(),
            platform: self.firer_platform.clone(),
        };
        let fired = fired_cron_dto(&task, firer.fire(&task.id, &task.prompt).await);
        if !fired.retryable {
            finalize_cron_occurrence(&mut document, &task.id, now_ms);
            let _ = cron::tasks_file::write_automation_tasks_body(
                fs.as_ref(),
                &cfg.cwd,
                &cron::serialize_tasks(&document),
            )
            .await;
        }
        Some(fired)
    }

    /// Mark a retry-exhausted occurrence complete without running it again.
    pub async fn acknowledge_cron_occurrence(&self, task_id: String, scheduled_at_ms: u64) -> bool {
        let _transition = self.loop_transition.lock().await;
        let cfg = match self.current_mobile_config() {
            Ok(cfg) => cfg,
            Err(_) => return false,
        };
        let fs = self.firer_platform.filesystem();
        let _process_guard = cron::lock_cron_file().await;
        let Ok(_file_guard) = cron::tasks_file::lock_automation_tasks(fs.as_ref(), &cfg.cwd).await
        else {
            return false;
        };
        let mut document = read_cron_tasks(fs.as_ref(), &cfg.cwd).await;
        let now = self.firer_platform.clock().now();
        let now_ms = now
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let Some(task) = document.tasks.iter().find(|task| task.id == task_id) else {
            return false;
        };
        if !cron_task_active(task) {
            return false;
        }
        let expected = cron::next_fire_epoch_ms_for_persisted_task(task, now);
        if expected != Some(scheduled_at_ms) || scheduled_at_ms > now_ms {
            return false;
        }
        finalize_cron_occurrence(&mut document, &task_id, now_ms);
        cron::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            &cfg.cwd,
            &cron::serialize_tasks(&document),
        )
        .await
        .is_ok()
    }

    /// Execute a persisted manual occurrence. Initial dispatch and every retry
    /// must use the same host-saved timestamp; terminal duplicates never rerun.
    pub async fn run_cron_task_now_at(
        &self,
        task_id: String,
        scheduled_at_ms: u64,
    ) -> Option<FiredCronJobDto> {
        let _transition = self.loop_transition.lock().await;
        let cfg = self.current_mobile_config().ok()?;
        let fs = self.firer_platform.filesystem();
        let task = read_cron_tasks(fs.as_ref(), &cfg.cwd)
            .await
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)?;
        // Stable manual occurrence identities require the versioned run ledger.
        if task.automation.is_none()
            || mobile_cron_schedule_error(&task.cron, task.recurring.unwrap_or(false)).is_some()
        {
            return None;
        }
        self.fire_automation_task(task_id, None, Some(scheduled_at_ms))
            .await
    }

    /// Execute a task immediately without moving its recurring/one-shot anchor.
    pub async fn run_cron_task_now(&self, task_id: String) -> Option<FiredCronJobDto> {
        let _transition = self.loop_transition.lock().await;
        let cfg = self.current_mobile_config().ok()?;
        let fs = self.firer_platform.filesystem();
        let existing = read_cron_tasks(fs.as_ref(), &cfg.cwd)
            .await
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)?;
        if existing.automation.is_some() {
            if mobile_cron_schedule_error(&existing.cron, existing.recurring.unwrap_or(false))
                .is_some()
            {
                return None;
            }
            return self.fire_automation_task(task_id, None, None).await;
        }
        let _process_guard = cron::lock_cron_file().await;
        let _file_guard = cron::tasks_file::lock_automation_tasks(fs.as_ref(), &cfg.cwd)
            .await
            .ok()?;
        let task = read_cron_tasks(fs.as_ref(), &cfg.cwd)
            .await
            .tasks
            .into_iter()
            .find(|task| task.id == task_id)?;
        let firer = MobileTurnFirer {
            cfg: cfg.clone(),
            platform: self.firer_platform.clone(),
        };
        if !cron_task_active(&task) {
            return None;
        }
        let result = firer
            .fire_session(task.prompt.clone(), task.automation.as_ref(), None)
            .await;
        let session_id = result.as_ref().ok().map(|value| value.session_id.clone());
        let mut dto = fired_cron_dto(&task, result.map(|value| value.summary));
        dto.session_id = session_id;
        Some(dto)
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl MobileEngineHandle {
    /// Inline-visualization host for a dedicated native WebView answering for
    /// `origin` (`lingxi-viz://visualization` on iOS,
    /// `https://lingxi-visualization.invalid` on Android). It shares this
    /// engine's store, so widgets the agent publishes are mountable at once.
    /// `None` for an origin that is not `scheme://host`.
    #[must_use]
    pub fn visualization_host(
        &self,
        origin: String,
    ) -> Option<Arc<crate::mobile::visualization_host::VisualizationHost>> {
        crate::mobile::visualization_host::VisualizationHost::for_config_home(
            self.fs.clone(),
            &self.lingxi_home,
            &origin,
            self.runtime().handle().clone(),
        )
        .map(Arc::new)
    }
}
#[cfg(test)]
#[path = "host/tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "host/tests/mobile_provider_allowlist_tests.rs"]
mod mobile_provider_allowlist_tests;

#[cfg(test)]
#[path = "host/tests/media_owner_cleanup_tests.rs"]
mod media_owner_cleanup_tests;

/// The Anthropic profile's exact-id registry — what `/model` (and every client
/// model picker riding `ClientEvent::ModelList`) can route under "anthropic".
#[cfg(test)]
#[path = "host/tests/anthropic_model_registry_tests.rs"]
mod anthropic_model_registry_tests;

/// `default_model` → `(request_model, profile)`, including the self-heal for a
/// reference no registered provider serves.
#[cfg(test)]
#[path = "host/tests/default_model_resolution_tests.rs"]
mod default_model_resolution_tests;

#[cfg(test)]
#[path = "host/tests/cron_automation_tests.rs"]
mod cron_automation_tests;

#[cfg(test)]
#[path = "host/tests/list_agents_tests.rs"]
mod list_agents_tests;

#[cfg(test)]
#[path = "host/tests/read_auto_allow_wiring_tests.rs"]
mod read_auto_allow_wiring_tests;
