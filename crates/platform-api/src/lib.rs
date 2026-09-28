//! Platform abstraction traits (`platform-api` crate).
//!
//! Library crates depend on these traits; platform crates (`platforms/posix`,
//! `platforms/windows`, etc.) implement them. The conversation state machine
//! (`core`) never imports a concrete runtime or OS API directly.
//!
//! See spec §4 (Trait System) and D17 (Runtime boundary).

// Platform filesystem primitives are safe by default. The Windows rooted-file
// implementation is the sole exception: the standard library has no
// handle-relative open/rename API, so that module wraps a small audited set of
// `ntdll` calls. Keeping this at `deny` lets the exception remain item-scoped.
#![deny(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 151 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 6 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

/// The claude-code version LingXi replicates byte-for-byte (the parity target),
/// distinct from this workspace's own `CARGO_PKG_VERSION`. claude-code embeds its
/// `VERSION` in outward-facing identifiers — the `AI_AGENT` child-env value
/// (`claude-code_2-1-267_agent`) and the WebFetch `User-Agent`
/// (`claude-code/2.1.267`). LingXi is a 1:1 copy, so it presents the same string.
/// Single source of truth (R-V1) so the AI_AGENT and User-Agent stamps never drift.
///
/// Raised 2.1.241 → 2.1.245 on 2026-08-25, once the main query-loop identity
/// (`querySource` / `print` vs non-interactive) matched the 2.1.245 binary.
/// Raised 2.1.246 → 2.1.252 on 2026-08-30 after the mcp/plugin byte-alignment
/// backlog was implemented against the newer oracle.
/// Raised 2.1.252 → 2.1.267 on 2026-09-10, after the 2.1.267 sweep: the oracle's
/// own `VERSION:"2.1.267"` sits in the metadata object that `Ma()` (User-Agent)
/// and `RMn()` (AI_AGENT) both read, and both templates are unchanged —
/// `claude-code/${…VERSION}` and `claude-code_${…VERSION.replace(/\./g,"-")}_${e}`
/// — so only the number moves. What the bump does NOT claim: the gaps the sweep
/// found are classified, not closed. Plugin surface modules and the JSX render
/// runtime are unbuilt, and `SubagentHandback` is gated FALSE upstream so its
/// absence is alignment. Those are registered divergences, not behaviour this
/// number overstates.
///
/// The bump is deliberately LAST. It is what this session tells servers and child
/// processes it is, so raising it before the behaviour matched would overstate
/// the port — and the port has been burned by the opposite error too (it once
/// advertised 2.1.217 while implementing 2.1.220), which is why every outward
/// identifier derives from this one constant — including the LSP `clientInfo`,
/// which kept its own copy until this bump and would have been the next thing to
/// drift (oracle: `clientInfo:{name:"Claude Code",version:{…}.VERSION}`).
///
/// ## 2.1.270 sweep, 2026-09-14 — HELD at 2.1.267, deliberately
///
/// The 2.1.270 backlog is now swept to the bottom: every item is implemented,
/// verified as not-a-gap, or sized against the code with its evidence recorded
/// next to the thing that makes it true. This number still does NOT move, and
/// the reason is the rule above rather than an oversight.
///
/// Implemented this round: the `!` rule negation (HP-7, 2.1.269), inline-skill
/// `disallowed-tools` (MP-1), `/output-style`'s reinstatement (CLI-1, 2.1.269)
/// with a runtime switch that actually takes effect, the `Proactive` and
/// `Concise` built-in output styles with their per-turn reminders, the 1 GiB
/// persisted-tool-result cap (TL-6, 2.1.266), and the keep-recent clear's
/// persist hook (CMP-2).
///
/// Four remain, each a subsystem rather than a patch, each sized at the oracle:
///
/// * **CLI-4** `/cost` prompt-cache reporting — needs a per-request cache
///   ledger with MISS ATTRIBUTION (diffing system prompt, tools and messages
///   across requests to name a cause), plus the `prompt_cache` status field.
/// * **CLI-5** `bashEditDiffEnabled` — a git tree-snapshot differ with a
///   per-repo failure ledger, index-lock handling, `status --porcelain=v2`
///   tree construction and `diff-tree` parsing with file/hunk caps.
/// * **HP-6** `sandbox.credentials.awsPairs` — one field of a credential-masking
///   MITM proxy that substitutes sentinels for real secrets and RE-SIGNS AWS
///   SigV4 requests; the field is meaningless without the proxy.
/// * **CMP-1** summarize / summarize-up-to — needs the rewind dispatch
///   redesigned: it currently unwinds the TUI and mutates on disk, while a
///   summarize needs a live model call over the CURRENT conversation before
///   anything unwinds.
///
/// ⇒ Raising the number now would tell servers, child processes and language
/// servers that those four exist. Under-claiming is the recoverable direction;
/// the port has been burned by the other one. Raise it WITH the last of the
/// four, and add the line saying what the sweep verified.
pub const CLAUDE_CODE_VERSION: &str = "2.1.267";

