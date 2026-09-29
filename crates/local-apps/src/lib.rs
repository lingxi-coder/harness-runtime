//! Local Apps — shared core of the on-device "Apps" capability.
//!
//! The conversation agent drives app creation (v3); this crate is the shared
//! foundation underneath it: the data model, the two-state workflow record,
//! the runtime state machine, atomic on-disk storage, git checkpoints, the
//! per-app data store, and [`AppService`] as the single source of truth.
//!
//! Layer boundaries: this crate knows nothing about the client protocol. The
//! engine maps [`events::AppEvent`]s onto client events and [`error::AppError`]s
//! onto `AppOperationFailed { code, message }`.
//!
//! Storage layout under the injected data root (all writes atomic via
//! `lingxi_core::host::rooted_fs`; every file carries `schemaVersion`):
//!
//! ```text
//! apps/index.json                          — { schemaVersion, apps: [AppRecord] }
//! apps/<app-id>/runtime.json               — AppRuntimeRecord
//! apps/<app-id>/workspace/.lingxi/app.json — app-scoped AppRecord mirror
//! ```
//!
//! Legacy pipeline documents (`interactions.json`, `design-spec.json`) from
//! pre-v3 stores are ignored on load and left on disk untouched.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 282 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod agent_sessions;
pub mod authoring;
pub mod background;
pub mod checkpoints;
pub mod data;
pub mod error;
pub mod events;
pub mod ids;
pub mod mailbox;
pub mod manifest;
pub mod mcp_authoring;
pub mod mcp_settings;
pub mod packer;
pub mod performance_thresholds;
pub mod permissions;
pub mod qa;
pub mod runtime_migration;
pub mod runtime_v2;
pub mod service;
pub mod state;
pub mod storage;
pub mod test_support;
pub mod types;