pub mod agent_name_registry;
pub mod agent_processes;
pub mod agent_view;
pub mod android_ui;
pub mod audio;
pub mod auth;
pub mod backgrounding;
pub mod bg_session_forker;
pub mod bridge;
pub mod budget;
pub mod calendar;
pub mod camera;
pub mod clipboard;
pub mod clock;
pub mod commands;
pub mod computer_control;
pub mod contacts;
pub mod coordinator_mode;
pub mod deep_link;
pub mod device_status;
pub mod display;
pub mod effect_handler;
pub mod env;
pub mod file_history_sink;
pub mod filesystem;
pub mod fork_resume_gate;
pub mod fork_subagent;
pub mod fusion;
pub mod fusion_setup;
pub mod haptics;
pub mod http;
pub mod ide;
mod live_session_words;
pub mod live_sessions;
pub mod local_app_paths;
pub mod location;
pub mod lsp;
pub mod mailbox;
pub mod mcp;
pub mod mobile_runtime_environment;
pub mod model_attempt;
pub mod model_capabilities;
pub mod notification;
pub mod observer_pairing;
pub mod orchestrator;
pub mod panel_pool;
pub mod parked_agent_store;
pub mod permission_gate;
pub mod plan_files;
pub mod plan_slug;
pub mod platform;
pub mod process;
pub mod prompting_gate;
pub mod read_auto_allow;
pub mod repo_root_reload;
#[cfg_attr(windows, allow(unsafe_code))]
pub mod rooted_fs;
pub mod runtime;
pub mod sandbox;
pub mod secure_storage;
pub mod session_flags;
pub mod session_retention;
pub mod share;
pub mod skill_loader;
pub mod stt;
pub mod subagent_output;
pub mod subagent_output_guard;
pub mod subagent_spawn;
pub mod subscription;
pub mod swarm;
pub mod tag_escape;
pub mod task_activity;
pub mod task_registry;
pub mod team_registry;
pub mod team_spawn;
pub mod teammate_worker;
pub mod tool_invoker;
pub mod traffic_mode;
pub mod tts;
pub mod uds_inbox;
pub mod voice;
pub mod web_search;
pub mod workflow_output;
pub use session_retention::{
    SessionRetentionError, SessionRetentionGate, SessionRetentionPin, SessionRetirement,
};
pub mod worktree;

pub use android_ui::{
    AndroidAccessRequest, AndroidAccessTier, AndroidAction, AndroidActionResult, AndroidAppInfo,
    AndroidAudioListenRequest, AndroidAudioSpeakRequest, AndroidAudioSpeakResult,
    AndroidAudioTranscript, AndroidAutomationError, AndroidAutomationSessionState,
    AndroidAutomationStatus, AndroidCaptureMode, AndroidGlobalAction, AndroidNodeQuery,
    AndroidRect, AndroidScreenshot, AndroidUiAutomation, AndroidUiNode, AndroidUiSnapshot,
    AndroidWaitCondition, MAX_ANDROID_AUDIO_LISTEN_MS, MAX_ANDROID_AUDIO_SPEAK_CHARS,
    MAX_ANDROID_UI_BATCH, MAX_ANDROID_UI_DEPTH, MAX_ANDROID_UI_NODES, MAX_ANDROID_UI_WAIT_MS,
};
pub use audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioInitiator, AudioOperation,
    AudioOperationContext, AudioOperationId, AudioOperationKind, AudioOperationReadiness,
    AudioOperationSuccess, AudioOwner, AudioReadinessState, AudioRecordingHandle, AudioService,
    AudioStatus,
};
pub use auth::{AuthError, AuthHandle, LoginInfo};
pub use backgrounding::{
    classify_backgrounding, BackgroundingDecision, BackgroundingSnapshot,
    DEFAULT_BACKGROUND_DEFER_MS,
};
pub use bridge::{BridgeConfig, BridgeConnection, BridgeError, BridgeTransport};
pub use budget::{
    BudgetCommitReceipt, BudgetEnforcerHandle, BudgetError, BudgetReservationId,
    BudgetSettlementReceipt,
};
pub use calendar::{CalendarError, CalendarEvent, CalendarProvider, CalendarQuery};
pub use camera::{CameraControl, CameraError, CameraPosition, CapturePhotoOpts, CapturedImage};
pub use clipboard::{Clipboard, ClipboardError};
pub use clock::Clock;
pub use commands::{SlashCommandDispatcher, SlashDispatchResult};
pub use computer_control::{ComputerControl, ComputerError, Screenshot};
pub use contacts::{Contact, ContactsError, ContactsProvider, ContactsQuery};
pub use deep_link::{DeepLinkError, DeepLinkOpener};
pub use device_status::{DeviceStatus, DeviceStatusError, DeviceStatusProvider};
pub use effect_handler::EffectHandler;
pub use file_history_sink::FileHistorySink;
pub use filesystem::{
    apply_line_window, file_content_from_prefix_bytes, FileContent, FileEvent, FileEventKind,
    FileSystem, FileSystemCacheIdentity, FlockGuard, FsError,
};
pub use fusion::{
    normalize_dimensions, panel_never_dispatched, parse_fusion_model_ref, parse_fusion_models,
    prepared_from_oneshot, validate_panel_report, DurableFusionOutboxRecord,
    DurableFusionTerminalRecord, EvidenceKind, FusionActivation, FusionAgentSurface,
    FusionAnalysis, FusionAttemptSettlementStatus, FusionCompletionSink, FusionContradiction,
    FusionCostClass, FusionDecision, FusionError, FusionExecutor, FusionInheritance,
    FusionLatencyClass, FusionModelChoice, FusionModelHints, FusionModelRef, FusionModelRole,
    FusionNeedsParentReason, FusionOrigin, FusionPreparedSummary, FusionPreset, FusionProgress,
    FusionPublicationReceipt, FusionPublicationState, FusionPublicationStatus,
    FusionRecommendation, FusionRequest, FusionResult, FusionRunControl, FusionRunFacts,
    FusionRunFactsRecorder, FusionRunId, FusionRunIdentity, FusionRunOutcome, FusionRunRecorder,
    FusionRunRecorderFactory, FusionSlashPublicationTarget, FusionStage, FusionStatus,
    FusionSubmission, FusionTerminalCapability, FusionTiming, FusionUniqueInsight, FusionUsage,
    NoopFusionCompletionSink, PanelClaim, PanelEvidence, PanelOutcome, PanelPosition, PanelReport,
    PanelRisk, PanelRunStatus, PreparedFusionRun, RiskSeverity, DEFAULT_FUSION_DIMENSIONS,
    DEFAULT_FUSION_DIMENSION_DESCRIPTIONS, FUSION_MAX_PANEL, FUSION_MIN_PANEL,
    FUSION_PANEL_POOL_CAP, FUSION_PANEL_TYPE, FUSION_SCHEMA_VERSION,
    FUSION_WORKFLOW_CALL_CAP_HARD_LIMIT,
};
pub use haptics::{HapticError, HapticService, HapticStyle};
pub use http::{
    HttpError, HttpTransport, RawByteStreamWithMeta, ResolvedAddressOverride, WebSocketConnection,
    WebSocketConnectionWithMeta, WebSocketMessageStream, WebSocketMessageStreamWithMeta,
};
pub use ide::{IdeEndpointInfo, IdeHandle, IdeStatus, IdeTransport};
pub use live_sessions::{SessionIdClaim, SessionWriterLease, SharedSessionWriterLease};
pub use location::{LocationError, LocationFix, LocationProvider};
pub use lsp::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
    NewDiagnosticsSource,
};
pub use mailbox::{
    MailboxError as RouterMailboxError, MailboxMessage, MailboxRouterHandle, RouteAck,
};
pub use mcp::*;
pub use mobile_runtime_environment::{
    MobileDeviceClass, MobileExecutionTarget, MobileHostEnvironment, MobileHostOs,
    MobileLaunchMode, MobileLifecyclePolicy, MobileNetworkPolicy, MobileRuntimeEnvironment,
    MobileToolRuntime, MOBILE_RUNTIME_ENVIRONMENT_VERSION,
};
pub use model_attempt::{
    ModelAttemptBillingMode, ModelAttemptContext, ModelAttemptContextError,
    ModelAttemptRegistrationId, ModelAttemptRun, ModelAttemptStage,
};
pub use notification::{NotificationError, NotificationRequest, NotificationService};
pub use orchestrator::{
    curated_model_listings, curated_model_names, curated_model_refs, is_curated_model,
    parse_model_ref, provider_default_model, provider_fallback_order, provider_has_curated_list,
    provider_model_catalog, provider_model_catalog_listings, qualified_model_ref,
    reasoning_control_spec_for_model, split_connection_profile,
    validated_reasoning_selection_for_model, ActiveGoalSnapshot, AgentInfo, AttachmentKind,
    CheckStatus, CompactionSummary, ConnectionRef, ContextPressureBanner, ContextPressureLevel,
    ContextUsageCategory, ContextUsageCategoryKind, ContextUsageSnapshot, ConversationControls,
    CostSnapshot, CurrentUsageSnapshot, DeferredToolReplay, DirectoryAddedHookSummary, DoctorCheck,
    DoctorReport, DoctorSummary, ForkOutcome, GoalClearedReason, GoalStatusAttachment,
    GoalStatusKind, HandleError, HookInfo, LoopUsageProvider, LoopUsageRow, McpActionState,
    McpServerInfo, McpStatus, McpToggleOutcome, MemoryEditorOutcome, ModelBillingMode,
    ModelCapabilities, ModelListing, ModelMetadata, ModelPricing, ModelPricingTier,
    ModelProvenance, ModelUsageRow, OrchestratorHandle, OutputEvent, OutputStream,
    OutputStyleListing, PermissionControlState, PermissionModeAvailability, PlanSnapshot,
    PromptSnapshot, PromptToolDescription, RateLimitSnapshot, ReasoningBudgetRange,
    ReasoningControlSpec, ReasoningSelection, RecapOutcome, RegisterRepoRootOutcome,
    RegisterRepoRootRequest, ResumeRuntimeSnapshot, RewindRowData, SkillInfo, StatusSnapshot,
    SummarizeDirection, TurnOutcome,
};
pub use panel_pool::{PanelPoolLease, PanelPoolPermit};
pub use permission_gate::{
    AutoModePrompt, PermissionDecision, PermissionDenial, PermissionGate, PermissionRequestSource,
};
pub use platform::Platform;
pub use process::{
    BackgroundExitSink, BackgroundTaskBinding, ForegroundOutcome, ForegroundRunResult,
    HookOutputObserver, HookRunOutcome, ProcessHandle, ProcessOutputFile, ProcessRunner,
};
pub use prompting_gate::{
    PermissionRequest, PromptDecision, PromptDefault, PromptError, PromptingGate,
};
pub use repo_root_reload::{RepoRootReloadOutcome, RepoRootReloadRequest, RepoRootReloader};
pub use rooted_fs::{
    atomic_write_pinned, lock_exclusive_pinned, open_read_file_pinned, sync_parent_pinned,
    truncate_file_pinned, AtomicWriteOptions, RootIdentity, RootedFileLock,
};
pub use runtime::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
pub use sandbox::{
    BackendPlanHandle, ProcessCommand, Sandbox, SandboxCapability, SandboxError, SandboxFeatures,
    SandboxPolicy, SandboxedCommand, SandboxedTag,
};
pub use secure_storage::{
    CredentialStoragePolicy, InMemorySecureStorage, SecureStorage, SecureStorageBackend,
    SecureStorageError,
};
pub use share::{ShareError, SharePayload, ShareResult, SharingService};
pub use skill_loader::{SkillLoad, SkillLoader};
pub use stt::SttTranscript;
pub use subagent_spawn::{
    StructuredOutputMode, SubagentInheritance, SubagentObservation, SubagentResult,
    SubagentSpawnError, SubagentSpawnObserver, SubagentSpawnRequest, SubagentSpawner,
    SubagentUsage, SubagentUsageRecorder, WorkflowQueryWatchdog,
};
pub use swarm::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
pub use task_registry::{
    TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
    TaskRegistryHandle, TaskUpdatePatch,
};
pub use team_registry::{TeamRegistryHandle, WorkerInfo};
pub use team_spawn::{TeamSpawnError, TeamSpawnSeam};
/// Shared cancellation handle for host-initiated interactive turns.
pub use tokio_util::sync::CancellationToken;
pub use tool_invoker::{
    SubagentInvocationContext, ToolExecutionPolicy, ToolInvoker, ToolInvokerError,
};
pub use tts::TtsAudio;
pub use voice::VoiceRecording;
pub use web_search::{WebSearchConfigProvider, WebSearchRuntimeConfig};
pub use workflow_output::{
    WorkflowOutputAccount, WorkflowOutputEventId, WorkflowOutputScope, WorkflowOutputScopes,
};
#[allow(unused_imports)]
pub use worktree::*;
pub use worktree::{WorktreeError, WorktreeHandle, WorktreeInfo, WorktreeManager};

pub mod teammate_plan;

pub mod task_notification;
mod task_notification_sanitize;

/// Shared task output layout and direct-child display parsing.
pub mod task_output;

/// Shared shell discovery and duration formatting.
pub mod shell_support;

pub mod shell_handoff;

/// Shared acknowledged shell supervision protocol.
pub mod shell_supervisor;
/// Platform-independent shell stall detector.
pub mod shell_watchdog;

pub mod human_task_message;

/// The refusal-fallback CHAIN walk (which model to try next).
///
/// Lives here rather than in `orchestrator` because both turn loops need it:
/// the main thread's, and the subagent runner's in the `agent` crate, which
/// cannot depend on `orchestrator`.
pub mod refusal_cascade;

/// The refusal-notice episode accumulator and collapse queue.
pub mod refusal_notice;

/// One refusal hop, decided identically for both turn loops.
pub mod refusal_driver;