pub use agent_sessions::{
    load_agent_history, load_profile, load_sessions, save_agent_history, save_profile,
    save_sessions, upsert_session,
};
pub use authoring::{
    authoring_contract_sha256, authoring_spec_sha256, canonical_json_bytes, canonical_sha256,
    commit_staged_authoring, delete_authoring_candidate, load_authoring_candidate,
    load_authoring_contract, save_authoring_contract, stage_authoring, AcceptanceCheck,
    AcceptanceEvidence, AppAuthoringCandidate, AppAuthoringContract, AppAuthoringSpec,
    AppAuthoringStage, AppCanvasDesign, AppDesignInputs, AppDesignSpec, AppDesignStates,
    AppPresentationSpec, AppProductSpec, AppStyleSpec, AppTarget, AppThemeSpec,
    AUTHORING_SCHEMA_VERSION, MAX_AUTHORING_BYTES,
};
pub use checkpoints::AppCheckpointStore;
pub use data::{
    AppDataStore, DataFilter, DataFilterOperator, DataMigrationPreview, DataMigrationResult,
    DataMutation, DataMutationResult, DataPage, DataQuery, DataRecord, DataSchemaState,
    DataSortDirection, DataSortKey, DATA_SCHEMA_VERSION, MAX_FILTER_IN_VALUES,
    MAX_MUTATION_BATCH_SIZE, MAX_QUERY_FILTERS, MAX_QUERY_PAGE_SIZE, MAX_RECORD_ID_BYTES,
};
pub use error::{AppError, AppErrorCode};
pub use events::{
    AppEvent, AppEventFanout, AppEventObserver, AppEventSubscription, NoopAppEventObserver,
    RecordingAppEventObserver,
};
pub use ids::{
    generate_authoring_handle, generate_qa_handle, is_valid_authoring_handle, is_valid_qa_handle,
    validate_authoring_handle, validate_qa_handle, AUTHORING_HANDLE_PATTERN, QA_HANDLE_PATTERN,
};
pub use manifest::{
    derive_publication_state, hash_mcp_catalog, load_manifest, load_mcp_catalog, save_manifest,
    save_mcp_catalog, AppDependencySnapshot, AppLayout, AppManifest, AppMcpCatalogRef,
    AppPublicationState, AppRuntimeProfileBinding, AppSurface, AppTemplateOrigin,
    DataCollectionSchema, DataFieldKind, DataFieldSchema, DeviceContext,
    WORKSPACE_SETTINGS_LOCAL_FILE,
};
pub use mcp_authoring::{
    approval_contract_sha256, catalog_sha256, delete_candidate_journal,
    derive_local_app_mcp_ceiling, load_candidate_journal, materialize_flow_value_binding,
    save_candidate_journal, validate_app_mcp_flow_binding,
    validate_app_mcp_flow_binding_for_consumer, validate_app_mcp_proposal,
    validate_generated_mcp_catalog, validate_generated_structured_result, value_matches_schema,
    AppMcpFlowBinding, AppMcpFlowContext, AppMcpProposal, AppMcpToolProposal, FlowSource,
    FlowValueBinding, GeneratedMcpBudget, GeneratedMcpIssue, HostValidatedMcpTool,
    McpAuthoringStage, McpCandidateJournal, McpConfirmationReceipt, McpReceiptBook,
    ValidatedAppMcpProposal, MAX_EXPOSED_MCP_DEFINITIONS_TOKENS, MAX_EXPOSED_MCP_DEFINITIONS_UTF16,
    MAX_GENERATED_MCP_DEFINITION_TOKENS, MAX_GENERATED_MCP_DEFINITION_UTF16,
    MAX_GENERATED_MCP_SCHEMA_BYTES, MAX_GENERATED_MCP_STRUCTURED_RESULT_BYTES,
    MAX_GENERATED_MCP_TOOLS,
};
pub use mcp_settings::{
    derive_mcp_status, effective_tool_surface_sha256, load_mcp_settings, mcp_catalog_tool_names,
    save_mcp_settings, AppMcpSettings, AppMcpStatus, MCP_SETTINGS_FILE,
    MCP_SETTINGS_SCHEMA_VERSION,
};
pub use packer::{
    pack, sha256_hex, InventoryEntry, PackResult, PackedFile, PackerError, PRUNE_DIR_NAMES,
};
pub use performance_thresholds::{
    load_baseline, validate as validate_performance_thresholds, PerformanceThresholds,
    ThresholdError,
};
pub use permissions::{
    load_permissions, save_permissions, save_workspace_permission_settings, AppCapability,
    AppPermissions, PermissionDecision, SessionPermissions,
};
pub use qa::{
    load_qa_publication_marker, load_qa_receipt, load_qa_result, load_qa_session, prune_qa_history,
    publish_qa_result, qa_begin, qa_begin_with_requirements, qa_begin_with_scope, qa_cleanup_run,
    qa_cleanup_session, qa_finalize, qa_read_evidence, qa_record_host_evidence, QaArtifactRef,
    QaEvidence, QaEvidenceBlock, QaEvidenceKind, QaEvidenceSource, QaFinalizeOutput, QaFinding,
    QaHostEvidenceContent, QaHostEvidenceInput, QaIdentity, QaImageFormat,
    QaNativeTargetProvenance, QaPublicationMarker, QaReceipt, QaResult, QaResultStatus,
    QaScenarioJudgement, QaScenarioRequirement, QaScenarioStatus, QaSession, QaUpstreamFailure,
    QaVerificationScope, QaVerificationStrategy, MAX_QA_HISTORY_RESULTS, QA_SCHEMA_VERSION,
};
pub use runtime_migration::{
    delete_runtime_profile_migration_journal, load_runtime_profile_migration_journal,
    save_runtime_profile_migration_journal, RuntimeProfileMigrationEdge,
    RuntimeProfileMigrationJournal, RuntimeProfileMigrationStatus, RUNTIME_PROFILE_MIGRATION_EDGES,
};
pub use runtime_v2::{
    allowed_for_origin, allowed_for_synchronous_flow, apply_approved_profile,
    compose_prompt_layers, normalized_input_hash, AgentBudget, AgentSessionRecord,
    AgentSessionStatus, AppAgentProfile, AppAgentProfileProposal, BackgroundJournalEntry,
    BackgroundTaskRecord, BackgroundTaskStatus, BackgroundTrigger, CapabilityDescriptor,
    CapabilityId, CapabilityRegistry, CapabilityScope, CapabilityTransport, FlowDefinition,
    FlowStep, InvocationContext, InvocationFrame, InvocationOrigin, InvocationReplayGuard,
    PromptLayer, PromptLayerKind, RuntimeContractError, StreamBuffer, StreamFrame, StreamValidator,
    MAX_AGENT_MAX_BRIDGE_CALLS, MAX_AGENT_MAX_MCP_CALLS, MAX_AGENT_MAX_TOKENS, MAX_AGENT_MAX_TURNS,
    MAX_AGENT_MAX_WALL_MS, RUNTIME_API_MAJOR, RUNTIME_API_VERSION, RUNTIME_CONTRACT_SCHEMA_VERSION,
};
pub use service::{
    AppService, CreateMode, MAX_MCP_INTENT_CAPABILITIES, MAX_MCP_INTENT_CAPABILITY_NAME_BYTES,
    PLACEHOLDER_APP_NAME,
};
pub use state::{runtime_transition_allowed, AppState};
pub use types::{
    AppCheckpoint, AppCheckpointKind, AppDependencyRecord, AppDependencyState, AppMcpIntent,
    AppRecord, AppRuntimeMode, AppRuntimeProfile, AppRuntimeRecord, AppRuntimeState,
    APPS_SCHEMA_VERSION, DEFAULT_GIT_VERSION_CONTROL,
};
