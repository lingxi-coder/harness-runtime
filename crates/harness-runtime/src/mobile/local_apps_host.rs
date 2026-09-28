//! Host-owned runtime, data, approval and WebView broker for local apps.
//!
//! The MCP provider deliberately has no direct filesystem, SQLite, process or
//! WebView handles.  This broker is the single trust boundary for those
//! operations and is also used by the native client command surface.

use crate::mobile::host::LocalAppBackgroundRunDto;
use crate::mobile::local_app_runtime_profiles::{toolchain_for_binding, RuntimeToolchain};
use crate::mobile::local_apps_mcp::LocalAppsMcpHost;
use crate::mobile::plan_approval::CreateApprovalAuthority;
use async_trait::async_trait;
use client_adapter::ClientEventSink;
use client_protocol::events::ClientEvent;
use client_protocol::local_apps::{
    AppAuthorizationDecisionDto, AppBridgeOperationDto, AppBridgeRequestDto, AppBridgeResponseDto,
    AppCapabilityKindDto, AppCapabilityRequestDto, AppDependencyChangeConfirmationRequestDto,
    AppDependencyChangeDto, AppDependencyChangeKindDto, AppEventDto, AppRuntimeProfileDto,
    AppSurfaceDto, AppUiActionKindDto, AppUiRequestDto, AppUiTargetDto, AppWorkflowStateDto,
    LocalAppGateStatusDto, LocalAppMcpProposalApprovalRequestDto, LocalAppMcpToolChangeKindDto,
    LocalAppMcpToolDiffDto, LocalAppMcpToolFieldDto, LocalAppMcpToolSurfaceDto,
    LocalAppPluginErrorCodeDto, LocalAppVerificationStatusDto, LocalAppVerificationSummaryDto,
    ManagedLocalAppMcpServerDto, ManagedLocalAppMcpStatusDto, McpAppWidgetDto,
};
use futures_util::StreamExt;
use local_apps::{
    derive_mcp_status, effective_tool_surface_sha256, load_manifest, load_mcp_settings,
    load_permissions, mcp_catalog_tool_names, save_mcp_settings, save_permissions, AppCapability,
    AppDataStore, AppDependencyState, AppLayout, AppMcpSettings, AppMcpStatus, AppPermissions,
    AppRuntimeMode, AppRuntimeProfile, AppRuntimeState, AppService, BackgroundTaskStatus,
    DataMigrationPreview, DataMutation, DataQuery, DataSortDirection, DataSortKey,
    PermissionDecision, SessionPermissions,
};
use mobile_linux_api::{
    LinuxCommandRequest, MobileLinuxRuntime, MountPurpose, MountSpec, NetworkPolicy, ResourceLimits,
};
use platform_api::McpError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch, Mutex, Notify, Semaphore};
use tokio::time::{sleep, timeout, Duration};
use tracing::Instrument;

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const APPROVAL_RECEIPT_TTL: Duration = Duration::from_secs(10 * 60);
const UI_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Keep a successful QA action window open briefly so a page-bridge request
/// already issued by the click handler can enter the Host after the native
/// evaluation result. Requests are then allowed a bounded time to settle.
const QA_ACTION_BRIDGE_GRACE: Duration = Duration::from_millis(100);
const QA_ACTION_BRIDGE_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_APP_WIDGET_MIME: &str = "text/html;profile=mcp-app";
const LOCAL_APP_WIDGET_FILE: &str = "mcp-app.html";
const LOCAL_APP_WIDGET_DIR: &str = "resources";
const MAX_HTTP_REQUEST_BYTES: usize = 16 * 1024;
const MAX_STATIC_ASSET_BYTES: u64 = 32 * 1024 * 1024;
const STATIC_REQUEST_CONCURRENCY: usize = 8;
const STATIC_ASSET_CHUNK_BYTES: usize = 256 * 1024;
const MAX_NETWORK_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const STATIC_ACCEPT_RETRY: Duration = Duration::from_millis(50);
const RUNTIME_SEED_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEPENDENCY_INSTALL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEPENDENCY_INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const PNPM_TOOLCHAIN_KEY: &str =
    crate::mobile::local_app_runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY;
const DEPENDENCY_SNAPSHOT_VERSION: u8 = 2;
const DEPENDENCY_SNAPSHOT_READY_FILE: &str = ".lingxi-dependency-ready";
const MAX_DEPENDENCY_SNAPSHOT_READY_BYTES: u64 = 4 * 1024;
const DEPENDENCY_SNAPSHOT_INVENTORY_FILE: &str = ".lingxi-dependency-inventory.json";
const DEPENDENCY_SNAPSHOT_INVENTORY_SCHEMA_VERSION: u8 = 1;
const MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES: usize = 4 * 1024 * 1024;
const LOCAL_APP_PERF_DIAGNOSTIC_ENV: &str = "LINGXI_LOCAL_APP_PERF_DIAGNOSTIC";
const DEPENDENCY_UPDATE_RECOVERY_FILE_REL: &str =
    ".lingxi-build-state/dependency-update-recovery.json";
const DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION: u32 = 1;
const MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES: usize = 16 * 1024 * 1024;
/// Emitted by `stage-local-app-runtime.py` beside the staged `node_modules`.
const BUNDLED_SEED_MANIFEST_FILE: &str = "runtime-manifest.json";
const WORKSPACE_DEPENDENCY_ATTESTATION_FILE: &str = ".lingxi-build-state/dependency-attestation";
/// Consecutive `accept()` failures that retire the static server.  A burst of
/// ECONNABORTED/EMFILE must not, so the cap is deliberately generous
/// (100 * 50 ms ~= 5 s of an unbroken failure); a listener whose I/O driver is
/// gone fails EVERY poll and reaches it immediately.
const STATIC_ACCEPT_ERROR_LIMIT: u32 = 100;
/// First port of the window an app's PERMANENT loopback port is drawn from, and
/// the window's length — see `bind_stable_loopback` for why it has to sit below
/// every shipped platform's ephemeral floor.
const APP_PORT_WINDOW_FIRST: u16 = 20_000;
const APP_PORT_WINDOW_LEN: u16 = 12_000;
const LOCAL_APP_BRIDGE_CONTROL_BYTES: usize = 64 * 1024;
const LOCAL_APP_BRIDGE_LLM_BYTES: usize = 8 * 1024 * 1024;
/// File writes travel as JSON with a base64 body. The decoded file cap is
/// [`files_ops::MAX_APP_FILE_BYTES`]; the wire envelope must be large enough
/// for the 4/3 expansion plus a small JSON wrapper.
const LOCAL_APP_BRIDGE_FILE_BYTES: usize = files_ops::MAX_APP_FILE_BYTES.div_ceil(3) * 4 + 1024;
const FLOW_EXECUTION_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const FLOW_STEP_TIMEOUT: Duration = Duration::from_secs(60);
const MCP_FLOW_EXECUTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

// Generation optimization Host extensions live in a child module so the
// existing broker remains the owner of WebView/data/runtime handles while the
// contract and QA journals stay private to the Host.  The MCP trait wiring is
// intentionally left to local_apps_mcp.rs' owner.
#[path = "local_apps_host/authoring.rs"]
mod authoring;
pub(crate) use authoring::PreparedWorkflowQaPublication;

/// The plan-driven create/modify preparation. See [`prepare`].
#[path = "local_apps_prepare.rs"]
pub(crate) mod prepare;

/// Availability of the exact profile dependency lock on this host. The
/// selector distinguishes a reusable shared snapshot from a device-bundled
/// seed; both avoid a network download but carry different provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeProfileDependencyAvailability {
    Cached,
    Bundled,
    DownloadRequired,
}

impl RuntimeProfileDependencyAvailability {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Bundled => "bundled",
            Self::DownloadRequired => "download_required",
        }
    }
}

static LOCAL_APP_BUILD_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
static LOCAL_APP_PERF_DIAGNOSTIC_ENABLED: OnceLock<bool> = OnceLock::new();
/// The policy every local app is served.
///
/// `worker-src 'self' blob:` and `script-src … 'wasm-unsafe-eval'` are DEFAULTS,
/// not a grant, because gating them would have been incoherent: this policy
/// already carries `'unsafe-inline'`, so the page can run any JavaScript it
/// shipped. WebAssembly is strictly WEAKER than that — no DOM, no network, no
/// files, only arithmetic and the imports the page hands it — and a `blob:`
/// worker runs the same same-origin JavaScript the page could have run on the
/// main thread. Charging a permission prompt for a capability the page already
/// exceeds buys nothing, and the failure mode when the generator forgets to ask
/// for it is bad: the app builds, then fails at runtime with a CSP refusal that
/// `inspect_ui` cannot see because the surface is a canvas.
///
/// The real boundary is elsewhere and unchanged: `default-src 'self'` plus
/// `connect-src 'self'` keep the page from loading or exfiltrating anything,
/// and every device/host power is gated per capability at the bridge.
///
/// Measured on device 2026-08-21: under the previous `worker-src 'none'` WebKit
/// rejected a worker with "The operation is insecure.", and without
/// `'wasm-unsafe-eval'` it rejected WebAssembly with "Refused to create a
/// WebAssembly object…" — both blocks were real, not theoretical.
const LOCAL_APP_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; media-src 'self' data: blob:; worker-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

#[derive(Debug)]
struct UiResolution {
    decision: AppAuthorizationDecisionDto,
    result_json: Option<String>,
    error: Option<String>,
}

#[derive(Debug)]
struct PendingNativeApproval {
    app_id: String,
    sender: oneshot::Sender<bool>,
    /// The exact request event this approval was announced with.
    ///
    /// r3-failure-paths-02: the sheet was announced ONCE. An Android client
    /// whose Activity is destroyed while the engine is retained headlessly
    /// (rotation, a low-memory kill, the user leaving the app during the five
    /// minutes `APPROVAL_TIMEOUT` allows) lost the sheet with no way to get it
    /// back: nothing re-emitted it and nothing could be asked for it, so the
    /// engine blocked until it timed out and failed the whole workflow. Keep
    /// the request so a reattaching client can be handed it again, unchanged
    /// and with the same `request_id` its answer must carry.
    event: AppEventDto,
}

#[derive(Clone, Debug)]
struct PendingDependencyChangeReceipt {
    receipt_id: String,
    app_id: String,
    baseline: DependencyBaselineIdentity,
    requested_json: Vec<u8>,
    effective_package_json: Vec<u8>,
    issued_at_ms: u64,
    expires_at_ms: u64,
    summary: Vec<DependencyChange>,
    claimed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedMcpCandidate {
    validated: local_apps::ValidatedAppMcpProposal,
    approval_contract_sha256: String,
    review_surface: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verification_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catalog_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    qa_context_sha256: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug)]
struct CreateProposalContext {
    selection: crate::mobile::local_app_template_catalog::ValidatedTemplateSelection,
    staging_evidence: Value,
    design_spec: Option<Value>,
    design_spec_sha256: Option<String>,
    contexts: BTreeMap<String, local_apps::AppMcpFlowContext>,
    /// The display name and brief the user confirmed through `LocalAppStageCreate`
    /// (WP5). This is the ONLY name/brief source the native create confirmation
    /// sheet and the scaffold commit are allowed to render or persist — never
    /// the empty shell `AppRecord.name`/`.brief`, which stays the `untitled`
    /// placeholder until `commit_scaffold` runs.
    name: String,
    brief: String,
    /// Outcome of the create-time MCP interview, staged through
    /// `LocalAppStageCreate` alongside `name`/`brief`. `None` means the
    /// interview was skipped or the app predates it — NOT that the user
    /// declined. See [`local_apps::AppMcpIntent`].
    ///
    /// ⚠️ Unlike `name`/`brief` this is DELIBERATELY outside the native create
    /// confirmation sheet's rendered set: it grants nothing (create still runs
    /// no MCP authoring — `create_without_mcp` stays the only create-time MCP
    /// path), so there is no authorization for the user to answer for here. It
    /// is a note-to-self carried onto the record for the Settings MCP flow to
    /// read later. The receipt's "the user already answered for these bytes"
    /// claim (see `stage_create_after_approval_is_refused_and_cannot_rewrite_the_approved_bytes`) covers the
    /// bytes the sheet renders; the moment this intent starts GRANTING
    /// anything, it must be rendered on that sheet before it is committed.
    mcp_intent: Option<local_apps::AppMcpIntent>,
}

#[derive(Clone, Debug)]
struct CreateScaffoldSeed {
    selection: crate::mobile::local_app_template_catalog::ValidatedTemplateSelection,
    template_root: PathBuf,
    contexts: BTreeMap<String, local_apps::AppMcpFlowContext>,
    /// Staged through `LocalAppStageCreate`; authoritative over whatever name/brief
    /// the model echoes back into `LocalAppScaffold`. See `CreateProposalContext`.
    name: String,
    brief: String,
    /// See `CreateProposalContext::mcp_intent`.
    mcp_intent: Option<local_apps::AppMcpIntent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundMcpFlowMode {
    Live,
    Qa,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoundMcpStepEvidence {
    step_id: String,
    capability: String,
    input_sha256: String,
    output_sha256: String,
}

#[derive(Clone, Debug)]
struct BoundMcpExecution {
    result: Value,
    step_calls: Vec<BoundMcpStepEvidence>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QaToolExecutionEvidence {
    tool_name: String,
    flow_id: String,
    context_sha256: String,
    input_sha256: String,
    result_sha256: String,
    step_calls: Vec<BoundMcpStepEvidence>,
}

#[derive(Clone, Debug)]
struct QaInFlightAction {
    qa_handle: String,
    scenario_id: String,
    target_id: String,
    event_id: String,
    /// Results of page-bridge mutations observed while the native action is
    /// pending. The datastore write itself is deliberately performed before
    /// this buffer is populated; only the Host evidence attribution waits for
    /// native success.
    pending_bridge_results: Vec<Value>,
}

#[derive(Clone, Debug)]
struct QaActiveActionState {
    event_id: String,
    bridge_requests_in_flight: usize,
    bridge_last_activity: tokio::time::Instant,
    bridge_settled: Arc<Notify>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DependencyBaselineIdentity {
    dependency_snapshot_sha256: String,
    requested_sha256: String,
    package_sha256: String,
    lockfile_sha256: String,
    toolchain_key: String,
    contract_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DependencyChangeKind {
    Add,
    Update,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DependencyChange {
    kind: DependencyChangeKind,
    package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

fn dependency_change_kind_dto(kind: &DependencyChangeKind) -> AppDependencyChangeKindDto {
    match kind {
        DependencyChangeKind::Add => AppDependencyChangeKindDto::Add,
        DependencyChangeKind::Update => AppDependencyChangeKindDto::Update,
        DependencyChangeKind::Remove => AppDependencyChangeKindDto::Remove,
    }
}

fn dependency_change_cache_status(kind: &DependencyChangeKind) -> String {
    match kind {
        DependencyChangeKind::Remove => "not_needed".into(),
        DependencyChangeKind::Add | DependencyChangeKind::Update => {
            // A ready app tree says nothing about whether this particular
            // package/version is in the pnpm store.  Do not inspect or mutate
            // that store before approval; expose an honest unknown status.
            "unknown_until_resolution".into()
        }
    }
}

fn value_sha256(value: &Value) -> Result<String, String> {
    local_apps::approval_contract_sha256(value.clone()).map_err(|issue| issue.message)
}

fn minimal_schema_witness(schema: &Value) -> Result<Value, String> {
    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        return enum_values
            .first()
            .cloned()
            .ok_or_else(|| "schema witness requires a non-empty enum".to_string());
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let properties = schema.get("properties").and_then(Value::as_object);
            let mut object = Map::new();
            for key in required {
                let key = key
                    .as_str()
                    .ok_or_else(|| "schema witness required keys must be strings".to_string())?;
                let child = properties.and_then(|properties| properties.get(key));
                let value = match child {
                    Some(child) => minimal_schema_witness(child)?,
                    None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                        return Err(format!(
                            "schema witness cannot satisfy closed object field {key:?}"
                        ));
                    }
                    None => Value::Null,
                };
                object.insert(key.to_string(), value);
            }
            Ok(Value::Object(object))
        }
        Some("array") => Ok(Value::Array(Vec::new())),
        Some("string") => Ok(Value::String(String::new())),
        Some("number") => Ok(json!(0)),
        Some("integer") => Ok(json!(0)),
        Some("boolean") => Ok(Value::Bool(false)),
        Some("null") => Ok(Value::Null),
        Some(other) => Err(format!("schema witness does not support type {other}")),
        None => Ok(Value::Null),
    }
}

fn active_mcp_flow_contexts_bytes(
    app_id: &str,
    contexts: &BTreeMap<String, local_apps::AppMcpFlowContext>,
) -> Result<Vec<u8>, String> {
    let active = contexts
        .iter()
        .map(|(flow_id, context)| {
            if context.app_id != app_id {
                return Err(format!(
                    "create_staging_invalid: staged MCP Flow {flow_id} belongs to {} instead of {app_id}",
                    context.app_id
                ));
            }
            let mut active = context.clone();
            active.source = local_apps::FlowSource::Active;
            Ok((flow_id.clone(), active))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    serde_json::to_vec_pretty(&active)
        .map_err(|error| format!("serialize active MCP flow contexts: {error}"))
}

fn persist_active_mcp_flow_contexts(
    workspace: &Path,
    app_id: &str,
    contexts: &BTreeMap<String, local_apps::AppMcpFlowContext>,
) -> Result<(), String> {
    let root = workspace.join(".lingxi");
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create active MCP flow context directory: {error}"))?;
    let path = root.join("mcp-flow-contexts.json");
    let temp_path = root.join("mcp-flow-contexts.json.tmp");
    std::fs::write(
        &temp_path,
        active_mcp_flow_contexts_bytes(app_id, contexts)?,
    )
    .map_err(|error| format!("write active MCP flow contexts: {error}"))?;
    std::fs::rename(&temp_path, &path)
        .map_err(|error| format!("commit active MCP flow contexts: {error}"))
}

struct DependencyInstallCompletion {
    lockfile_sha256: String,
    toolchain_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledDependencyPackage {
    name: String,
    version: String,
    license: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedDependencyInventory {
    schema_version: u8,
    toolchain_key: String,
    lock_digest: String,
    tree_digest: String,
    inventory_digest: String,
    packages: Vec<InstalledDependencyPackage>,
}

/// Opt-in device-visible timing for Local App work. Mobile installs do not
/// install a tracing subscriber, so spans alone are not observable there.
/// Phase names are fixed Host strings: never place app IDs, paths, package
/// names, or other user-controlled values in this diagnostic.
pub(crate) struct LocalAppPerfDiagnosticTimer {
    phase: &'static str,
    started: Option<std::time::Instant>,
}

impl LocalAppPerfDiagnosticTimer {
    pub(crate) fn start(phase: &'static str) -> Self {
        let enabled = *LOCAL_APP_PERF_DIAGNOSTIC_ENABLED
            .get_or_init(|| std::env::var_os(LOCAL_APP_PERF_DIAGNOSTIC_ENV).is_some());
        Self {
            phase,
            started: enabled.then(std::time::Instant::now),
        }
    }
}

impl Drop for LocalAppPerfDiagnosticTimer {
    fn drop(&mut self) {
        if let Some(started) = self.started.as_ref() {
            if let Some(line) = local_app_perf_diagnostic_line(true, self.phase, started.elapsed())
            {
                eprintln!("{line}");
            }
        }
    }
}

fn local_app_perf_diagnostic_line(
    enabled: bool,
    phase: &'static str,
    elapsed: Duration,
) -> Option<String> {
    enabled.then(|| {
        format!(
            "[local-app-perf] phase={phase} elapsed_us={}",
            elapsed.as_micros()
        )
    })
}

#[derive(Debug)]
struct DependencyUpdateFileBackup {
    relative: &'static str,
    bytes: Option<Vec<u8>>,
}

#[derive(Debug)]
struct DependencyUpdateRollback {
    previous_dependency: local_apps::AppDependencyRecord,
    files: Vec<DependencyUpdateFileBackup>,
    manifest_bytes: Vec<u8>,
    node_modules_backup: Option<PathBuf>,
    build_backup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DependencyUpdateRecoveryStatus {
    InProgress,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DependencyUpdateRecoveryFile {
    relative: String,
    bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DependencyUpdateRecoveryJournal {
    schema_version: u32,
    app_id: String,
    status: DependencyUpdateRecoveryStatus,
    previous_dependency: local_apps::AppDependencyRecord,
    files: Vec<DependencyUpdateRecoveryFile>,
    manifest_bytes: Vec<u8>,
    node_modules_backup: Option<String>,
    build_backup: Option<String>,
}

enum RuntimeHandle {
    Static { shutdown: oneshot::Sender<()> },
}

pub(super) struct PendingAppProfileProposal {
    pub(super) proposal: local_apps::AppAgentProfileProposal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RuntimeStartStatus {
    Pending,
    Running,
    Failed(String),
}

enum RuntimeEntryState {
    Starting {
        gate: watch::Sender<RuntimeStartStatus>,
    },
    Running {
        handle: RuntimeHandle,
    },
}

struct RuntimeEntry {
    state: RuntimeEntryState,
    last_used: u64,
    generation: u64,
    /// Build provenance selected when this runtime start was reserved.  The
    /// port is intentionally stable for IndexedDB origin continuity, so QA
    /// uses this identity in addition to the generation counter.
    build_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimePublicationIdentity {
    generation: u64,
    build_id: String,
}

type RuntimePublicationCell = Arc<RwLock<Option<RuntimePublicationIdentity>>>;

/// Releases a `Starting` reservation whose owner never resolved it.
///
/// `start_reserved_runtime` can leave through `?` on a persist error and can be
/// dropped outright when the foreign caller cancels `submit`.  Without this the
/// entry — and the gate every later `StartApp` subscribes to — stays in the map
/// for the process's life, hanging every subsequent start; on iOS, where the
/// instance quota is 1, that bricks the whole local-app surface.
struct RuntimeReservation {
    runtimes: Arc<Mutex<HashMap<String, RuntimeEntry>>>,
    app_id: String,
    generation: u64,
}

impl RuntimeReservation {
    fn abandon(runtimes: &mut HashMap<String, RuntimeEntry>, app_id: &str, generation: u64) {
        let still_reserved = runtimes.get(app_id).is_some_and(|entry| {
            entry.generation == generation
                && matches!(entry.state, RuntimeEntryState::Starting { .. })
        });
        if !still_reserved {
            return;
        }
        if let Some(RuntimeEntry {
            state: RuntimeEntryState::Starting { gate },
            ..
        }) = runtimes.remove(app_id)
        {
            let _ = gate.send(RuntimeStartStatus::Failed(
                "runtime start was abandoned".into(),
            ));
        }
    }
}

impl Drop for RuntimeReservation {
    fn drop(&mut self) {
        // Only fires while the entry is STILL our `Starting` reservation, so the
        // success path (state replaced with `Running`) and every
        // `fail_reserved_runtime_start` path are no-ops — there is nothing to
        // commit explicitly.
        if let Ok(mut runtimes) = self.runtimes.try_lock() {
            Self::abandon(&mut runtimes, &self.app_id, self.generation);
            return;
        }
        let runtimes = Arc::clone(&self.runtimes);
        let app_id = std::mem::take(&mut self.app_id);
        let generation = self.generation;
        crate::mobile::local_apps_profile::worker_runtime().spawn(async move {
            Self::abandon(&mut *runtimes.lock().await, &app_id, generation);
        });
    }
}

/// Loopback ports an in-flight start has CHOSEN but has not yet persisted as
/// its app's pin, each paired with the app holding it.
///
/// `sibling_pinned_ports` reads the RECORDS, and a record only learns its port
/// when `update_runtime_record` writes it — two persists and, on the full
/// runtime, a deliberate ~11 ms after `bind_stable_loopback` picked it.  (That
/// distance is what keeps the next binder out of the kernel's 1.2-2.8 ms
/// refusal window after the probe listener closes; it must not be shortened.)
/// For that whole stretch the port sits in NO snapshot a sibling can read: a
/// concurrently-starting app derives or scans to the same port, binds it
/// cleanly because the probe is already gone, and pins it too.  `set_runtime`
/// then refuses to move either pin, so neither app can run while the other
/// does — and on Android the two share one `http://127.0.0.1:<port>` origin's
/// `localStorage` / `IndexedDB`.
///
/// A lease closes that stretch without closing the window: it is taken at the
/// instant a candidate is chosen and released only once the pin is durable, so
/// at any single INSTANT "persisted pins UNION live leases" names every port an
/// in-flight start owns.
///
/// Reading that union is NOT one instant, and the difference is the whole of
/// the subtlety here.  An allocator reads the pins first and takes its lease
/// second, so a sibling can persist its pin and release its lease entirely
/// between those two steps: the sibling's port is missing from the pin half
/// (read too early) and missing from the lease half (sampled too late), even
/// though neither half was ever wrong on its own.  A sample of a union is not
/// a sample of an instant.
///
/// What makes the sample sound is the ORDER those halves are consulted in,
/// plus a SECOND pin read taken after the lease (`bind_stable_loopback`).  A
/// lease is released only once the pin it covers is durable, so once we hold
/// the lease on a candidate, any sibling that could have chosen it either
/// still holds its own lease — in which case our take already failed — or has
/// already made its pin visible to that second read.  There is no third state,
/// and no sibling can newly choose the port while we hold it.  It needs no new
/// on-disk format — the records stay the registry, and this covers only the
/// gap before a record has the answer.
type PortLeases = Arc<std::sync::Mutex<HashMap<u16, String>>>;

/// A panic while choosing a port must not brick every later start, so the
/// poison is discarded rather than propagated: the map is a set of live
/// reservations, and a half-written insert cannot corrupt it.
fn lock_port_leases(leases: &PortLeases) -> std::sync::MutexGuard<'_, HashMap<u16, String>> {
    leases
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Releases a leased port whose start never persisted it.
///
/// Same lifetime shape as [`RuntimeReservation`], for the same reason: the
/// stretch it covers is crossed by `?` on two persist failures, by every
/// `fail_reserved_runtime_start` bail-out, by a panic, and by the foreign
/// caller cancelling `submit` outright.  A port leaked on any of those is a
/// port no app in the profile can ever use again for the life of the process.
///
/// Unlike `RuntimeReservation` this holds a std mutex, so `drop` completes
/// synchronously on whichever runtime the guard dies on — the guard is minted
/// on the worker runtime and dropped on the ambient one.
#[derive(Debug)]
struct PortLease {
    leases: PortLeases,
    app_id: String,
    port: u16,
    released: bool,
}

impl PortLease {
    /// Takes `port` for `app_id`, or `None` when another in-flight start
    /// already holds it.  Test-and-insert under one lock: two allocators
    /// racing on the same candidate cannot both come away with it.
    fn take(leases: &PortLeases, app_id: &str, port: u16) -> Option<Self> {
        {
            let mut held = lock_port_leases(leases);
            if held.contains_key(&port) {
                return None;
            }
            held.insert(port, app_id.to_string());
        }
        Some(Self {
            leases: Arc::clone(leases),
            app_id: app_id.to_string(),
            port,
            released: false,
        })
    }

    /// Hand-off point: the pin is now in the app's record, so
    /// `sibling_pinned_ports` sees the port and the lease is redundant.
    ///
    /// Releasing is the same operation `drop` performs — what `commit` buys is
    /// the ORDER.  It must be called after the persist and nowhere else: a
    /// release taken before it re-opens exactly the stretch this type exists
    /// to cover.
    fn commit(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut held = lock_port_leases(&self.leases);
        // Only while the entry is STILL ours, mirroring
        // `RuntimeReservation::abandon`'s generation check: a late drop must
        // never hand away a port some other start has since leased.
        if held
            .get(&self.port)
            .is_some_and(|owner| owner == &self.app_id)
        {
            held.remove(&self.port);
        }
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// One app's in-flight `LocalAppScaffold` slot — §C.1 step 1's reservation.
///
/// Taken before validation and held until the transaction leaves by ANY path,
/// including a panic, because `Drop` is what releases it. Straight-line
/// cleanup after the awaits is not enough: the transaction's future is dropped
/// whenever the connection is torn down mid-call, while the broker outlives it
/// in the process-wide profile cache, and a leaked slot would make every later
/// scaffold of that app answer `scaffold_in_flight` for the life of the
/// process — bricking the very draft the reservation exists to protect.
///
/// It excludes a second `LocalAppScaffold` for the same app and NOTHING else.
/// A concurrent `DeleteApp` is excluded by `storage::lock_app_build`, which
/// the transaction holds across the landing and the commit.
struct ScaffoldReservation {
    app_id: String,
    slots: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl ScaffoldReservation {
    /// Reserve `app_id`, or refuse because another scaffold already holds it.
    ///
    /// A poisoned mutex is RECOVERED rather than propagated: the only code
    /// that ever holds this lock is the insert here and the remove in `Drop`,
    /// so poisoning can only have come from a panic elsewhere in the process,
    /// and treating it as "no app can ever be scaffolded again" would be a
    /// worse failure than the one that poisoned it.
    fn take(
        slots: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        app_id: &str,
    ) -> Result<Self, String> {
        let mut held = slots.lock().unwrap_or_else(|error| error.into_inner());
        if !held.insert(app_id.to_string()) {
            return Err(format!(
                "scaffold_in_flight: app {app_id} already has a scaffold in progress"
            ));
        }
        drop(held);
        Ok(Self {
            app_id: app_id.to_string(),
            slots: Arc::clone(slots),
        })
    }
}

impl Drop for ScaffoldReservation {
    fn drop(&mut self) {
        self.slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.app_id);
    }
}

/// One claimed [`local_apps::McpConfirmationReceipt`] slot, held from the
/// moment `claim_candidate` succeeds until the scaffold transaction leaves by
/// ANY path — an ordinary `Err`, a panic, or the future being dropped (a Stop,
/// a 529 non-streaming fallback, connection teardown). Before this guard
/// existed, only the explicit `Err` arm called `release_claim`, so every OTHER
/// exit left `McpReceiptBook::issue`'s in-use predicate tripped for the app
/// until the process restarted — the same class of leak `ScaffoldReservation`
/// exists to prevent, one level down.
///
/// `McpConfirmationReceipt::release_claim` is already a no-op once the
/// receipt is `consumed`, so this guard drops harmlessly after a successful
/// `commit_claimed_candidate`: it needs no separate hand-off method the way
/// [`PortLease::commit`] does. That rests entirely on `release_claim` being a
/// no-op once `consumed` — if that ever stops being true, this guard needs an
/// explicit "defused" flag set at the commit point.
struct ReceiptClaim {
    book: Arc<std::sync::Mutex<local_apps::McpReceiptBook>>,
    receipt_id: String,
}

impl ReceiptClaim {
    /// Wrap an ALREADY-CLAIMED receipt id. Does not itself call
    /// `claim_candidate` — the caller does that first, under the same lock,
    /// so a failed claim never produces a guard with nothing to release.
    fn held(book: Arc<std::sync::Mutex<local_apps::McpReceiptBook>>, receipt_id: String) -> Self {
        Self { book, receipt_id }
    }
}

impl Drop for ReceiptClaim {
    fn drop(&mut self) {
        self.book
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .release_claim(&self.receipt_id);
    }
}

/// r4-failure-paths-06: guards a create-only MCP candidate's durable state
/// (candidate journal + candidate file) across the native-approval await in
/// the create branch of `approve_mcp_proposal`.
///
/// Constructed once that state is durable, and defused with [`Self::keep`]
/// on the ONE outcome that must survive it (approval granted). Every other
/// exit — an explicit deny, an error, or the request's future being dropped
/// (a Stop, a 529 non-streaming fallback, connection teardown) — must not
/// leave a `Prepared` candidate on disk. Before this guard existed, both
/// `delete_mcp_candidate_state` calls lived only in the `Ok(false)`/`Err`
/// match arms on `request_mcp_candidate_approval`'s result, which a dropped
/// future never reaches, so a Stop during the (up to five-minute) native
/// confirmation wait leaked the journal and candidate file for the life of
/// the process. `delete_mcp_candidate_state` is documented idempotent
/// ("crash-recovery cleanup"), so this guard running again after either
/// manual cleanup arm is harmless.
struct McpCreateCandidateGuard<'a> {
    broker: &'a LocalAppsHostBroker,
    layout: AppLayout,
    app_id: String,
    workflow_run_id: String,
    keep: bool,
}

impl McpCreateCandidateGuard<'_> {
    /// The candidate was approved: its durable state must survive this guard.
    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for McpCreateCandidateGuard<'_> {
    fn drop(&mut self) {
        if !self.keep {
            // Best-effort: Drop cannot propagate an error to a caller that no
            // longer exists, and the ordinary paths already surface (or
            // idempotently repeat) this same cleanup with a real error.
            let _ = self.broker.delete_mcp_candidate_state(
                &self.layout,
                &self.app_id,
                &self.workflow_run_id,
            );
        }
    }
}

/// One entry in `pending_dependency_change_confirmations`, held across the
/// native dependency-review await in `confirm_dependency_change`.
///
/// `confirm_dependency_change` only became MODEL-callable when its
/// `LOCAL_APP_TOOLS` row landed (r2-never-wired-02), and
/// `local_apps_tools.rs` gives every operation except `create`/`scaffold`
/// `InterruptBehavior::Cancel` — so an ESC during the (up to five-minute)
/// review sheet DROPS this future. The explicit `remove` calls live only in
/// the cancelled/timed-out match arms on the `timeout(..)` result, which a
/// dropped future never reaches, so before this guard a Stop orphaned the
/// entry for the life of the broker and the user's later tap resolved into a
/// receiver nobody was holding.
///
/// The approved path needs no defusing: `resolve_dependency_change_confirmation`
/// takes the entry out of the map itself before sending, and `HashMap::remove`
/// on an absent key is a no-op — so this guard is idempotent with every arm.
struct PendingDependencyConfirmationGuard<'a> {
    pending: &'a Mutex<HashMap<String, oneshot::Sender<bool>>>,
    request_id: String,
}

impl Drop for PendingDependencyConfirmationGuard<'_> {
    fn drop(&mut self) {
        // `try_lock` first for the same reason as `RuntimeReservation::drop`:
        // `Drop` cannot await. Unlike the runtime map there is no owned handle
        // to hand a spawned task here, but this mutex is only ever held for a
        // single `insert` or `remove` with no await in between, so a failed
        // `try_lock` needs a collision inside a few instructions.
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.remove(&self.request_id);
        }
    }
}

// The `device.*` operations of the bridge — capture / pick / record / locate
// / notify. A CHILD module (not a sibling) so it reaches the broker's private
// fields and `authorize_declared_capability` without widening their
// visibility; split out purely for size. The dispatch match stays here.
#[path = "local_apps_host_device.rs"]
mod device_ops;

// `files.read` / `files.write` — app-private file store operations.
#[path = "local_apps_host_files.rs"]
mod files_ops;

// `llm.chat` — the app-initiated model call. A child module for the same
// reason as `device_ops`.
#[path = "local_apps_host_llm.rs"]
mod llm_ops;

// `agent.post` — the app-to-conversation mailbox write.
#[path = "local_apps_host_agent.rs"]
mod agent_ops;
pub(crate) use agent_ops::{
    AgentOutputRouter, AgentOutputStream, AgentTurnControl, AgentTurnUsageState,
    LocalAppsAgentExecutor,
};

#[path = "local_apps_host_background.rs"]
mod background_ops;

/// A bridge failure: human-readable message plus an optional stable machine
/// code the page can branch on (`AppBridgeResponseDto::error_code`). Every
/// legacy `Result<_, String>` site lowers through `From<String>` into a
/// code-less failure; only paths that deliberately publish a contract code
/// construct one with [`BridgeFailure::coded`].
#[derive(Debug)]
pub(crate) struct BridgeFailure {
    code: Option<&'static str>,
    message: String,
}

impl BridgeFailure {
    pub(crate) fn coded(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
        }
    }
}

impl From<String> for BridgeFailure {
    fn from(message: String) -> Self {
        Self {
            code: None,
            message,
        }
    }
}

impl From<&str> for BridgeFailure {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

/// ONE canonical spelling for a session-catalog cwd key. `canonicalize`
/// collapses the platform's symlink split (`/var` vs `/private/var` on
/// iOS/macOS), so mint, listing, resume and the cwd gates all derive the SAME
/// sanitized `projects/` directory.
///
/// r1-engine-core-010: a bare `canonicalize(path).unwrap_or(raw)` disagrees
/// with itself across a path's lifetime — mint runs while the workspace
/// directory still exists (canonicalizes to e.g. `/private/var/...`), but a
/// later cleanup can run after that directory is gone, where `canonicalize`
/// fails outright and the raw fallback (`/var/...`) spells a DIFFERENT
/// catalog directory than the one mint wrote into, so the cleanup's
/// `remove_file` silently misses. Walk up to the nearest surviving ancestor,
/// canonicalize THAT, and reappend the removed suffix — this reproduces the
/// same spelling `canonicalize` would have produced while the leaf still
/// existed, so mint and a post-deletion cleanup always agree.
pub(crate) fn canonical_cwd_string(path: &std::path::Path) -> String {
    let mut removed_suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                for name in removed_suffix.into_iter().rev() {
                    canonical.push(name);
                }
                return canonical.to_string_lossy().to_string();
            }
            Err(_) => match ancestor.file_name().map(std::ffi::OsString::from) {
                Some(name) => {
                    removed_suffix.push(name);
                    if !ancestor.pop() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    path.to_string_lossy().to_string()
}

/// Delete a session file this host minted into an app's workspace catalog.
/// Used by both create paths when `set_init_session` refuses their id — the
/// set-once pin is the arbiter, and the loser's file would otherwise linger as
/// a phantom conversation row in the app's session list.
pub(crate) fn remove_app_session_file(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    record: &local_apps::AppRecord,
    session_id: &str,
) -> bool {
    std::fs::remove_file(app_session_file(lingxi_home, data_root, record, session_id)).is_ok()
}

/// The app's whole session CATALOG directory — `<lingxi_home>/projects/<dir>`,
/// where `<dir>` is the sanitized spelling of the app workspace's canonical
/// cwd.
///
/// This directory lives OUTSIDE the app's own `apps/<id>` tree, so
/// `AppService::delete_app` cannot reach it: every transcript the host minted
/// for the app — including a chat-origin app's full FORK of the user's
/// conversation — survives the app unless a caller removes this directory
/// explicitly (`handle_delete_app` does).
///
/// One derivation, three callers ([`remove_app_session_file`],
/// [`app_session_file`] and the delete path), so none of them can disagree
/// about which directory is the app's catalog. `canonical_cwd_string`'s
/// ancestor walk means this stays the SAME spelling after the workspace has
/// been deleted, which is exactly the state the delete path reads it in.
pub(crate) fn app_session_dir(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    record: &local_apps::AppRecord,
) -> std::path::PathBuf {
    let workspace_cwd = canonical_cwd_string(&data_root.join(&record.workspace_rel));
    lingxi_home
        .join("projects")
        .join(session::jsonl::path::project_dir_name(&workspace_cwd))
}

/// Where a session this host minted for an app lives on disk. The one spelling
/// [`remove_app_session_file`] and [`reconcile_app_init_session_title`] both
/// derive their path from, so they can never disagree about which file is the
/// app's pinned init session.
fn app_session_file(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    record: &local_apps::AppRecord,
    session_id: &str,
) -> std::path::PathBuf {
    app_session_dir(lingxi_home, data_root, record).join(format!("{session_id}.jsonl"))
}

/// The session-catalog facts the `LocalAppScaffold` commit point needs in order
/// to rename an app's pinned init session: where transcripts live
/// (`<lingxi_home>/projects/…`) and the filesystem that reads and appends them.
///
/// Attached by the engine builder, which owns both. `self.root` is already the
/// apps data root, so `lingxi_home` is the only path the broker is missing —
/// and it is deliberately passed rather than re-derived from `root`, because
/// `mobile_apps_data_root` degrades to `cwd` when `lingxi_home` has no usable
/// parent, and inverting that guess would point the rename at the wrong
/// catalog on exactly the configuration that already went wrong.
#[derive(Clone)]
pub(crate) struct SessionCatalog {
    /// The engine's per-profile data dir — `projects/` hangs off it.
    pub(crate) lingxi_home: std::path::PathBuf,
    /// The filesystem transcripts are read and appended through.
    pub(crate) fs: Arc<dyn platform_api::FileSystem>,
}

/// The latest effective `custom-title` for `session_id` in a transcript: the
/// title it resolves to, and whether that title is still one MOBILE wrote —
/// i.e. whether the user has never renamed this session themselves.
///
/// "Latest effective" mirrors [`session::jsonl::reader`] exactly: it folds
/// every `custom-title` line whose `sessionId` matches into one map slot, so
/// the LAST one on disk wins, and a record whose `customTitle` is not a string
/// is skipped (the reader's `and_then(Value::as_str)` drops it too).
///
/// ⚠️ The second half deliberately does NOT read the marker off the last
/// record. It cannot: the transcript writer's own 32 KiB metadata backstop
/// re-emits the CURRENT title as a PLAIN, unmarked `custom-title`
/// (`session::jsonl::re_append::plan_re_append` rebuilds the record from
/// `{type, customTitle, sessionId}` and has no marker to carry), so in any
/// interview long enough to trip it the last record is unmarked even though
/// nobody renamed anything. Reading the marker off the last record alone made
/// [`reconcile_app_init_session_title`] unreachable in production — see that
/// function and [`latest_custom_title_is_mobile_placeholder`].
///
/// So the scan tracks the ANCHOR — the title on the most recent marked record
/// — and treats an unmarked record as a user rename only when its text
/// DIFFERS from the anchor. A backstop echo copies the anchor's text verbatim;
/// a `/rename` writes something else.
fn latest_custom_title(transcript: &str, session_id: &str) -> Option<(String, bool)> {
    let mut latest: Option<String> = None;
    // The title on the most recent record that carried the mobile marker.
    // `None` until one is seen — an unmarked record BEFORE any anchor
    // (a `session::branch` fork's title, say) is superseded by the anchor and
    // must not poison it.
    let mut anchor: Option<String> = None;
    let mut user_renamed = false;
    for line in transcript.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("custom-title") {
            continue;
        }
        if value.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        let Some(title) = value.get("customTitle").and_then(Value::as_str) else {
            continue;
        };
        if value.get("mobileEmptySession").and_then(Value::as_u64) == Some(1) {
            // Mobile is the only writer that marks, and it only marks a title
            // it was entitled to write, so its own record re-establishes the
            // baseline.
            anchor = Some(title.to_string());
            user_renamed = false;
        } else if anchor.as_deref().is_some_and(|anchored| anchored != title) {
            user_renamed = true;
        }
        latest = Some(title.to_string());
    }
    latest.map(|title| (title, anchor.is_some() && !user_renamed))
}

/// Whether this session's title is still one MOBILE wrote, i.e. the user has
/// never renamed it.
///
/// ⚠️ The title TEXT cannot decide this and must never be used to.
/// `/rename` (`orchestrator`'s `append_custom_title`), a hook's `sessionTitle`
/// and mobile's own placeholder anchor all write the SAME `custom-title`
/// channel with the same shape; the only discriminator is the extra
/// `"mobileEmptySession":1` field that
/// [`session::jsonl::writer::JsonlWriter::append_mobile_empty_session`] adds.
/// An ordinary `custom-title` carrying text mobile never wrote, anywhere after
/// the anchor, turns this `false` and keeps it `false` — which is the point.
///
/// ⛔ It is NOT enough to look at the marker on the LAST record, and that
/// mistake made this whole path dead code in production. `JsonlWriter`'s
/// metadata backstop fires once
/// [`session::jsonl::re_append::METADATA_REAPPEND_BACKSTOP_BYTES`] (32 KiB)
/// have been appended, re-emitting the current title as a PLAIN `custom-title`
/// — [`session::jsonl::re_append::plan_re_append`] rebuilds the record from
/// `{type, customTitle, sessionId}` and has no marker to carry. Worse, mobile
/// keeps ONE writer across sessions and `JsonlWriter::retarget` does not reset
/// that counter, so a user who chatted before pressing "+" can trip the
/// backstop on the interview's very FIRST append. An interview therefore
/// strips the marker as a matter of course, and a last-record test would make
/// every app created through this flow keep `untitled` forever.
///
/// So [`latest_custom_title`] anchors on the most recent MARKED record and
/// only counts a LATER unmarked record as a user rename when its text differs
/// from that anchor. A backstop echo copies the anchor verbatim; a `/rename`
/// does not.
///
/// The one case this cannot separate is a user who runs `/rename` and types
/// the placeholder string EXACTLY: `append_custom_title` then emits a record
/// byte-identical (modulo timestamp) to a backstop echo, so no reader can tell
/// them apart. Clause 3 of [`reconcile_app_init_session_title`] still declines
/// whenever the title already equals `record.name`, so the residue is a user
/// who deliberately renamed their session to `untitled` and then confirmed a
/// different app name.
///
/// Known cases where this declines for a session the user never touched. The
/// bias is deliberate and one-directional: a false negative costs a stale
/// title, a false positive overwrites something a user typed.
/// - a transcript with no `custom-title` at all — nothing this host anchored,
///   so nothing for it to reconcile;
/// - a CHAT-ORIGIN app, whose init session is forked by
///   `session::branch::create_branch_to_cwd`. That fork writes its own
///   unmarked `custom-title` (from `record.name`, i.e. the placeholder), so a
///   chat-origin shell keeps its `untitled` session title. Closing that would
///   mean either marking a forked, non-empty session as a mobile empty session
///   — which is what `mobileEmptySession` means elsewhere — or reasoning from
///   the title text, which is exactly what this function exists to avoid. It
///   is left open rather than papered over.
pub(crate) fn latest_custom_title_is_mobile_placeholder(
    transcript: &str,
    session_id: &str,
) -> bool {
    latest_custom_title(transcript, session_id).is_some_and(|(_, marker)| marker)
}

/// The ONE reconciliation between an app's pinned init session title and
/// `record.name`, shared by the `LocalAppScaffold` commit point (which calls it
/// immediately) and the boot backfill sweep (which is the retry that makes a
/// failed immediate rename recoverable rather than permanent).
///
/// The full predicate, all three clauses required:
/// 1. `record.scaffolded` — an app still in its interview is SUPPOSED to read
///    `untitled`; renaming it early would put a real name in the library on a
///    record that still opens the interview.
/// 2. the session's effective title is still one MOBILE wrote — the user has
///    not renamed it. Anchored on the most recent `mobileEmptySession: 1`
///    record, NOT on the marker of the last record: the transcript writer's
///    32 KiB metadata backstop re-emits the title unmarked, which is exactly
///    what an interview does. See [`latest_custom_title`] and
///    [`latest_custom_title_is_mobile_placeholder`].
/// 3. that title differs from `record.name` — otherwise there is nothing to do,
///    and this is also what makes the boot sweep idempotent.
///
/// The rename is written with `append_mobile_empty_session` again, KEEPING the
/// marker: the user still has not renamed anything, so a later `/rename` must
/// still be able to take precedence over a subsequent reconcile.
///
/// Returns `Ok(true)` when a rename was written, `Ok(false)` when the predicate
/// declined. A missing transcript is `Ok(false)`, not an error — the sweep's
/// re-anchor step, which runs before this one, writes `record.name` directly.
pub(crate) async fn reconcile_app_init_session_title(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    fs: Arc<dyn platform_api::FileSystem>,
    record: &local_apps::AppRecord,
) -> Result<bool, String> {
    // Clause 1. Today no production state can reach this with a name that
    // differs from the session title — a shell is minted with `record.name`,
    // and `record.name` cannot change before the scaffold commits — so the
    // guard is unobservable through the app paths. It is still load-bearing as
    // a specification, and
    // `reconciliation_waits_for_the_scaffold_commit_before_renaming` pins it
    // directly so it cannot be deleted as dead code: a record that is still in
    // its interview must keep showing the placeholder, whatever its name says.
    if !record.scaffolded {
        return Ok(false);
    }
    let Some(init_id) = record.init_session_id.as_deref() else {
        return Ok(false);
    };
    let path = app_session_file(lingxi_home, data_root, record, init_id);
    let Some(path_str) = path.to_str() else {
        return Err(format!(
            "init-session path is not UTF-8: {}",
            path.display()
        ));
    };
    let Ok(file) = fs.read_file(path_str, None, None).await else {
        return Ok(false);
    };
    let Some((title, still_mobile_placeholder)) = latest_custom_title(&file.content, init_id)
    else {
        return Ok(false);
    };
    if !still_mobile_placeholder || title == record.name {
        return Ok(false);
    }
    session::jsonl::writer::JsonlWriter::new(path, fs)
        .append_mobile_empty_session(init_id, &record.name)
        .await
        .map_err(|error| format!("rename pinned init session: {error}"))?;
    Ok(true)
}

/// What to tell the agent immediately after an app is created.
///
/// It must NOT say "build it now". A create happens in a conversation that is
/// rooted somewhere ELSE — the library's intake chat sits in the project scope,
/// and an agent-driven create can happen in any chat at all. The new app's
/// workspace is a different directory, and the build workflow's agents inherit
/// the CALLING session's cwd, not the app's.
///
/// Observed on device: this used to read "the workspace already contains the
/// repository-verified foundation … then call LocalAppBuild", the agent obeyed
/// literally, and the whole build ran against the project workspace. It found a
/// previous run's leftover `apps/<other-id>/workspace` directory there and
/// edited that instead — every build failed on a workspace that was never the
/// app's, and nothing in the error said which directory was wrong.
///
/// The app already has its own session (`init_session_id` in this same
/// response). Handing off to it is what puts the agent in the right cwd with the
/// right `LINGXI.md` auto-loaded.
pub(crate) fn create_next_step_guidance() -> String {
    "The app now exists as an EMPTY shell, and this conversation is not rooted in it. Stop here: do not write source, do not call LocalAppBuild, and do not start a build workflow from this conversation — its working directory is not the app's workspace, so anything written here lands outside the app. The app has its own workspace and its own session (init_session_id in this result); continue there, where the guided workspace contract explains the interview and hands off to the `lingxi-local-app:create-local-app` skill (that exact, plugin-qualified name is how it is registered; the bare name does not resolve). Do not recreate the app, do not run a package-manager scaffold command, and do not install dependencies yet: the interview, a native create confirmation, and only then LocalAppScaffold happen first — never call LocalAppScaffold directly from this step.".into()
}

struct LocalAppsRuntimeConfiguration {
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    physical_memory_bytes: u64,
    runtime_root: Option<PathBuf>,
}

/// Profile-scoped broker.  The service is attached after its durable load has
/// completed, while command/capability resolution can be wired immediately.
pub(crate) struct LocalAppsHostBroker {
    root: PathBuf,
    event_sink: Arc<dyn ClientEventSink>,
    runtime_configuration: RwLock<LocalAppsRuntimeConfiguration>,
    service: OnceLock<Arc<AppService>>,
    mcp_registry: OnceLock<std::sync::Weak<mcp::McpRegistry>>,
    /// The same mobile LSP registry used by plugin materialization and file
    /// tools. A weak reference avoids keeping language-server processes alive
    /// after the owning engine connection is torn down.
    lsp_registry: OnceLock<std::sync::Weak<lsp::LspRegistry>>,
    /// Set once at profile load (same call site as `attach_service`), so the
    /// broker's `llm.chat` bridge operation reaches the live model.
    llm: OnceLock<Arc<crate::mobile::local_apps_profile::SharedLlm>>,
    /// Set at the same profile-load site as `llm` — live per-connection
    /// device handles behind a swap cell (see `local_apps_device`).
    device: OnceLock<Arc<crate::mobile::local_apps_device::SharedDeviceCapabilities>>,
    /// The single active `device.recordAudio*` session (one per broker — the
    /// platform has ONE audio session). Arc'd like `runtimes` so the duration
    /// watchdog task can reach it. See `device_ops`.
    recording: Arc<Mutex<Option<device_ops::ActiveRecording>>>,
    /// Captures retained so `llm.chat` can attach them by handle instead of
    /// copying base64 through every WebView/FFI layer. See
    /// [`crate::mobile::local_apps_device::MediaCache`].
    media: crate::mobile::local_apps_device::MediaCache,
    /// Apps with an `llm.chat` call in flight. One per app: an app-initiated
    /// call spends the user's quota, so a page cannot fan out.
    ///
    /// A std mutex behind an `Arc` on purpose: the slot is released by
    /// `LlmInflightGuard::drop`, which cannot await, and the set is only ever
    /// insert/remove — no lock is ever held across an await.
    pub(super) llm_inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Serializes mailbox read-modify-writes. Held across the file update
    /// and NOTHING else — never across an emit, never across a client call.
    mailbox_writes: Mutex<()>,
    /// Serializes Agent session catalog read-modify-writes. Atomic file
    /// replacement alone cannot prevent concurrent creates/updates from
    /// overwriting a stale catalog snapshot.
    agent_session_writes: Mutex<()>,
    /// Serializes the per-app MCP settings CAS read/modify/write transaction.
    /// Atomic replacement protects readers from partial JSON, but without this
    /// lock two commands could both validate the same expected revision and
    /// silently overwrite each other.
    mcp_settings_writes: Mutex<()>,
    /// Conversation whose Local App exposure/pin state should be reflected in
    /// native inventory snapshots.
    active_mcp_conversation: Mutex<Option<String>>,
    /// Stable native host facts, attached by the mobile composition root from
    /// the same `MobileConfig` that renders the mobile runtime reminder.
    ///
    /// This is the ONLY source of an app's device context. The agent cannot
    /// supply one: the reminder is all it sees, and the reminder's device
    /// vocabulary (`phone`/`tablet`) does not name an iOS form factor.
    /// Unattached — desktop embedders and host tests — means no context.
    host_environment: OnceLock<platform_api::MobileHostEnvironment>,
    /// Host-owned app Agent execution seam, attached by the mobile composition
    /// root after the app service and MCP host are ready.
    agent_executor: OnceLock<Arc<dyn LocalAppsAgentExecutor>>,
    /// Active app Agent turns keyed by host-minted turn id.
    agent_turns: Arc<Mutex<HashMap<String, Arc<AgentTurnControl>>>>,
    /// One-time Profile proposals awaiting an explicit trusted-client decision.
    pending_profile_proposals: Mutex<HashMap<String, PendingAppProfileProposal>>,
    /// Serializes background task claims and journal transitions within one
    /// profile. The native scheduler may deliver duplicate wake-ups.
    background_task_writes: Mutex<()>,
    /// In-memory duplicate-delivery guard; persisted `Running` state handles
    /// process death, while this set handles concurrent WorkManager/BGTask
    /// deliveries in one process.
    background_inflight: Mutex<std::collections::HashSet<String>>,
    /// Serializes `device.recordAudioStart` — and ONLY starts.
    ///
    /// Separate from `recording` because a start crosses into Swift and the
    /// first mic use shows an OS permission alert with unbounded think time.
    /// Stops and reclaims take `recording` alone, so they can never be
    /// blocked behind that alert. Taken with `try_lock`: a second start
    /// answers `audio_session_busy` rather than queueing behind it.
    recording_start: Mutex<()>,
    /// Identity and service for the currently pending permission/start call.
    /// A runtime teardown can cancel this exact operation without waiting for
    /// the OS microphone prompt to return.
    recording_pending: Arc<Mutex<Option<device_ops::PendingRecordingStart>>>,
    /// Cancellation state for short audio requests across authorization and
    /// native admission, keyed by the host-minted Local App runtime generation.
    audio_runtime_scopes: std::sync::Mutex<device_ops::LocalAppAudioScopeRegistry>,
    /// Weak self-reference handed to the runtime-exit watchers, which are
    /// spawned onto the profile worker and outlive the call that started
    /// them. Weak so a watcher can never be what keeps the broker alive.
    self_ref: OnceLock<std::sync::Weak<LocalAppsHostBroker>>,
    pending_capabilities: Mutex<HashMap<String, oneshot::Sender<AppAuthorizationDecisionDto>>>,
    pending_dependency_change_confirmations: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    pending_create_confirmations: Mutex<HashMap<String, PendingNativeApproval>>,
    pending_mcp_proposal_approvals: Mutex<HashMap<String, PendingNativeApproval>>,
    pending_ui: Mutex<HashMap<String, oneshot::Sender<UiResolution>>>,
    pending_dependency_change_receipts: Mutex<HashMap<String, PendingDependencyChangeReceipt>>,
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`, for the same reason as
    /// `scaffold_reservations` (:1172): [`ReceiptClaim::drop`] releases a
    /// leaked claim, and `Drop` cannot `.await`.
    pending_mcp_receipts: Arc<std::sync::Mutex<local_apps::McpReceiptBook>>,
    session_permissions: Mutex<SessionPermissions>,
    runtimes: Arc<Mutex<HashMap<String, RuntimeEntry>>>,
    /// Per-app synchronous mirror used only by the bounded terminal QA commit.
    /// Runtime transitions update the same cell while holding `runtimes`; the
    /// commit holds a read guard through marker/pointer publication, closing
    /// the prepare-to-commit restart race without awaiting under the registry
    /// critical section or blocking unrelated apps.
    runtime_publication_identities: Arc<std::sync::Mutex<HashMap<String, RuntimePublicationCell>>>,
    /// A bounded action window opened before native UI dispatch. Page bridge
    /// writes are attributed only while this Host-owned window is active.
    qa_inflight_actions: Arc<Mutex<HashMap<String, QaInFlightAction>>>,
    /// Synchronous liveness mirror for QA action cancellation. A dropped
    /// `act_on_ui` future can clear this map from `Drop` without awaiting,
    /// preventing a later page bridge request from entering a stale action
    /// window while the async action map is eventually reclaimed.
    qa_active_actions: Arc<std::sync::Mutex<HashMap<String, QaActiveActionState>>>,
    /// See [`PortLeases`].  Broker-scoped because a profile's apps are what
    /// collide with each other, and one broker is exactly one profile.
    port_leases: PortLeases,
    /// Serializes port ALLOCATION — the sibling-pin read plus the choice —
    /// across this broker's starts.
    ///
    /// What it buys: the pin snapshot goes stale the instant another start
    /// persists one, and a lease is only taken AFTER the snapshot is read.
    /// Without this gate an allocator can read the pins, lose the scheduler for
    /// the length of another app's entire lease, and then choose from a set
    /// that never contained that app's port at all — so it wastes the whole
    /// scan re-deriving candidates it has no reason to reject.
    ///
    /// It does NOT buy mutual exclusion on a candidate: two ungated allocators
    /// sitting between the same pair of steps still cannot both come away with
    /// one port, because `PortLease::take` is a test-and-insert under a single
    /// mutex and the loser scans on.  Claiming otherwise here was the ninth
    /// false comment this module has shipped; the guarantee lives in `take`.
    ///
    /// What it does NOT buy, because this was mis-stated here once already: it
    /// does not make one start's snapshot fresh.  A sibling's persist and its
    /// `PortLease::commit` both run AFTER that sibling has left this gate, so
    /// they land freely inside the window a later start holds it — pins read at
    /// the top of a gated allocation can be stale by the bottom of the very
    /// same allocation.  The gate narrows the staleness to "no OTHER allocation
    /// is in progress"; what closes it is the second pin read
    /// `bind_stable_loopback` takes after leasing its candidate.
    ///
    /// Extending the gate over the persist instead would close the same hole
    /// and is deliberately not done: `AppService::with_app` holds its state
    /// lock across a blocking disk write, so that shape would hold this mutex
    /// across another subsystem's lock — the ordering hazard, and the
    /// held-across-blocking-work hazard, both at once.  Held across service
    /// reads and the bind hop only — never across a call into client or
    /// listener code, which is the rule `AppEmissionQueue` exists to keep.
    port_allocation: Mutex<()>,
    /// Serializes dependency snapshot publication/materialization per lock
    /// digest so concurrent app creates do not run the same install twice.
    dependency_snapshot_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// App ids with a `LocalAppScaffold` transaction in flight — §C.1 step 1.
    ///
    /// ⛔ IN-PROCESS ONLY, and deliberately so. The obvious alternative — a
    /// persistent set-once field in the style of `AppService::set_init_session`
    /// — is WRONG here: a reservation that reaches disk survives the process
    /// being killed mid-transaction, and nothing ever clears it, so the draft
    /// is bricked forever. That is the exact opposite of §C.1 step 4's
    /// retry-safety. The engine is one process on device, so an in-process set
    /// is sufficient; after a restart the set is empty and `scaffolded` is
    /// still `false`, so the retry simply works.
    ///
    /// `AppService::with_app` cannot hold this either: its guard lives only as
    /// long as its own completion task, while steps 2-4 run entirely outside
    /// that lock.
    ///
    /// A std mutex behind an `Arc` on purpose, like `llm_inflight`: the slot is
    /// released by [`ScaffoldReservation::drop`], which cannot await, and the
    /// set is only ever insert/remove — no lock is ever held across an await.
    scaffold_reservations: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Where an app's pinned init session lives, so the §C.1 step 4 commit can
    /// rename it out of its `untitled` placeholder. See [`SessionCatalog`].
    ///
    /// Optional on purpose: a broker built without it (every unit-test root
    /// that has no session catalog at all) simply skips the immediate rename,
    /// and the boot backfill sweep — which is handed `lingxi_home` and the
    /// filesystem directly — still reconciles the title on the next launch.
    session_catalog: OnceLock<SessionCatalog>,
    next_request_id: AtomicU64,
}

impl LocalAppsHostBroker {
    /// Test-convenience constructor (production goes through
    /// [`Self::new_with_physical_memory`], which every test root that needs a
    /// memory figure also uses).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        root: PathBuf,
        event_sink: Arc<dyn ClientEventSink>,
        mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
        _full_runtime: bool,
        runtime_root: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::new_with_physical_memory(root, event_sink, mobile_linux, false, runtime_root, 0)
    }

    pub(crate) fn new_with_physical_memory(
        root: PathBuf,
        event_sink: Arc<dyn ClientEventSink>,
        mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
        _full_runtime: bool,
        runtime_root: Option<PathBuf>,
        physical_memory_bytes: u64,
    ) -> Arc<Self> {
        if let Err(error) = Self::recover_dependency_updates_on_boot(&root) {
            tracing::warn!(
                root = %root.display(),
                %error,
                "dependency update recovery deferred until the next profile load"
            );
        }
        let broker = Arc::new(Self {
            root,
            event_sink,
            runtime_configuration: RwLock::new(LocalAppsRuntimeConfiguration {
                mobile_linux,
                physical_memory_bytes,
                runtime_root,
            }),
            service: OnceLock::new(),
            mcp_registry: OnceLock::new(),
            lsp_registry: OnceLock::new(),
            llm: OnceLock::new(),
            device: OnceLock::new(),
            recording: Arc::new(Mutex::new(None)),
            media: crate::mobile::local_apps_device::MediaCache::default(),
            llm_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            mailbox_writes: Mutex::new(()),
            agent_session_writes: Mutex::new(()),
            mcp_settings_writes: Mutex::new(()),
            active_mcp_conversation: Mutex::new(None),
            host_environment: OnceLock::new(),
            agent_executor: OnceLock::new(),
            agent_turns: Arc::new(Mutex::new(HashMap::new())),
            pending_profile_proposals: Mutex::new(HashMap::new()),
            background_task_writes: Mutex::new(()),
            background_inflight: Mutex::new(std::collections::HashSet::new()),
            recording_start: Mutex::new(()),
            recording_pending: Arc::new(Mutex::new(None)),
            audio_runtime_scopes: std::sync::Mutex::new(
                device_ops::LocalAppAudioScopeRegistry::default(),
            ),
            self_ref: OnceLock::new(),
            pending_capabilities: Mutex::new(HashMap::new()),
            pending_dependency_change_confirmations: Mutex::new(HashMap::new()),
            pending_create_confirmations: Mutex::new(HashMap::new()),
            pending_mcp_proposal_approvals: Mutex::new(HashMap::new()),
            pending_ui: Mutex::new(HashMap::new()),
            pending_dependency_change_receipts: Mutex::new(HashMap::new()),
            pending_mcp_receipts: Arc::new(std::sync::Mutex::new(
                local_apps::McpReceiptBook::default(),
            )),
            session_permissions: Mutex::new(SessionPermissions::default()),
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            runtime_publication_identities: Arc::new(std::sync::Mutex::new(HashMap::new())),
            qa_inflight_actions: Arc::new(Mutex::new(HashMap::new())),
            qa_active_actions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            port_leases: Arc::new(std::sync::Mutex::new(HashMap::new())),
            port_allocation: Mutex::new(()),
            dependency_snapshot_locks: Mutex::new(HashMap::new()),
            scaffold_reservations: Arc::new(
                std::sync::Mutex::new(std::collections::HashSet::new()),
            ),
            session_catalog: OnceLock::new(),
            next_request_id: AtomicU64::new(1),
        });
        // The one place an `Arc<Self>` exists; the exit watchers downgrade
        // from it rather than being handed a strong clone.
        let _ = broker.self_ref.set(Arc::downgrade(&broker));
        broker
    }

    /// Weak handle for tasks that outlive the call that spawned them.
    fn weak_self(&self) -> std::sync::Weak<LocalAppsHostBroker> {
        self.self_ref.get().cloned().unwrap_or_default()
    }

    fn runtime_publication_cell(&self, app_id: &str) -> Result<RuntimePublicationCell, String> {
        let mut identities = self
            .runtime_publication_identities
            .lock()
            .map_err(|_| "runtime publication identity registry is poisoned".to_string())?;
        Ok(identities
            .entry(app_id.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(None)))
            .clone())
    }

    pub(crate) fn attach_service(&self, service: Arc<AppService>) -> Result<(), Arc<AppService>> {
        self.service.set(service)
    }

    pub(crate) fn attach_mcp_registry(
        &self,
        registry: std::sync::Weak<mcp::McpRegistry>,
    ) -> Result<(), std::sync::Weak<mcp::McpRegistry>> {
        self.mcp_registry.set(registry)
    }

    fn upgraded_mcp_registry(&self) -> Option<Arc<mcp::McpRegistry>> {
        self.mcp_registry.get().and_then(std::sync::Weak::upgrade)
    }

    pub(crate) fn attach_lsp_registry(
        &self,
        registry: std::sync::Weak<lsp::LspRegistry>,
    ) -> Result<(), std::sync::Weak<lsp::LspRegistry>> {
        self.lsp_registry.set(registry)
    }

    pub(crate) fn upgraded_lsp_registry(&self) -> Option<Arc<lsp::LspRegistry>> {
        self.lsp_registry.get().and_then(std::sync::Weak::upgrade)
    }

    fn managed_mcp_config(
        scope: &mcp::registry::ConversationExport,
        conversation_id: &str,
    ) -> Result<mcp::McpServerConfig, String> {
        Ok(mcp::McpServerConfig {
            name: scope.server_name(),
            spec: platform_api::McpTransportSpec::InProcess {
                registry_key: scope
                    .scoped_registry_key(conversation_id)
                    .map_err(|error| error.to_string())?,
            },
            scope: mcp::ConfigScope::Settings(protocol::SettingsScope::Managed),
            disabled: false,
            timeout_ms: Some(crate::mobile::host::LOCAL_APPS_MCP_TIMEOUT_MS),
            always_load: true,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        })
    }

    /// Make one enabled Local App MCP visible to one conversation and create
    /// the real logical MCP connection whose discovered tools are registered
    /// into that conversation's shared ToolRegistry.
    pub(crate) async fn expose_managed_mcp_for_conversation(
        &self,
        conversation_id: &str,
        app_id: &str,
        pin: bool,
    ) -> Result<bool, String> {
        let Some(registry) = self.upgraded_mcp_registry() else {
            return Ok(false);
        };
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let Some(active) = manifest.active_mcp_catalog.as_ref() else {
            return Ok(false);
        };
        let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
            .map_err(|error| error.to_string())?;
        let settings = load_mcp_settings(&layout)
            .map_err(|error| error.to_string())?
            .reconcile_with_catalog(&catalog, false)
            .map_err(|error| error.to_string())?;
        if !settings.enabled || settings.enabled_tools.is_empty() {
            return Ok(false);
        }
        let effective_surface =
            effective_tool_surface_sha256(&active.tool_surface_sha256, &settings.enabled_tools)
                .map_err(|error| error.to_string())?;
        let scope = mcp::registry::ConversationExport::new(app_id, effective_surface)
            .map_err(|error| error.to_string())?;
        registry
            .register_managed_local_app(scope.clone(), active.catalog_sha256.clone(), false)
            .await
            .map_err(|error| error.to_string())?;
        registry
            .set_managed_local_app_runtime(
                app_id,
                true,
                Some(settings.enabled_tools.clone()),
                managed_mcp_widget_resource(&layout, &manifest.name, &catalog)?
                    .map(|(_, resource)| resource),
            )
            .await
            .map_err(|error| error.to_string())?;

        let update = registry
            .expose_managed_local_app_with_diff(conversation_id, app_id, pin)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(evicted_app_id) = update.evicted_app_id {
            registry
                .disconnect(&format!("local_app_{evicted_app_id}"))
                .await
                .map_err(|error| error.to_string())?;
        }

        let desired = Self::managed_mcp_config(&scope, conversation_id)?;
        let desired_registry_key = match &desired.spec {
            platform_api::McpTransportSpec::InProcess { registry_key } => registry_key,
            _ => unreachable!("managed Local App MCP is always in-process"),
        };
        let existing = registry.get_config(&scope.server_name()).await;
        let same_conversation_route =
            existing
                .as_ref()
                .is_some_and(|current| match &current.spec {
                    platform_api::McpTransportSpec::InProcess { registry_key } => {
                        let current_route = registry_key.rsplit_once(':').map(|(route, _)| route);
                        let desired_route = desired_registry_key
                            .rsplit_once(':')
                            .map(|(route, _)| route);
                        current_route == desired_route
                    }
                    _ => false,
                });
        if same_conversation_route {
            return Ok(true);
        }
        if existing.is_some() {
            registry
                .disconnect(&scope.server_name())
                .await
                .map_err(|error| error.to_string())?;
        }
        registry
            .connect(desired)
            .await
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    pub(crate) async fn set_managed_mcp_conversation_pinned(
        &self,
        conversation_id: &str,
        app_id: &str,
        pinned: bool,
    ) -> Result<(), String> {
        *self.active_mcp_conversation.lock().await = Some(conversation_id.to_string());
        if pinned {
            if !self
                .expose_managed_mcp_for_conversation(conversation_id, app_id, true)
                .await?
            {
                return Err("mcp_not_enabled: Local App MCP is not enabled".into());
            }
        } else if let Some(registry) = self.upgraded_mcp_registry() {
            match registry
                .pin_local_app_exposure(conversation_id, app_id, false)
                .await
            {
                Ok(_) | Err(McpError::ToolNotFound(_)) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        self.emit_managed_mcp_inventory().await
    }

    /// Retarget the shared model ToolRegistry to one conversation's logical
    /// Local App MCP partition. Existing per-conversation LRU/pin metadata is
    /// retained, while live logical connections from the previous session are
    /// removed before the new session is exposed.
    pub(crate) async fn activate_managed_mcp_conversation(
        &self,
        conversation_id: &str,
        cwd: &str,
    ) -> Result<(), String> {
        *self.active_mcp_conversation.lock().await = Some(conversation_id.to_string());
        let Some(registry) = self.upgraded_mcp_registry() else {
            return Ok(());
        };
        for managed in registry.managed_local_apps().await {
            registry
                .disconnect(&managed.scope.server_name())
                .await
                .map_err(|error| error.to_string())?;
        }

        let mut desired = registry.local_app_exposures(conversation_id).await;
        let canonical_cwd = canonical_cwd_string(Path::new(cwd));
        if let Ok(service) = self.service() {
            for record in service.list_apps().await {
                let workspace = canonical_cwd_string(&self.root.join(&record.workspace_rel));
                if workspace == canonical_cwd
                    && !desired.iter().any(|entry| entry.app_id == record.id)
                {
                    desired.push(mcp::registry::LocalAppExposure {
                        app_id: record.id,
                        pinned: false,
                        in_flight: 0,
                        last_used: u64::MAX,
                        exposure_generation: 0,
                    });
                    break;
                }
            }
        }
        desired.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
        for entry in desired {
            let _ = self
                .expose_managed_mcp_for_conversation(conversation_id, &entry.app_id, entry.pinned)
                .await?;
        }
        self.emit_managed_mcp_inventory().await
    }

    pub(crate) async fn set_managed_mcp_enabled(
        &self,
        app_id: &str,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<(), String> {
        let _guard = self.mcp_settings_writes.lock().await;
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let active = manifest.active_mcp_catalog.as_ref();
        if enabled && active.is_none() {
            return Err("mcp_authoring_required: Local App has no approved MCP catalog".into());
        }
        let mut settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
        if let Some(active) = active {
            let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                .map_err(|error| error.to_string())?;
            settings = settings
                .reconcile_with_catalog(&catalog, false)
                .map_err(|error| error.to_string())?;
            if enabled && settings.enabled_tools.is_empty() {
                settings.enabled_tools =
                    mcp_catalog_tool_names(&catalog).map_err(|error| error.to_string())?;
            }
        }
        if enabled && settings.enabled_tools.is_empty() {
            return Err("invalid_mcp_settings: no model-visible tools are enabled".into());
        }
        settings.enabled = enabled;
        save_mcp_settings(&layout, &settings, Some(expected_revision))
            .map_err(|error| error.to_string())?;
        drop(_guard);
        self.sync_managed_local_app_publication(app_id).await?;
        self.emit_managed_mcp_inventory().await
    }

    pub(crate) async fn set_managed_mcp_tool_enabled(
        &self,
        app_id: &str,
        tool_name: &str,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<(), String> {
        let _guard = self.mcp_settings_writes.lock().await;
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let active = manifest.active_mcp_catalog.as_ref().ok_or_else(|| {
            "mcp_authoring_required: Local App has no approved MCP catalog".to_string()
        })?;
        let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
            .map_err(|error| error.to_string())?;
        let catalog_tools = mcp_catalog_tool_names(&catalog).map_err(|error| error.to_string())?;
        let mut settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
        settings = settings
            .reconcile_with_catalog(&catalog, false)
            .map_err(|error| error.to_string())?;
        if !catalog_tools.iter().any(|name| name == tool_name) {
            return Err(format!(
                "invalid_mcp_settings: tool {tool_name:?} is not in the active catalog"
            ));
        }
        let mut enabled_tools: std::collections::BTreeSet<String> =
            settings.enabled_tools.into_iter().collect();
        if enabled {
            enabled_tools.insert(tool_name.to_string());
        } else {
            enabled_tools.remove(tool_name);
        }
        settings.enabled_tools = catalog_tools
            .into_iter()
            .filter(|name| enabled_tools.contains(name))
            .collect();
        if settings.enabled_tools.is_empty() {
            settings.enabled = false;
        }
        save_mcp_settings(&layout, &settings, Some(expected_revision))
            .map_err(|error| error.to_string())?;
        drop(_guard);
        self.sync_managed_local_app_publication(app_id).await?;
        self.emit_managed_mcp_inventory().await
    }

    pub(crate) async fn sync_managed_local_app_publication(
        &self,
        app_id: &str,
    ) -> Result<(), String> {
        let Some(registry) = self.upgraded_mcp_registry() else {
            return Ok(());
        };
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let active_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?;
        match local_apps::derive_publication_state(&manifest, active_build_id.as_deref(), false) {
            Ok(local_apps::AppPublicationState::Draft) => {
                registry
                    .unregister_managed_local_app(app_id)
                    .await
                    .map_err(|error| error.to_string())?;
                registry
                    .disconnect(&format!("local_app_{app_id}"))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(
                local_apps::AppPublicationState::PublishedUnverified
                | local_apps::AppPublicationState::PublishedVerified,
            ) => {
                let Some(active) = manifest.active_mcp_catalog.as_ref() else {
                    registry
                        .unregister_managed_local_app(app_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    registry
                        .disconnect(&format!("local_app_{app_id}"))
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                };
                let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                    .map_err(|error| error.to_string())?;
                let settings = load_mcp_settings(&layout)
                    .map_err(|error| error.to_string())?
                    .reconcile_with_catalog(&catalog, false)
                    .map_err(|error| error.to_string())?;
                let effective_surface = effective_tool_surface_sha256(
                    &active.tool_surface_sha256,
                    &settings.enabled_tools,
                )
                .map_err(|error| error.to_string())?;
                let scope = mcp::registry::ConversationExport::new(app_id, effective_surface)
                    .map_err(|error| error.to_string())?;
                registry
                    .register_managed_local_app(scope, active.catalog_sha256.clone(), false)
                    .await
                    .map_err(|error| error.to_string())?;
                let runtime_enabled = settings.enabled
                    && !settings.enabled_tools.is_empty()
                    && active_build_id.as_deref() == Some(active.build_id.as_str());
                registry
                    .set_managed_local_app_runtime(
                        app_id,
                        runtime_enabled,
                        Some(settings.enabled_tools),
                        managed_mcp_widget_resource(&layout, &manifest.name, &catalog)?
                            .map(|(_, resource)| resource),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Err(error) => {
                registry
                    .unregister_managed_local_app(app_id)
                    .await
                    .map_err(|registry_error| registry_error.to_string())?;
                return Err(error.to_string());
            }
        }
        Ok(())
    }

    pub(crate) async fn unregister_managed_local_app(&self, app_id: &str) -> Result<(), String> {
        let Some(registry) = self.upgraded_mcp_registry() else {
            return Ok(());
        };
        registry
            .unregister_managed_local_app(app_id)
            .await
            .map_err(|error| error.to_string())?;
        self.emit_managed_mcp_inventory().await?;
        Ok(())
    }

    async fn rebind_active_mcp_catalog_to_current_build(
        &self,
        app_id: &str,
        layout: &AppLayout,
    ) -> Result<(), String> {
        let mut manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let Some(active) = manifest.active_mcp_catalog.clone() else {
            return Ok(());
        };
        let active_build_id = crate::mobile::local_apps_build::active_build_id(layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "active_state_corrupt: published app is missing its active build".to_string()
            })?;
        if active.build_id == active_build_id {
            self.sync_managed_local_app_publication(app_id).await?;
            return Ok(());
        }
        let mut catalog = local_apps::load_mcp_catalog(layout, &active.catalog_sha256)
            .map_err(|error| error.to_string())?;
        if catalog.get("appId").and_then(Value::as_str) != Some(app_id)
            || catalog.get("buildId").and_then(Value::as_str) != Some(active.build_id.as_str())
        {
            return Err("active_state_corrupt: active MCP catalog identity mismatch".into());
        }
        let contexts = self.load_active_mcp_flow_contexts(layout).ok();
        let flow_contract_unchanged = contexts.as_ref().is_some_and(|contexts| {
            catalog
                .get("execution")
                .and_then(Value::as_array)
                .is_some_and(|bindings| {
                    !bindings.is_empty()
                        && bindings.iter().all(|binding| {
                            let flow_id = binding
                                .get("flow")
                                .and_then(|flow| flow.get("flowId"))
                                .and_then(Value::as_str);
                            let expected = binding.get("contextSha256").and_then(Value::as_str);
                            match (flow_id, expected) {
                                (Some(flow_id), Some(expected)) => contexts
                                    .get(flow_id)
                                    .and_then(|context| serde_json::to_value(context).ok())
                                    .and_then(|context| value_sha256(&context).ok())
                                    .is_some_and(|actual| actual == expected),
                                _ => false,
                            }
                        })
                })
        });
        if !flow_contract_unchanged {
            let _guard = self.mcp_settings_writes.lock().await;
            let mut settings = load_mcp_settings(layout).map_err(|error| error.to_string())?;
            if settings.enabled {
                let expected = settings.revision;
                settings.enabled = false;
                save_mcp_settings(layout, &settings, Some(expected))
                    .map_err(|error| error.to_string())?;
            }
            drop(_guard);
            self.sync_managed_local_app_publication(app_id).await?;
            self.emit_managed_mcp_inventory().await?;
            return Ok(());
        }
        catalog["buildId"] = Value::String(active_build_id.clone());
        let catalog_sha256 =
            local_apps::hash_mcp_catalog(catalog.clone()).map_err(|error| error.to_string())?;
        local_apps::save_mcp_catalog(layout, &catalog_sha256, &catalog)
            .map_err(|error| error.to_string())?;
        if let Some(current) = manifest.active_mcp_catalog.as_mut() {
            current.build_id = active_build_id;
            current.catalog_sha256 = catalog_sha256;
        }
        local_apps::save_manifest(layout, &manifest).map_err(|error| error.to_string())?;
        self.sync_managed_local_app_publication(app_id).await?;
        self.emit_managed_mcp_inventory().await?;
        Ok(())
    }

    pub(crate) async fn emit_managed_mcp_inventory(&self) -> Result<(), String> {
        let service = self.service()?;
        let registry = self.upgraded_mcp_registry();
        let active_conversation = self.active_mcp_conversation.lock().await.clone();
        let pinned_apps: std::collections::HashSet<String> =
            if let (Some(registry), Some(conversation_id)) =
                (registry.as_ref(), active_conversation.as_deref())
            {
                registry
                    .local_app_exposures(conversation_id)
                    .await
                    .into_iter()
                    .filter(|entry| entry.pinned)
                    .map(|entry| entry.app_id)
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
        let mut servers = Vec::new();
        for record in service.list_apps().await {
            let layout = self.layout(&record.id)?;
            let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
            let active_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
                .map_err(|error| error.to_string())?;
            let ui_verification = self.qa_ui_verification_summary(&record.id).await;
            let publication = local_apps::derive_publication_state(
                &manifest,
                active_build_id.as_deref(),
                ui_verification.status == LocalAppVerificationStatusDto::Passed,
            )
            .map_err(|error| error.to_string())?;
            if matches!(publication, local_apps::AppPublicationState::Draft) {
                continue;
            }
            let authoring_in_progress = local_apps::load_candidate_journal(&layout)
                .ok()
                .is_some_and(|journal| {
                    journal.app_id == record.id
                        && journal.stage < local_apps::McpAuthoringStage::Promoted
                });
            let mut settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
            let mut status = if authoring_in_progress {
                AppMcpStatus::Authoring
            } else {
                AppMcpStatus::NeedsSetup
            };
            let mut server_name = format!("local_app_{}", record.id);
            let mut enabled = false;
            let mut enabled_tools = settings.enabled_tools.clone();
            let mut build_id = active_build_id.clone().unwrap_or_default();
            let mut catalog_sha256 = String::new();
            let mut tool_surface_sha256 = String::new();
            let mut tool_count = 0u32;
            let mut authoring_revision = 0u64;
            let mut widget = None;
            let mut tools = Vec::new();
            let mut mcp_verification = LocalAppVerificationSummaryDto {
                status: LocalAppVerificationStatusDto::Unverified,
                summary: "No approved Local App MCP catalog is active yet.".into(),
                code: Some("needs_setup".into()),
            };

            if let Some(active) = manifest.active_mcp_catalog.as_ref() {
                let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                    .map_err(|error| error.to_string())?;
                // r3-never-wired-05: a catalog whose recorded identity does not
                // match this app and build used to `return Err(...)` from here,
                // which aborted the WHOLE managed-MCP inventory listing — every
                // other app vanished from both clients because ONE app's state
                // was corrupt. Mark this one app `Failed` and keep listing.
                // This is also the production producer for
                // `LocalAppVerificationStatusDto::Failed`, which both clients
                // already render (`local_apps_verification_status_failed`) and
                // which had none.
                let identity_matches = catalog.get("appId").and_then(Value::as_str)
                    == Some(record.id.as_str())
                    && catalog.get("buildId").and_then(Value::as_str)
                        == Some(active.build_id.as_str());
                if !identity_matches {
                    status = AppMcpStatus::Error;
                    // `tools`, `catalog_sha256` and `tool_surface_sha256` keep
                    // their pre-catalog defaults, so nothing derived from the
                    // untrusted catalog is presented as a callable surface.
                    // (`enabled`/`enabled_tools` may still be overwritten below
                    // from the live registry — that reflects what is actually
                    // registered, which is a fact about the process, not a claim
                    // about this catalog.)
                    mcp_verification = LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Failed,
                        summary:
                            "The approved MCP catalog does not match this app and build, so it \
                             cannot be trusted. Re-run MCP authoring to rebuild it."
                                .into(),
                        // No `clients/translations/` key exists for this code yet,
                        // so both clients fall back to `summary` verbatim (their
                        // `default:`/`else ->` arm) while the STATUS badge beside
                        // it is localized. Naming it anyway keeps the `code == nil`
                        // arm — which both clients map to "verification passed" —
                        // from ever being reached by a failure.
                        code: Some("active_state_corrupt".into()),
                    };
                } else {
                    settings = settings
                        .reconcile_with_catalog(&catalog, false)
                        .map_err(|error| error.to_string())?;
                    enabled_tools = settings.enabled_tools.clone();
                    let needs_revalidation =
                        active_build_id.as_deref() != Some(active.build_id.as_str());
                    enabled = settings.enabled && !enabled_tools.is_empty() && !needs_revalidation;
                    status = derive_mcp_status(
                        &manifest,
                        &AppMcpSettings {
                            enabled,
                            enabled_tools: enabled_tools.clone(),
                            ..settings.clone()
                        },
                        authoring_in_progress,
                        needs_revalidation,
                        false,
                    );
                    build_id = active.build_id.clone();
                    catalog_sha256 = active.catalog_sha256.clone();
                    tool_surface_sha256 = if enabled {
                        effective_tool_surface_sha256(&active.tool_surface_sha256, &enabled_tools)
                            .map_err(|error| error.to_string())?
                    } else {
                        active.tool_surface_sha256.clone()
                    };
                    authoring_revision = active.authoring_revision;
                    tools = mcp_tool_surfaces_from_catalog(&catalog)?;
                    tool_count = u32::try_from(tools.len()).unwrap_or(u32::MAX);
                    widget = managed_mcp_widget_resource(&layout, &record.name, &catalog)?
                        .map(|(widget, _)| widget);
                    mcp_verification = if needs_revalidation {
                        LocalAppVerificationSummaryDto {
                            status: LocalAppVerificationStatusDto::Unverified,
                            summary: "The approved MCP catalog no longer matches the active build and must be revalidated.".into(),
                            code: Some("needs_revalidation".into()),
                        }
                    } else {
                        LocalAppVerificationSummaryDto {
                            status: LocalAppVerificationStatusDto::Passed,
                            summary: "MCP schema, Flow, call and isolation verification passed."
                                .into(),
                            code: None,
                        }
                    };
                }
            }

            if let Some(registry) = registry.as_ref() {
                if let Some(managed) = registry.managed_local_app(&record.id).await {
                    server_name = managed.scope.server_name();
                }
                if let Some(runtime) = registry.managed_local_app_runtime(&record.id).await {
                    enabled = runtime.enabled;
                    if let Some(runtime_enabled_tools) = runtime.enabled_tools {
                        enabled_tools = runtime_enabled_tools;
                    }
                }
            }
            let pinned_to_current_conversation = pinned_apps.contains(&record.id);
            servers.push(ManagedLocalAppMcpServerDto {
                server_name,
                app_id: record.id,
                app_name: record.name,
                enabled,
                status: lower_managed_mcp_status(status),
                settings_revision: settings.revision,
                enabled_tools,
                pinned_to_current_conversation,
                build_id,
                catalog_sha256,
                tool_surface_sha256,
                tool_count,
                authoring_revision,
                publication_state: match publication {
                    local_apps::AppPublicationState::Draft => AppWorkflowStateDto::Draft,
                    local_apps::AppPublicationState::PublishedUnverified => {
                        AppWorkflowStateDto::PublishedUnverified
                    }
                    local_apps::AppPublicationState::PublishedVerified => {
                        AppWorkflowStateDto::PublishedVerified
                    }
                },
                mcp_verification,
                ui_verification,
                widget,
                tools,
            });
        }
        servers.sort_by(|left, right| left.app_id.cmp(&right.app_id));
        // r2-never-wired-01: `VerificationSummaryChanged` had zero producers
        // anywhere in the engine, so both clients' per-app verification
        // fields could never become non-nil. The publication/verification
        // triple is already computed per server above; derive the summary
        // event from the SAME values rather than recomputing them, so the
        // two events can never disagree.
        for server in &servers {
            self.event_sink
                .emit(ClientEvent::AppEvent {
                    event: AppEventDto::VerificationSummaryChanged {
                        app_id: server.app_id.clone(),
                        publication_state: server.publication_state,
                        mcp_verification: server.mcp_verification.clone(),
                        ui_verification: server.ui_verification.clone(),
                    },
                })
                .await;
        }
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::ManagedMcpInventoryChanged { servers },
            })
            .await;
        Ok(())
    }

    pub(crate) fn refresh_runtime_configuration(
        &self,
        mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
        runtime_root: Option<PathBuf>,
        physical_memory_bytes: u64,
    ) {
        *self
            .runtime_configuration
            .write()
            .expect("local-app runtime configuration poisoned") = LocalAppsRuntimeConfiguration {
            mobile_linux,
            physical_memory_bytes,
            runtime_root,
        };
    }

    fn mobile_linux(&self) -> Option<Arc<dyn MobileLinuxRuntime>> {
        self.runtime_configuration
            .read()
            .expect("local-app runtime configuration poisoned")
            .mobile_linux
            .clone()
    }

    pub(crate) fn build_lock(&self) -> Arc<Mutex<()>> {
        LOCAL_APP_BUILD_LOCK
            .get_or_init(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub(crate) async fn has_active_runtimes(&self) -> bool {
        !self.runtimes.lock().await.is_empty()
    }

    pub(crate) fn attach_llm(
        &self,
        llm: Arc<crate::mobile::local_apps_profile::SharedLlm>,
    ) -> Result<(), Arc<crate::mobile::local_apps_profile::SharedLlm>> {
        self.llm.set(llm)
    }

    /// Bind the native host facts. Set once, at the same composition-root
    /// call site as [`Self::attach_agent_executor`].
    pub(crate) fn attach_host_environment(
        &self,
        environment: platform_api::MobileHostEnvironment,
    ) -> Result<(), platform_api::MobileHostEnvironment> {
        self.host_environment.set(environment)
    }

    /// Bind the session catalog the scaffold commit renames the pinned init
    /// session in. Set once, at the same composition-root call site as
    /// [`Self::attach_host_environment`].
    pub(crate) fn attach_session_catalog(
        &self,
        catalog: SessionCatalog,
    ) -> Result<(), SessionCatalog> {
        self.session_catalog.set(catalog)
    }

    /// The confirmed native target for apps generated on this host.
    ///
    /// `None` when no host facts are attached or the client could not classify
    /// the device — an absent context already means unknown, so neither case
    /// invents a platform.
    fn host_device_context(&self) -> Option<local_apps::DeviceContext> {
        self.host_environment
            .get()
            .and_then(local_apps::DeviceContext::from_host_environment)
    }

    pub(crate) fn attach_agent_executor(
        &self,
        executor: Arc<dyn LocalAppsAgentExecutor>,
    ) -> Result<(), Arc<dyn LocalAppsAgentExecutor>> {
        self.agent_executor.set(executor)
    }

    pub(crate) fn attach_device(
        &self,
        device: Arc<crate::mobile::local_apps_device::SharedDeviceCapabilities>,
    ) -> Result<(), Arc<crate::mobile::local_apps_device::SharedDeviceCapabilities>> {
        self.device.set(device)
    }

    pub(crate) async fn reset_permissions(&self, app_id: &str) -> Result<(), String> {
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let current = load_permissions(&layout).map_err(|error| error.to_string())?;
        let mut reset = AppPermissions::default();
        reset.grant_epoch = current.grant_epoch.saturating_add(1).max(1);
        save_permissions(&layout, &reset).map_err(|error| error.to_string())?;
        self.session_permissions.lock().await.revoke_app(app_id);
        for outcome in self
            .cancel_background_tasks_for_revoked_schedule(
                app_id,
                "background scheduling permission was revoked",
            )
            .await?
        {
            self.emit_background_task_changed(&outcome).await;
        }
        Ok(())
    }

    fn service(&self) -> Result<Arc<AppService>, String> {
        self.service
            .get()
            .cloned()
            .ok_or_else(|| "local apps service is still starting; retry shortly".into())
    }

    fn request_id(&self, prefix: &str) -> String {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{id}")
    }

    fn validate_workflow_run_id(workflow_run_id: &str) -> Result<(), String> {
        if workflow_run_id.is_empty()
            || workflow_run_id.len() > 128
            || !workflow_run_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return Err("workflow_run_id is invalid".into());
        }
        Ok(())
    }

    fn mcp_candidate_rel(
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<PathBuf, local_apps::AppError> {
        local_apps::ids::validate_app_id(app_id)?;
        Self::validate_workflow_run_id(workflow_run_id)
            .map_err(local_apps::AppError::InvalidRequest)?;
        Ok(PathBuf::from("apps")
            .join(app_id)
            .join(local_apps::manifest::MCP_CATALOGS_DIR)
            .join("candidates")
            .join(format!("{workflow_run_id}.json")))
    }

    fn save_mcp_candidate(
        &self,
        app_id: &str,
        workflow_run_id: &str,
        candidate: &PersistedMcpCandidate,
    ) -> Result<(), String> {
        let path =
            Self::mcp_candidate_rel(app_id, workflow_run_id).map_err(|error| error.to_string())?;
        let mut body = serde_json::to_vec_pretty(candidate)
            .map_err(|error| format!("serialize MCP candidate: {error}"))?;
        body.push(b'\n');
        platform_api::rooted_fs::atomic_write(
            &self.root,
            &path,
            &body,
            platform_api::rooted_fs::AtomicWriteOptions::default(),
        )
        .map_err(|error| local_apps::AppError::from_fs("write MCP candidate", &error).to_string())
    }

    fn load_mcp_candidate(
        &self,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<PersistedMcpCandidate, String> {
        let path =
            Self::mcp_candidate_rel(app_id, workflow_run_id).map_err(|error| error.to_string())?;
        let body = platform_api::rooted_fs::read_to_string_limited(&self.root, &path, 512 * 1024)
            .map_err(|error| {
            local_apps::AppError::from_fs("read MCP candidate", &error).to_string()
        })?;
        serde_json::from_str(&body).map_err(|error| format!("parse MCP candidate: {error}"))
    }

    fn delete_mcp_candidate_state(
        &self,
        layout: &AppLayout,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<(), String> {
        local_apps::delete_candidate_journal(layout).map_err(|error| error.to_string())?;
        let relative =
            Self::mcp_candidate_rel(app_id, workflow_run_id).map_err(|error| error.to_string())?;
        match platform_api::rooted_fs::remove_file(&self.root, &relative) {
            Ok(()) | Err(platform_api::FsError::NotFound(_)) => {}
            Err(error) => {
                return Err(local_apps::AppError::from_fs(
                    "delete create-only MCP candidate",
                    &error,
                )
                .to_string());
            }
        }
        // r1-backlog-scaffold-build-03 / r1-failure-paths-008: this run's
        // create staging (`.lingxi-build-state/template-candidates/<app>/
        // <run>/staging/`, holding a full template copy plus the confirmed
        // name/brief in evidence.json) is never read again once the
        // candidate state it gates is torn down. Reclaim exactly that
        // subtree — NOT the whole `<run>/` directory, which is also home to
        // `validated-selection.json` and the selector capability
        // (`local_app_template_catalog::journal_path`). Two of this
        // function's callers are NOT terminal for the workflow run: the
        // `Err(error)` arm after a five-minute native-approval timeout, and
        // the `McpCreateCandidateGuard` Drop on a Stop / teardown mid-sheet.
        // Deleting the validated selection there would turn a retry of
        // `LocalAppStageCreate` with the same handle into
        // `validated_selection_missing`, i.e. a dead create run.
        Self::remove_create_staging(&self.root, app_id, workflow_run_id)
    }

    /// Reclaim `<root>/.lingxi-build-state/template-candidates/<app>/<run>/
    /// staging/` — the run's template copy, `evidence.json`, `design-spec
    /// .json` and staged MCP flow contexts (see `create_staging_root`).
    fn remove_create_staging(
        root: &Path,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<(), String> {
        let staging = root
            .join(".lingxi-build-state/template-candidates")
            .join(app_id)
            .join(workflow_run_id)
            .join("staging");
        Self::remove_owned_path(&staging)
    }

    fn load_active_mcp_flow_contexts(
        &self,
        layout: &AppLayout,
    ) -> Result<BTreeMap<String, local_apps::AppMcpFlowContext>, String> {
        let rel = layout
            .workspace_rel()
            .join(".lingxi/mcp-flow-contexts.json");
        let body = platform_api::rooted_fs::read_to_string_limited(&self.root, &rel, 512 * 1024)
            .map_err(|error| match error {
                platform_api::FsError::NotFound(_) => {
                    "mcp_flow_contexts_missing: Host could not resolve any trusted MCP flow contexts for this app".to_string()
                }
                other => local_apps::AppError::from_fs("read MCP flow contexts", &other).to_string(),
            })?;
        serde_json::from_str(&body).map_err(|error| format!("parse MCP flow contexts: {error}"))
    }

    fn build_mcp_review_surface(
        manifest: &local_apps::AppManifest,
        validated: &local_apps::ValidatedAppMcpProposal,
        active_catalog: Option<&local_apps::AppMcpCatalogRef>,
        create_context: Option<&CreateProposalContext>,
    ) -> Value {
        json!({
            "appId": validated.proposal.app_id,
            "manifestRevision": manifest.revision,
            "summary": validated.proposal.summary,
            "proposalSha256": validated.proposal_sha256,
            "toolSurfaceSha256": validated.tool_surface_sha256,
            "tools": validated.tools.iter().map(|tool| json!({
                "name": tool.definition.name,
                "title": tool.definition.title,
                "description": tool.definition.description,
                "inputSchema": tool.definition.input_schema,
                "outputSchema": tool.definition.output_schema,
                "flow": tool.flow,
                "ceiling": tool.ceiling,
            })).collect::<Vec<_>>(),
            "requiredFlowChanges": validated.proposal.required_flow_changes,
            "excludedCapabilities": validated.proposal.excluded_capabilities,
            "previousActiveCatalog": active_catalog,
            "initialCreate": create_context.map(|context| json!({
                "templateId": context.selection.template_id,
                "templateCatalogDigest": context.selection.catalog_digest,
                "templateSha256": context.selection.template_sha256,
                "templateInventorySha256": context.selection.template_inventory_sha256,
                "runtimeProfile": context.selection.runtime_profile,
                "surface": context.selection.surface,
                "selectionReason": context.selection.reason,
                "rejectedCandidates": context.selection.rejected,
                "stagingEvidence": context.staging_evidence,
                "designSpecSha256": context.design_spec_sha256,
                "designSpec": context.design_spec,
            })),
        })
    }

    async fn wait_for_native_approval(
        &self,
        pending: &Mutex<HashMap<String, PendingNativeApproval>>,
        request_id: String,
        app_id: &str,
        event: AppEventDto,
    ) -> Result<bool, String> {
        self.wait_for_native_approval_with_timeout(
            pending,
            request_id,
            app_id,
            event,
            APPROVAL_TIMEOUT,
        )
        .await
    }

    /// Same as [`Self::wait_for_native_approval`] with an injectable deadline.
    /// Production always goes through the wrapper above, which fixes it at
    /// `APPROVAL_TIMEOUT`; tests call this directly with a short duration so
    /// the `Err(_)` timeout arm (r4-tests-honesty-05) does not cost the suite
    /// five real minutes.
    async fn wait_for_native_approval_with_timeout(
        &self,
        pending: &Mutex<HashMap<String, PendingNativeApproval>>,
        request_id: String,
        app_id: &str,
        event: AppEventDto,
        deadline: Duration,
    ) -> Result<bool, String> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut requests = pending.lock().await;
            // r1-backlog-native-confirmation-04: a caller that drops this
            // future mid-await (task cancellation, request abandonment) skips
            // every arm below, so the entry it inserted here would otherwise
            // never be removed — its `sender` is dead (the paired `receiver`
            // dropped with the future) but the map still holds it forever,
            // and the duplicate guard below would then refuse every later
            // approval for this app. Prune dead entries first so a stale one
            // never blocks a fresh request.
            requests.retain(|_, request| !request.sender.is_closed());
            if requests.values().any(|request| request.app_id == app_id) {
                return Err(
                    "approval_pending: this Local App already has a pending approval".into(),
                );
            }
            requests.insert(
                request_id.clone(),
                PendingNativeApproval {
                    app_id: app_id.to_string(),
                    sender,
                    event: event.clone(),
                },
            );
        }
        self.event_sink.emit(ClientEvent::AppEvent { event }).await;
        match timeout(deadline, receiver).await {
            Ok(Ok(approved)) => Ok(approved),
            Ok(Err(_)) => {
                pending.lock().await.remove(&request_id);
                let message = "native Local App approval was cancelled".to_string();
                // r1-failure-paths-002: proactively retract the native sheet on
                // every client. Clients discard their pending approval keyed on
                // `request_id` (e.g. iOS's `discardPendingApproval`), so a bare
                // `Err` here left a stale sheet on screen indefinitely — the
                // caller only learns the workflow failed, never the client.
                //
                // ⚠️ KNOWN-WRONG `code`, deliberately left as-is:
                // `LocalAppPluginErrorCodeDto` (client-protocol/src/local_apps.rs)
                // has NO cancelled/aborted/timed-out member, and `code` is not
                // optional. Android DOES render it —
                // `LocalAppsViewModel.localizedPluginError` maps
                // `PROPOSAL_INVALID` to `local_apps_error_proposal_invalid`
                // ("MCP 提案未通过校验，需要继续修改。"), which is wrong copy for a
                // cancelled/timed-out sheet and doubly wrong on the CREATE
                // confirmation path below (`wait_for_native_approval` also serves
                // `CreateConfirmationRequested`, which has no MCP proposal at
                // all). iOS ignores `code` and shows `message`, so iOS is correct
                // already; the dismissal itself keys on `request_id` on BOTH
                // platforms and works regardless of `code`.
                // Fixing the copy is a four-file, cross-platform change this
                // module cannot land alone: append (never insert — UniFFI encodes
                // by declaration ordinal) an `ApprovalAborted` member at the END
                // of `LocalAppPluginErrorCodeDto`; add its string to the five
                // `clients/translations/*.json` sources and regenerate the iOS
                // `.xcstrings` / Android `strings.xml` catalogs; add the arm to
                // `LocalAppsViewModel.localizedPluginError`; then emit it here and
                // in the timeout arm below.
                self.event_sink
                    .emit(ClientEvent::AppEvent {
                        event: AppEventDto::LocalAppOperationFailed {
                            app_id: Some(app_id.to_string()),
                            code: LocalAppPluginErrorCodeDto::ProposalInvalid,
                            message: message.clone(),
                            request_id: Some(request_id.clone()),
                        },
                    })
                    .await;
                Err(message)
            }
            Err(_) => {
                pending.lock().await.remove(&request_id);
                let message = "native Local App approval timed out".to_string();
                // r1-failure-paths-002: same retraction as the cancelled arm
                // above, for the timeout arm — including its KNOWN-WRONG `code`
                // and the four-file fix that would correct it.
                self.event_sink
                    .emit(ClientEvent::AppEvent {
                        event: AppEventDto::LocalAppOperationFailed {
                            app_id: Some(app_id.to_string()),
                            code: LocalAppPluginErrorCodeDto::ProposalInvalid,
                            message: message.clone(),
                            request_id: Some(request_id.clone()),
                        },
                    })
                    .await;
                Err(message)
            }
        }
    }

    /// The gates the user is being told still have to pass, rendered inside
    /// both native approval sheets.
    ///
    /// r1-never-wired-06: this used to be a hardcoded two-row constant, so a
    /// `create_without_mcp` confirmation — an app that will have no MCP surface
    /// at all — still promised the user an "MCP schema, Flow, call and
    /// isolation QA" gate that nothing would ever run for it. Derive the MCP
    /// row from the tool surface actually being approved instead; `ui_runner`
    /// stays unconditional because it is TRUE unconditionally (this Host has no
    /// UI verification runner, which is the same fact `ui_verification` reports
    /// as `Unavailable` in `emit_managed_mcp_inventory`).
    ///
    /// Both `gate_id`s are consumed: iOS localizes them by id in
    /// `LocalAppApprovalSheets.swift`'s `localizedGateLabel` /
    /// `localizedGateDetail`, and an unknown future id falls back to the
    /// `label`/`detail` sent from here.
    fn pending_verification_gates(proposed_tools: usize) -> Vec<LocalAppGateStatusDto> {
        let mut gates = Vec::new();
        if proposed_tools > 0 {
            gates.push(LocalAppGateStatusDto {
                gate_id: "mcp_qa".into(),
                label: "MCP schema, Flow, call and isolation QA".into(),
                status: LocalAppVerificationStatusDto::Pending,
                available: true,
                detail: None,
            });
        }
        gates.push(LocalAppGateStatusDto {
            gate_id: "ui_runner".into(),
            label: "UI verification runner".into(),
            status: LocalAppVerificationStatusDto::Unavailable,
            available: false,
            detail: Some("UI evidence is unavailable on this host.".into()),
        });
        gates
    }

    fn create_selection_for_run(
        &self,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<crate::mobile::local_app_template_catalog::ValidatedTemplateSelection, String> {
        let handle = self.create_selection_handle_for_run(app_id, workflow_run_id)?;
        crate::mobile::local_app_template_catalog::resolve_typed(
            &self.root,
            app_id,
            workflow_run_id,
            &handle,
        )
    }

    fn create_selection_handle_for_run(
        &self,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<String, String> {
        let relative = PathBuf::from(".lingxi-build-state/template-candidates")
            .join(app_id)
            .join(workflow_run_id)
            .join("validated-selection.json");
        let body =
            platform_api::rooted_fs::read_to_string_limited(&self.root, &relative, 256 * 1024)
                .map_err(|error| format!("validated_selection_missing: {error}"))?;
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| format!("validated_selection_invalid: {error}"))?;
        let handle = value
            .get("handle")
            .and_then(Value::as_str)
            .ok_or_else(|| "validated_selection_invalid: handle is missing".to_string())?;
        Ok(handle.to_string())
    }

    fn create_staging_root(&self, app_id: &str, workflow_run_id: &str, handle: &str) -> PathBuf {
        self.root
            .join(".lingxi-build-state/template-candidates")
            .join(app_id)
            .join(workflow_run_id)
            .join("staging")
            .join(handle)
    }

    fn load_create_proposal_context(
        &self,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<CreateProposalContext, String> {
        let selection = self.create_selection_for_run(app_id, workflow_run_id)?;
        let staging_handle = self.create_selection_handle_for_run(app_id, workflow_run_id)?;
        let staging_root = self.create_staging_root(app_id, workflow_run_id, &staging_handle);
        let evidence_path = staging_root.join("evidence.json");
        let evidence_body = std::fs::read_to_string(&evidence_path)
            .map_err(|error| format!("create_staging_evidence_missing: {error}"))?;
        let staging_evidence: Value = serde_json::from_str(&evidence_body)
            .map_err(|error| format!("create_staging_evidence_invalid: {error}"))?;
        // WP5: `stage_create` is the only production writer of this evidence
        // file and always persists the confirmed name/brief onto it (see
        // above); a missing field here means staging is corrupt, not that the
        // caller may fall back to the shell record's `untitled` placeholder.
        let name = staging_evidence
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "create_staging_evidence_invalid: staged evidence is missing name".to_string()
            })?
            .to_string();
        let brief = staging_evidence
            .get("brief")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "create_staging_evidence_invalid: staged evidence is missing brief".to_string()
            })?
            .to_string();
        // WP-MCP-intent: `stage_create` always writes this key, `null` when
        // no interview ran this call — so a MISSING key (not merely a `null`
        // value) means staging predates this field or is corrupt, while a
        // present-but-malformed value is a genuine parse error. Either way
        // this is the only place a staged intent is read back for the
        // create-in-progress path, so a silent fallback to `None` here would
        // let a corrupt or unparsable staged answer quietly turn into
        // "never asked" on the committed record.
        let mcp_intent = match staging_evidence.get("mcpIntent") {
            None => {
                return Err(
                    "create_staging_evidence_invalid: staged evidence is missing mcpIntent"
                        .to_string(),
                )
            }
            Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value::<local_apps::AppMcpIntent>(value.clone()).map_err(
                    |error| {
                        format!("create_staging_evidence_invalid: staged mcpIntent is malformed: {error}")
                    },
                )?,
            ),
        };
        let design_path = staging_root.join("design-spec.json");
        let (design_spec, design_spec_sha256) = match std::fs::read(&design_path) {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("create_staging_design_invalid: {error}"))?;
                (Some(value), Some(format!("{:x}", Sha256::digest(&bytes))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
            Err(error) => return Err(format!("create_staging_design_missing: {error}")),
        };
        // r4-failure-paths-09: `stage_create` already digested the exact
        // design-spec bytes it committed and recorded them on this same
        // evidence file as `designSpecSha256`, but this function used to
        // re-hash whatever `design-spec.json` it found and hand THAT back as
        // the run's `design_spec_sha256` without ever comparing the two. A
        // `design-spec.json` left in the staging directory by an earlier,
        // partial run — or one absent when evidence says it was staged — was
        // therefore adopted silently. Compare, including the present/absent
        // case, and fail with the same named error the other corruption
        // checks above use.
        let recorded_design_sha256 =
            match staging_evidence.get("designSpecSha256") {
                None => return Err(
                    "create_staging_evidence_invalid: staged evidence is missing designSpecSha256"
                        .to_string(),
                ),
                Some(Value::Null) => None,
                Some(Value::String(digest)) => Some(digest.as_str()),
                Some(_) => {
                    return Err(
                        "create_staging_evidence_invalid: staged designSpecSha256 is not a string"
                            .to_string(),
                    )
                }
            };
        if recorded_design_sha256 != design_spec_sha256.as_deref() {
            return Err(format!(
                "create_staging_evidence_invalid: staged design-spec.json digest {observed:?} \
                 does not match the designSpecSha256 recorded at stage time ({recorded:?})",
                observed = design_spec_sha256.as_deref(),
                recorded = recorded_design_sha256,
            ));
        }
        let context_candidates = [
            staging_root.join(".lingxi/mcp-flow-contexts.json"),
            staging_root.join("template/.lingxi/mcp-flow-contexts.json"),
        ];
        let mut last_error = None;
        for path in context_candidates {
            match std::fs::read_to_string(&path) {
                Ok(body) => {
                    let contexts: BTreeMap<String, local_apps::AppMcpFlowContext> =
                        serde_json::from_str(&body).map_err(|error| {
                            format!("parse staged MCP flow contexts {}: {error}", path.display())
                        })?;
                    return Ok(CreateProposalContext {
                        selection,
                        staging_evidence,
                        design_spec,
                        design_spec_sha256,
                        contexts,
                        name,
                        brief,
                        mcp_intent,
                    });
                }
                Err(error) => {
                    last_error = Some(format!("{}: {error}", path.display()));
                }
            }
        }
        Err(format!(
            "mcp_flow_contexts_missing: Host could not resolve trusted staged MCP flow contexts for app {app_id} run {workflow_run_id} ({})",
            last_error.unwrap_or_else(|| "no staging context candidates".into())
        ))
    }

    fn load_create_scaffold_seed(
        &self,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<CreateScaffoldSeed, String> {
        let create_context = self.load_create_proposal_context(app_id, workflow_run_id)?;
        let staging_handle = self.create_selection_handle_for_run(app_id, workflow_run_id)?;
        let template_root = self
            .create_staging_root(app_id, workflow_run_id, &staging_handle)
            .join("template");
        let metadata = std::fs::symlink_metadata(&template_root)
            .map_err(|error| format!("create_staging_template_missing: {error}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(
                "create_staging_template_invalid: template root is not a real directory".into(),
            );
        }
        // r1-backlog-scaffold-build-07: `stage_create` digests every staged
        // template file into `evidence.json`'s `stagedFiles` at write time
        // (see the `staged_files.push` loop above it), but until now nothing
        // ever read those digests back. The multi-minute native confirmation
        // wait sits BETWEEN that write and this seed being landed into the
        // real workspace, so tampering with the staged template in that
        // window went undetected — the staging-time `materialized != bytes`
        // check runs before the wait even starts and cannot cover it.
        let staged_files = create_context
            .staging_evidence
            .get("stagedFiles")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                "create_staging_template_invalid: staged evidence is missing stagedFiles"
                    .to_string()
            })?;
        for entry in staged_files {
            let path = entry.get("path").and_then(Value::as_str).ok_or_else(|| {
                "create_staging_template_invalid: a stagedFiles entry is missing path".to_string()
            })?;
            let expected_sha256 = entry.get("sha256").and_then(Value::as_str).ok_or_else(|| {
                format!(
                    "create_staging_template_invalid: stagedFiles entry {path:?} is missing sha256"
                )
            })?;
            let relative_path = std::path::Path::new(path);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(format!(
                    "create_staging_template_invalid: unsafe stagedFiles path {path:?}"
                ));
            }
            let bytes = std::fs::read(template_root.join(relative_path)).map_err(|error| {
                format!(
                    "create_staging_template_invalid: staged file changed since staging \
                     (unreadable) {path:?}: {error}"
                )
            })?;
            let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
            if actual_sha256 != expected_sha256 {
                return Err(format!(
                    "create_staging_template_invalid: staged file changed since staging {path:?}"
                ));
            }
        }
        Ok(CreateScaffoldSeed {
            selection: create_context.selection,
            template_root,
            contexts: create_context.contexts,
            name: create_context.name,
            brief: create_context.brief,
            mcp_intent: create_context.mcp_intent,
        })
    }

    /// Raise the native MCP-proposal approval sheet for a prepared candidate.
    ///
    /// Only an ALREADY-SCAFFOLDED app reaches here: the create-time
    /// confirmation is the user's plan approval (see
    /// [`crate::mobile::local_apps_prepare`]), so there is no second sheet to answer
    /// for an app whose shape is not yet fixed.
    async fn request_mcp_candidate_approval(
        &self,
        app_id: &str,
        workflow_run_id: &str,
        manifest: &local_apps::AppManifest,
        candidate: &PersistedMcpCandidate,
    ) -> Result<bool, String> {
        let proposed = mcp_tool_surfaces_from_candidate(candidate)?;
        // Read before `proposed` is moved into the request DTO below.
        let proposed_tool_count = proposed.len();
        let current = if let Some(active) = manifest.active_mcp_catalog.as_ref() {
            let layout = self.layout(app_id)?;
            let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                .map_err(|error| error.to_string())?;
            mcp_tool_surfaces_from_catalog(&catalog)?
        } else {
            Vec::new()
        };
        let request_id = self.request_id("app-mcp-proposal-approval");
        let event = AppEventDto::McpProposalApprovalRequested {
            request: LocalAppMcpProposalApprovalRequestDto {
                request_id: request_id.clone(),
                app_id: app_id.to_string(),
                workflow_run_id: workflow_run_id.to_string(),
                summary: candidate.validated.proposal.summary.clone(),
                proposal_sha256: candidate.validated.proposal_sha256.clone(),
                approval_contract_sha256: candidate.approval_contract_sha256.clone(),
                tool_surface_sha256: candidate.validated.tool_surface_sha256.clone(),
                tool_diffs: mcp_tool_diffs(current, proposed),
                required_flow_changes: candidate.validated.proposal.required_flow_changes.clone(),
                excluded_capabilities: candidate.validated.proposal.excluded_capabilities.clone(),
                pending_gates: Self::pending_verification_gates(proposed_tool_count),
            },
        };
        self.wait_for_native_approval(
            &self.pending_mcp_proposal_approvals,
            request_id,
            app_id,
            event,
        )
        .await
    }

    async fn issue_dependency_change_receipt(
        &self,
        app_id: &str,
        baseline: DependencyBaselineIdentity,
        requested_json: Vec<u8>,
        effective_package_json: Vec<u8>,
        summary: Vec<DependencyChange>,
    ) -> Result<PendingDependencyChangeReceipt, String> {
        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms + APPROVAL_RECEIPT_TTL.as_millis() as u64;
        let mut receipts = self.pending_dependency_change_receipts.lock().await;
        if let Some(current) = receipts.get(app_id) {
            if current.claimed && current.expires_at_ms >= issued_at_ms {
                return Err(format!(
                    "dependency change receipt {} is already in use for app {}",
                    current.receipt_id, app_id
                ));
            }
        }
        let receipt = PendingDependencyChangeReceipt {
            receipt_id: uuid::Uuid::new_v4().to_string(),
            app_id: app_id.to_string(),
            baseline,
            requested_json,
            effective_package_json,
            issued_at_ms,
            expires_at_ms,
            summary,
            claimed: false,
        };
        receipts.insert(app_id.to_string(), receipt.clone());
        Ok(receipt)
    }

    async fn claim_dependency_change_receipt(
        &self,
        app_id: &str,
        receipt_id: &str,
    ) -> Result<PendingDependencyChangeReceipt, String> {
        let mut receipts = self.pending_dependency_change_receipts.lock().await;
        let Some(current) = receipts.get_mut(app_id) else {
            return Err(format!(
                "dependency change receipt {receipt_id} is missing or was already consumed for app {app_id}"
            ));
        };
        if current.receipt_id != receipt_id {
            return Err(format!(
                "dependency change receipt {receipt_id} is stale or superseded for app {app_id}"
            ));
        }
        if current.expires_at_ms < now_ms() {
            return Err(format!(
                "dependency change receipt {receipt_id} expired for app {app_id}"
            ));
        }
        if current.claimed {
            return Err(format!(
                "dependency change receipt {receipt_id} is already in use for app {app_id}"
            ));
        }
        current.claimed = true;
        Ok(current.clone())
    }

    async fn release_dependency_change_receipt_claim(&self, app_id: &str, receipt_id: &str) {
        let mut receipts = self.pending_dependency_change_receipts.lock().await;
        if let Some(current) = receipts.get_mut(app_id) {
            if current.receipt_id == receipt_id {
                current.claimed = false;
            }
        }
    }

    async fn consume_dependency_change_receipt(&self, app_id: &str, receipt_id: &str) {
        let mut receipts = self.pending_dependency_change_receipts.lock().await;
        if receipts
            .get(app_id)
            .is_some_and(|current| current.receipt_id == receipt_id)
        {
            receipts.remove(app_id);
        }
    }

    fn layout(&self, app_id: &str) -> Result<AppLayout, String> {
        AppLayout::new(&self.root, app_id).map_err(|error| error.to_string())
    }

    fn app_dependency_marker(layout: &AppLayout) -> PathBuf {
        layout
            .root()
            .join(layout.workspace_rel())
            .join("node_modules/vite/bin/vite.js")
    }

    pub(crate) fn toolchain_for_layout(layout: &AppLayout) -> Result<RuntimeToolchain, String> {
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let binding = manifest.runtime_profile.as_ref().ok_or_else(|| {
            "runtime profile binding is required to select a toolchain".to_string()
        })?;
        toolchain_for_binding(binding).map_err(|error| error.to_string())
    }

    fn dependency_store_root(&self, toolchain_key: &str) -> PathBuf {
        let toolchain_key_dir = toolchain_key
            .chars()
            .map(|ch| match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' => ch,
                _ => '_',
            })
            .collect::<String>();
        self.root
            .join("dependency-cache")
            .join("pnpm")
            .join(toolchain_key_dir)
    }

    fn dependency_snapshot_root(&self, lock_digest: &str, toolchain_key: &str) -> PathBuf {
        self.dependency_store_root(toolchain_key)
            .join("snapshots")
            .join(lock_digest)
    }

    async fn dependency_snapshot_lock(&self, lock_digest: &str) -> Arc<Mutex<()>> {
        let mut locks = self.dependency_snapshot_locks.lock().await;
        locks
            .entry(lock_digest.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn workspace_dependencies_ready(layout: &AppLayout) -> Result<bool, String> {
        Self::workspace_dependencies_ready_path(&layout.root().join(layout.workspace_rel()))
    }

    fn dependency_lock_digest(layout: &AppLayout) -> Result<String, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let path = workspace.join("pnpm-lock.yaml");
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("read pnpm-lock.yaml {}: {error}", path.display()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn dependency_inputs_match(layout: &AppLayout) -> Result<bool, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let expected_files: Vec<(&'static str, Vec<u8>)> = match manifest.runtime_profile.as_ref() {
            Some(binding) => {
                let contract =
                    crate::mobile::local_app_runtime_profiles::contract_for_binding(binding)
                        .map_err(|error| error.to_string())?;
                let has_snapshot = manifest.dependency_snapshot.is_some();
                contract
                    .managed_files
                    .iter()
                    .chain(
                        contract
                            .editable_files
                            .iter()
                            .filter(|(relative, _)| *relative == "app/mcp-widget/package.json"),
                    )
                    .copied()
                    .filter(|(relative, _)| {
                        matches!(
                            *relative,
                            "package.json"
                                | "pnpm-lock.yaml"
                                | "pnpm-workspace.yaml"
                                | "app/mcp-widget/package.json"
                        )
                    })
                    .map(|(relative, bytes)| {
                        let expected = match (relative, has_snapshot) {
                            ("package.json", true) => std::fs::read(workspace.join(
                                crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
                            ))
                            .map_err(|error| {
                                format!(
                                    "read {}: {error}",
                                    crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL
                                )
                            })?,
                            ("pnpm-lock.yaml", true) => std::fs::read(
                                workspace
                                    .join(crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL),
                            )
                            .map_err(|error| {
                                format!(
                                    "read {}: {error}",
                                    crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL
                                )
                            })?,
                            _ => bytes.to_vec(),
                        };
                        Ok((relative, expected))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
            None => return Ok(false),
        };
        for (relative, expected) in expected_files {
            let path = workspace.join(relative);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(format!(
                        "inspect dependency input {}: {error}",
                        path.display()
                    ))
                }
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Ok(false);
            }
            if std::fs::read(&path)
                .map_err(|error| format!("read dependency input {}: {error}", path.display()))?
                != expected
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn validate_dependency_package_name(package: &str) -> Result<(), String> {
        if package.is_empty() || package.len() > 214 {
            return Err(format!("invalid package name {package:?}"));
        }
        let chars: Vec<char> = package.chars().collect();
        if package.starts_with('@') {
            let slash_count = chars.iter().filter(|&&ch| ch == '/').count();
            if slash_count != 1 {
                return Err(format!(
                    "scoped package name must contain one slash: {package:?}"
                ));
            }
        } else if chars.iter().filter(|&&ch| ch == '/').count() != 0 {
            return Err(format!(
                "unscoped package name must not contain slash: {package:?}"
            ));
        }
        if chars.iter().any(|&ch| {
            !(ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || matches!(ch, '@' | '/' | '.' | '_' | '-'))
        }) {
            return Err(format!(
                "package name {package:?} must use lowercase npm characters only"
            ));
        }
        Ok(())
    }

    fn validate_dependency_version(version: &str) -> Result<(), String> {
        let trimmed = version.trim();
        if trimmed.is_empty() {
            return Err("dependency version must not be empty".into());
        }
        let lowered = trimmed.to_ascii_lowercase();
        for forbidden in [
            "file:",
            "link:",
            "portal:",
            "patch:",
            "workspace:",
            "catalog:",
            "catalogs:",
            "npm:",
            "git+",
            "github:",
            "http://",
            "https://",
            "../",
            "./",
            "/",
            "\\",
        ] {
            if lowered.contains(forbidden) {
                return Err(format!(
                    "dependency version {version:?} must resolve from the npm registry only"
                ));
            }
        }
        Ok(())
    }

    fn dependency_manifest_bytes(
        contract: &crate::mobile::local_app_runtime_profiles::RuntimeProfileContract,
    ) -> Result<&'static [u8], String> {
        contract
            .managed_files
            .iter()
            .find(|(relative, _)| *relative == "package.json")
            .map(|(_, bytes)| *bytes)
            .ok_or_else(|| {
                format!(
                    "runtime profile {} r{} is missing package.json",
                    contract.family, contract.revision
                )
            })
    }

    fn load_requested_dependency_map(workspace: &Path) -> Result<BTreeMap<String, String>, String> {
        let bytes = Self::read_regular_dependency_input_bytes(
            workspace,
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
        )?;
        let path = workspace.join(crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL);
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let dependencies = value
            .get("dependencies")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{} must contain an object `dependencies`", path.display()))?;
        let mut map = BTreeMap::new();
        for (package, version) in dependencies {
            let version = version.as_str().ok_or_else(|| {
                format!(
                    "{} dependency {package:?} must map to a string version",
                    path.display()
                )
            })?;
            map.insert(package.clone(), version.to_string());
        }
        Ok(map)
    }

    fn read_regular_dependency_input_bytes(
        workspace: &Path,
        relative: &str,
    ) -> Result<Vec<u8>, String> {
        let path = workspace.join(relative);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect {}: {error}", path.display()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(format!(
                "dependencies_dirty: dependency input must be a regular file: {}",
                path.display()
            ));
        }
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))
    }

    fn load_trusted_dependency_baseline(
        layout: &AppLayout,
        dependency_record: &local_apps::AppDependencyRecord,
    ) -> Result<
        (
            local_apps::AppRuntimeProfileBinding,
            &'static crate::mobile::local_app_runtime_profiles::RuntimeProfileContract,
            BTreeMap<String, String>,
            DependencyBaselineIdentity,
        ),
        String,
    > {
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let binding = manifest.runtime_profile.clone().ok_or_else(|| {
            format!(
                "app {} has no runtime profile yet; scaffold it before editing dependencies",
                layout.app_id()
            )
        })?;
        let snapshot = manifest.dependency_snapshot.clone().ok_or_else(|| {
            format!(
                "app {} has no verified dependency snapshot; rerun scaffold or dependency install before editing dependencies",
                layout.app_id()
            )
        })?;
        if snapshot.verified_profile_contract_sha256 != binding.contract_sha256 {
            return Err(format!(
                "runtime_contract_corrupt: app {} dependency snapshot no longer matches runtime profile {}",
                layout.app_id(),
                binding.family
            ));
        }
        if dependency_record.lockfile_sha256.as_deref() != Some(snapshot.lockfile_sha256.as_str())
            || dependency_record.toolchain_key.as_deref() != Some(snapshot.toolchain_key.as_str())
        {
            return Err(format!(
                "dependencies_dirty: app {} dependency record no longer matches the verified snapshot; this cannot be repaired by re-editing package.json/lockfile — report the drift to the user/workflow as a finding instead of retrying",
                layout.app_id()
            ));
        }
        let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
            .map_err(|error| error.to_string())?;
        let workspace = layout.root().join(layout.workspace_rel());
        let requested_bytes = Self::read_regular_dependency_input_bytes(
            &workspace,
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
        )?;
        if crate::mobile::local_app_runtime_profiles::hash_bytes(&requested_bytes)
            != snapshot.requested_sha256
        {
            return Err(format!(
                "dependencies_dirty: app {} requested dependency baseline was modified outside the host-managed dependency flow",
                layout.app_id()
            ));
        }
        let effective_package_bytes = Self::read_regular_dependency_input_bytes(
            &workspace,
            crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
        )?;
        if crate::mobile::local_app_runtime_profiles::hash_bytes(&effective_package_bytes)
            != snapshot.package_sha256
        {
            return Err(format!(
                "dependencies_dirty: app {} effective dependency baseline drifted from the verified snapshot",
                layout.app_id()
            ));
        }
        let lockfile_bytes = Self::read_regular_dependency_input_bytes(
            &workspace,
            crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL,
        )?;
        if crate::mobile::local_app_runtime_profiles::hash_bytes(&lockfile_bytes)
            != snapshot.lockfile_sha256
        {
            return Err(format!(
                "dependencies_dirty: app {} lockfile baseline drifted from the verified snapshot",
                layout.app_id()
            ));
        }
        let requested_dependencies = Self::load_requested_dependency_map(&workspace)?;
        let expected_package_json =
            Self::build_effective_package_json(contract, &requested_dependencies)?;
        if effective_package_bytes != expected_package_json {
            return Err(format!(
                "dependencies_dirty: app {} package.json no longer matches the committed dependency baseline",
                layout.app_id()
            ));
        }
        let baseline = DependencyBaselineIdentity {
            dependency_snapshot_sha256: manifest
                .dependency_snapshot_hash()
                .map_err(|error| error.to_string())?,
            requested_sha256: snapshot.requested_sha256,
            package_sha256: snapshot.package_sha256,
            lockfile_sha256: snapshot.lockfile_sha256,
            toolchain_key: snapshot.toolchain_key,
            contract_sha256: binding.contract_sha256.clone(),
        };
        Ok((binding, contract, requested_dependencies, baseline))
    }

    fn serialize_requested_dependency_map(
        dependencies: &BTreeMap<String, String>,
    ) -> Result<Vec<u8>, String> {
        let dependencies = dependencies
            .iter()
            .map(|(package, version)| (package.clone(), Value::String(version.clone())))
            .collect::<Map<String, Value>>();
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(Map::from_iter([(
            "dependencies".to_string(),
            Value::Object(dependencies),
        )])))
        .map_err(|error| format!("serialize requested dependency manifest: {error}"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn build_effective_package_json(
        contract: &crate::mobile::local_app_runtime_profiles::RuntimeProfileContract,
        requested_dependencies: &BTreeMap<String, String>,
    ) -> Result<Vec<u8>, String> {
        let template = Self::dependency_manifest_bytes(contract)?;
        let mut package_json: Value = serde_json::from_slice(template)
            .map_err(|error| format!("parse runtime profile package.json: {error}"))?;
        let package_object = package_json
            .as_object_mut()
            .ok_or_else(|| "runtime profile package.json must be an object".to_string())?;
        let mut dependencies = contract
            .core_packages
            .iter()
            .map(|(package, version)| {
                (
                    (*package).to_string(),
                    Value::String((*version).to_string()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (package, version) in requested_dependencies {
            dependencies.insert(package.clone(), Value::String(version.clone()));
        }
        package_object.insert(
            "dependencies".to_string(),
            Value::Object(Map::from_iter(dependencies)),
        );
        let mut bytes = serde_json::to_vec_pretty(&package_json)
            .map_err(|error| format!("serialize effective package.json: {error}"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn prepare_dependency_change(
        layout: &AppLayout,
        dependency_record: &local_apps::AppDependencyRecord,
        changes_value: &Value,
    ) -> Result<
        (
            local_apps::AppRuntimeProfileBinding,
            DependencyBaselineIdentity,
            Vec<DependencyChange>,
            Vec<u8>,
            Vec<u8>,
        ),
        String,
    > {
        let changes: Vec<DependencyChange> = serde_json::from_value(changes_value.clone())
            .map_err(|error| {
                format!("invalid_argument: changes must be an array of objects: {error}")
            })?;
        if changes.is_empty() {
            return Err("invalid_argument: changes must not be empty".into());
        }
        let (binding, contract, mut requested_dependencies, baseline) =
            Self::load_trusted_dependency_baseline(layout, dependency_record)?;
        let original = requested_dependencies.clone();
        let core_packages = contract
            .core_packages
            .iter()
            .map(|(package, _)| *package)
            .collect::<std::collections::HashSet<_>>();
        for change in &changes {
            Self::validate_dependency_package_name(&change.package)?;
            if core_packages.contains(change.package.as_str()) {
                return Err(format!(
                    "dependency {} is core to runtime profile {} and can only change through runtime profile migration",
                    change.package, binding.family
                ));
            }
            match change.kind {
                DependencyChangeKind::Add | DependencyChangeKind::Update => {
                    let version = change.version.as_deref().ok_or_else(|| {
                        format!(
                            "dependency {} requires a version for {:?}",
                            change.package, change.kind
                        )
                    })?;
                    Self::validate_dependency_version(version)?;
                    requested_dependencies.insert(change.package.clone(), version.to_string());
                }
                DependencyChangeKind::Remove => {
                    if change.version.is_some() {
                        return Err(format!(
                            "dependency {} remove must not include a version",
                            change.package
                        ));
                    }
                    if requested_dependencies.remove(&change.package).is_none() {
                        return Err(format!(
                            "dependency {} is not currently requested by this app",
                            change.package
                        ));
                    }
                }
            }
        }
        if requested_dependencies == original {
            return Err("dependency change makes no observable change".into());
        }
        let requested_json = Self::serialize_requested_dependency_map(&requested_dependencies)?;
        let effective_package_json =
            Self::build_effective_package_json(contract, &requested_dependencies)?;
        Ok((
            binding,
            baseline,
            changes,
            requested_json,
            effective_package_json,
        ))
    }

    fn prepare_dependency_staging(layout: &AppLayout) -> Result<PathBuf, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let state_root = workspace.join(".lingxi-build-state");
        let staging = state_root.join("dependency-staging");
        if let Ok(entries) = std::fs::read_dir(&state_root) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("node_modules.previous-")
                {
                    Self::remove_owned_path(&entry.path())?;
                }
            }
        }
        Self::remove_owned_path(&staging)?;
        std::fs::create_dir_all(&staging)
            .map_err(|error| format!("create dependency staging directory: {error}"))?;
        for file in ["package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"] {
            let source = workspace.join(file);
            let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
                format!("inspect dependency input {}: {error}", source.display())
            })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "dependency input must be a regular file: {}",
                    source.display()
                ));
            }
            std::fs::copy(&source, staging.join(file))
                .map_err(|error| format!("stage dependency input {}: {error}", source.display()))?;
        }
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        if let Some(binding) = manifest.runtime_profile.as_ref() {
            let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(binding)
                .map_err(|error| error.to_string())?;
            if let Some((relative, bytes)) = contract
                .editable_files
                .iter()
                .find(|(relative, _)| *relative == "app/mcp-widget/package.json")
            {
                // Workspace dependency resolution must see the same widget importer
                // as the frozen lock. Use the pinned profile manifest, never arbitrary
                // editable package metadata as an unapproved dependency request.
                let target = staging.join(relative);
                std::fs::create_dir_all(target.parent().expect("widget manifest parent"))
                    .map_err(|error| format!("create staged widget importer: {error}"))?;
                std::fs::write(target, bytes)
                    .map_err(|error| format!("stage widget importer: {error}"))?;
            }
        }
        Ok(staging)
    }

    fn reset_dependency_staging_node_modules(staging: &Path) -> Result<(), String> {
        let node_modules = staging.join("node_modules");
        Self::remove_owned_path(&node_modules)?;
        std::fs::create_dir_all(&node_modules).map_err(|error| {
            format!(
                "recreate dependency staging node_modules {}: {error}",
                node_modules.display()
            )
        })
    }

    fn dependency_install_request(
        build_mount: &MountSpec,
        store_mount: &MountSpec,
        dependency_staging_guest_path: String,
        build_state_root: &str,
        memory_mb: u32,
        network: NetworkPolicy,
        frozen_lockfile: bool,
        lockfile_only: bool,
        no_runtime: bool,
        toolchain: RuntimeToolchain,
    ) -> LinuxCommandRequest {
        let mut env = BTreeMap::new();
        env.insert("PATH".into(), toolchain.path().into());
        env.insert("CI".into(), "1".into());
        env.insert("HOME".into(), format!("{build_state_root}/home"));
        env.insert("TMPDIR".into(), format!("{build_state_root}/tmp"));
        env.insert("TMP".into(), format!("{build_state_root}/tmp"));
        env.insert("TEMP".into(), format!("{build_state_root}/tmp"));
        env.insert(
            "XDG_CACHE_HOME".into(),
            format!("{build_state_root}/xdg-cache"),
        );
        env.insert(
            "XDG_CONFIG_HOME".into(),
            format!("{build_state_root}/xdg-config"),
        );
        env.insert(
            "XDG_DATA_HOME".into(),
            format!("{build_state_root}/xdg-data"),
        );
        env.insert("PNPM_HOME".into(), format!("{build_state_root}/pnpm-home"));
        env.insert(
            "COREPACK_HOME".into(),
            format!("{build_state_root}/corepack"),
        );

        let mut args = vec!["install".into()];
        if lockfile_only {
            args.push("--lockfile-only".into());
        }
        args.push(if frozen_lockfile {
            "--frozen-lockfile".into()
        } else {
            "--no-frozen-lockfile".into()
        });
        args.push("--ignore-scripts".into());
        if no_runtime {
            args.push("--no-runtime".into());
        }
        args.extend([
            "--prefer-offline".into(),
            "--store-dir".into(),
            platform_api::local_app_paths::LOCAL_APP_DEPENDENCY_STORE.to_string(),
            "--reporter=append-only".into(),
        ]);

        LinuxCommandRequest {
            command: toolchain.pnpm_command().into(),
            args,
            cwd: Some(dependency_staging_guest_path),
            env,
            stdin: None,
            timeout_ms: Some(DEPENDENCY_INSTALL_TIMEOUT.as_millis() as u64),
            network,
            resource_limits: ResourceLimits {
                max_memory_mb: Some(memory_mb),
                ..ResourceLimits::default()
            },
            mounts: vec![build_mount.clone(), store_mount.clone()],
        }
    }

    async fn run_dependency_install_command(
        runtime: &dyn MobileLinuxRuntime,
        request: LinuxCommandRequest,
    ) -> Result<(), String> {
        let network = request.network;
        let resource_limits = request.resource_limits;
        let frozen_lockfile = request.args.iter().any(|arg| arg == "--frozen-lockfile");
        let install_span = tracing::debug_span!(
            "local_app_dependency_install",
            network = ?network,
            frozen_lockfile = frozen_lockfile,
        );
        let _perf = LocalAppPerfDiagnosticTimer::start(if frozen_lockfile {
            "dependency_pnpm_frozen_install"
        } else {
            "dependency_pnpm_resolve"
        });
        match runtime.run_isolated(request).instrument(install_span).await {
            Ok(result) => {
                result
                    .enforcement
                    .ensure_for(network, resource_limits)
                    .map_err(|error| error.to_string())?;
                if result.timed_out || result.cancelled || result.exit_code != 0 {
                    let detail = if !result.stderr.trim().is_empty() {
                        result.stderr
                    } else {
                        result.stdout
                    };
                    Err(format!(
                        "pnpm install failed (exit_code={}, timed_out={}, cancelled={}): {}",
                        result.exit_code,
                        result.timed_out,
                        result.cancelled,
                        detail.chars().take(8_000).collect::<String>()
                    ))
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(format!("dependency install worker failed: {error}")),
        }
    }

    fn remove_owned_path(path: &Path) -> Result<(), String> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect owned path {}: {error}", path.display())),
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(path)
                .map_err(|error| format!("remove owned directory {}: {error}", path.display()))
        } else {
            std::fs::remove_file(path)
                .map_err(|error| format!("remove owned file {}: {error}", path.display()))
        }
    }

    fn dependency_snapshot_is_ready(
        snapshot_root: &Path,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> Result<bool, String> {
        let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_verify");
        let root_metadata = match std::fs::symlink_metadata(snapshot_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect dependency snapshot {}: {error}",
                    snapshot_root.display()
                ))
            }
        };
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return Ok(false);
        }
        let marker = snapshot_root.join(DEPENDENCY_SNAPSHOT_READY_FILE);
        let marker_metadata = match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect dependency snapshot marker {}: {error}",
                    marker.display()
                ))
            }
        };
        if !marker_metadata.is_file()
            || marker_metadata.file_type().is_symlink()
            || marker_metadata.len() > MAX_DEPENDENCY_SNAPSHOT_READY_BYTES
        {
            return Ok(false);
        }
        let marker_contents = match std::fs::read_to_string(&marker) {
            Ok(contents) => contents,
            Err(_) => return Ok(false),
        };
        let mut marker_lines = marker_contents.lines();
        let expected_version = DEPENDENCY_SNAPSHOT_VERSION.to_string();
        if marker_lines.next() != Some(expected_version.as_str())
            || marker_lines.next() != Some(lock_digest)
            || marker_lines.next() != Some(toolchain_key)
        {
            return Ok(false);
        }
        let Some(expected_tree_digest) = marker_lines.next() else {
            return Ok(false);
        };
        if marker_lines.next().is_some() || expected_tree_digest.is_empty() {
            return Ok(false);
        }
        let node_modules = snapshot_root.join("node_modules");
        let node_modules_metadata = match std::fs::symlink_metadata(&node_modules) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("inspect dependency snapshot node_modules: {error}")),
        };
        if !node_modules_metadata.is_dir() || node_modules_metadata.file_type().is_symlink() {
            return Ok(false);
        }
        // Snapshot directories are read-only by convention, not by an OS
        // sandbox boundary. Re-hash the bytes on every lookup so a tree that
        // changed after an earlier successful lookup cannot inherit stale
        // in-process trust from its marker or path alone.
        if validate_dependency_tree(&node_modules).is_err() {
            return Ok(false);
        }
        let actual_tree_digest = match dependency_tree_digest(&node_modules) {
            Ok(digest) => digest,
            Err(_) => return Ok(false),
        };
        if actual_tree_digest != expected_tree_digest {
            return Ok(false);
        }
        if read_verified_dependency_inventory(
            snapshot_root,
            lock_digest,
            expected_tree_digest,
            toolchain_key,
        )?
        .is_none()
        {
            return Ok(false);
        }
        let vite = node_modules.join("vite/bin/vite.js");
        let vite_metadata = match std::fs::symlink_metadata(&vite) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect dependency snapshot Vite executable: {error}"
                ))
            }
        };
        Ok(vite_metadata.is_file() && !vite_metadata.file_type().is_symlink())
    }

    fn workspace_dependencies_match_snapshot(
        workspace: &Path,
        snapshot_root: &Path,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> Result<bool, String> {
        if !Self::workspace_dependencies_ready_path(workspace)?
            || !Self::dependency_snapshot_is_ready(snapshot_root, lock_digest, toolchain_key)?
        {
            return Ok(false);
        }
        let marker = snapshot_root.join(DEPENDENCY_SNAPSHOT_READY_FILE);
        let expected_tree_digest = dependency_tree_digest_from_marker(&marker)?;
        let Some(expected_tree_digest) = expected_tree_digest else {
            return Ok(false);
        };
        let expected_attestation =
            dependency_attestation(lock_digest, &expected_tree_digest, toolchain_key);
        let attestation = workspace.join(WORKSPACE_DEPENDENCY_ATTESTATION_FILE);
        if std::fs::read_to_string(&attestation).ok().as_deref()
            == Some(expected_attestation.as_str())
        {
            return Ok(true);
        }
        let workspace_node_modules = workspace.join("node_modules");
        validate_dependency_tree(&workspace_node_modules)?;
        if dependency_tree_digest(&workspace_node_modules)? != expected_tree_digest {
            return Ok(false);
        }
        crate::mobile::local_apps_build::write_file(
            workspace,
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
            expected_attestation.as_bytes(),
            true,
        )
        .map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn workspace_dependencies_ready_path(workspace: &Path) -> Result<bool, String> {
        let vite = workspace.join("node_modules/vite/bin/vite.js");
        match std::fs::symlink_metadata(&vite) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "workspace dependency marker is invalid: {} must be a regular file",
                vite.display()
            )),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "inspect workspace dependency marker {}: {error}",
                vite.display()
            )),
        }
    }

    fn publish_dependency_snapshot(
        source_node_modules: &Path,
        snapshot_root: &Path,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> Result<(), String> {
        if Self::dependency_snapshot_is_ready(snapshot_root, lock_digest, toolchain_key)? {
            return Ok(());
        }
        match std::fs::symlink_metadata(snapshot_root) {
            Ok(_) => Self::remove_owned_path(snapshot_root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect existing dependency snapshot {}: {error}",
                    snapshot_root.display()
                ))
            }
        }
        let parent = snapshot_root.parent().ok_or_else(|| {
            format!(
                "dependency snapshot has no parent: {}",
                snapshot_root.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create dependency snapshot parent: {error}"))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let staging_root = parent.join(format!(".{lock_digest}.staging-{stamp}"));
        Self::remove_owned_path(&staging_root)?;
        std::fs::create_dir_all(&staging_root)
            .map_err(|error| format!("create dependency snapshot staging: {error}"))?;
        let staged_node_modules = staging_root.join("node_modules");
        let prepare_result = (|| -> Result<(), String> {
            clone_or_copy_tree(source_node_modules, &staged_node_modules)
                .map_err(|error| error.to_string())?;
            if !Self::workspace_dependencies_ready_path(&staging_root)? {
                return Err(format!(
                    "staged Vite executable was not produced at {}",
                    staged_node_modules.join("vite/bin/vite.js").display()
                ));
            }
            validate_dependency_tree(&staged_node_modules)?;
            let tree_digest = dependency_tree_digest(&staged_node_modules)?;
            {
                let _perf =
                    LocalAppPerfDiagnosticTimer::start("dependency_inventory_collect_publish");
                let mut packages = BTreeMap::new();
                collect_installed_packages(&staged_node_modules, &mut packages)?;
                write_verified_dependency_inventory(
                    &staging_root,
                    lock_digest,
                    &tree_digest,
                    &packages,
                    toolchain_key,
                )?;
            }
            std::fs::write(
                staging_root.join(DEPENDENCY_SNAPSHOT_READY_FILE),
                dependency_attestation(lock_digest, &tree_digest, toolchain_key),
            )
            .map_err(|error| format!("write dependency snapshot marker: {error}"))?;
            make_dependency_files_read_only(&staged_node_modules)
                .map_err(|error| format!("protect dependency snapshot: {error}"))
        })();
        if let Err(error) = prepare_result {
            let _ = Self::remove_owned_path(&staging_root);
            return Err(format!("prepare dependency snapshot: {error}"));
        }
        if let Err(error) = std::fs::rename(&staging_root, snapshot_root) {
            let _ = Self::remove_owned_path(&staging_root);
            if snapshot_root.exists()
                && Self::dependency_snapshot_is_ready(snapshot_root, lock_digest, toolchain_key)?
            {
                return Ok(());
            }
            return Err(format!("publish dependency snapshot: {error}"));
        }
        Ok(())
    }

    fn materialize_dependency_snapshot(
        snapshot_root: &Path,
        staging_root: &Path,
    ) -> Result<(), String> {
        let destination = staging_root.join("node_modules");
        Self::remove_owned_path(&destination)?;
        clone_or_copy_tree(&snapshot_root.join("node_modules"), &destination)
            .map_err(|error| format!("materialize dependency snapshot: {error}"))
    }

    /// Adopt the read-only dependency tree staged into the app bundle as this
    /// device's snapshot for `lock_digest`.
    ///
    /// `stage-local-app-runtime.py` records `pnpm_lock_sha256` in
    /// `runtime-manifest.json` after validating the tree against the pinned
    /// template lockfile, so the seed carries its own identity and the match is
    /// exact rather than assumed. An app whose lockfile has drifted from the
    /// bundled one gets `false` and falls through to a real install -- the seed
    /// is an accelerator, never an override.
    ///
    /// Publication goes through `publish_dependency_snapshot` rather than
    /// writing an attestation here, so the tree digest and the marker are
    /// produced by the same code that validates every other snapshot.
    fn adopt_bundled_dependency_seed(
        runtime_root: &Path,
        lock_digest: &str,
        snapshot_root: &Path,
        toolchain_key: &str,
    ) -> Result<bool, String> {
        if toolchain_key != RuntimeToolchain::Current.key() {
            return Ok(false);
        }
        let manifest_path = runtime_root.join(BUNDLED_SEED_MANIFEST_FILE);
        let manifest = match std::fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            // A build that ships no seed is the ordinary Store configuration,
            // not a fault: fall through to a real install.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "read bundled dependency seed manifest {}: {error}",
                    manifest_path.display()
                ))
            }
        };
        let manifest: Value = serde_json::from_slice(&manifest).map_err(|error| {
            format!(
                "parse bundled dependency seed manifest {}: {error}",
                manifest_path.display()
            )
        })?;
        let seed_digest = manifest
            .get("pnpm_lock_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "bundled dependency seed manifest is missing pnpm_lock_sha256: {}",
                    manifest_path.display()
                )
            })?;
        if seed_digest != lock_digest {
            return Ok(false);
        }
        let seed_node_modules = runtime_root.join("node_modules");
        match std::fs::symlink_metadata(&seed_node_modules) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "bundled dependency seed is not a directory: {}",
                    seed_node_modules.display()
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect bundled dependency seed {}: {error}",
                    seed_node_modules.display()
                ))
            }
        }
        Self::publish_dependency_snapshot(
            &seed_node_modules,
            snapshot_root,
            lock_digest,
            toolchain_key,
        )?;
        Ok(true)
    }

    fn promote_dependency_tree(workspace: &Path, staging: &Path) -> Result<(), String> {
        let staged_node_modules = staging.join("node_modules");
        let marker = staged_node_modules.join("vite/bin/vite.js");
        let marker_metadata = std::fs::symlink_metadata(&marker)
            .map_err(|error| format!("inspect staged Vite executable: {error}"))?;
        if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
            return Err(format!(
                "staged Vite executable is not a regular file: {}",
                marker.display()
            ));
        }
        let current = workspace.join("node_modules");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let backup = workspace
            .join(".lingxi-build-state")
            .join(format!("node_modules.previous-{stamp}"));
        let had_current = std::fs::symlink_metadata(&current).is_ok();
        if had_current {
            let metadata = std::fs::symlink_metadata(&current)
                .map_err(|error| format!("inspect current dependencies: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("workspace node_modules must not be a symlink".into());
            }
            std::fs::rename(&current, &backup)
                .map_err(|error| format!("stage previous dependencies: {error}"))?;
        }
        if let Err(error) = std::fs::rename(&staged_node_modules, &current) {
            if had_current {
                let _ = std::fs::rename(&backup, &current);
            }
            return Err(format!("promote staged dependencies: {error}"));
        }
        // r4-engine-core-05: the rename above already committed the new
        // `node_modules` tree — the install SUCCEEDED. Everything from here
        // is post-commit litter cleanup; propagating either failure via `?`
        // would turn that success into `Err` (and, on the create path, roll
        // back the whole scaffold) while a full, working dependency tree
        // sits in the workspace. Best-effort only: warn and leave the
        // leftovers for a later sweep instead of disowning the promotion.
        if had_current {
            if let Err(error) = Self::remove_owned_path(&backup) {
                tracing::warn!(
                    path = %backup.display(),
                    error = %error,
                    "node_modules backup cleanup deferred after a successful dependency promotion"
                );
            }
        }
        if let Err(error) = Self::remove_owned_path(staging) {
            tracing::warn!(
                path = %staging.display(),
                error = %error,
                "dependency staging cleanup deferred after a successful dependency promotion"
            );
        }
        Ok(())
    }

    async fn install_dependencies_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let wait = input.get("wait").and_then(Value::as_bool).unwrap_or(false);
        let dependency = self.ensure_dependency_install(&app_id, wait).await?;
        Ok(json!({
            "ok": dependency.state == AppDependencyState::Ready,
            "app_id": app_id,
            "dependencies": dependency,
        }))
    }

    pub(crate) async fn ensure_dependency_install(
        &self,
        app_id: &str,
        wait: bool,
    ) -> Result<local_apps::AppDependencyRecord, String> {
        let service = self.service()?;
        service
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let toolchain = Self::toolchain_for_layout(&layout)?;
        let toolchain_key = toolchain.key();
        let workspace = layout.root().join(layout.workspace_rel());
        if !Self::dependency_inputs_match(&layout)? {
            if load_manifest(&layout)
                .map_err(|error| error.to_string())?
                .runtime_profile
                .is_some()
            {
                // The message must not name
                // `LocalAppConfirmDependencyChange`/`LocalAppUpdateDependencies`, but NOT
                // because they are uncallable — an earlier revision of this comment said
                // they "are not in `LOCAL_APP_TOOLS`", which is false: both have rows
                // (`local_apps_tools.rs:112`/`:116`) pinned by
                // `dependency_review_operations_are_wired_as_builtin_tools`. Only their old
                // `mcp__local_apps__*` spelling is refused, and the builtin path is the
                // supported one (`local_apps_mcp.rs`'s
                // `the_mcp_surface_no_longer_serves_the_static_host_operations`).
                //
                // The real reason: neither can repair THIS failure. `update_dependencies`
                // requires a `receipt_id` and claims a host-minted dependency-change
                // receipt (:10087-10107); the drift here is a hand-edited workspace
                // package.json/lockfile that no receipt describes. Naming them would send
                // the agent to a call that cannot fix it, so tell it to report the drift.
                return Err(
                    "dependencies_dirty: workspace package.json or pnpm-lock.yaml differs from the host-owned dependency snapshot; this cannot be repaired by re-editing package.json/lockfile — report the drift to the user/workflow as a finding instead of retrying"
                        .into(),
                );
            } else {
                let target = crate::mobile::local_apps_build::detect_build_target(&layout)
                    .map_err(|error| error.to_string())?;
                crate::mobile::local_apps_build::restore_host_managed_files(&workspace, target)
                    .map_err(|error| error.to_string())?;
            }
        }
        let dependency = service
            .dependency_record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let lock_digest = Self::dependency_lock_digest(&layout)?;
        if dependency.state == AppDependencyState::Ready
            && dependency.lockfile_sha256.as_deref() == Some(lock_digest.as_str())
            && dependency.toolchain_key.as_deref() == Some(toolchain_key)
            && Self::workspace_dependencies_match_snapshot(
                &workspace,
                &self.dependency_snapshot_root(&lock_digest, toolchain_key),
                &lock_digest,
                toolchain_key,
            )?
        {
            return Ok(dependency);
        }
        if dependency.state == AppDependencyState::Ready {
            service
                .queue_dependency_install(app_id)
                .await
                .map_err(|error| error.to_string())?;
        }
        let started = match service.start_dependency_install(app_id).await {
            Ok(_) => true,
            Err(local_apps::AppError::InvalidRequest(_)) => false,
            Err(error) => return Err(error.to_string()),
        };
        if started {
            let weak = self.weak_self();
            let app_id = app_id.to_string();
            tokio::spawn(async move {
                let Some(host) = weak.upgrade() else {
                    return;
                };
                host.run_dependency_install(app_id).await;
            });
        }
        if wait {
            return self.wait_for_dependency_install(app_id).await;
        }
        service
            .dependency_record(app_id)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn wait_for_dependency_install(
        &self,
        app_id: &str,
    ) -> Result<local_apps::AppDependencyRecord, String> {
        let service = self.service()?;
        timeout(DEPENDENCY_INSTALL_TIMEOUT, async {
            loop {
                let dependency = service
                    .dependency_record(app_id)
                    .await
                    .map_err(|error| error.to_string())?;
                match dependency.state {
                    AppDependencyState::Installing => sleep(DEPENDENCY_INSTALL_POLL_INTERVAL).await,
                    _ => return Ok(dependency),
                }
            }
        })
        .await
        .map_err(|_| {
            format!(
                "workspace dependency installation is still running after {} seconds",
                DEPENDENCY_INSTALL_TIMEOUT.as_secs()
            )
        })?
    }

    async fn finalize_dependency_install(
        &self,
        layout: &AppLayout,
        dependency_staging: &Path,
        expected_lock_digest: &str,
    ) -> Result<DependencyInstallCompletion, String> {
        let toolchain = Self::toolchain_for_layout(layout)?;
        let toolchain_key = toolchain.key();
        let workspace = layout.root().join(layout.workspace_rel());
        // The lockfile is host-managed, but re-check it immediately before
        // promotion so a concurrent restore/edit cannot publish a tree built
        // for an older digest into the live workspace.
        let before_promotion = Self::dependency_lock_digest(layout)?;
        if before_promotion != expected_lock_digest {
            return Err("dependency lock changed before promotion".into());
        }
        Self::promote_dependency_tree(&workspace, dependency_staging)?;
        if !Self::workspace_dependencies_ready(layout)? {
            return Err(format!(
                "dependency install finished but {} was not produced",
                Self::app_dependency_marker(layout).display()
            ));
        }
        let actual_lock_digest = Self::dependency_lock_digest(layout)?;
        if actual_lock_digest != expected_lock_digest {
            return Err("dependency lock changed while installation was running".into());
        }
        let snapshot_root = self.dependency_snapshot_root(expected_lock_digest, toolchain_key);
        let snapshot_marker = snapshot_root.join(DEPENDENCY_SNAPSHOT_READY_FILE);
        let tree_digest = dependency_tree_digest_from_marker(&snapshot_marker)?
            .ok_or_else(|| "dependency snapshot marker is malformed".to_string())?;
        let attestation = dependency_attestation(expected_lock_digest, &tree_digest, toolchain_key);
        crate::mobile::local_apps_build::write_file(
            &workspace,
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
            attestation.as_bytes(),
            true,
        )
        .map_err(|error| error.to_string())?;
        refresh_runtime_profile_snapshot(
            layout,
            &tree_digest,
            Some((&snapshot_root, expected_lock_digest)),
        )?;
        Ok(DependencyInstallCompletion {
            lockfile_sha256: actual_lock_digest,
            toolchain_key: toolchain_key.to_string(),
        })
    }

    fn capture_dependency_update_rollback(
        &self,
        layout: &AppLayout,
        previous_dependency: local_apps::AppDependencyRecord,
    ) -> Result<DependencyUpdateRollback, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let mut files = Vec::new();
        for relative in [
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
            crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::TREE_PROOF_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SNAPSHOT_FILE_REL,
            "package.json",
            "pnpm-lock.yaml",
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
        ] {
            let path = workspace.join(relative);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "read dependency rollback source {}: {error}",
                        path.display()
                    ))
                }
            };
            files.push(DependencyUpdateFileBackup { relative, bytes });
        }
        let manifest_path = layout.root().join(layout.manifest_rel());
        let manifest_bytes = std::fs::read(&manifest_path).map_err(|error| {
            format!(
                "read dependency rollback manifest {}: {error}",
                manifest_path.display()
            )
        })?;
        let node_modules = workspace.join("node_modules");
        let node_modules_backup = match std::fs::symlink_metadata(&node_modules) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err("workspace node_modules must not be a symlink".into());
                }
                if metadata.is_dir() {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default();
                    let backup = workspace
                        .join(".lingxi-build-state")
                        .join(format!("dependency-update-rollback-node_modules-{stamp}"));
                    Self::remove_owned_path(&backup)?;
                    clone_or_copy_tree(&node_modules, &backup).map_err(|error| {
                        format!("backup dependency tree {}: {error}", node_modules.display())
                    })?;
                    Some(backup)
                } else {
                    None
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "inspect dependency rollback tree {}: {error}",
                    node_modules.display()
                ))
            }
        };
        let build_root = layout.root().join(layout.build_rel(false));
        let build_backup_result = (|| -> Result<Option<PathBuf>, String> {
            match std::fs::symlink_metadata(&build_root) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err("promoted build root must not be a symlink".into());
                    }
                    if !metadata.is_dir() {
                        return Ok(None);
                    }
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default();
                    let backup = workspace
                        .join(".lingxi-build-state")
                        .join(format!("dependency-update-rollback-build-{stamp}"));
                    Self::remove_owned_path(&backup)?;
                    clone_or_copy_tree(&build_root, &backup).map_err(|error| {
                        format!("backup promoted build {}: {error}", build_root.display())
                    })?;
                    Ok(Some(backup))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!(
                    "inspect dependency rollback build {}: {error}",
                    build_root.display()
                )),
            }
        })();
        let build_backup = match build_backup_result {
            Ok(backup) => backup,
            Err(error) => {
                if let Some(backup) = &node_modules_backup {
                    let _ = Self::remove_owned_path(backup);
                }
                return Err(error);
            }
        };
        Ok(DependencyUpdateRollback {
            previous_dependency,
            files,
            manifest_bytes,
            node_modules_backup,
            build_backup,
        })
    }

    fn dependency_update_recovery_path(layout: &AppLayout) -> PathBuf {
        layout
            .root()
            .join(layout.workspace_rel())
            .join(DEPENDENCY_UPDATE_RECOVERY_FILE_REL)
    }

    fn dependency_update_workspace(layout: &AppLayout) -> Result<PathBuf, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let metadata = std::fs::symlink_metadata(&workspace).map_err(|error| {
            format!(
                "inspect dependency update workspace {}: {error}",
                workspace.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "dependency update workspace is not a real directory: {}",
                workspace.display()
            ));
        }
        Ok(workspace)
    }

    fn dependency_update_file_is_allowed(relative: &str) -> bool {
        matches!(
            relative,
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL
                | crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL
                | crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL
                | crate::mobile::local_app_runtime_profiles::TREE_PROOF_FILE_REL
                | crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL
                | crate::mobile::local_app_runtime_profiles::SNAPSHOT_FILE_REL
                | "package.json"
                | "pnpm-lock.yaml"
                | WORKSPACE_DEPENDENCY_ATTESTATION_FILE
        )
    }

    fn dependency_update_backup_name(
        layout: &AppLayout,
        backup: &Path,
        kind: &str,
    ) -> Result<String, String> {
        let state_root = layout
            .root()
            .join(layout.workspace_rel())
            .join(".lingxi-build-state");
        let relative = backup.strip_prefix(&state_root).map_err(|_| {
            format!(
                "dependency rollback backup is outside the build state: {}",
                backup.display()
            )
        })?;
        let mut components = relative.components();
        let Some(Component::Normal(name)) = components.next() else {
            return Err(format!(
                "dependency rollback backup is not a single safe path: {}",
                backup.display()
            ));
        };
        if components.next().is_some() {
            return Err(format!(
                "dependency rollback backup is not a single safe path: {}",
                backup.display()
            ));
        }
        let name = name.to_str().ok_or_else(|| {
            format!(
                "dependency rollback backup name is not UTF-8: {}",
                backup.display()
            )
        })?;
        if !name.starts_with(kind) || name.len() == kind.len() {
            return Err(format!(
                "dependency rollback backup has an invalid name: {}",
                backup.display()
            ));
        }
        Ok(name.to_string())
    }

    fn dependency_update_recovery_journal(
        layout: &AppLayout,
        rollback: &DependencyUpdateRollback,
        status: DependencyUpdateRecoveryStatus,
    ) -> Result<DependencyUpdateRecoveryJournal, String> {
        let files = rollback
            .files
            .iter()
            .map(|file| DependencyUpdateRecoveryFile {
                relative: file.relative.to_string(),
                bytes: file.bytes.clone(),
            })
            .collect();
        let journal = DependencyUpdateRecoveryJournal {
            schema_version: DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION,
            app_id: layout.app_id().to_string(),
            status,
            previous_dependency: rollback.previous_dependency.clone(),
            files,
            manifest_bytes: rollback.manifest_bytes.clone(),
            node_modules_backup: rollback
                .node_modules_backup
                .as_deref()
                .map(|backup| {
                    Self::dependency_update_backup_name(
                        layout,
                        backup,
                        "dependency-update-rollback-node_modules-",
                    )
                })
                .transpose()?,
            build_backup: rollback
                .build_backup
                .as_deref()
                .map(|backup| {
                    Self::dependency_update_backup_name(
                        layout,
                        backup,
                        "dependency-update-rollback-build-",
                    )
                })
                .transpose()?,
        };
        Self::validate_dependency_update_recovery_journal(layout, &journal)?;
        Ok(journal)
    }

    fn dependency_update_backup_path(
        layout: &AppLayout,
        name: &str,
        kind: &str,
    ) -> Result<PathBuf, String> {
        if name.is_empty() || !name.starts_with(kind) || name.len() == kind.len() {
            return Err(format!("invalid dependency rollback backup name: {name:?}"));
        }
        let path = Path::new(name);
        let mut components = path.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(format!(
                "dependency rollback backup must be a single path component: {name:?}"
            ));
        }
        let state_root = layout
            .root()
            .join(layout.workspace_rel())
            .join(".lingxi-build-state");
        Ok(state_root.join(name))
    }

    fn validate_dependency_update_recovery_journal(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        if journal.schema_version != DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION {
            return Err(format!(
                "dependency update recovery journal schemaVersion {} is unsupported (expected {})",
                journal.schema_version, DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION
            ));
        }
        if journal.app_id != layout.app_id() {
            return Err(format!(
                "dependency update recovery journal belongs to app {}, expected {}",
                journal.app_id,
                layout.app_id()
            ));
        }
        if journal.previous_dependency.app_id != layout.app_id() {
            return Err(format!(
                "dependency update recovery record belongs to app {}, expected {}",
                journal.previous_dependency.app_id,
                layout.app_id()
            ));
        }
        if journal.previous_dependency.schema_version != local_apps::APPS_SCHEMA_VERSION {
            return Err(format!(
                "dependency update recovery record schemaVersion {} is unsupported (expected {})",
                journal.previous_dependency.schema_version,
                local_apps::APPS_SCHEMA_VERSION
            ));
        }
        if journal.files.len() > 16 {
            return Err("dependency update recovery journal has too many files".into());
        }
        let mut total_bytes = journal.manifest_bytes.len();
        let mut seen = std::collections::HashSet::new();
        for file in &journal.files {
            if !Self::dependency_update_file_is_allowed(&file.relative) {
                return Err(format!(
                    "dependency update recovery journal contains an unexpected file: {}",
                    file.relative
                ));
            }
            let path = Path::new(&file.relative);
            if file.relative.is_empty()
                || path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
                || !seen.insert(file.relative.as_str())
            {
                return Err(format!(
                    "dependency update recovery journal contains an unsafe or duplicate file: {}",
                    file.relative
                ));
            }
            total_bytes = total_bytes.saturating_add(file.bytes.as_ref().map_or(0, Vec::len));
            if total_bytes > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES {
                return Err("dependency update recovery journal is too large".into());
            }
        }
        for expected in [
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
            crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::TREE_PROOF_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SNAPSHOT_FILE_REL,
            "package.json",
            "pnpm-lock.yaml",
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
        ] {
            if !seen.iter().any(|relative| *relative == expected) {
                return Err(format!(
                    "dependency update recovery journal is missing file: {expected}"
                ));
            }
        }
        let manifest: local_apps::AppManifest = serde_json::from_slice(&journal.manifest_bytes)
            .map_err(|error| format!("parse dependency update recovery manifest: {error}"))?;
        if manifest.app_id != layout.app_id() {
            return Err(format!(
                "dependency update recovery manifest belongs to app {}, expected {}",
                manifest.app_id,
                layout.app_id()
            ));
        }
        manifest
            .validate()
            .map_err(|error| format!("validate dependency update recovery manifest: {error}"))?;
        if let Some(name) = &journal.node_modules_backup {
            Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-node_modules-",
            )?;
        }
        if let Some(name) = &journal.build_backup {
            Self::dependency_update_backup_path(layout, name, "dependency-update-rollback-build-")?;
        }
        Ok(())
    }

    fn write_dependency_update_recovery_journal(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        Self::validate_dependency_update_recovery_journal(layout, journal)?;
        let mut bytes = serde_json::to_vec(journal)
            .map_err(|error| format!("serialize dependency update recovery journal: {error}"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES {
            return Err("dependency update recovery journal is too large".into());
        }
        let workspace = Self::dependency_update_workspace(layout)?;
        crate::mobile::local_apps_build::write_file(
            &workspace,
            DEPENDENCY_UPDATE_RECOVERY_FILE_REL,
            &bytes,
            true,
        )
        .map_err(|error| format!("write dependency update recovery journal: {error}"))
    }

    fn load_dependency_update_recovery_journal(
        layout: &AppLayout,
    ) -> Result<Option<DependencyUpdateRecoveryJournal>, String> {
        let _workspace = Self::dependency_update_workspace(layout)?;
        let path = Self::dependency_update_recovery_path(layout);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "inspect dependency update recovery journal {}: {error}",
                    path.display()
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "dependency update recovery journal is not a regular file: {}",
                path.display()
            ));
        }
        if metadata.len() > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES as u64 {
            return Err(format!(
                "dependency update recovery journal is too large: {}",
                path.display()
            ));
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "read dependency update recovery journal {}: {error}",
                path.display()
            )
        })?;
        if bytes.len() > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES {
            return Err(format!(
                "dependency update recovery journal is too large: {}",
                path.display()
            ));
        }
        let journal: DependencyUpdateRecoveryJournal = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse dependency update recovery journal: {error}"))?;
        Self::validate_dependency_update_recovery_journal(layout, &journal)?;
        Ok(Some(journal))
    }

    fn remove_dependency_update_recovery_journal(layout: &AppLayout) -> Result<(), String> {
        Self::remove_owned_path(&Self::dependency_update_recovery_path(layout))
    }

    fn restore_dependency_update_recovery_files(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        Self::validate_dependency_update_recovery_journal(layout, journal)?;
        let workspace = Self::dependency_update_workspace(layout)?;
        for file in &journal.files {
            let path = workspace.join(&file.relative);
            match &file.bytes {
                Some(bytes) => crate::mobile::local_apps_build::write_file(
                    &workspace,
                    &file.relative,
                    bytes,
                    true,
                )
                .map_err(|error| error.to_string())?,
                None => {
                    if std::fs::symlink_metadata(&path).is_ok() {
                        Self::remove_owned_path(&path)?;
                    }
                }
            }
        }
        let manifest: local_apps::AppManifest = serde_json::from_slice(&journal.manifest_bytes)
            .map_err(|error| format!("parse dependency rollback manifest: {error}"))?;
        local_apps::save_manifest(layout, &manifest).map_err(|error| error.to_string())?;

        let node_modules = workspace.join("node_modules");
        if std::fs::symlink_metadata(&node_modules).is_ok() {
            Self::remove_owned_path(&node_modules)?;
        }
        if let Some(name) = &journal.node_modules_backup {
            let backup = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-node_modules-",
            )?;
            let metadata = std::fs::symlink_metadata(&backup).map_err(|error| {
                format!(
                    "inspect dependency rollback tree {}: {error}",
                    backup.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "dependency rollback tree is not a real directory: {}",
                    backup.display()
                ));
            }
            clone_or_copy_tree(&backup, &node_modules).map_err(|error| {
                format!(
                    "restore dependency rollback tree {}: {error}",
                    node_modules.display()
                )
            })?;
        }

        let build_root = layout.root().join(layout.build_rel(false));
        if std::fs::symlink_metadata(&build_root).is_ok() {
            Self::remove_owned_path(&build_root)?;
        }
        if let Some(name) = &journal.build_backup {
            let backup = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-build-",
            )?;
            let metadata = std::fs::symlink_metadata(&backup).map_err(|error| {
                format!(
                    "inspect dependency rollback build {}: {error}",
                    backup.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "dependency rollback build is not a real directory: {}",
                    backup.display()
                ));
            }
            clone_or_copy_tree(&backup, &build_root).map_err(|error| {
                format!(
                    "restore dependency rollback build {}: {error}",
                    build_root.display()
                )
            })?;
        }
        local_apps::storage::save_dependency_record(layout.root(), &journal.previous_dependency)
            .map_err(|error| format!("restore dependency record: {error}"))
    }

    fn cleanup_dependency_update_recovery(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        Self::validate_dependency_update_recovery_journal(layout, journal)?;
        let workspace = Self::dependency_update_workspace(layout)?;
        let staging = workspace.join(".lingxi-build-state/dependency-staging");
        Self::remove_owned_path(&staging)?;
        if let Some(name) = &journal.node_modules_backup {
            let path = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-node_modules-",
            )?;
            Self::remove_owned_path(&path)?;
        }
        if let Some(name) = &journal.build_backup {
            let path = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-build-",
            )?;
            Self::remove_owned_path(&path)?;
        }
        Self::remove_dependency_update_recovery_journal(layout)
    }

    pub(crate) fn recover_dependency_updates_on_boot(root: &Path) -> Result<(), String> {
        let apps_root = root.join("apps");
        let metadata = match std::fs::symlink_metadata(&apps_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect local apps directory: {error}")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "local apps directory is not a real directory: {}",
                apps_root.display()
            ));
        }
        let mut first_error = None;
        let entries = std::fs::read_dir(&apps_root)
            .map_err(|error| format!("read local apps directory: {error}"))?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(format!("read local app entry: {error}"));
                    }
                    continue;
                }
            };
            let app_path = entry.path();
            let app_metadata = match std::fs::symlink_metadata(&app_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(format!(
                            "inspect local app entry {}: {error}",
                            app_path.display()
                        ));
                    }
                    continue;
                }
            };
            if app_metadata.file_type().is_symlink() || !app_metadata.is_dir() {
                continue;
            }
            let app_name = entry.file_name();
            let Some(app_id) = app_name.to_str() else {
                continue;
            };
            let Ok(layout) = AppLayout::new(root.to_path_buf(), app_id.to_string()) else {
                continue;
            };
            let has_journal =
                match std::fs::symlink_metadata(Self::dependency_update_recovery_path(&layout)) {
                    Ok(_) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(format!(
                                "inspect dependency update recovery journal for {app_id}: {error}"
                            ));
                        }
                        false
                    }
                };
            if !has_journal {
                continue;
            }
            let recovery_result = (|| -> Result<(), String> {
                let _build_lock =
                    local_apps::storage::lock_app_build(root, app_id).map_err(|error| {
                        format!("lock app {app_id} for dependency recovery: {error}")
                    })?;
                let Some(journal) = Self::load_dependency_update_recovery_journal(&layout)? else {
                    return Ok(());
                };
                match journal.status {
                    DependencyUpdateRecoveryStatus::InProgress => {
                        Self::restore_dependency_update_recovery_files(&layout, &journal)?;
                        // Once all authoritative old state is restored, make
                        // cleanup idempotent across another crash. A committed
                        // journal means "keep what is on disk"; the on-disk
                        // state is now the old state.
                        let mut cleaned = journal.clone();
                        cleaned.status = DependencyUpdateRecoveryStatus::Committed;
                        Self::write_dependency_update_recovery_journal(&layout, &cleaned)?;
                        Self::cleanup_dependency_update_recovery(&layout, &cleaned)
                    }
                    DependencyUpdateRecoveryStatus::Committed => {
                        Self::cleanup_dependency_update_recovery(&layout, &journal)
                    }
                }
            })();
            if let Err(error) = recovery_result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn restore_dependency_update_rollback(
        &self,
        service: &Arc<AppService>,
        app_id: &str,
        layout: &AppLayout,
        rollback: &DependencyUpdateRollback,
    ) -> Result<(), String> {
        if rollback.previous_dependency.app_id != app_id {
            return Err(format!(
                "dependency rollback record belongs to app {}, expected {app_id}",
                rollback.previous_dependency.app_id
            ));
        }
        let journal = Self::dependency_update_recovery_journal(
            layout,
            rollback,
            DependencyUpdateRecoveryStatus::InProgress,
        )?;
        Self::restore_dependency_update_recovery_files(layout, &journal)?;
        service
            .restore_dependency_record(rollback.previous_dependency.clone())
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn discard_dependency_update_rollback(rollback: DependencyUpdateRollback) {
        if let Some(backup) = rollback.node_modules_backup {
            if let Err(error) = Self::remove_owned_path(&backup) {
                tracing::warn!(path = %backup.display(), error = %error, "failed to remove dependency rollback tree after commit");
            }
        }
        if let Some(backup) = rollback.build_backup {
            if let Err(error) = Self::remove_owned_path(&backup) {
                tracing::warn!(path = %backup.display(), error = %error, "failed to remove build rollback tree after commit");
            }
        }
    }

    async fn dependency_install_once(
        &self,
        layout: &AppLayout,
        app_id: &str,
    ) -> Result<DependencyInstallCompletion, String> {
        let toolchain = Self::toolchain_for_layout(layout)?;
        let toolchain_key = toolchain.key();
        let workspace = layout.root().join(layout.workspace_rel());
        let lock_digest = match Self::dependency_lock_digest(layout) {
            Ok(digest) => digest,
            Err(error) => return Err(error),
        };
        let snapshot_root = self.dependency_snapshot_root(&lock_digest, toolchain_key);
        let snapshot_lock = self.dependency_snapshot_lock(&lock_digest).await;
        let lock_wait_span = tracing::debug_span!(
            "local_app_dependency_snapshot_lock_wait",
            app_id = %app_id,
            lock_digest = %lock_digest,
        );
        let _snapshot_guard = {
            let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_lock_wait");
            snapshot_lock.lock().instrument(lock_wait_span).await
        };
        let dependency_staging = match Self::prepare_dependency_staging(layout) {
            Ok(path) => path,
            Err(error) => return Err(error),
        };
        let mut snapshot_ready =
            match Self::dependency_snapshot_is_ready(&snapshot_root, &lock_digest, toolchain_key) {
                Ok(ready) => ready,
                Err(error) => {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
            };
        if !snapshot_ready {
            // First install on this device: the app bundle already carries a
            // tree resolved from the pinned template lockfile, so adopt it
            // instead of resolving the same 169 packages over the network
            // inside the Linux guest.
            //
            // A seed that cannot be adopted is never fatal. Store builds ship
            // none at all, an app whose lockfile has drifted legitimately needs
            // a real install, and a damaged bundle should degrade to the slow
            // path rather than make app creation impossible -- so failures are
            // recorded and fall through.
            if let Ok(runtime_root) = self.configured_runtime_root() {
                match Self::adopt_bundled_dependency_seed(
                    &runtime_root,
                    &lock_digest,
                    &snapshot_root,
                    toolchain_key,
                ) {
                    Ok(adopted) => snapshot_ready = adopted,
                    Err(error) => {
                        tracing::warn!(app_id = %app_id, error = %error, "bundled dependency seed could not be adopted");
                    }
                }
            }
        }
        if snapshot_ready {
            let snapshot_span = tracing::debug_span!(
                "local_app_dependency_snapshot_materialize",
                app_id = %app_id,
                lock_digest = %lock_digest,
                cache_hit = true,
            );
            let materialize_result = {
                let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_materialize");
                snapshot_span.in_scope(|| {
                    Self::materialize_dependency_snapshot(&snapshot_root, &dependency_staging)
                })
            };
            if let Err(error) = materialize_result {
                let _ = Self::remove_owned_path(&dependency_staging);
                return Err(error);
            }
            return self
                .finalize_dependency_install(layout, &dependency_staging, &lock_digest)
                .await;
        }
        let Some(runtime) = self.mobile_linux() else {
            let _ = Self::remove_owned_path(&dependency_staging);
            return Err(
                "the mobile Node runtime is unavailable for dependency installation".into(),
            );
        };
        let build_mount = MountSpec {
            host_path: workspace.clone(),
            guest_path: platform_api::local_app_paths::local_app_build_project(app_id, "store"),
            read_only: false,
            purpose: MountPurpose::LocalAppBuild,
        };
        let dependency_store = self.dependency_store_root(toolchain_key);
        if let Err(error) = std::fs::create_dir_all(&dependency_store) {
            let message = format!("create pnpm dependency store: {error}");
            let _ = Self::remove_owned_path(&dependency_staging);
            return Err(message);
        }
        let store_mount = MountSpec {
            host_path: dependency_store,
            guest_path: platform_api::local_app_paths::LOCAL_APP_DEPENDENCY_STORE.to_string(),
            read_only: false,
            purpose: MountPurpose::Shared,
        };
        let project_guest_path = build_mount.guest_path.clone();
        let dependency_staging_guest_path =
            format!("{project_guest_path}/.lingxi-build-state/dependency-staging");
        let build_state_root = format!("{project_guest_path}/.lingxi-build-state");
        let memory_mb =
            crate::mobile::local_apps_build::build_memory_budget_mb(self.physical_memory_bytes());
        let request = Self::dependency_install_request(
            &build_mount,
            &store_mount,
            dependency_staging_guest_path,
            &build_state_root,
            memory_mb,
            NetworkPolicy::Allowed,
            true,
            false,
            true,
            toolchain,
        );
        let outcome = Self::run_dependency_install_command(runtime.as_ref(), request).await;
        match outcome {
            Ok(()) => {
                let snapshot_span = tracing::debug_span!(
                    "local_app_dependency_snapshot_publish",
                    app_id = %app_id,
                    lock_digest = %lock_digest,
                    cache_hit = false,
                );
                let publish_result = {
                    let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_publish");
                    snapshot_span.in_scope(|| {
                        Self::publish_dependency_snapshot(
                            &dependency_staging.join("node_modules"),
                            &snapshot_root,
                            &lock_digest,
                            toolchain_key,
                        )
                    })
                };
                if let Err(error) = publish_result {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
                self.finalize_dependency_install(layout, &dependency_staging, &lock_digest)
                    .await
            }
            Err(error) => {
                let _ = Self::remove_owned_path(&dependency_staging);
                Err(error)
            }
        }
    }

    /// Scaffold-landing caller (`land_scaffold`'s commit path): the record's
    /// dependency state is NOT yet `Installing` here, so this wrapper owns
    /// the whole transition itself.
    async fn install_scaffold_dependencies(
        &self,
        service: &Arc<AppService>,
        app_id: &str,
        layout: &AppLayout,
    ) -> Result<(), String> {
        service
            .start_dependency_install(app_id)
            .await
            .map_err(|error| error.to_string())?;
        self.run_started_dependency_install(service, app_id, layout)
            .await
    }

    /// Run one dependency-install attempt assuming the record is ALREADY
    /// `Installing` -- e.g. `ensure_dependency_install` transitions it there
    /// itself before spawning `run_dependency_install`. Re-calling
    /// `start_dependency_install` here would always fail (the state is
    /// already `Installing`), leaving the record permanently stuck: this is
    /// the body both callers of the state transition above actually need to
    /// run, split out so each caller owns its own precondition.
    async fn run_started_dependency_install(
        &self,
        service: &Arc<AppService>,
        app_id: &str,
        layout: &AppLayout,
    ) -> Result<(), String> {
        match self.dependency_install_once(layout, app_id).await {
            Ok(completion) => service
                .complete_dependency_install_with_metadata(
                    app_id,
                    Some(completion.lockfile_sha256),
                    Some(completion.toolchain_key),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Err(error) => {
                let _ = service.fail_dependency_install(app_id, error.clone()).await;
                Err(error)
            }
        }
    }

    async fn run_dependency_install(&self, app_id: String) {
        let service = match self.service() {
            Ok(service) => service,
            Err(error) => {
                tracing::warn!(app_id = %app_id, error = %error, "dependency install lost service");
                return;
            }
        };
        let layout = match self.layout(&app_id) {
            Ok(layout) => layout,
            Err(error) => {
                let _ = service
                    .fail_dependency_install(&app_id, error.clone())
                    .await;
                tracing::warn!(app_id = %app_id, error = %error, "dependency install lost layout");
                return;
            }
        };
        // Serialize dependency mutation with build, checkpoint restore, and
        // physical deletion. The lock is intentionally held across the
        // isolated command so a delete cannot remove the workspace while pnpm
        // is still writing its app-local node_modules tree.
        let _build_lock = match local_apps::storage::lock_app_build(&self.root, &app_id) {
            Ok(lock) => lock,
            Err(error) => {
                let message = error.to_string();
                let _ = service
                    .fail_dependency_install(&app_id, message.clone())
                    .await;
                tracing::warn!(app_id = %app_id, error = %message, "dependency install could not lock app");
                return;
            }
        };
        // `ensure_dependency_install` already transitioned this record to
        // `Installing` before spawning us -- run the install body directly
        // rather than through `install_scaffold_dependencies`, which would
        // try to make that SAME transition again and always fail because the
        // state is already `Installing`, leaving the record stuck forever.
        if let Err(error) = self
            .run_started_dependency_install(&service, &app_id, &layout)
            .await
        {
            // Every other early return out of this worker already lands the
            // record in `Failed` (:4618, :4633). This arm must too: the record
            // is `Installing` when we get here, and the one Err this arm can
            // still receive with the install itself having succeeded is the
            // `complete_dependency_install_with_metadata` persist failure,
            // whose Ok-arm has made no state transition at all. Without this,
            // that window leaves the record terminal `Installing` -- nothing
            // on any boot path sweeps it, so every later `LocalAppBuild`
            // polls the full `DEPENDENCY_INSTALL_TIMEOUT` and fails.
            // Re-failing an already-`Failed` record is an idempotent rewrite
            // (`fail_dependency_install` has no state guard).
            let _ = service
                .fail_dependency_install(&app_id, error.clone())
                .await;
            tracing::warn!(app_id = %app_id, error = %error, "dependency install failed");
        }
    }

    pub(crate) fn physical_memory_bytes(&self) -> u64 {
        self.runtime_configuration
            .read()
            .expect("local-app runtime configuration poisoned")
            .physical_memory_bytes
    }

    pub(crate) fn configured_runtime_root(&self) -> Result<PathBuf, String> {
        self.runtime_configuration
            .read()
            .expect("local-app runtime configuration poisoned")
            .runtime_root
            .clone()
            .ok_or_else(|| {
                "local-app runtime root is not configured; stage the Vite local-app-runtime first"
                    .to_string()
            })
    }

    fn runtime_seed_ready(root: &Path) -> Result<bool, String> {
        let vite = root.join("node_modules/vite/bin/vite.js");
        let vite_ready = match std::fs::symlink_metadata(&vite) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "verified local-app runtime seed is invalid: {} must be a regular file",
                vite.display()
            )),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "inspect verified local-app runtime seed {}: {error}",
                vite.display()
            )),
        }?;
        if !vite_ready {
            return Ok(false);
        }
        if !Self::runtime_seed_root_is_digest_addressed(root) {
            return Ok(true);
        }
        Self::runtime_seed_ready_marker(root)
    }

    fn runtime_seed_root_is_digest_addressed(root: &Path) -> bool {
        root.file_name()
            .and_then(|leaf| leaf.to_str())
            .is_some_and(|leaf| {
                leaf.len() == 64
                    && leaf
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    }

    fn runtime_seed_ready_marker(root: &Path) -> Result<bool, String> {
        let Some(digest) = root.file_name().and_then(|leaf| leaf.to_str()) else {
            return Err(format!(
                "verified local-app runtime root {} has no digest leaf",
                root.display()
            ));
        };
        let marker = Self::runtime_seed_marker_path(root, ".ready")?;
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "verified local-app runtime ready marker is invalid: {} must not be a symlink",
                    marker.display()
                ))
            }
            Ok(_) => {
                return Err(format!(
                    "verified local-app runtime ready marker is invalid: {} must be a regular file",
                    marker.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect verified local-app runtime ready marker {}: {error}",
                    marker.display()
                ))
            }
        }
        let mut marker_file = std::fs::File::open(&marker).map_err(|error| {
            format!(
                "open verified local-app runtime ready marker {}: {error}",
                marker.display()
            )
        })?;
        let mut content = Vec::with_capacity(65);
        marker_file
            .by_ref()
            .take(65)
            .read_to_end(&mut content)
            .map_err(|error| {
                format!(
                    "read verified local-app runtime ready marker {}: {error}",
                    marker.display()
                )
            })?;
        if content.len() != digest.len() || content.as_slice() != digest.as_bytes() {
            return Err(format!(
                "verified local-app runtime ready marker is invalid: {} must contain exactly its digest leaf",
                marker.display()
            ));
        }
        Ok(true)
    }

    fn runtime_seed_marker_path(root: &Path, suffix: &str) -> Result<PathBuf, String> {
        let digest = root.file_name().ok_or_else(|| {
            format!(
                "verified local-app runtime root {} has no digest leaf",
                root.display()
            )
        })?;
        let parent = root.parent().ok_or_else(|| {
            format!(
                "verified local-app runtime root {} has no parent directory",
                root.display()
            )
        })?;
        let mut marker_name = std::ffi::OsString::from(".");
        marker_name.push(digest);
        marker_name.push(suffix);
        Ok(parent.join(marker_name))
    }

    fn runtime_seed_failure_marker(root: &Path) -> Result<Option<PathBuf>, String> {
        let marker = Self::runtime_seed_marker_path(root, ".failed")?;
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                Ok(Some(marker))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "verified local-app runtime failure marker is invalid: {} must not be a symlink",
                marker.display()
            )),
            Ok(_) => Err(format!(
                "verified local-app runtime failure marker is invalid: {} must be a regular file",
                marker.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "inspect verified local-app runtime failure marker {}: {error}",
                marker.display()
            )),
        }
    }

    fn read_runtime_seed_failure(root: &Path) -> Result<Option<String>, String> {
        let Some(marker) = Self::runtime_seed_failure_marker(root)? else {
            return Ok(None);
        };
        let reason = std::fs::read_to_string(&marker).map_err(|error| {
            format!(
                "read verified local-app runtime failure marker {}: {error}",
                marker.display()
            )
        })?;
        let detail = reason.trim();
        if detail.is_empty() {
            Ok(Some(format!(
                "verified local-app runtime seed staging failed; see {}",
                marker.display()
            )))
        } else {
            Ok(Some(format!(
                "verified local-app runtime seed staging failed: {detail}"
            )))
        }
    }

    pub(crate) async fn await_fixed_runtime_root(
        &self,
        timeout_duration: Duration,
    ) -> Result<PathBuf, String> {
        let root = self.configured_runtime_root()?;
        let start = tokio::time::Instant::now();
        loop {
            match Self::runtime_seed_ready(&root)? {
                true => return Ok(root.clone()),
                false => {
                    if let Some(failure) = Self::read_runtime_seed_failure(&root)? {
                        return Err(failure);
                    }
                }
            }
            if start.elapsed() >= timeout_duration {
                return Err(format!(
                    "local-app runtime root is configured at {}, but the verified runtime seed is not ready yet; waited {} ms for node_modules/vite/bin/vite.js",
                    root.display(),
                    timeout_duration.as_millis()
                ));
            }
            sleep(RUNTIME_SEED_POLL_INTERVAL).await;
        }
    }

    pub(crate) async fn resolve_capability(
        &self,
        request_id: &str,
        decision: AppAuthorizationDecisionDto,
    ) -> bool {
        self.pending_capabilities
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|sender| sender.send(decision).is_ok())
    }

    /// Resolve one native dependency-change confirmation request.  This is a
    /// separate one-shot channel from generic capability approvals so the
    /// package diff and supply-chain policy shown by the client cannot be
    /// replaced by a generic allow/deny response.
    pub(crate) async fn resolve_dependency_change_confirmation(
        &self,
        request_id: &str,
        approved: bool,
    ) -> bool {
        self.pending_dependency_change_confirmations
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|sender| sender.send(approved).is_ok())
    }

    pub(crate) async fn resolve_create_confirmation(
        &self,
        request_id: &str,
        approved: bool,
    ) -> bool {
        Self::resolve_native_approval(&self.pending_create_confirmations, request_id, approved)
            .await
    }

    pub(crate) async fn resolve_mcp_proposal_approval(
        &self,
        request_id: &str,
        approved: bool,
    ) -> bool {
        Self::resolve_native_approval(&self.pending_mcp_proposal_approvals, request_id, approved)
            .await
    }

    /// Re-announce every native approval this broker is still blocked on.
    ///
    /// r3-failure-paths-02: the reattach path. `PluginCommandDto::
    /// GetManagedMcpInventory` is the snapshot command both clients already
    /// send when they (re)bind — Android's `LocalAppsViewModel.
    /// requestSnapshots`, iOS's `refreshManagedMcpInventory` — so the pending
    /// sheets ride the channel that already exists rather than a new command
    /// this lane cannot add to the wire.
    ///
    /// Every event carries the SAME `request_id` as the original emission, so a
    /// client that never lost the sheet just re-renders the one it has (both
    /// clients key their pending approval on `request_id`), and a client that
    /// did lose it gets it back and can still answer it.
    ///
    /// Dead entries are pruned first for the same reason
    /// `wait_for_native_approval_with_timeout` prunes them: a waiter whose
    /// future was dropped leaves a closed sender behind, and re-emitting a
    /// sheet nobody is listening for would strand the user in front of a
    /// prompt whose answer goes nowhere.
    pub(crate) async fn reemit_pending_native_approvals(&self) {
        let mut events = Vec::new();
        for pending in [
            &self.pending_create_confirmations,
            &self.pending_mcp_proposal_approvals,
        ] {
            let mut requests = pending.lock().await;
            requests.retain(|_, request| !request.sender.is_closed());
            events.extend(requests.values().map(|request| request.event.clone()));
        }
        for event in events {
            self.event_sink.emit(ClientEvent::AppEvent { event }).await;
        }
    }

    async fn resolve_native_approval(
        pending: &Mutex<HashMap<String, PendingNativeApproval>>,
        request_id: &str,
        approved: bool,
    ) -> bool {
        pending
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|request| request.sender.send(approved).is_ok())
    }

    pub(crate) async fn resolve_ui(
        &self,
        request_id: &str,
        decision: AppAuthorizationDecisionDto,
        result_json: Option<String>,
        error: Option<String>,
    ) -> bool {
        self.pending_ui
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|sender| {
                sender
                    .send(UiResolution {
                        decision,
                        result_json,
                        error,
                    })
                    .is_ok()
            })
    }

    async fn request_capability(
        &self,
        app_id: &str,
        capability: AppCapabilityKindDto,
        domain: Option<String>,
        reason: &str,
    ) -> Result<AppAuthorizationDecisionDto, String> {
        let request_id = self.request_id("app-capability");
        let (sender, receiver) = oneshot::channel();
        self.pending_capabilities
            .lock()
            .await
            .insert(request_id.clone(), sender);
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppCapabilityRequested {
                    request: AppCapabilityRequestDto {
                        request_id: request_id.clone(),
                        app_id: app_id.to_string(),
                        capability,
                        domain,
                        reason: reason.to_string(),
                    },
                },
            })
            .await;
        match timeout(APPROVAL_TIMEOUT, receiver).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) => Err("capability request was cancelled".into()),
            Err(_) => {
                self.pending_capabilities.lock().await.remove(&request_id);
                Err("capability request timed out".into())
            }
        }
    }

    async fn authorize_capability(
        &self,
        app_id: &str,
        capability: AppCapability,
        wire_capability: AppCapabilityKindDto,
        reason: &str,
    ) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let persisted = load_permissions(&layout).map_err(|error| error.to_string())?;
        if persisted.allows(capability)
            || self
                .session_permissions
                .lock()
                .await
                .allows(app_id, capability)
        {
            return Ok(());
        }
        let decision = self
            .request_capability(app_id, wire_capability, None, reason)
            .await?;
        match raise_decision(decision) {
            PermissionDecision::Deny => Err(Self::DENIED_CAPABILITY_MESSAGE.into()),
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowSession => {
                self.session_permissions
                    .lock()
                    .await
                    .grant(app_id, capability);
                Ok(())
            }
            PermissionDecision::AlwaysAllow => {
                let mut permissions = persisted;
                permissions.grant(capability);
                save_permissions(&layout, &permissions).map_err(|error| error.to_string())
            }
        }
    }

    /// Message [`Self::authorize_capability`] returns for an explicit user
    /// denial. [`Self::authorize_declared_capability`] compares against it to
    /// attach the `permission_denied` code — same-file constant, never prose
    /// matching.
    const DENIED_CAPABILITY_MESSAGE: &'static str = "user denied the local app capability";

    /// Manifest-only gate for read-only capabilities that do not need a
    /// separate user prompt. The declaration is still required so a generated
    /// app cannot silently discover host state it did not request.
    pub(super) fn ensure_declared_capability(
        &self,
        app_id: &str,
        capability: AppCapability,
    ) -> Result<(), BridgeFailure> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.capabilities.contains(&capability) {
            return Err(BridgeFailure::coded(
                "capability_not_declared",
                format!("capability {capability:?} is not declared in the app manifest"),
            ));
        }
        Ok(())
    }

    /// Declared-then-prompt gate shared by every plan-declared capability
    /// (device, llm, agent_notify): an app may only ever be ASKED about a
    /// capability its confirmed plan declared. An undeclared capability fails
    /// typed (`capability_not_declared`) WITHOUT raising a prompt — the same
    /// manifest-first contract [`Self::authorize_domain`] applies to network
    /// hosts. Declared capabilities then ride the existing persisted →
    /// session → prompt ladder unchanged.
    async fn authorize_declared_capability(
        &self,
        app_id: &str,
        capability: AppCapability,
        wire_capability: AppCapabilityKindDto,
        reason: &str,
    ) -> Result<(), BridgeFailure> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.capabilities.contains(&capability) {
            return Err(BridgeFailure::coded(
                "capability_not_declared",
                format!("capability {capability:?} is not declared in the app manifest"),
            ));
        }
        self.authorize_capability(app_id, capability, wire_capability, reason)
            .await
            .map_err(|message| {
                if message == Self::DENIED_CAPABILITY_MESSAGE {
                    BridgeFailure::coded("permission_denied", message)
                } else {
                    BridgeFailure::from(message)
                }
            })
    }

    async fn authorize_domain(&self, app_id: &str, domain: &str) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest
            .allowed_domains
            .iter()
            .any(|allowed| allowed == domain)
        {
            return Err(format!(
                "HTTPS domain {domain:?} is not declared in the app manifest"
            ));
        }
        let persisted = load_permissions(&layout).map_err(|error| error.to_string())?;
        if persisted.allows_domain(domain)
            || self
                .session_permissions
                .lock()
                .await
                .allows_domain(app_id, domain)
        {
            return Ok(());
        }
        let decision = self
            .request_capability(
                app_id,
                AppCapabilityKindDto::NetworkDomain,
                Some(domain.to_string()),
                "The local app requested first-time access to this HTTPS domain.",
            )
            .await?;
        match raise_decision(decision) {
            PermissionDecision::Deny => Err("user denied access to the network domain".into()),
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowSession => self
                .session_permissions
                .lock()
                .await
                .grant_domain(app_id, domain)
                .map_err(|error| error.to_string()),
            PermissionDecision::AlwaysAllow => {
                let mut permissions = persisted;
                permissions
                    .grant_domain(domain)
                    .map_err(|error| error.to_string())?;
                save_permissions(&layout, &permissions).map_err(|error| error.to_string())
            }
        }
    }

    pub(crate) async fn approve_destructive_manifest_migration(
        &self,
        app_id: &str,
        preview: &DataMigrationPreview,
    ) -> Result<(), String> {
        if !preview.destructive {
            return Ok(());
        }
        let decision = self
            .request_capability(
                app_id,
                AppCapabilityKindDto::DataMutation,
                None,
                &manifest_migration_reason(preview),
            )
            .await?;
        if matches!(raise_decision(decision), PermissionDecision::Deny) {
            return Err("user denied destructive manifest migration".into());
        }
        Ok(())
    }

    async fn runtime_profiles_value(&self, _input: Value) -> Result<Value, String> {
        // Several profile families can intentionally share an exact
        // toolchain+lock snapshot. Verify that immutable tree once for this
        // catalog operation, then discard the proof: a later call (or a new
        // Host over the same root) must revalidate the bytes rather than trust
        // process-global path/mtime metadata.
        let mut dependency_availability_by_lock = HashMap::new();
        Ok(json!({
            "profiles": crate::mobile::local_app_runtime_profiles::list_runtime_profiles()
                .into_iter()
                .map(|entry| {
                    let dependency_status = if entry.available {
                        self.runtime_profile_dependency_availability_cached(
                            entry.family,
                            entry.revision,
                            &mut dependency_availability_by_lock,
                        )
                    } else {
                        RuntimeProfileDependencyAvailability::DownloadRequired
                    };
                    json!({
                        "family": entry.family.as_str(),
                        "revision": entry.revision,
                        "surface": entry.surface.as_str(),
                        "toolchain_key": entry.toolchain_key,
                        "core_packages": entry.core_packages,
                        "contract_sha256": entry.contract_sha256,
                        "available": entry.available,
                        "availability_reason": entry.availability_reason,
                        // Cache/download both describe the exact dependency
                        // provenance. A compiled source bundle is not a
                        // dependency cache: only a verified shared snapshot
                        // is `cached`, a matching configured seed is
                        // `bundled`, and neither is `download_required`.
                        "cache_status": if !entry.available { "unavailable" } else {
                            dependency_status.as_str()
                        },
                        "download_status": if !entry.available { "gated" } else {
                            dependency_status.as_str()
                        },
                        "available_migrations": local_apps::RUNTIME_PROFILE_MIGRATION_EDGES
                            .iter()
                            .filter(|edge| edge.family == entry.family && edge.from_revision == entry.revision)
                            .map(|edge| json!({
                                "family": edge.family.as_str(),
                                "from_revision": edge.from_revision,
                                "to_revision": edge.to_revision,
                                "rebuild_compatible": edge.rebuild_compatible,
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(),
        }))
    }

    fn read_json_object(path: &Path) -> Option<Value> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let value: Value = serde_json::from_slice(bytes.as_slice()).ok()?;
        let _ = value.as_object()?;
        Some(value)
    }

    /// Whether this host already has a verified dependency tree for the exact
    /// profile lock, either in the shared cache or in the configured bundled
    /// runtime seed. Read-only: unlike `adopt_bundled_dependency_seed`, this
    /// helper never publishes a cache entry.
    fn runtime_profile_dependency_availability(
        &self,
        family: AppRuntimeProfile,
        revision: u32,
    ) -> RuntimeProfileDependencyAvailability {
        self.runtime_profile_dependency_availability_cached(family, revision, &mut HashMap::new())
    }

    fn runtime_profile_dependency_availability_cached(
        &self,
        family: AppRuntimeProfile,
        revision: u32,
        availability_by_lock: &mut HashMap<String, RuntimeProfileDependencyAvailability>,
    ) -> RuntimeProfileDependencyAvailability {
        let Ok(binding) =
            crate::mobile::local_app_runtime_profiles::current_binding_for_family(family)
        else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        if binding.revision != revision {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        }
        let Ok(contract) =
            crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        let toolchain_key = contract.toolchain_key;
        let lock_digest = crate::mobile::local_app_runtime_profiles::lockfile_sha256(contract);
        if let Some(availability) = availability_by_lock.get(&lock_digest) {
            return *availability;
        }
        let availability =
            self.runtime_profile_dependency_availability_for_lock(&lock_digest, toolchain_key);
        availability_by_lock.insert(lock_digest, availability);
        availability
    }

    fn runtime_profile_dependency_availability_for_lock(
        &self,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> RuntimeProfileDependencyAvailability {
        let snapshot_root = self.dependency_snapshot_root(lock_digest, toolchain_key);
        if Self::dependency_snapshot_is_ready(&snapshot_root, lock_digest, toolchain_key)
            .unwrap_or(false)
        {
            return RuntimeProfileDependencyAvailability::Cached;
        }
        let Ok(runtime_root) = self.configured_runtime_root() else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        let Some(manifest) = Self::read_json_object(&runtime_root.join(BUNDLED_SEED_MANIFEST_FILE))
        else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        if manifest.get("pnpm_lock_sha256").and_then(Value::as_str) != Some(lock_digest) {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        }
        let Ok(metadata) = std::fs::symlink_metadata(runtime_root.join("node_modules")) else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            RuntimeProfileDependencyAvailability::Bundled
        } else {
            RuntimeProfileDependencyAvailability::DownloadRequired
        }
    }

    async fn query_data_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        let qa_event_id = input
            .get("qa_handle")
            .and_then(Value::as_str)
            .map(|_| self.request_id("qa-query"));
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let layout = self.layout(&app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let query = normalize_query(&input)?;
        let result = tokio::task::spawn_blocking(move || {
            AppDataStore::with_cached(layout, |store| store.query(&manifest, &query))
        })
        .await
        .map_err(|error| format!("data query worker failed: {error}"))?
        .map(|page| json!(page))
        .map_err(|error| error.to_string())?;
        self.validate_qa_request(&input).await?;
        if let Some(qa_event_id) = qa_event_id {
            // The durable artifact is the actual native query page. Core
            // validates its collection/record/revision against the causally
            // linked bridge mutation; wrapping it in request metadata would
            // make the real `records` array invisible to that validation.
            let evidence = self
                .record_qa_observation(&input, "query_data", result.clone(), qa_event_id, None)
                .await?;
            if let Some(handle) = input.get("qa_handle").and_then(Value::as_str) {
                return Ok(authoring::qa_result_with_evidence_ids(
                    result,
                    handle,
                    authoring::qa_observation_id(&evidence),
                ));
            }
        }
        Ok(result)
    }

    async fn mutate_data_value(
        &self,
        input: Value,
        require_approval: bool,
        qa_bridge_event_id: Option<&str>,
    ) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        if require_approval {
            self.authorize_capability(
                &app_id,
                AppCapability::DataMutation,
                AppCapabilityKindDto::DataMutation,
                "The agent requested permission to modify this app's persisted data.",
            )
            .await?;
        }
        let layout = self.layout(&app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let mutations = normalize_mutations(&input)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let result = tokio::task::spawn_blocking(move || {
            AppDataStore::with_cached(layout, |store| store.mutate(&manifest, &mutations, now_ms))
        })
        .await
        .map_err(|error| format!("data mutation worker failed: {error}"))?
        .map(|results| json!({ "results": results }))
        .map_err(|error| error.to_string())?;
        if let Some(event_id) = qa_bridge_event_id {
            // The page receives the actual datastore result immediately. Only
            // Host evidence attribution waits for the native action result;
            // failed/cancelled native actions must never erase this business
            // write or expose a fabricated empty response.
            if let Err(error) = self.validate_qa_request(&input).await {
                tracing::warn!(app_id = %app_id, %error, "QA identity changed after page write; omitting evidence attribution");
                return Ok(result);
            }
            if let Err(error) = self
                .buffer_qa_bridge_result(&app_id, event_id, result.clone())
                .await
            {
                tracing::warn!(app_id = %app_id, %error, "QA bridge evidence attribution unavailable after page write");
            }
            return Ok(result);
        }
        self.validate_qa_request(&input).await?;
        Ok(result)
    }

    async fn request_ui(&self, request: AppUiRequestDto) -> Result<Value, String> {
        let request_id = request.request_id.clone();
        let is_qa_request = request.request_id.starts_with("qa-ui-");
        let (sender, receiver) = oneshot::channel();
        self.pending_ui
            .lock()
            .await
            .insert(request_id.clone(), sender);
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppUiRequest { request },
            })
            .await;
        let resolution = match timeout(UI_TIMEOUT, receiver).await {
            Ok(Ok(resolution)) => resolution,
            Ok(Err(_)) => return Err("WebView action was cancelled".into()),
            Err(_) => {
                self.pending_ui.lock().await.remove(&request_id);
                return Err("WebView action timed out".into());
            }
        };
        if matches!(resolution.decision, AppAuthorizationDecisionDto::Deny) {
            return Err("user denied the WebView action".into());
        }
        if let Some(error) = resolution.error {
            // Native QA clients preserve an authenticated envelope even when
            // the page action itself fails. Keep that envelope on the normal
            // result channel and localize the failure inside `result`; the
            // Host can then validate `lingxi_qa`, persist failed UiAction
            // evidence, and return its Host-issued evidence IDs. A transport
            // or identity failure has no such envelope and must remain a
            // top-level error.
            if is_qa_request {
                if let Some(result_json) = resolution.result_json {
                    if let Ok(Value::Object(mut envelope)) = serde_json::from_str(&result_json) {
                        if envelope.get("lingxi_qa").is_some() {
                            let failed_result = match envelope.remove("result") {
                                Some(Value::Object(mut result)) => {
                                    result.insert("ok".into(), Value::Bool(false));
                                    result.insert("error".into(), Value::String(error));
                                    Value::Object(result)
                                }
                                _ => json!({"ok": false, "error": error}),
                            };
                            envelope.insert("result".into(), failed_result);
                            return Ok(Value::Object(envelope));
                        }
                    }
                }
            }
            return Err(error);
        }
        let result = resolution.result_json.unwrap_or_else(|| "{}".into());
        serde_json::from_str(&result)
            .map_err(|error| format!("invalid WebView result JSON: {error}"))
    }

    pub(crate) async fn execute_bridge(&self, request: AppBridgeRequestDto) {
        let result = self.execute_bridge_inner(&request).await;
        let response = match result {
            Ok(value) => AppBridgeResponseDto {
                request_id: request.request_id,
                app_id: request.app_id,
                ok: true,
                result_json: Some(value.to_string()),
                error: None,
                error_code: None,
            },
            Err(failure) => AppBridgeResponseDto {
                request_id: request.request_id,
                app_id: request.app_id,
                ok: false,
                result_json: None,
                error: Some(failure.message),
                error_code: failure.code.map(str::to_string),
            },
        };
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppBridgeResponse { response },
            })
            .await;
    }

    async fn execute_bridge_inner(
        &self,
        request: &AppBridgeRequestDto,
    ) -> Result<Value, BridgeFailure> {
        // Track every page request, not just the eventual mutation: an async
        // click handler may await a query/device bridge before issuing its
        // persisted write. Only MutateData receives the scoped event id.
        let qa_bridge_guard = self.begin_qa_bridge_request(&request.app_id).await;
        let qa_action_event_id = qa_bridge_guard
            .as_ref()
            .map(|guard| guard.event_id().to_string());
        let qa_bridge_event_id = matches!(request.operation, AppBridgeOperationDto::MutateData)
            .then_some(qa_action_event_id.as_deref())
            .flatten();
        let result = self
            .execute_bridge_inner_scoped(request, qa_bridge_event_id)
            .await;
        // Dropping the guard synchronously settles the in-flight counter even
        // when the bridge future is cancelled before this point.
        drop(qa_bridge_guard);
        result
    }

    async fn execute_bridge_inner_scoped(
        &self,
        request: &AppBridgeRequestDto,
        qa_bridge_event_id: Option<&str>,
    ) -> Result<Value, BridgeFailure> {
        let payload_json = request.payload_json.as_deref().unwrap_or("{}");
        let payload_limit = if matches!(
            request.operation,
            AppBridgeOperationDto::LlmChat | AppBridgeOperationDto::LlmStream
        ) {
            LOCAL_APP_BRIDGE_LLM_BYTES
        } else if matches!(
            request.operation,
            AppBridgeOperationDto::FileRead | AppBridgeOperationDto::FileWrite
        ) {
            LOCAL_APP_BRIDGE_FILE_BYTES
        } else {
            LOCAL_APP_BRIDGE_CONTROL_BYTES
        };
        if payload_json.len() > payload_limit {
            return Err(BridgeFailure::coded(
                "payload_too_large",
                format!(
                    "bridge payload is {} bytes; the limit for this operation is {payload_limit}",
                    payload_json.len()
                ),
            ));
        }
        let payload: Value = serde_json::from_str(payload_json).map_err(|error| {
            BridgeFailure::coded(
                "payload_invalid",
                format!("invalid bridge payload JSON: {error}"),
            )
        })?;
        // The page-facing DTO is intentionally small and legacy-compatible;
        // the v2 attribution context is created here, inside the trusted host,
        // before any capability handler runs. A page cannot manufacture its
        // origin, app instance, or grant epoch.
        let invocation_context = self
            .build_bridge_invocation_context(request)
            .map_err(BridgeFailure::from)?;
        let mut input = payload.as_object().cloned().ok_or_else(|| {
            BridgeFailure::coded("payload_invalid", "bridge payload must be a JSON object")
        })?;
        input.insert("app_id".into(), Value::String(request.app_id.clone()));
        match request.operation {
            AppBridgeOperationDto::QueryData => self
                .query_data_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            // The page is acting for the foreground user, not an agent.  Its
            // app id is host-bound and the manifest still constrains writes.
            AppBridgeOperationDto::MutateData => self
                .mutate_data_value(Value::Object(input), false, qa_bridge_event_id)
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::RuntimeStatus => {
                let runtime = self
                    .service()?
                    .runtime_record(&request.app_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(json!(runtime))
            }
            AppBridgeOperationDto::NetworkRequest => self
                .network_request(&request.app_id, Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::CapturePhoto => {
                self.capture_photo_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::PickImage => {
                self.pick_image_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::RecordAudioStart => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.record_audio_start_value(&invocation_context, runtime_generation, &payload)
                    .await
            }
            AppBridgeOperationDto::RecordAudioStop => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.record_audio_stop_value(&invocation_context, runtime_generation)
                    .await
            }
            AppBridgeOperationDto::GetLocation => self.get_location_value(&request.app_id).await,
            AppBridgeOperationDto::PostNotification => {
                self.post_notification_value(&request.app_id, &payload)
                    .await
            }
            AppBridgeOperationDto::TranscribeSpeech => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.transcribe_speech_value(&invocation_context, runtime_generation, &payload)
                    .await
            }
            AppBridgeOperationDto::ClipboardGetText => {
                self.clipboard_get_text_value(&request.app_id).await
            }
            AppBridgeOperationDto::ClipboardSetText => {
                self.clipboard_set_text_value(&request.app_id, &payload)
                    .await
            }
            AppBridgeOperationDto::Share => self.share_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::SynthesizeSpeech => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.synthesize_speech_value(&invocation_context, runtime_generation, &payload)
                    .await
            }
            AppBridgeOperationDto::FileRead => {
                self.file_read_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::FileWrite => {
                self.file_write_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::DeviceStatus => self.device_status_value(&request.app_id).await,
            AppBridgeOperationDto::Haptics => self.haptics_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::DeepLink => {
                self.deep_link_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::CalendarListEvents => {
                self.calendar_list_events_value(&request.app_id, &payload)
                    .await
            }
            AppBridgeOperationDto::ContactsSearch => {
                self.contacts_search_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::MediaGet => self.media_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::LlmChat => self.llm_chat_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::LlmStream => {
                self.llm_stream_value(&request.app_id, &request.request_id, &payload)
                    .await
            }
            AppBridgeOperationDto::AgentPost => {
                self.agent_post_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::AgentSessionCreate => self
                .agent_session_create_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::AgentSessionList => self
                .agent_session_list_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::AgentSessionResume => {
                input.insert("action".into(), Value::String("resume".into()));
                self.agent_session_update_value(Value::Object(input))
                    .await
                    .map_err(BridgeFailure::from)
            }
            AppBridgeOperationDto::AgentSessionClose => {
                input.insert("action".into(), Value::String("close".into()));
                self.agent_session_update_value(Value::Object(input))
                    .await
                    .map_err(BridgeFailure::from)
            }
            AppBridgeOperationDto::AgentSend => {
                self.agent_send_value(&request.app_id, &request.request_id, &payload)
                    .await
            }
            AppBridgeOperationDto::AgentStream => {
                self.agent_stream_value(&request.app_id, &request.request_id, &payload)
                    .await
            }
            AppBridgeOperationDto::AgentCancel => {
                self.agent_cancel_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::AgentProfileProposeUpdate => self
                .agent_profile_propose_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundSchedule => self
                .background_schedule_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundList => self
                .background_list_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundStatus => self
                .background_status_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundCancel => self
                .background_cancel_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundRetry => self
                .background_retry_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            _ => Err("unsupported bridge operation for this engine version".into()),
        }
    }

    fn build_bridge_invocation_context(
        &self,
        request: &AppBridgeRequestDto,
    ) -> Result<local_apps::InvocationContext, String> {
        let capability = match request.operation {
            AppBridgeOperationDto::QueryData => local_apps::CapabilityId::DataQuery,
            AppBridgeOperationDto::MutateData => local_apps::CapabilityId::DataMutate,
            AppBridgeOperationDto::NetworkRequest => local_apps::CapabilityId::NetworkRequest,
            AppBridgeOperationDto::RuntimeStatus => local_apps::CapabilityId::RuntimeStatus,
            AppBridgeOperationDto::CapturePhoto => local_apps::CapabilityId::Camera,
            AppBridgeOperationDto::PickImage => local_apps::CapabilityId::PhotoLibrary,
            AppBridgeOperationDto::RecordAudioStart | AppBridgeOperationDto::RecordAudioStop => {
                local_apps::CapabilityId::Microphone
            }
            AppBridgeOperationDto::GetLocation => local_apps::CapabilityId::Location,
            AppBridgeOperationDto::TranscribeSpeech => local_apps::CapabilityId::SpeechToText,
            AppBridgeOperationDto::PostNotification => local_apps::CapabilityId::Notifications,
            AppBridgeOperationDto::ClipboardGetText | AppBridgeOperationDto::ClipboardSetText => {
                local_apps::CapabilityId::Clipboard
            }
            AppBridgeOperationDto::Share => local_apps::CapabilityId::Share,
            AppBridgeOperationDto::SynthesizeSpeech => local_apps::CapabilityId::TextToSpeech,
            AppBridgeOperationDto::FileRead => local_apps::CapabilityId::FilesRead,
            AppBridgeOperationDto::FileWrite => local_apps::CapabilityId::FilesWrite,
            AppBridgeOperationDto::DeviceStatus => local_apps::CapabilityId::DeviceStatus,
            AppBridgeOperationDto::Haptics => local_apps::CapabilityId::Haptics,
            AppBridgeOperationDto::DeepLink => local_apps::CapabilityId::DeepLink,
            AppBridgeOperationDto::CalendarListEvents => local_apps::CapabilityId::Calendar,
            AppBridgeOperationDto::ContactsSearch => local_apps::CapabilityId::Contacts,
            AppBridgeOperationDto::MediaGet => local_apps::CapabilityId::Media,
            AppBridgeOperationDto::LlmChat => local_apps::CapabilityId::LlmComplete,
            AppBridgeOperationDto::LlmStream => local_apps::CapabilityId::LlmStream,
            AppBridgeOperationDto::AgentPost => local_apps::CapabilityId::AgentEmit,
            AppBridgeOperationDto::AgentSessionCreate => {
                local_apps::CapabilityId::AgentSessionCreate
            }
            AppBridgeOperationDto::AgentSessionList => local_apps::CapabilityId::AgentSessionList,
            AppBridgeOperationDto::AgentSessionResume => {
                local_apps::CapabilityId::AgentSessionResume
            }
            AppBridgeOperationDto::AgentSessionClose => local_apps::CapabilityId::AgentSessionClose,
            AppBridgeOperationDto::AgentSend => local_apps::CapabilityId::AgentSend,
            AppBridgeOperationDto::AgentStream => local_apps::CapabilityId::AgentStream,
            AppBridgeOperationDto::AgentCancel => local_apps::CapabilityId::AgentCancel,
            AppBridgeOperationDto::AgentProfileProposeUpdate => {
                local_apps::CapabilityId::AgentProfilePropose
            }
            AppBridgeOperationDto::BackgroundSchedule => {
                local_apps::CapabilityId::BackgroundSchedule
            }
            AppBridgeOperationDto::BackgroundList
            | AppBridgeOperationDto::BackgroundStatus
            | AppBridgeOperationDto::BackgroundCancel
            | AppBridgeOperationDto::BackgroundRetry => {
                local_apps::CapabilityId::BackgroundSchedule
            }
            _ => return Err("unsupported bridge operation for runtime v2 context".into()),
        };
        let layout = self.layout(&request.app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.runtime_api_compatible() {
            return Err(format!(
                "runtime_api_incompatible: app manifest targets runtime API v{}",
                manifest.runtime_api_version
            ));
        }
        let permissions = load_permissions(&layout).map_err(|error| error.to_string())?;
        let context = local_apps::InvocationContext {
            app_id: request.app_id.clone(),
            app_instance_id: format!("page-{}", request.app_id),
            request_id: request.request_id.clone(),
            turn_id: None,
            origin: local_apps::InvocationOrigin::PageForeground,
            grant_epoch: permissions.grant_epoch,
            capability_instance: Some(format!("{}:{}", request.app_id, capability.as_str())),
            call_chain: Vec::new(),
        };
        context.validate().map_err(|error| error.to_string())?;
        Ok(context)
    }

    async fn network_request(&self, app_id: &str, input: Value) -> Result<Value, String> {
        let url_text = required_string(&input, "url")?;
        let url = reqwest::Url::parse(url_text).map_err(|error| format!("invalid URL: {error}"))?;
        if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
            return Err(
                "network bridge accepts plain HTTPS URLs without embedded credentials".into(),
            );
        }
        let domain = url
            .host_str()
            .ok_or_else(|| "network URL has no hostname".to_string())?;
        if domain == "localhost" || !domain.contains('.') || domain.parse::<IpAddr>().is_ok() {
            return Err("network bridge requires a public DNS hostname".into());
        }
        self.authorize_domain(app_id, domain).await?;
        let port = url.port_or_known_default().unwrap_or(443);
        let resolved: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
            .await
            .map_err(|error| format!("resolve network domain: {error}"))?
            .collect();
        if resolved.is_empty() || resolved.iter().any(|address| !public_ip(address.ip())) {
            return Err("network domain resolved to a private, local, or invalid address".into());
        }
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err("network method must be GET, POST, PUT, PATCH, or DELETE".into());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .resolve_to_addrs(domain, &resolved)
            .build()
            .map_err(|error| format!("create network client: {error}"))?;
        let mut builder = client.request(method.parse().map_err(|_| "invalid HTTP method")?, url);
        if let Some(headers) = input.get("headers").and_then(Value::as_object) {
            if headers.len() > 32 {
                return Err("network request has more than 32 headers".into());
            }
            for (name, value) in headers {
                let Some(value) = value.as_str() else {
                    return Err("network header values must be strings".into());
                };
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "host" | "cookie" | "authorization" | "proxy-authorization"
                ) {
                    return Err(format!("network header {name:?} is reserved"));
                }
                builder = builder.header(name, value);
            }
        }
        if let Some(body) = input.get("body") {
            let encoded = if let Some(text) = body.as_str() {
                text.as_bytes().to_vec()
            } else {
                serde_json::to_vec(body).map_err(|error| error.to_string())?
            };
            if encoded.len() > 1024 * 1024 {
                return Err("network request body exceeds 1 MiB".into());
            }
            builder = builder.body(encoded);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| format!("network request failed: {error}"))?;
        let status = response.status().as_u16();
        let headers: Map<String, Value> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), Value::String(value.to_string())))
            })
            .collect();
        let bytes = read_limited_stream(
            response.bytes_stream(),
            MAX_NETWORK_RESPONSE_BYTES,
            "read network response",
            "network response exceeds 2 MiB",
        )
        .await?;
        Ok(json!({
            "status": status,
            "headers": headers,
            "body": String::from_utf8_lossy(&bytes),
        }))
    }

    pub(crate) async fn manage_runtime_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let action = required_string(&input, "action")?;
        match action {
            "start" | "open" | "resume" => self.start_runtime(&app_id).await,
            "stop" | "suspend" => self.stop_runtime(&app_id).await,
            "restart" => {
                self.stop_runtime(&app_id).await?;
                self.start_runtime(&app_id).await
            }
            _ => {
                Err("runtime action must be start, stop, restart, open, suspend, or resume".into())
            }
        }
    }

    async fn start_runtime(&self, app_id: &str) -> Result<Value, String> {
        let service = self.service()?;
        service
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let requested_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start requires an active build".to_string())?;
        let publication_cell = self.runtime_publication_cell(app_id)?;
        let access_tick = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut wait_for_start = None;
        let mut return_running = false;
        let mut reserved_generation = None;
        {
            let mut runtimes = self.runtimes.lock().await;
            if let Some(entry) = runtimes.get_mut(app_id) {
                entry.last_used = access_tick;
                match &entry.state {
                    RuntimeEntryState::Starting { gate } => {
                        wait_for_start = Some(gate.subscribe());
                    }
                    RuntimeEntryState::Running { .. } => {
                        if entry.build_id != requested_build_id {
                            return Err(
                                "runtime_stale_build: running runtime does not serve the active build; stop and restart it"
                                    .into(),
                            );
                        }
                        return_running = true;
                    }
                }
            } else {
                let generation = self.next_request_id.fetch_add(1, Ordering::Relaxed);
                let (gate, _) = watch::channel(RuntimeStartStatus::Pending);
                *publication_cell
                    .write()
                    .map_err(|_| "runtime publication identity is poisoned".to_string())? = None;
                runtimes.insert(
                    app_id.to_string(),
                    RuntimeEntry {
                        state: RuntimeEntryState::Starting { gate },
                        last_used: access_tick,
                        generation,
                        build_id: requested_build_id.clone(),
                    },
                );
                reserved_generation = Some(generation);
            }
        }
        if let Some(receiver) = wait_for_start {
            return self.wait_for_runtime_start(app_id, receiver).await;
        }
        if return_running {
            let runtime = service
                .runtime_record(app_id)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(json!({
                "app_id": app_id,
                "state": runtime.state,
                "url": runtime.port.map(|port| format!("http://127.0.0.1:{port}")),
                "build_id": requested_build_id,
            }));
        }
        let Some(generation) = reserved_generation else {
            return Err("runtime start reservation disappeared before completion".into());
        };
        self.start_reserved_runtime(app_id, generation).await
    }

    async fn wait_for_runtime_start(
        &self,
        app_id: &str,
        mut receiver: watch::Receiver<RuntimeStartStatus>,
    ) -> Result<Value, String> {
        loop {
            let status = receiver.borrow_and_update().clone();
            match status {
                RuntimeStartStatus::Pending => {
                    receiver
                        .changed()
                        .await
                        .map_err(|_| "runtime start was cancelled".to_string())?;
                }
                RuntimeStartStatus::Running => {
                    let runtime = self
                        .service()?
                        .runtime_record(app_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let build_id =
                        crate::mobile::local_apps_build::active_build_id(&self.layout(app_id)?)
                            .map_err(|error| error.to_string())?;
                    return Ok(json!({
                        "app_id": app_id,
                        "state": runtime.state,
                        "url": runtime.port.map(|port| format!("http://127.0.0.1:{port}")),
                        "build_id": build_id,
                    }));
                }
                RuntimeStartStatus::Failed(detail) => return Err(detail),
            }
        }
    }

    async fn start_reserved_runtime(&self, app_id: &str, generation: u64) -> Result<Value, String> {
        let _reservation = RuntimeReservation {
            runtimes: Arc::clone(&self.runtimes),
            app_id: app_id.to_string(),
            generation,
        };
        let service = self.service()?;
        let layout = self.layout(app_id)?;
        let expected_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start requires an active build".to_string())?;
        let reservation_matches = {
            let runtimes = self.runtimes.lock().await;
            runtimes.get(app_id).is_some_and(|entry| {
                entry.generation == generation && entry.build_id == expected_build_id
            })
        };
        if !reservation_matches {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    None,
                    "runtime start build identity changed before launch".into(),
                )
                .await;
        }
        let manifest = local_apps::load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.runtime_api_compatible() {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    None,
                    format!(
                        "runtime_api_incompatible: app manifest targets runtime API v{}; regenerate or rebuild this app for v{}",
                        manifest.runtime_api_version,
                        local_apps::RUNTIME_API_MAJOR
                    ),
                )
                .await;
        }
        if let Err(error) = crate::mobile::local_apps_build::validate_build_for_launch(&layout) {
            return self
                .fail_reserved_runtime_start(app_id, generation, None, error.to_string())
                .await;
        }
        let static_root = layout
            .root()
            .join(layout.build_rel(false))
            .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR);
        if !static_root.join("index.html").is_file() {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    None,
                    "static build output is missing index.html; generate the app first".into(),
                )
                .await;
        }
        let current = service
            .runtime_record(app_id)
            .await
            .map_err(|e| e.to_string())?;
        // The listener has to be registered with the SAME immortal I/O driver
        // that will serve it.  `start_reserved_runtime` runs on the AMBIENT
        // runtime — for an agent-driven `manage_runtime {action:"start"}` that
        // is the per-engine one — and tokio invalidates every resource
        // registered with a dropped runtime's driver, so a listener bound here
        // fails every later `accept()` forever behind an entry that still
        // reports `running`.
        let bound = {
            let app_id = app_id.to_string();
            let assigned = current.port;
            let leases = Arc::clone(&self.port_leases);
            let registry = Arc::clone(&service);
            // Everything from the pin read to the choice is one step against
            // this broker's other ALLOCATIONS — but only against those: a
            // sibling that is already past its own allocation still persists
            // its pin and releases its lease inside this window, which is why
            // the choice is re-checked against a fresh read below rather than
            // trusted because the gate is held.  See `port_allocation`.
            let _allocation = self.port_allocation.lock().await;
            // The FIRST read is taken on THIS runtime, before the hop: the
            // derivation has to know which ports stopped siblings own
            // permanently, which no bind probe on the worker runtime can
            // discover.  The re-read after the lease runs on the worker
            // runtime, where it is an ordinary `AppService` read — no runtime
            // affinity, nothing blocking.
            let sibling_pins = sibling_pinned_ports(&service, &app_id).await;
            crate::mobile::local_apps_profile::worker_runtime()
                .spawn(async move {
                    bind_stable_loopback(&app_id, assigned, &sibling_pins, &leases, &registry).await
                })
                .await
                .map_err(|error| format!("bind stable app port: {error}"))?
        };
        let (listener, port, port_lease) = match bound {
            Ok(bound) => bound,
            Err(error) => {
                return self
                    .fail_reserved_runtime_start(app_id, generation, current.port, error)
                    .await;
            }
        };
        let latest_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start lost its active build".to_string())?;
        if latest_build_id != expected_build_id {
            drop(port_lease);
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    "runtime start build identity changed while binding".into(),
                )
                .await;
        }
        if let Err(error) = service
            .set_runtime_mode(app_id, AppRuntimeMode::StaticExport)
            .await
        {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    format!("persist runtime mode: {error}"),
                )
                .await;
        }
        if let Err(error) = service
            .update_runtime_record(app_id, AppRuntimeState::Starting, Some(port), None, None)
            .await
        {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    format!("persist starting runtime state: {error}"),
                )
                .await;
        }
        // The pin is durable HERE and not one line earlier: `with_app` has
        // already written the new record back under the state lock by the time
        // it returns, so from this point `sibling_pinned_ports` reports the
        // port for every later allocator and the lease has nothing left to
        // cover.  Every path above this line drops the guard instead, which
        // returns the port to the pool.
        //
        // That ORDER — persist, THEN release — is load-bearing beyond tidiness:
        // it is the whole premise of `bind_stable_loopback`'s post-lease pin
        // re-read.  A release moved above the persist would leave a port that
        // is in neither the records nor the leases, which is exactly the hole
        // both mechanisms exist to close.
        if let Some(lease) = port_lease {
            lease.commit();
        }

        let publication_cell = self.runtime_publication_cell(app_id)?;
        let (shutdown, receiver) = oneshot::channel();
        self.spawn_static_server(
            service.clone(),
            app_id.to_string(),
            generation,
            publication_cell.clone(),
            listener,
            static_root,
            receiver,
        );
        let handle = RuntimeHandle::Static { shutdown };
        if let Err(error) = service
            .update_runtime_record(app_id, AppRuntimeState::Running, Some(port), None, None)
            .await
            .map_err(|error| error.to_string())
        {
            self.cleanup_runtime_handle(handle).await;
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    format!("persist running runtime state: {error}"),
                )
                .await;
        }
        let latest_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start lost its active build".to_string())?;
        if latest_build_id != expected_build_id {
            self.cleanup_runtime_handle(handle).await;
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    "runtime start build identity changed before commit".into(),
                )
                .await;
        }
        let gate = {
            let mut runtimes = self.runtimes.lock().await;
            let Some(entry) = runtimes.get_mut(app_id) else {
                self.cleanup_runtime_handle(handle).await;
                return Err("runtime start reservation disappeared before completion".into());
            };
            if entry.generation != generation {
                self.cleanup_runtime_handle(handle).await;
                return Err("runtime start reservation changed before completion".into());
            }
            let mut publication_identity = publication_cell
                .write()
                .map_err(|_| "runtime publication identity is poisoned".to_string())?;
            let previous =
                std::mem::replace(&mut entry.state, RuntimeEntryState::Running { handle });
            let gate = match previous {
                RuntimeEntryState::Starting { gate } => gate,
                RuntimeEntryState::Running { handle } => {
                    entry.state = RuntimeEntryState::Running { handle };
                    return Err("runtime start reservation was already resolved".into());
                }
            };
            *publication_identity = Some(RuntimePublicationIdentity {
                generation,
                build_id: expected_build_id.clone(),
            });
            gate
        };
        let _ = gate.send(RuntimeStartStatus::Running);
        Ok(json!({
            "app_id": app_id,
            "state": "running",
            "url": format!("http://127.0.0.1:{port}"),
            "build_id": expected_build_id,
            "runtime_generation": generation,
        }))
    }

    async fn stop_runtime(&self, app_id: &str) -> Result<Value, String> {
        let service = self.service()?;
        service
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let runtime_generation = self
            .runtime_identity(app_id)
            .await?
            .map(|(generation, _)| generation);
        self.release_app_runtime_state(app_id, runtime_generation)
            .await;
        let publication_cell = self.runtime_publication_cell(app_id)?;
        // Classify and remove under ONE acquisition: a start woken in the gap
        // between a `remove` and its rollback `insert` finds no entry, kills the
        // runtime it just spawned and returns without resolving the gate,
        // leaving a reservation nothing can ever complete.
        let handle = {
            let mut runtimes = self.runtimes.lock().await;
            let mut publication_identity = publication_cell
                .write()
                .map_err(|_| "runtime publication identity is poisoned".to_string())?;
            match runtimes.get(app_id).map(|entry| &entry.state) {
                None => {
                    *publication_identity = None;
                    return Ok(json!({"app_id": app_id, "state": "stopped"}));
                }
                Some(RuntimeEntryState::Starting { .. }) => {
                    return Err("runtime is still starting; retry stop shortly".into());
                }
                Some(RuntimeEntryState::Running { .. }) => {}
            }
            match runtimes.remove(app_id).map(|entry| entry.state) {
                Some(RuntimeEntryState::Running { handle }) => {
                    *publication_identity = None;
                    handle
                }
                _ => return Ok(json!({"app_id": app_id, "state": "stopped"})),
            }
        };
        let runtime = service
            .runtime_record(app_id)
            .await
            .map_err(|e| e.to_string())?;
        service
            .update_runtime_record(
                app_id,
                AppRuntimeState::Stopping,
                runtime.port,
                runtime.pid,
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        match handle {
            RuntimeHandle::Static { shutdown } => {
                let _ = shutdown.send(());
            }
        }
        service
            .update_runtime_record(app_id, AppRuntimeState::Stopped, runtime.port, None, None)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({"app_id": app_id, "state": "stopped"}))
    }

    async fn fail_reserved_runtime_start(
        &self,
        app_id: &str,
        generation: u64,
        port: Option<u16>,
        detail: String,
    ) -> Result<Value, String> {
        let gate = {
            let mut runtimes = self.runtimes.lock().await;
            let matches_generation = runtimes.get(app_id).is_some_and(|entry| {
                entry.generation == generation
                    && matches!(entry.state, RuntimeEntryState::Starting { .. })
            });
            if !matches_generation {
                None
            } else {
                match runtimes.remove(app_id) {
                    Some(RuntimeEntry {
                        state: RuntimeEntryState::Starting { gate },
                        ..
                    }) => Some(gate),
                    Some(entry) => {
                        runtimes.insert(app_id.to_string(), entry);
                        None
                    }
                    None => None,
                }
            }
        };
        if let Some(gate) = gate {
            let _ = gate.send(RuntimeStartStatus::Failed(detail.clone()));
        }
        // Same rule as `stop_runtime`'s failed kill: this bookkeeping write must
        // never mask the real failure, so its result stays discarded.
        //
        // Three callers reach here BEFORE the record leaves `stopped` (no
        // runtime mount, no static build, and the squatted permanent port).
        // `stopped -> failed` is now a legal edge (`local_apps::state::
        // runtime_transition_allowed`), added precisely so this write lands:
        // while it was rejected, the state, the `lastError`, and the
        // `RuntimeChanged` event were all discarded, leaving an app that could
        // not start and carried no recorded reason. The detail also reaches
        // every concurrent waiter through the gate above, and `failed ->
        // starting` keeps the record recoverable.
        if let Ok(service) = self.service() {
            let _ = service
                .update_runtime_record(
                    app_id,
                    AppRuntimeState::Failed,
                    port,
                    None,
                    Some(detail.clone()),
                )
                .await;
        }
        Err(detail)
    }

    async fn cleanup_runtime_handle(&self, handle: RuntimeHandle) {
        match handle {
            RuntimeHandle::Static { shutdown } => {
                let _ = shutdown.send(());
            }
        }
    }

    /// Everything an app's runtime owned that must not outlive it.
    ///
    /// Called from EVERY way a runtime can end — the explicit stop, the Full
    /// handle's exit watch, and the static listener's reconciliation — not
    /// just the one the user drives. A crashed app used to keep the iOS
    /// audio-session lease open with nothing left able to release it, which
    /// takes FlowMode, hold-to-talk and transcribeSpeech down with it for the
    /// life of the process.
    ///
    /// Session grants go too: the user answered "allow for this session"
    /// while USING the app, and a grant that quietly survives the app's death
    /// behaves as "always allow" while staying invisible to permissions.json
    /// and unrevokable short of a full reset.
    pub(crate) async fn release_app_runtime_state(
        &self,
        app_id: &str,
        runtime_generation: Option<u64>,
    ) {
        if let Some(generation) = runtime_generation {
            self.cancel_local_app_audio_scope(app_id, generation);
            self.force_stop_recording(app_id, generation).await;
        }
        self.clear_media(app_id);
        self.session_permissions.lock().await.revoke_app(app_id);
    }

    /// Return the Host-owned runtime generation and the build provenance that
    /// was selected when it was started.  The stable loopback port is omitted
    /// deliberately: it preserves the app's IndexedDB origin and cannot prove
    /// which promoted build a native WebView currently displays.
    pub(crate) async fn runtime_identity(
        &self,
        app_id: &str,
    ) -> Result<Option<(u64, String)>, String> {
        let runtimes = self.runtimes.lock().await;
        Ok(runtimes.get(app_id).and_then(|entry| {
            matches!(entry.state, RuntimeEntryState::Running { .. })
                .then(|| (entry.generation, entry.build_id.clone()))
        }))
    }

    /// Serves the static export on the anchored runtime, and reconciles the
    /// record when the LISTENER dies the way the Full handle's exit watch does.
    /// A static handle has no process to poll, so without this a retired server
    /// leaves `Running { Static }` in the map and `start_runtime`'s
    /// `return_running` short-circuit keeps handing out a URL nothing answers.
    ///
    /// `service` is passed in rather than re-resolved: the only caller already
    /// holds it, and a bail-out here would drop the listener while the entry
    /// went on to report `running`.
    fn spawn_static_server(
        &self,
        service: Arc<AppService>,
        app_id: String,
        generation: u64,
        publication_cell: RuntimePublicationCell,
        listener: TcpListener,
        root: PathBuf,
        shutdown: oneshot::Receiver<()>,
    ) {
        let runtimes = Arc::clone(&self.runtimes);
        let broker = self.weak_self();
        crate::mobile::local_apps_profile::worker_runtime().spawn(async move {
            let Some(detail) = run_static_server(listener, root, shutdown).await else {
                return;
            };
            reconcile_static_runtime_exit(
                runtimes,
                service,
                app_id,
                generation,
                publication_cell,
                detail,
                broker,
            )
            .await;
        });
    }

    pub(crate) async fn restore_checkpoint_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let checkpoint_id = required_string(&input, "checkpoint_id")?.to_string();
        let service = self.service()?;
        service.record(&app_id).await.map_err(|e| e.to_string())?;
        // Pre-flight the one precondition the restore cannot recover from,
        // BEFORE reading any digest, BEFORE prompting the user and BEFORE
        // stopping the runtime: an app created with `git_enabled: false` has
        // no checkpoints to restore, and the store rejects it deep inside
        // git2 with a raw "could not find repository" message. Discovering
        // that after the stop leaves the user with an approved restore that
        // did nothing except take their app offline.
        if !service
            .git_version_control_enabled(&app_id)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err(
                "this app was created without Git version control, so it has no checkpoints to \
                 restore"
                    .into(),
            );
        }
        let layout = self.layout(&app_id)?;
        let restore_reason = "Restoring rewinds application source code. The host will rebuild the fixed local-app scaffold from its verified runtime snapshot before the app can serve again. App data is not changed.".to_string();
        let decision = self
            .request_capability(
                &app_id,
                AppCapabilityKindDto::RestoreCheckpoint,
                None,
                &restore_reason,
            )
            .await?;
        if matches!(raise_decision(decision), PermissionDecision::Deny) {
            return Err("user denied checkpoint restoration".into());
        }
        // Remember whether the app was serving BEFORE the restore so a
        // successful rebuild can put it back the way the user had it.
        let was_running = service
            .runtime_record(&app_id)
            .await
            .map(|runtime| {
                matches!(
                    runtime.state,
                    AppRuntimeState::Starting | AppRuntimeState::Running
                )
            })
            .unwrap_or(false);
        self.stop_runtime(&app_id).await?;
        // The service does the durable work: a PreRestore safety checkpoint
        // first, then the workspace-only Git restore (data/runtime/build
        // paths sit outside the repository and are never reset).
        let safety = service
            .restore_checkpoint(&app_id, &checkpoint_id)
            .await
            .map_err(|error| error.to_string())?;
        // Rebuild the restored source so the served output matches it.
        let builder = crate::mobile::local_apps_build::LocalAppBuilder {
            mobile_linux: self.mobile_linux(),
            host: self,
        };
        if let Err(error) = builder.build_workspace(&layout).await {
            let source_rollback_error = service
                .restore_checkpoint(&app_id, &safety.id)
                .await
                .err()
                .map(|error| error.to_string());
            let rollback_build_error = if source_rollback_error.is_none() {
                builder
                    .build_workspace(&layout)
                    .await
                    .err()
                    .map(|error| error.to_string())
            } else {
                None
            };
            let restarted = if was_running && source_rollback_error.is_none() {
                self.manage_runtime_value(json!({
                    "app_id": app_id.clone(),
                    "action": "start",
                }))
                .await
                .is_ok()
            } else {
                false
            };
            return Err(format!(
                "checkpoint {checkpoint_id} was restored, but rebuilding failed: {error}; source rollback: {}; rollback rebuild: {}; runtime restarted: {restarted}. Read the build log via read_logs (log=\"build\"), fix the source, then run the build tool again.",
                source_rollback_error.as_deref().unwrap_or("completed"),
                rollback_build_error.as_deref().unwrap_or("completed")
            ));
        }
        // Best-effort restart when the runtime was serving before the
        // restore; a failure here leaves the app restored+rebuilt but
        // stopped, which the caller can see and fix via manage_runtime.
        let restarted = if was_running {
            self.manage_runtime_value(json!({ "app_id": app_id.clone(), "action": "start" }))
                .await
                .is_ok()
        } else {
            false
        };
        self.rebind_active_mcp_catalog_to_current_build(&app_id, &layout)
            .await?;
        self.emit_current_verification_summary(&app_id, &layout)
            .await;
        Ok(json!({
            "ok": true,
            "app_id": app_id,
            "checkpoint_id": checkpoint_id,
            "rebuilt": true,
            "restarted": restarted,
        }))
    }

    /// Tell the CLIENT that an agent-driven create failed.
    ///
    /// The tool result already tells the model, and that used to be the only
    /// notification: `emit_app_failure` is reachable exclusively from the
    /// command handlers, so a create started by the agent produced no client
    /// event on either outcome. A client that armed a "creating…" state when
    /// the user submitted a brief therefore had nothing to disarm it with — the
    /// spinner and the disabled create button stayed that way until the app was
    /// killed.
    ///
    /// `app_id` is `None` because there is no app: the failure is precisely
    /// that one never came into being.
    pub(crate) async fn emit_create_failure(&self, error: &local_apps::AppError) {
        self.event_sink
            .emit(ClientEvent::AppOperationFailed {
                app_id: None,
                code: crate::mobile::local_apps_bridge::lower_error_code(error.code()),
                message: error.to_string(),
                // Correctly `None`, not a stub: `request_id` is the
                // correlation key a client puts on its own `CreateApp`, and
                // this failure belongs to an AGENT-driven create that no
                // client command started. There is nothing to echo.
                request_id: None,
            })
            .await;
    }

    /// Write the GUIDED workspace contract for a `CreateMode::Shell` app —
    /// the pre-commit initializer of the "+" button's create.
    ///
    /// This is the twin of [`Self::land_scaffold`], and the difference is the
    /// whole point: it lays down no source, stamps no surface, and touches
    /// nothing but `workspace/LINGXI.md`. A shell has no shape yet, so there
    /// is nothing to scaffold; what it needs is a contract that sends the
    /// agent to interview the user.
    ///
    /// Runs inside the create transaction, after `layout.initialize()` (so the
    /// workspace directory exists) and BEFORE the index commit that makes the
    /// app visible — an initializer failure rolls the whole create back, so an
    /// app can never become visible with an empty workspace and no contract.
    ///
    /// ⚠️ `workspace/LINGXI.md` is the ONE channel that reaches the model on
    /// every turn (it is auto-loaded by the memory hierarchy for any session
    /// rooted in this workspace). If this file is missing, the interview never
    /// starts: the agent sees an empty directory, assumes a normal app, and
    /// starts writing source that `LocalAppScaffold` is going to delete.
    pub(crate) async fn write_guided_contract_value(
        &self,
        record: &local_apps::AppRecord,
    ) -> Result<(), String> {
        let layout = self.layout(&record.id)?;
        let workspace = layout.root().join(layout.workspace_rel());
        let contract = guided_workspace_contract(record);
        tokio::task::spawn_blocking(move || {
            std::fs::write(workspace.join("LINGXI.md"), contract)
                .map_err(|error| format!("write guided workspace LINGXI.md: {error}"))
        })
        .await
        .map_err(|error| format!("join guided contract worker: {error}"))?
    }

    /// `LocalAppScaffold` — the transaction that turns the "+" button's empty
    /// shell into a formed app. §C.1.
    ///
    /// The STEP ORDER below is the specification, not an implementation
    /// detail. Each step's comment says what it is protecting.
    ///
    /// Nothing this call does is visible in the catalog until step 4 returns
    /// `Ok`: any earlier failure leaves `scaffolded == false` and none of
    /// `name` / `brief` / `workflow_model` persisted, the reservation released
    /// by its guard, the build lock released with it, and the app retryable.
    /// The retry is safe precisely because a first scaffold WIPES the editable
    /// surface, so every attempt starts from clean ground (§C.0.1).
    pub(crate) async fn scaffold_shell_app_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        // STEP 1 — reserve, in process, before ANYTHING else, so a second
        // concurrent call is refused rather than racing this one into the same
        // workspace. Held until every path out of this function, `Drop`
        // included. See [`ScaffoldReservation`] for why it must never persist.
        let _reservation = ScaffoldReservation::take(&self.scaffold_reservations, &app_id)?;

        // STEP 2 — validate. Every bound is re-checked here and again in
        // `AppService::commit_scaffold`: the MCP schema's `maxLength` is a
        // hint to the model, not an enforcement point, and this path also
        // refuses before touching the workspace rather than after seeding it.
        let name = confirmed_field(&input, "name")?.to_string();
        if name.len() > local_apps::service::MAX_NAME_BYTES {
            return Err(format!(
                "invalid_argument: name is {} bytes (limit {})",
                name.len(),
                local_apps::service::MAX_NAME_BYTES
            ));
        }
        let brief = confirmed_field(&input, "brief")?.to_string();
        if brief.len() > local_apps::service::MAX_BRIEF_BYTES {
            return Err(format!(
                "invalid_argument: brief is {} bytes (limit {})",
                brief.len(),
                local_apps::service::MAX_BRIEF_BYTES
            ));
        }
        if input.get("runtime_profile").is_some() || input.get("surface").is_some() {
            return Err(
                "invalid_argument: the Host-issued scaffold receipt is authoritative; do not also send runtime_profile or surface".into(),
            );
        }
        let receipt_id = input
            .get("receipt_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                "invalid_argument: receipt_id is required; create scaffold only accepts a Host-issued unified create confirmation receipt".to_string()
            })?;
        let workflow_run_id = required_string(&input, "workflow_run_id")?.to_string();
        Self::validate_workflow_run_id(&workflow_run_id)?;
        let layout = self.layout(&app_id)?;
        let journal =
            local_apps::load_candidate_journal(&layout).map_err(|error| error.to_string())?;
        if journal.workflow_run_id != workflow_run_id {
            return Err(
                "receipt_invalid: workflow run does not match the prepared create candidate".into(),
            );
        }
        if journal.stage < local_apps::McpAuthoringStage::Approved {
            return Err("approval_required: create candidate is not approved".into());
        }
        let candidate = self.load_mcp_candidate(&app_id, &workflow_run_id)?;
        let create_seed = self.load_create_scaffold_seed(&app_id, &workflow_run_id)?;
        // WP5: the candidate staged through `LocalAppStageCreate` — the exact
        // values the user already saw and approved in the native create
        // confirmation sheet — is authoritative here. The model may still echo
        // `name`/`brief` back into this call (the schema still requires them so
        // a caller cannot silently omit confirmation), but only the staged
        // values are ever committed; a mismatched echo is not an error.
        let name = create_seed.name.clone();
        let brief = create_seed.brief.clone();
        self.pending_mcp_receipts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .claim_candidate(
                &receipt_id,
                &app_id,
                &workflow_run_id,
                &journal.approval_contract_sha256,
                &candidate.validated.proposal_sha256,
                now_ms(),
            )
            .map_err(|issue| issue.message)?;
        // Held across every path out of this function — success, an ordinary
        // `Err`, a panic, or the future being dropped mid-transaction — so the
        // claim can never outlive this call the way it used to when only the
        // `Err` arm below released it. See [`ReceiptClaim`].
        let _receipt_claim =
            ReceiptClaim::held(Arc::clone(&self.pending_mcp_receipts), receipt_id.clone());
        let receipt_binding = create_seed.selection.runtime_profile.clone();
        let scaffolded = async {
            let surface = receipt_binding.family.surface();
            let workflow_model = match input.get("workflow_model") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let model = value
                        .as_str()
                        .ok_or_else(|| {
                            "invalid_argument: workflow_model must be a string".to_string()
                        })?
                        .trim();
                    if model.is_empty() {
                        None
                    } else if model.len() > local_apps::service::MAX_WORKFLOW_MODEL_BYTES {
                        return Err(format!(
                            "invalid_argument: workflow_model is {} bytes (limit {})",
                            model.len(),
                            local_apps::service::MAX_WORKFLOW_MODEL_BYTES
                        ));
                    } else {
                        Some(model.to_string())
                    }
                }
            };

            let service = self.service()?;
            let record = service
                .record(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            let original_dependency = service
                .dependency_record(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            if record.scaffolded {
                return Err(format!(
                    "app {app_id} is already scaffolded; its shape and name were fixed when it was \
                     formed and cannot be changed"
                ));
            }

            let mut proposed = record.clone();
            proposed.name = name.clone();
            proposed.brief = brief.clone();
            // WP-MCP-intent: rendered into the formal contract below via
            // `formal_workspace_contract(proposed, ...)` — same reasoning as
            // name/brief above: the shell record has no intent yet, so the
            // contract must read it from the staged candidate, not from
            // `record`.
            proposed.mcp_intent = create_seed.mcp_intent.clone();
            if let Some(model) = &workflow_model {
                proposed.workflow_model = Some(model.clone());
            }
            let (build_lock, recovery_lock, recovery) = self
                .land_scaffold(
                    &proposed,
                    surface,
                    Some(receipt_binding.clone()),
                    Some(create_seed.clone()),
                )
                .await?;
            let layout = self.layout(&app_id)?;
            let result: Result<local_apps::AppRecord, String> = async {
                self.install_scaffold_dependencies(&service, &app_id, &layout)
                    .await?;
                // The plan-driven path journals its own commit proof under the
                // run id the approval bound. A run that never went through
                // `prepare` has no state document, so this is a no-op.
                self.record_prepare_scaffold_commit(&app_id, &workflow_run_id, &receipt_binding)?;
                service
                    .commit_scaffold(
                        &app_id,
                        &name,
                        &brief,
                        workflow_model.as_deref(),
                        create_seed.mcp_intent.as_ref(),
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            match result {
                Ok(committed) => {
                    if let Err(error) = recovery.commit() {
                        tracing::warn!(
                            app_id = %app_id,
                            %error,
                            "scaffold recovery cleanup deferred after commit"
                        );
                    }
                    drop(build_lock);
                    drop(recovery_lock);
                    Ok(committed)
                }
                Err(error) => {
                    let recovery_error = recovery.rollback().err();
                    let dependency_error = service
                        .restore_dependency_record(original_dependency.clone())
                        .await
                        .err()
                        .map(|error| error.to_string());
                    drop(build_lock);
                    drop(recovery_lock);
                    match (recovery_error, dependency_error) {
                        (Some(recovery_error), Some(dependency_error)) => Err(format!(
                            "{error}; scaffold rollback failed: {recovery_error}; dependency rollback failed: {dependency_error}"
                        )),
                        (Some(recovery_error), None) => {
                            Err(format!("{error}; scaffold rollback failed: {recovery_error}"))
                        }
                        (None, Some(dependency_error)) => {
                            Err(format!("{error}; dependency rollback failed: {dependency_error}"))
                        }
                        (None, None) => Err(error),
                    }
                }
            }
        }
        .await;
        let committed = match scaffolded {
            Ok(committed) => {
                // The scaffold above already committed to disk and to the app
                // record: files are written, `record.scaffolded` is true, a
                // retry would now hit "already scaffolded". A failure in this
                // purely-bookkeeping receipt commit must not be reported as a
                // failed create on top of that — it would tell the model (and
                // the user) the app was never made when it was, and a retry
                // could not recover since the app already exists. Warn and
                // continue, matching the same idiom already used for the
                // journal-stamp failure a few lines below.
                if let Err(issue) = self
                    .pending_mcp_receipts
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .commit_claimed_candidate(
                        &receipt_id,
                        &app_id,
                        &workflow_run_id,
                        &journal.approval_contract_sha256,
                        &candidate.validated.proposal_sha256,
                    )
                {
                    tracing::warn!(
                        app_id = %app_id,
                        workflow_run_id = %workflow_run_id,
                        error = %issue.message,
                        "create scaffold committed but receipt commit bookkeeping failed"
                    );
                }
                let layout = self.layout(&app_id)?;
                let create_only = candidate.validated.tools.is_empty();
                if create_only {
                    if let Err(error) =
                        self.delete_mcp_candidate_state(&layout, &app_id, &workflow_run_id)
                    {
                        tracing::warn!(
                            app_id = %app_id,
                            workflow_run_id = %workflow_run_id,
                            %error,
                            "plain create committed but create-only candidate cleanup failed"
                        );
                    }
                } else {
                    // The candidate journal survives an MCP-carrying create
                    // (it still gates the tool grant), but the run's create
                    // staging is spent the moment the scaffold commits —
                    // and this arm is the only reclaim point that scaffold
                    // success ever reaches (`delete_mcp_candidate_state`,
                    // which sweeps it for a create-only app, is never called
                    // here). Best-effort so a reclaim failure cannot fail a
                    // committed scaffold.
                    if let Err(error) =
                        Self::remove_create_staging(&self.root, &app_id, &workflow_run_id)
                    {
                        tracing::warn!(
                            app_id = %app_id,
                            workflow_run_id = %workflow_run_id,
                            %error,
                            "scaffold committed but create staging reclaim failed"
                        );
                    }
                    // r2-never-wired-07: this arm used to stamp a
                    // `consumed_receipt_sha256` into the journal and re-seal
                    // it. NOTHING ever read that digest, and nothing could
                    // usefully read it: replay is refused in-process by
                    // `McpReceiptBook` (`commit_claimed_candidate` above, and
                    // `consume_candidate` in `promote_mcp_candidate`), and the
                    // book is memory-only — after a process restart EVERY
                    // receipt id is already refused as `receipt_missing`, so a
                    // journal reader could not add a refusal the restart had
                    // not already made. The in-process book is the whole
                    // replay defence; the write was a re-seal of the same
                    // journal with a field no code path consulted.
                }
                committed
            }
            // No explicit `release_claim` here: `_receipt_claim`'s `Drop`
            // covers this arm AND every other exit (panic, dropped future)
            // that used to leak the claim.
            Err(error) => return Err(error),
        };

        // STEP 5 — the pinned init session's title, AFTER the commit and
        // deliberately outside it. The interview ran in a session titled
        // `untitled` (the shell's placeholder name, minted into a PERSISTED
        // session directory), and that title is what the user's session list
        // shows forever otherwise.
        //
        // ⚠️ A failure here is logged and NOT rolled back. The scaffold has
        // already committed — the app is formed, its workspace is seeded and
        // its record says so — and unwinding that because a metadata line did
        // not append would destroy real work over a cosmetic field. What makes
        // that acceptable is that the boot backfill sweep runs the SAME
        // reconciliation on every launch, so a title left behind here is
        // repaired rather than stranded.
        if let Some(catalog) = self.session_catalog.get() {
            match reconcile_app_init_session_title(
                &catalog.lingxi_home,
                &self.root,
                catalog.fs.clone(),
                &committed,
            )
            .await
            {
                Ok(true) => tracing::info!(
                    app_id = %committed.id,
                    "renamed the pinned init session after scaffold"
                ),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    app_id = %committed.id,
                    %error,
                    "pinned init-session rename failed; boot reconciliation will retry"
                ),
            }
        }
        Ok(json!({
            "app": committed,
            "next_step": scaffold_next_step_guidance(),
        }))
    }

    /// §C.1 step 3: everything that reaches DISK, under `lock_app_build` from
    /// the first byte, with the lock handed back to the caller still held.
    ///
    /// ⚠️ The lock is not optional and the in-process reservation is not a
    /// substitute. `lock_app_build`'s own contract is that a caller holds it
    /// for the COMPLETE operation that mutates an app's workspace tree, and
    /// physical deletion takes the SAME lock (`storage::trash_app_dir` via
    /// `lock_app_build_if_present`). The reservation excludes another
    /// `LocalAppScaffold`; it does not exclude a concurrent `DeleteApp`, which
    /// renames the app directory into `.trash` while the seed is still being
    /// written — leaving files under a path nothing indexes and nothing
    /// reclaims. Returning the guard, rather than dropping it here, is what
    /// keeps it held across the commit point.
    ///
    /// ⚠️ A durable shell snapshot/journal is written BEFORE the manifest or
    /// workspace is changed. `manifest.surface` is stamped before the seed,
    /// and `record.scaffolded` is written LAST (step 4). These orders are
    /// deliberate: a crash before the record commit is rolled back from the
    /// journal before the next service load, while a committed record causes
    /// only recovery-material cleanup.
    async fn land_scaffold(
        &self,
        proposed: &local_apps::AppRecord,
        surface: local_apps::AppSurface,
        requested_binding: Option<local_apps::AppRuntimeProfileBinding>,
        create_seed: Option<CreateScaffoldSeed>,
    ) -> Result<
        (
            platform_api::rooted_fs::RootedFileLock,
            platform_api::rooted_fs::RootedFileLock,
            local_apps::storage::ScaffoldRecoveryHandle,
        ),
        String,
    > {
        let layout = self.layout(&proposed.id)?;
        let binding = requested_binding.ok_or_else(|| {
            "runtime profile binding is required; scaffold must consume a native confirmation receipt"
                .to_string()
        })?;
        if binding.family.surface() != surface {
            return Err(format!(
                "runtime profile {} requires the {} surface, but scaffold requested {}",
                binding.family,
                binding.family.surface().as_str(),
                surface.as_str()
            ));
        }
        let target =
            crate::mobile::local_apps_build::LocalAppBuildTarget::from_runtime_binding(&binding)
                .map_err(|error| error.to_string())?;
        let template_origin = create_seed
            .as_ref()
            .map(|seed| local_apps::AppTemplateOrigin {
                plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
                plugin_version: "builtin".into(),
                template_id: seed.selection.template_id.clone(),
                template_sha256: seed.selection.template_sha256.clone(),
            })
            .unwrap_or_else(|| builtin_template_origin(&binding));
        // Rendered from the PROPOSED record — the confirmed name and brief.
        // Rendering it from the creation record writes `# Local App: untitled`
        // with an empty brief, permanently: see `formal_workspace_contract`.
        let context = formal_workspace_contract(proposed, &binding);
        let name = proposed.name.clone();
        let brief = proposed.brief.clone();
        let device_context = self.host_device_context();
        let root = self.root.clone();
        let app_id = proposed.id.clone();
        let create_seed = create_seed.clone();
        tokio::task::spawn_blocking(
            move ||
                -> Result<
                    (
                        platform_api::rooted_fs::RootedFileLock,
                        platform_api::rooted_fs::RootedFileLock,
                        local_apps::storage::ScaffoldRecoveryHandle,
                    ),
                    String,
                > {
                // 3a — take the global recovery lock before the per-app build
                // lock. Store loading takes this global lock before its index
                // lock, preventing an index/build inversion while recovering.
                let recovery_lock = local_apps::storage::lock_scaffold_recovery(&root)
                    .map_err(|error| error.to_string())?;
                let build_lock = local_apps::storage::lock_app_build(&root, &app_id)
                    .map_err(|error| error.to_string())?;
                // The complete shell snapshot and journal are durable before
                // any manifest/workspace mutation. A crash after this point is
                // therefore recoverable before the next service load.
                let recovery = local_apps::storage::begin_scaffold_recovery(
                    &root,
                    &app_id,
                    &name,
                    &brief,
                )
                .map_err(|error| error.to_string())?;
                let landed: Result<(), String> = (|| {
                    // 3c — the manifest's `surface` and `name`, under the
                    // §C.1.4 invariant.
                    stamp_scaffold_identity(&layout, &name, &binding, template_origin)?;
                    // 3d — wipe the editable surface, then seed it. `true` is
                    // the first-scaffold flag: everything an agent wrote during
                    // the interview is removed before the seed lands, because
                    // a pre-written `app/app.js` would out-resolve the seeded
                    // `app/app.jsx` and the seed would become dead code.
                    crate::mobile::local_apps_build::scaffold_workspace_initialized(&layout, target, true)
                        .map_err(|error| error.to_string())?;
                    // 3e — the formal contract, overwriting the guided one.
                    let workspace = layout.root().join(layout.workspace_rel());
                    if let Some(seed) = create_seed.as_ref() {
                        copy_directory_contents(&seed.template_root, &workspace)?;
                        persist_active_mcp_flow_contexts(&workspace, &app_id, &seed.contexts)?;
                    } else {
                        let artifacts = scaffold_runtime_profile(Some(binding.clone()), surface)?;
                        persist_runtime_profile_files(&workspace, &artifacts)?;
                    }
                    std::fs::write(workspace.join("LINGXI.md"), &context)
                        .map_err(|error| format!("write workspace LINGXI.md: {error}"))?;
                    // The native target, on the same manifest, so a formed app
                    // carries it whether or not the agent ever calls
                    // `LocalAppManifest`. Same first-write window as the name.
                    if let Some(device_context) = device_context {
                        let mut manifest = local_apps::load_manifest(&layout)
                            .map_err(|error| error.to_string())?;
                        manifest.device_context = Some(device_context);
                        local_apps::save_manifest(&layout, &manifest)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok(())
                })();
                if let Err(error) = landed {
                    let recovery_error = recovery.rollback().err();
                    drop(build_lock);
                    drop(recovery_lock);
                    return match recovery_error {
                        Some(recovery_error) => Err(format!(
                            "{error}; scaffold rollback failed: {recovery_error}"
                        )),
                        None => Err(error),
                    };
                }
                Ok((build_lock, recovery_lock, recovery))
            },
        )
        .await
        .map_err(|error| format!("join scaffold landing worker: {error}"))?
    }
}

/// Write the app's identity onto its manifest — `surface` and `name` — under
/// the §C.1.4 hash invariant.
///
/// ⛔ FIRST WRITE ONLY. `AppManifest::hash()` serialises the WHOLE struct
/// INCLUDING `name`, and `AppDataStore::ensure_manifest` compares that hash
/// against the SQLite `_lingxi_schema.manifest_hash` row. Changing `name`
/// after a data store exists therefore breaks EVERY subsequent data read and
/// write with "database manifest mismatch" — silent, total, user-visible data
/// loss. A freshly created shell is safe because it has no collections, so
/// `AppDataStore::open` (which is what writes that row) has never run and the
/// database file does not exist. That is asserted here rather than assumed.
///
/// This is the real reason renaming an app is not offered, and this function
/// must NEVER be generalised into a rename path.
fn stamp_scaffold_identity(
    layout: &AppLayout,
    name: &str,
    binding: &local_apps::AppRuntimeProfileBinding,
    template_origin: local_apps::AppTemplateOrigin,
) -> Result<(), String> {
    let database = layout.database_path();
    if database.exists() {
        return Err(format!(
            "app {} already has a database at {}; writing manifest.name now would change \
             AppManifest::hash() and make every later data read and write fail with a database \
             manifest mismatch",
            layout.app_id(),
            database.display()
        ));
    }
    let mut manifest = local_apps::load_manifest(layout).map_err(|error| error.to_string())?;
    manifest.surface = Some(binding.family.surface());
    manifest.runtime_profile = Some(binding.clone());
    manifest.dependency_snapshot = None;
    manifest.template_origin = Some(template_origin);
    manifest.name = name.to_string();
    local_apps::save_manifest(layout, &manifest).map_err(|error| error.to_string())
}

fn scaffold_runtime_profile(
    requested_binding: Option<local_apps::AppRuntimeProfileBinding>,
    surface: local_apps::AppSurface,
) -> Result<crate::mobile::local_app_runtime_profiles::RuntimeProfileScaffoldArtifacts, String> {
    let binding = requested_binding.ok_or_else(|| {
        "runtime profile binding is required; scaffold must consume a native confirmation receipt"
            .to_string()
    })?;
    if binding.family.surface() != surface {
        return Err(format!(
            "runtime profile {} requires the {} surface, but scaffold requested {}",
            binding.family,
            binding.family.surface().as_str(),
            surface.as_str()
        ));
    }
    crate::mobile::local_app_runtime_profiles::scaffold_artifacts_for_binding(&binding)
        .map_err(|error| error.to_string())
}

fn persist_runtime_profile_files(
    workspace: &Path,
    artifacts: &crate::mobile::local_app_runtime_profiles::RuntimeProfileScaffoldArtifacts,
) -> Result<(), String> {
    for (relative, bytes) in &artifacts.files {
        crate::mobile::local_apps_build::write_file(workspace, relative, bytes, true)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(source)
        .map_err(|error| format!("read create staging {}: {error}", source.display()))?
    {
        let entry = entry
            .map_err(|error| format!("read create staging entry {}: {error}", source.display()))?;
        let source_path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "inspect create staging entry {}: {error}",
                source_path.display()
            )
        })?;
        let destination_path = destination.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(format!(
                "create_staging_invalid: staged template contains symlink {}",
                source_path.display()
            ));
        }
        if file_type.is_dir() {
            std::fs::create_dir_all(&destination_path).map_err(|error| {
                format!(
                    "create destination directory {}: {error}",
                    destination_path.display()
                )
            })?;
            copy_directory_contents(&source_path, &destination_path)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!(
                "create_staging_invalid: staged template contains special file {}",
                source_path.display()
            ));
        }
        let bytes = std::fs::read(&source_path)
            .map_err(|error| format!("read staged file {}: {error}", source_path.display()))?;
        let relative = destination_path
            .strip_prefix(destination)
            .expect("staged file destination stays within workspace");
        let relative_str = relative.to_str().ok_or_else(|| {
            "create_staging_invalid: staged template path is not valid UTF-8".to_string()
        })?;
        crate::mobile::local_apps_build::write_file(destination, relative_str, &bytes, true)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn builtin_template_origin(
    binding: &local_apps::AppRuntimeProfileBinding,
) -> local_apps::AppTemplateOrigin {
    local_apps::AppTemplateOrigin {
        plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
        plugin_version: "builtin".into(),
        template_id: format!(
            "{}-r{}",
            binding.family.as_str().replace('_', "-"),
            binding.revision
        ),
        template_sha256: binding.contract_sha256.clone(),
    }
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered = object
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(Map::from_iter(ordered))
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

fn dependency_yaml_scalar(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("dependency lock contains an empty YAML scalar".into());
    }
    if value.starts_with('\'') {
        if value.len() < 2 || !value.ends_with('\'') {
            return Err("dependency lock contains an unterminated single-quoted scalar".into());
        }
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value).map_err(|error| {
            format!("dependency lock contains an invalid quoted scalar: {error}")
        });
    }
    Ok(value.to_string())
}

/// Read the exact direct dependency specifiers from pnpm's root importer.
///
/// The host pins pnpm's lockfile format through `PNPM_TOOLCHAIN_KEY`, so this
/// intentionally parses only the small, stable `importers -> . ->
/// dependencies -> <package> -> specifier` surface that must agree with the
/// Host-minted effective package. It does not use package-resolution entries
/// as proof: the same package can occur there transitively without being a
/// requested root dependency.
fn pnpm_root_dependency_specifiers(lockfile: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let lockfile = std::str::from_utf8(lockfile)
        .map_err(|error| format!("resolved dependency lockfile is not UTF-8: {error}"))?;
    let mut documents = Vec::new();
    let mut document = String::new();
    for line in lockfile.lines() {
        if line == "---" || line.starts_with("--- #") {
            if !document.trim().is_empty() {
                documents.push(std::mem::take(&mut document));
            }
        } else {
            document.push_str(line);
            document.push('\n');
        }
    }
    if !document.trim().is_empty() {
        documents.push(document);
    }
    let mut dependencies = None;
    let mut found_config = false;
    for document in documents {
        match pnpm_document_dependency_specifiers(&document)? {
            Some(specifiers) => {
                if dependencies.replace(specifiers).is_some() {
                    return Err(
                        "resolved dependency lockfile contains multiple dependency documents"
                            .into(),
                    );
                }
            }
            None => {
                if found_config || dependencies.is_some() {
                    return Err(
                        "resolved dependency lockfile has ambiguous configuration documents".into(),
                    );
                }
                found_config = true;
            }
        }
    }
    dependencies.ok_or_else(|| {
        "resolved dependency lockfile is missing the root dependency importer".into()
    })
}

// pnpm 12 puts package-manager/configuration dependencies in a separate YAML
// document. Parse each document independently so its importer map cannot hide
// duplicate keys or a second application dependency graph.
fn pnpm_document_dependency_specifiers(
    lockfile: &str,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let mut in_importers = false;
    let mut in_root_importer = false;
    let mut in_dependencies = false;
    let mut found_importers = false;
    let mut found_root_importer = false;
    let mut found_other_importer = false;
    let mut found_dependencies = false;
    let mut config_sections = HashSet::new();
    let mut unexpected_config_section = false;
    let mut current_package: Option<String> = None;
    let mut dependency_keys = HashSet::new();
    let mut specifiers = BTreeMap::new();

    for line in lockfile.lines() {
        let leading = line.trim_start_matches(' ');
        if leading.starts_with('\t') {
            return Err("resolved dependency lockfile uses tabs for indentation".into());
        }
        let indent = line.len() - leading.len();
        let trimmed = leading.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent == 0 {
            if trimmed == "importers:" {
                if found_importers {
                    return Err("resolved dependency lockfile repeats the importers map".into());
                }
                found_importers = true;
                in_importers = true;
                continue;
            }
            in_importers = false;
            in_root_importer = false;
            in_dependencies = false;
            current_package = None;
            continue;
        }
        if !in_importers {
            continue;
        }
        if indent == 2 {
            let importer = trimmed.strip_suffix(':').ok_or_else(|| {
                "resolved dependency lockfile has an invalid importer".to_string()
            })?;
            in_root_importer = dependency_yaml_scalar(importer)? == ".";
            if in_root_importer && found_root_importer {
                return Err("resolved dependency lockfile repeats the root importer".into());
            }
            found_root_importer |= in_root_importer;
            found_other_importer |= !in_root_importer;
            in_dependencies = false;
            current_package = None;
            continue;
        }
        if !in_root_importer {
            continue;
        }
        if indent == 4 {
            unexpected_config_section |= !matches!(
                trimmed.split(':').next(),
                Some("configDependencies" | "packageManagerDependencies")
            );
            if let Some(key) = trimmed
                .split(':')
                .next()
                .filter(|key| matches!(*key, "configDependencies" | "packageManagerDependencies"))
            {
                if !config_sections.insert(key) {
                    return Err(
                        "resolved dependency lockfile repeats a root configuration map".into(),
                    );
                }
            }
            in_dependencies = trimmed == "dependencies:";
            if in_dependencies && found_dependencies {
                return Err(
                    "resolved dependency lockfile repeats the root dependencies map".into(),
                );
            }
            if matches!(trimmed, "devDependencies:" | "optionalDependencies:") {
                return Err(format!(
                    "resolved dependency lockfile contains unexpected root {trimmed}"
                ));
            }
            found_dependencies |= in_dependencies;
            current_package = None;
            continue;
        }
        if !in_dependencies {
            continue;
        }
        if indent == 6 {
            let package = trimmed.strip_suffix(':').ok_or_else(|| {
                "resolved dependency lockfile has an invalid dependency key".to_string()
            })?;
            let package = dependency_yaml_scalar(package)?;
            if !dependency_keys.insert(package.clone()) {
                return Err(format!(
                    "resolved dependency lockfile repeats root dependency {package}"
                ));
            }
            current_package = Some(package);
            continue;
        }
        if indent == 8 {
            let Some(package) = current_package.as_ref() else {
                continue;
            };
            if let Some(specifier) = trimmed.strip_prefix("specifier:") {
                let specifier = dependency_yaml_scalar(specifier)?;
                if specifiers.insert(package.clone(), specifier).is_some() {
                    return Err(format!(
                        "resolved dependency lockfile repeats the specifier for {package}"
                    ));
                }
            }
        }
    }

    if found_root_importer && !found_dependencies && !config_sections.is_empty() {
        if found_other_importer || unexpected_config_section {
            return Err("resolved dependency lockfile configuration document has unexpected importer fields".into());
        }
        return Ok(None);
    }
    if !found_root_importer || !found_dependencies {
        return Err("resolved dependency lockfile is missing the root dependency importer".into());
    }
    if !config_sections.is_empty() {
        return Err(
            "resolved dependency lockfile mixes configuration and application dependencies".into(),
        );
    }
    if dependency_keys.len() != specifiers.len() {
        let missing = dependency_keys
            .iter()
            .find(|package| !specifiers.contains_key(*package))
            .cloned()
            .unwrap_or_else(|| "<unknown>".to_string());
        return Err(format!(
            "resolved dependency lockfile is missing the root specifier for {missing}"
        ));
    }
    Ok(Some(specifiers))
}

fn effective_package_dependency_specifiers(
    package_json: &[u8],
) -> Result<BTreeMap<String, String>, String> {
    let package: Value = serde_json::from_slice(package_json)
        .map_err(|error| format!("parse effective dependency package: {error}"))?;
    let dependencies = package
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| "effective dependency package is missing dependencies".to_string())?;
    dependencies
        .iter()
        .map(|(name, version)| {
            let version = version.as_str().ok_or_else(|| {
                format!("effective dependency {name} must use a string specifier")
            })?;
            Ok((name.clone(), version.to_string()))
        })
        .collect()
}

fn validate_resolved_dependency_lock(package_json: &[u8], lockfile: &[u8]) -> Result<(), String> {
    let expected = effective_package_dependency_specifiers(package_json)?;
    let actual = pnpm_root_dependency_specifiers(lockfile)?;
    if actual == expected {
        return Ok(());
    }
    let mismatch = expected
        .iter()
        .find(|(package, version)| actual.get(*package) != Some(*version))
        .map(|(package, version)| format!("{package}@{version}"))
        .or_else(|| {
            actual
                .keys()
                .find(|package| !expected.contains_key(*package))
                .map(|package| format!("unexpected {package}"))
        })
        .unwrap_or_else(|| "unknown mismatch".to_string());
    Err(format!(
        "resolved dependency lockfile does not match the effective package root importer ({mismatch})"
    ))
}

fn installed_package_manifest(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) != Some("package.json") {
        return false;
    }
    let Some(package_dir) = path.parent() else {
        return false;
    };
    let Some(parent) = package_dir.parent() else {
        return false;
    };
    if parent.file_name().and_then(|name| name.to_str()) == Some("node_modules") {
        return true;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    grandparent.file_name().and_then(|name| name.to_str()) == Some("node_modules")
        && parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('@'))
}

fn package_license_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Object(object) => object
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn collect_installed_packages(
    root: &Path,
    packages: &mut BTreeMap<(String, String), Option<String>>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("read dependency tree {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!("inspect dependency tree entry {}: {error}", path.display())
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_installed_packages(&path, packages)?;
            continue;
        }
        if !file_type.is_file() || !installed_package_manifest(&path) {
            continue;
        }
        let body = std::fs::read(&path).map_err(|error| {
            format!(
                "read installed package manifest {}: {error}",
                path.display()
            )
        })?;
        let manifest: Value = serde_json::from_slice(&body).map_err(|error| {
            format!(
                "parse installed package manifest {}: {error}",
                path.display()
            )
        })?;
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "installed package manifest {} is missing name",
                    path.display()
                )
            })?
            .to_string();
        let version = manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "installed package manifest {} is missing version",
                    path.display()
                )
            })?
            .to_string();
        let license = manifest.get("license").and_then(package_license_string);
        packages.entry((name, version)).or_insert(license);
    }
    Ok(())
}

fn dependency_snapshot_inventory_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join(DEPENDENCY_SNAPSHOT_INVENTORY_FILE)
}

fn dependency_inventory_digest(inventory: &VerifiedDependencyInventory) -> Result<String, String> {
    let mut unsigned = inventory.clone();
    unsigned.inventory_digest.clear();
    let bytes = serde_json::to_vec(&unsigned)
        .map_err(|error| format!("serialize dependency inventory for digest: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn write_verified_dependency_inventory(
    snapshot_root: &Path,
    lock_digest: &str,
    tree_digest: &str,
    packages: &BTreeMap<(String, String), Option<String>>,
    toolchain_key: &str,
) -> Result<(), String> {
    let mut inventory = VerifiedDependencyInventory {
        schema_version: DEPENDENCY_SNAPSHOT_INVENTORY_SCHEMA_VERSION,
        toolchain_key: toolchain_key.to_string(),
        lock_digest: lock_digest.to_string(),
        tree_digest: tree_digest.to_string(),
        inventory_digest: String::new(),
        packages: packages
            .iter()
            .map(|((name, version), license)| InstalledDependencyPackage {
                name: name.clone(),
                version: version.clone(),
                license: license.clone(),
            })
            .collect(),
    };
    inventory.inventory_digest = dependency_inventory_digest(&inventory)?;
    let bytes = serde_json::to_vec_pretty(&inventory)
        .map_err(|error| format!("serialize dependency inventory: {error}"))?;
    if bytes.len() > MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES {
        return Err(format!(
            "dependency inventory exceeds {} bytes",
            MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES
        ));
    }
    let path = dependency_snapshot_inventory_path(snapshot_root);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| {
        format!(
            "write dependency inventory {}: {error}",
            temporary.display()
        )
    })?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("publish dependency inventory {}: {error}", path.display()))?;
    make_dependency_files_read_only(&path)
        .map_err(|error| format!("protect dependency inventory {}: {error}", path.display()))
}

/// Read only an inventory whose provenance and content digest match the
/// immutable snapshot it sits beside.  A malformed or stale sidecar is a
/// cache miss, never permission to trust a tree or an invented ready state.
fn read_verified_dependency_inventory(
    snapshot_root: &Path,
    lock_digest: &str,
    tree_digest: &str,
    toolchain_key: &str,
) -> Result<Option<BTreeMap<(String, String), Option<String>>>, String> {
    let path = dependency_snapshot_inventory_path(snapshot_root);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES as u64
    {
        return Ok(None);
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let inventory: VerifiedDependencyInventory = match serde_json::from_slice(&bytes) {
        Ok(inventory) => inventory,
        Err(_) => return Ok(None),
    };
    if inventory.schema_version != DEPENDENCY_SNAPSHOT_INVENTORY_SCHEMA_VERSION
        || inventory.toolchain_key != toolchain_key
        || inventory.lock_digest != lock_digest
        || inventory.tree_digest != tree_digest
        || inventory.inventory_digest.is_empty()
        || dependency_inventory_digest(&inventory)? != inventory.inventory_digest
    {
        return Ok(None);
    }
    let mut packages = BTreeMap::new();
    let mut previous_identity: Option<(&str, &str)> = None;
    for package in &inventory.packages {
        let identity = (package.name.as_str(), package.version.as_str());
        if package.name.trim().is_empty()
            || package.name.trim() != package.name
            || package.version.trim().is_empty()
            || package.version.trim() != package.version
            || package
                .license
                .as_ref()
                .is_some_and(|license| license.trim().is_empty() || license.trim() != license)
            || previous_identity.is_some_and(|previous| previous >= identity)
            || packages
                .insert(
                    (package.name.clone(), package.version.clone()),
                    package.license.clone(),
                )
                .is_some()
        {
            return Ok(None);
        }
        previous_identity = Some(identity);
    }
    Ok(Some(packages))
}

fn spdx_ref_for_package(name: &str, version: &str) -> String {
    let normalized = format!("{name}-{version}")
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' => ch,
            _ => '-',
        })
        .collect::<String>();
    let identity = format!("{name}\0{version}");
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    format!("SPDXRef-Package-{normalized}-{digest}")
}

fn installed_dependency_sbom(
    node_modules_root: &Path,
    binding: &local_apps::AppRuntimeProfileBinding,
    tree_sha256: &str,
) -> Result<Vec<u8>, String> {
    installed_dependency_sbom_with_inventory(node_modules_root, binding, tree_sha256, None)
}

fn installed_dependency_sbom_with_inventory(
    node_modules_root: &Path,
    binding: &local_apps::AppRuntimeProfileBinding,
    tree_sha256: &str,
    snapshot_inventory: Option<(&Path, &str)>,
) -> Result<Vec<u8>, String> {
    let toolchain_key = toolchain_for_binding(binding)
        .map_err(|error| error.to_string())?
        .key();
    let mut packages = BTreeMap::<(String, String), Option<String>>::new();
    let used_inventory = snapshot_inventory
        .and_then(|(snapshot_root, lock_digest)| {
            read_verified_dependency_inventory(
                snapshot_root,
                lock_digest,
                tree_sha256,
                toolchain_key,
            )
            .ok()
            .flatten()
        })
        .filter(|cached| !cached.is_empty())
        .map(|cached| {
            packages = cached;
        })
        .is_some();
    if !used_inventory {
        collect_installed_packages(node_modules_root, &mut packages)?;
    }
    if packages.is_empty() {
        return Err(format!(
            "dependency snapshot cannot be verified because {} contains no installed package manifests",
            node_modules_root.display()
        ));
    }
    let root_id = format!(
        "SPDXRef-LingXiRuntime-{}-r{}",
        binding.family.as_str(),
        binding.revision
    );
    let mut package_values = vec![canonicalize_json(Value::Object(Map::from_iter([
        ("SPDXID".to_string(), Value::String(root_id.clone())),
        (
            "name".to_string(),
            Value::String(format!(
                "LingXi Local App Installed Dependencies {} r{}",
                binding.family.as_str(),
                binding.revision
            )),
        ),
        (
            "versionInfo".to_string(),
            Value::String(format!("{}+{}", binding.contract_sha256, tree_sha256)),
        ),
        (
            "downloadLocation".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
        (
            "licenseConcluded".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
        (
            "licenseDeclared".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
        (
            "copyrightText".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
    ])))];
    let mut relationships = Vec::new();
    for ((name, version), license) in packages {
        let package_id = spdx_ref_for_package(&name, &version);
        let license = license.unwrap_or_else(|| "NOASSERTION".to_string());
        package_values.push(canonicalize_json(Value::Object(Map::from_iter([
            ("SPDXID".to_string(), Value::String(package_id.clone())),
            ("name".to_string(), Value::String(name)),
            ("versionInfo".to_string(), Value::String(version)),
            (
                "downloadLocation".to_string(),
                Value::String("NOASSERTION".to_string()),
            ),
            (
                "licenseConcluded".to_string(),
                Value::String("NOASSERTION".to_string()),
            ),
            ("licenseDeclared".to_string(), Value::String(license)),
            (
                "copyrightText".to_string(),
                Value::String("NOASSERTION".to_string()),
            ),
        ]))));
        relationships.push(canonicalize_json(Value::Object(Map::from_iter([
            ("spdxElementId".to_string(), Value::String(root_id.clone())),
            (
                "relationshipType".to_string(),
                Value::String("DEPENDS_ON".to_string()),
            ),
            ("relatedSpdxElement".to_string(), Value::String(package_id)),
        ]))));
    }
    let document = canonicalize_json(Value::Object(Map::from_iter([
        (
            "spdxVersion".to_string(),
            Value::String("SPDX-2.3".to_string()),
        ),
        (
            "dataLicense".to_string(),
            Value::String("CC0-1.0".to_string()),
        ),
        (
            "SPDXID".to_string(),
            Value::String("SPDXRef-DOCUMENT".to_string()),
        ),
        (
            "name".to_string(),
            Value::String(format!(
                "LingXi Installed Dependency SBOM {} r{}",
                binding.family.as_str(),
                binding.revision
            )),
        ),
        (
            "documentNamespace".to_string(),
            Value::String(format!(
                "https://lingxi.local/app-dependencies/{}/r{}/{}/{}",
                binding.family.as_str(),
                binding.revision,
                binding.contract_sha256,
                tree_sha256,
            )),
        ),
        (
            "creationInfo".to_string(),
            Value::Object(Map::from_iter([
                (
                    "created".to_string(),
                    Value::String("2026-08-27T00:00:00Z".to_string()),
                ),
                (
                    "creators".to_string(),
                    Value::Array(vec![Value::String(
                        "Tool: lingxi-local-app-installed-dependencies".to_string(),
                    )]),
                ),
            ])),
        ),
        (
            "documentDescribes".to_string(),
            Value::Array(vec![Value::String(root_id.clone())]),
        ),
        ("packages".to_string(), Value::Array(package_values)),
        ("relationships".to_string(), Value::Array(relationships)),
        ("files".to_string(), Value::Array(vec![])),
    ])));
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize dependency SBOM: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn refresh_runtime_profile_snapshot(
    layout: &AppLayout,
    tree_sha256: &str,
    snapshot_inventory: Option<(&Path, &str)>,
) -> Result<local_apps::AppDependencySnapshot, String> {
    let mut manifest = local_apps::load_manifest(layout).map_err(|error| error.to_string())?;
    let binding = manifest.runtime_profile.clone().ok_or_else(|| {
        format!(
            "app {} is missing its runtime profile binding",
            layout.app_id()
        )
    })?;
    let workspace = layout.root().join(layout.workspace_rel());
    let requested_bytes = std::fs::read(
        workspace.join(crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL),
    )
    .map_err(|error| format!("read requested dependency snapshot input: {error}"))?;
    let package_bytes = std::fs::read(
        workspace.join(crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL),
    )
    .map_err(|error| format!("read effective dependency package: {error}"))?;
    let lockfile_bytes =
        std::fs::read(workspace.join(crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL))
            .map_err(|error| format!("read dependency lockfile: {error}"))?;
    let sbom_span = tracing::debug_span!(
        "local_app_dependency_sbom",
        app_id = %layout.app_id(),
        tree_digest = %tree_sha256,
        cached_inventory = snapshot_inventory.is_some(),
    );
    let sbom = {
        let _perf = LocalAppPerfDiagnosticTimer::start("dependency_sbom_generate");
        sbom_span.in_scope(|| {
            installed_dependency_sbom_with_inventory(
                &workspace.join("node_modules"),
                &binding,
                tree_sha256,
                snapshot_inventory,
            )
        })?
    };
    let artifacts = crate::mobile::local_app_runtime_profiles::snapshot_artifacts_for_binding(
        &binding,
        crate::mobile::local_app_runtime_profiles::hash_bytes(&requested_bytes),
        crate::mobile::local_app_runtime_profiles::hash_bytes(&package_bytes),
        crate::mobile::local_app_runtime_profiles::hash_bytes(&lockfile_bytes),
        tree_sha256.to_string(),
        &sbom,
    )
    .map_err(|error| error.to_string())?;
    for (relative, bytes) in &artifacts.files {
        crate::mobile::local_apps_build::write_file(&workspace, relative, bytes, true)
            .map_err(|error| error.to_string())?;
    }
    manifest.dependency_snapshot = Some(artifacts.snapshot.clone());
    local_apps::save_manifest(layout, &manifest).map_err(|error| error.to_string())?;
    Ok(artifacts.snapshot)
}

/// What to tell the agent immediately after `LocalAppScaffold` commits.
///
/// Unlike [`create_next_step_guidance`], this one runs in a session that IS
/// rooted in the app's workspace — that is the whole point of the shell flow —
/// so the correct next move is to re-read the contract that has just been
/// rewritten under it and continue there, not to hand off to another session.
fn scaffold_next_step_guidance() -> String {
    "The app now has its shape and its source tree. Re-read this workspace's LINGXI.md before \
     doing anything else: it has been REPLACED by the formal contract for the surface you just \
     committed, and it names the editable entry points, the host-managed files you must not \
     touch, and the rules this surface must be written to. Anything written into the workspace \
     before this call is gone, as the guided contract said it would be. Implement the plan the \
     user approved, then build it with LocalAppBuild and start the preview once the build \
     succeeds. Do not create a second scaffold, do not run a package manager, and do not call \
     LocalAppScaffold again — the shape and the name are now fixed."
        .into()
}

/// Render the create-time MCP interview outcome for [`formal_workspace_contract`].
/// Empty string when the interview never ran — there is nothing to tell the
/// model about a question that was not asked, and the surrounding contract
/// reads correctly either way since this is spliced in as its own line.
fn mcp_intent_contract_line(intent: Option<&local_apps::AppMcpIntent>) -> String {
    match intent {
        None => String::new(),
        Some(local_apps::AppMcpIntent::Declined) => {
            "MCP intent: asked during creation; the user declined MCP for this app.\n\n".into()
        }
        Some(local_apps::AppMcpIntent::Requested { capabilities }) => format!(
            "MCP intent: asked during creation; the user asked for MCP access to {}.\n\n",
            capabilities.join(", ")
        ),
    }
}

/// Render the FORMAL workspace contract — the `workspace/LINGXI.md` a
/// formed app carries, and the twin of [`guided_workspace_contract`].
///
/// ⚠️ `record` is the identity the contract SPEAKS. On the `LocalAppScaffold`
/// path the caller must pass the PROPOSED record (the confirmed name and
/// brief), not the one creation wrote: the shell was created as `untitled`
/// with an empty brief, this file is written exactly ONCE (it is absent from
/// `restore_host_managed_files`, and a second scaffold is refused), and it is
/// the only channel that reaches the model on every turn. Render it from the
/// creation record and the whole interview is lost in the one artefact meant
/// to carry it.
fn formal_workspace_contract(
    record: &local_apps::AppRecord,
    binding: &local_apps::AppRuntimeProfileBinding,
) -> String {
    // Two scaffolds, two contracts. The shared clauses are repeated rather
    // than composed: this text is the agent's whole picture of the
    // workspace, and a reader that has to assemble it from fragments is how
    // "edit home-screen.jsx" survived into a workspace that has no such
    // file.
    let profile_identity = format!(
        "- This app is permanently bound to runtime profile `{}` revision `{}` with contract SHA-256 `{}`. This line is an informational mirror for the agent; the persisted manifest binding and host catalog are authoritative. Do not infer or replace the profile from imports or package files.\n",
        binding.family.as_str(),
        binding.revision,
        binding.contract_sha256,
    );
    let setup_path = match binding.family {
        local_apps::AppRuntimeProfile::ReactDom => format!(
            "{profile_identity}\
             - This app's surface is `dom`. The workspace is already prepared from the plan the user approved; implement that plan and build it with `LocalAppBuild`. The surface and runtime profile are fixed at creation; do not infer them from source.\n\
             - This workspace already contains the repository-verified Vite + Ionic foundation. The host prepares app-local dependencies in `workspace/node_modules`. Do not run `npm create vite`, do not create a second scaffold, do not add a wrapper build layer, and do not run a package manager in this local-app workspace.\n\
             - Host-managed files are `.gitignore`, `package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `jsconfig.json`, `index.html`, `vite.config.mjs`, `LINGXI.md`, the whole `.lingxi/` directory, `node_modules/`, `lib/lingxi-bridge.js`, `lib/device-context.js`, `lib/platform-adapter.js`, `lib/lingxi-provider.jsx`, and `styles/foundation.css`. Do not edit them.\n\
             - Default editable entry points are `app/screens/home-screen.jsx`, `app/screens/detail-screen.jsx`, and `app/globals.css`. You may edit files under `app/`, `src/`, `components/`, `styles/`, `public/`, and add non-host-managed helpers under `lib/`.\n\
             - The UI kit is Ionic. Import components from `@ionic/react`; never from `@ionic/core/components`, which cannot be bundled here. There is no Tailwind: use Ionic's CSS variables and its utility classes (`ion-padding`, `ion-margin`, `ion-text-center`, `ion-justify-content-*`, `ion-hide-*`), and put anything else in `app/globals.css`.\n\
             - Routing is `IonRouterOutlet` with react-router 6 `Routes`/`Route`. Every routed screen must render `IonPage` as its ROOT element, or the outlet has nothing to animate and the platform back gesture does not attach. Navigate with `routerLink`, not an onClick handler.\n\
             - The platform look is chosen for you: the checked-in provider calls `setupIonicReact` with the host's OS, so components already render iOS or Material chrome. Do not branch on the user agent and do not hard-code one platform's metrics.\n\
             - Use repo tools exposed in this workspace for source status, diff, and checkpoint versioning when available; checkpoints are workspace Git history. The host rebuilds directly from this workspace as the sole writable mount, keeps temporary output under `.lingxi-build-state/`, and promotes only the validated output.\n"
        ),
        local_apps::AppRuntimeProfile::Canvas2d
        | local_apps::AppRuntimeProfile::Three3d
        | local_apps::AppRuntimeProfile::Phaser2d
        | local_apps::AppRuntimeProfile::Babylon3d => {
            let helper = match binding.family {
                local_apps::AppRuntimeProfile::Canvas2d
                | local_apps::AppRuntimeProfile::Three3d => "lib/frame-loop.js",
                local_apps::AppRuntimeProfile::Phaser2d => "lib/phaser-runtime.js",
                local_apps::AppRuntimeProfile::Babylon3d => "lib/babylon-runtime.js",
                local_apps::AppRuntimeProfile::ReactDom => unreachable!(),
            };
            // The Phaser and Babylon templates ship `lib/frame-loop.js` NEXT
            // TO their engine adapter, and both their
            // `.lingxi/source-policy.json` and
            // `local_apps_build::HOST_MANAGED_FILES` list it as host-managed.
            // Naming only `{helper}` for those two profiles would leave the
            // contract silently narrower than the set actually enforced: the
            // lease accepts an edit to `lib/frame-loop.js`, the next build's
            // `restore_host_managed_files` reverts it, and the only trace is
            // a `tracing::warn!` while the model loops against a file the
            // contract never told it was managed.
            let managed_extra = match binding.family {
                local_apps::AppRuntimeProfile::Phaser2d
                | local_apps::AppRuntimeProfile::Babylon3d => "`lib/frame-loop.js`, ",
                _ => "",
            };
            let engine_rule = match binding.family {
                local_apps::AppRuntimeProfile::Canvas2d =>
                    "- This is a Canvas 2D profile: use the checked-in `lib/frame-loop.js` helper and the Canvas 2D APIs; do not add a game engine or physics library.",
                local_apps::AppRuntimeProfile::Three3d =>
                    "- This is a Three.js profile: import the locked `three` package directly and use the checked-in `lib/frame-loop.js` helper; do not add React Three Fiber, drei, or an external physics library.",
                local_apps::AppRuntimeProfile::Phaser2d =>
                    "- This is a Phaser profile: use the locked `phaser` package through the checked-in `lib/phaser-runtime.js` adapter; do not replace it with `createFrameLoop`, another engine, or an external physics library.",
                local_apps::AppRuntimeProfile::Babylon3d =>
                    "- This is a Babylon.js profile: use the locked Babylon packages through the checked-in `lib/babylon-runtime.js` adapter; do not replace it with `createFrameLoop`, React Three Fiber, or an external physics library.",
                local_apps::AppRuntimeProfile::ReactDom => unreachable!(),
            };
            format!(
                "{profile_identity}\
                 - This app's surface is `canvas`. The workspace is already prepared from the plan the user approved; implement that plan and build it with `LocalAppBuild`. It is one drawn surface plus overlays; do not infer a screen hierarchy or the surface from source.\n\
                 - This workspace already contains the repository-verified Vite + Ionic foundation, scaffolded for a single DRAWN SURFACE. The host prepares app-local dependencies in `workspace/node_modules`. Do not run `npm create vite`, do not create a second scaffold, do not add a wrapper build layer, and do not run a package manager in this local-app workspace.\n\
                 - Host-managed files are `.gitignore`, `package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `jsconfig.json`, `index.html`, `vite.config.mjs`, `LINGXI.md`, the whole `.lingxi/` directory, `node_modules/`, `lib/lingxi-bridge.js`, `lib/device-context.js`, `lib/platform-adapter.js`, `lib/lingxi-provider.jsx`, {managed_extra}`{helper}`, and `styles/foundation.css`. Do not edit them; `{helper}` is the profile's checked-in runtime adapter.\n\
                 - Default editable entry points are `app/screens/game-screen.jsx`, `src/stores/game-store.js`, and `app/globals.css`. You may edit files under `app/`, `src/`, `components/`, `styles/`, `public/`, and add non-host-managed helpers under `lib/`, but never edit the managed adapter `{helper}`.\n\
                 - There is NO router: menus, pause and game-over are Ionic components layered on top of the canvas, not separate pages.\n\
                 {engine_rule}\n\
                 - Keep per-frame simulation state in a ref, NOT in React or the store. The store is for the phase machine, score and settings; pushing positions through React re-renders turns the app into a slideshow.\n\
                 - Use repo tools exposed in this workspace for source status, diff, and checkpoint versioning when available; checkpoints are workspace Git history. The host rebuilds directly from this workspace as the sole writable mount, keeps temporary output under `.lingxi-build-state/`, and promotes only the validated output.\n"
            )
        }
    };
    // `format!`, not a bare `&str`: this string is interpolated into the
    // enclosing `format!` as a VALUE, so its own `{{` and `{id}` would be
    // copied through verbatim and the agent would read a malformed example
    // of the one call it is required to make.
    let build_preview = format!(
        "- `LocalAppBuild {{\"app_id\":\"{id}\"}}` — offline `vite build` \
         (30-minute budget). The host waits for the app-local dependency state, mounts \
         the workspace as the sole writable `LocalAppBuild` root, runs the workspace's own \
         `node_modules/vite`, writes into private build-state, and serves only the promoted \
         `build/store/dist/`.\n",
        id = record.id,
    );
    let mcp_intent_line = mcp_intent_contract_line(record.mcp_intent.as_ref());
    format!(
        "# Local App: {name} ({id})\n\n\
         Brief: {brief}\n\n\
         {mcp_intent_line}\
         ## Workspace contract\n\
         - This workspace is already bound to local app `{id}`. Treat `{id}` as authoritative; do not call `LocalAppList` or `LocalAppGet` to rediscover or confirm it, and do not call `LocalAppCreate` again.\n\
         - Edit ONLY app-owned files under `app/`, `src/`, `components/`, `lib/`, `styles/`, `public/`.\n\
         - The host draws NO chrome around a running app: the app must provide every visible title, navigation and back affordance. The host floats ONE control over the bottom-leading corner, so keep the leading 80 CSS px by the bottom 80 CSS px clear from the safe area and keep time-critical controls off its temporary expansion strip.\n\
         {setup_path}\
         - The page reaches host data/network/device ONLY through `window.lingxi.v2` \
         (see `lib/lingxi-bridge.js`).\n\
         - Declare data collections / network domains / capabilities through \
         `LocalAppManifest` BEFORE the page relies on them; runtime \
         authorization still prompts the user. Every collection is `{{id,name,fields}}`; every field is `{{id,label,kind,required?,enumOptions?}}`; IDs use lower snake_case. Never declare host-owned `recordId`, `revision`, `createdAtMs`, or `updatedAtMs` as fields. Repair and retry any rejected manifest before building.\n\
         - If a material requirement is unresolved, call `AskUserQuestion` so the native client presents its sheet; never leave an unresolved question in ordinary assistant text. Everything else is settled by the approved plan.\n\n\
         ## Build & preview\n\
         {build_preview}\
         - `LocalAppRuntime {{\"app_id\":\"{id}\",\"action\":\"start\"}}` \
         — serve the built output and return the preview url.\n\
         - `LocalAppLogs {{\"app_id\":\"{id}\",\"log\":\"build\"}}` — build log.\n\
         - `LocalAppInstallDeps {{\"app_id\":\"{id}\",\"wait\":true}}` \
         — dependency state; `lastError` names why an install failed.\n\n\
         ### When a build fails\n\
         `LocalAppBuild` is the ONLY build path in this workspace, so \
         do NOT try a different build command, package manager, or scaffold tool — \
         there is nothing else to fall back to and improvising cannot succeed. Instead:\n\
         1. Read the failure: `LocalAppLogs {{\"app_id\":\"{id}\",\"log\":\"build\"}}`.\n\
         2. A `not yet available` build means dependencies are not ready. Call \
         `LocalAppInstallDeps {{\"app_id\":\"{id}\",\"wait\":true}}` and read \
         its `lastError`.\n\
         3. If the cause is your source, fix it and build again.\n\
         4. If the cause is the HOST — a missing toolchain, a failed dependency install, \
         an unavailable runtime — report it to the user and stop. Those cannot be worked \
         around from inside this workspace, and retrying will not clear them.\n\n\
         ## Deliver\n\
         A successful `LocalAppBuild` is the completion condition. Start the preview with \
         `LocalAppRuntime {{\"app_id\":\"{id}\",\"action\":\"start\"}}`, then hand the user the \
         app entry point and a SHORT trial checklist drawn from the approved plan's acceptance \
         checks. Do NOT run UI operations, capture acceptance screenshots, or score the app \
         yourself — the user tries it. If the build succeeded but the preview failed to launch, \
         say so separately and plainly: the build IS done and the launch is retryable; never \
         report a preview-launch failure as a build or verification failure.\n\n\
         ## On-demand testing\n\
         Testing is NOT automatic. When the USER asks to test the app, use the independent \
         testing capability: the `$local-app-test` / `$frontend-qa` skill drives the running \
         preview through the app's on-device use-test path. \
         `LocalAppInspectUi` / `LocalAppActOnUi` read and drive it; \
         `LocalAppCaptureUi {{\"app_id\":\"{id}\"}}` gives a still image when the DOM cannot \
         describe what the app is showing — a canvas or WebGL surface has no inspectable \
         elements, so `LocalAppInspectUi` returns an empty list whether the app is drawing \
         correctly, drawing nothing, or has crashed; \
         `LocalAppQueryData {{\"app_id\":\"{id}\",\"collection\":\"<collection_id>\"}}` confirms \
         a UI write reached native storage under `records[].document`, since a value that exists \
         only in page state is NOT persistence; \
         `LocalAppLogs {{\"app_id\":\"{id}\",\"log\":\"runtime\"}}` reads the runtime log. \
         Report what a test found, but never make delivery depend on it.\n\
         - After the user confirms a working state, record it with \
         `LocalAppCheckpointCreate`.\n",
        name = record.name,
        id = record.id,
        brief = record.brief,
        mcp_intent_line = mcp_intent_line,
        setup_path = setup_path,
        build_preview = build_preview,
    )
}

/// Thin bootstrap contract for an unformed `CreateMode::Shell` workspace.
///
/// The Host supplies only immutable app identity, the recorded brief, and the
/// no-write boundary. The create skill is the single coordinator: it plans the
/// app with the user, has the Host prepare the workspace from the approved plan,
/// implements it, and delivers it once the build succeeds.
fn guided_workspace_contract(record: &local_apps::AppRecord) -> String {
    format!(
        "# Local App (new, not yet shaped)\n\n\
         This app was just created and **has no shape yet** — its workspace is empty.\n\n\
         This workspace is already bound to local app `{id}`. Treat `{id}` as authoritative: \
         do not call `LocalAppList` or `LocalAppGet` to rediscover or reconfirm it, and do not \
         call `LocalAppCreate` again.\n\n\
         Recorded brief: \"{brief}\". Carry it forward exactly; it may already contain the \
         user's product intent.\n\n\
         **Do not write source, build, install dependencies, or operate the runtime in this \
         shell.** There is nowhere for source to go, and every build, dependency, runtime, log, \
         manifest, UI, data and background tool refuses an app with no shape — that refusal is \
         the contract, not a transient failure.\n\n\
         Immediately use the `Skill` tool to start \
         `lingxi-local-app:create-local-app` (the plugin-qualified name is required). That skill \
         owns the whole flow: it plans the app with the user, and the Host prepares this \
         workspace from the plan the user approves. Do not run a separate questionnaire here, \
         and do not call `LocalAppScaffold` directly.\n\n\
         MCP exposure is not a prerequisite for creating the app. After creation, the user may \
         optionally expose named business capabilities through app settings. Integrations the app \
         needs for its own product behavior are separate from MCP exposure.\n\n\
         After the skill completes, re-read this file and continue under the formal contract.\n",
        id = record.id,
        brief = record.brief,
    )
}

/// r4-failure-paths-09: `stage_create` writes every individual file
/// atomically (temp + rename) but is not atomic as a TRANSACTION. Between
/// `create_dir_all(<staging>)` and the final `evidence.json` rename there are
/// roughly eight fallible steps, each of which returns with `?`; before this
/// guard existed, a failure at any of them left a half-materialized staging
/// tree under
/// `.lingxi-build-state/template-candidates/<app>/<run>/staging/<handle>` that
/// nothing ever reclaimed, and a later `load_create_proposal_context` could
/// read a `design-spec.json` from it.
///
/// `evidence.json` is the commit marker: `stage_create` renames it into place
/// last, so a staging directory that HAS it is a complete candidate and a
/// staging directory that lacks it is partial by definition. Keying the
/// reclaim on that marker rather than on "this call created the directory"
/// means a retry that fails early can never destroy a previously COMPLETED
/// candidate, while a partial tree — this call's or an earlier call's — is
/// always swept.
struct PartialStagingReaper<'a> {
    staging: &'a std::path::Path,
    committed: bool,
}

impl Drop for PartialStagingReaper<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.staging.join("evidence.json").exists() {
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(self.staging) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    staging = %self.staging.display(),
                    %error,
                    "could not reclaim a partial create staging tree"
                );
            }
        }
    }
}

impl LocalAppsHostBroker {
    async fn execute_bound_mcp_flow(
        &self,
        app_id: &str,
        tool_input: Value,
        definition: &platform_api::McpToolDefinitionDto,
        binding: &local_apps::AppMcpFlowBinding,
        context: &local_apps::AppMcpFlowContext,
        catalog_ceiling: platform_api::McpPermissionCeiling,
        mode: BoundMcpFlowMode,
    ) -> Result<BoundMcpExecution, String> {
        let input_bytes = serde_json::to_vec(&tool_input)
            .map_err(|_| "invalid_argument: tool input is not serializable".to_string())?
            .len();
        if input_bytes > local_apps::mcp_authoring::MAX_MCP_CALL_BYTES {
            return Err("call_payload_limit: MCP call payload exceeds 256 KiB".into());
        }
        if !local_apps::value_matches_schema(&tool_input, &definition.input_schema) {
            return Err("invalid_argument: tool input does not satisfy its schema".into());
        }
        if context.app_id != app_id || context.source != local_apps::FlowSource::Active {
            return Err("cross_app_flow: active typed Flow is not owned by this App".into());
        }
        if context.input_schema != definition.input_schema
            || definition
                .output_schema
                .as_ref()
                .is_some_and(|schema| *schema != context.output_schema)
        {
            return Err("binding_invalid: Flow schemas do not match the active tool".into());
        }
        let capability_registry = local_apps::CapabilityRegistry::default();
        local_apps::validate_app_mcp_flow_binding(binding, app_id, context, &capability_registry)
            .map_err(|issues| format_binding_issue(&issues))?;
        if context.flow.steps.len() > local_apps::mcp_authoring::MAX_MCP_FLOW_STEPS {
            return Err("flow_step_limit: MCP-bound Flow exceeds 32 steps".into());
        }
        let derived_ceiling =
            local_apps::derive_local_app_mcp_ceiling(&capability_registry, &context.flow);
        if derived_ceiling != catalog_ceiling {
            return Err(
                "permission_ceiling_drift: active Flow ceiling does not match the approved catalog"
                    .into(),
            );
        }
        if matches!(derived_ceiling, platform_api::McpPermissionCeiling::Deny)
            || matches!(catalog_ceiling, platform_api::McpPermissionCeiling::Deny)
        {
            return Err("permission_ceiling: Host denied this MCP Flow".into());
        }

        let mut binding_slots = HashSet::new();
        for step in &context.flow.steps {
            let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&step.input_json) else {
                continue;
            };
            binding_slots.extend(
                binding
                    .inputs
                    .keys()
                    .filter(|name| object.contains_key(*name))
                    .cloned(),
            );
        }
        if binding_slots.len() != binding.inputs.len() {
            return Err("binding_input_missing: active Flow has no slot for a typed input".into());
        }

        let result = timeout(MCP_FLOW_EXECUTION_TIMEOUT, async {
            let mut outputs = BTreeMap::new();
            let mut step_calls = Vec::new();
            let mut materialized_inputs = HashSet::new();
            let mut call_bytes = input_bytes;
            for (index, step) in context.flow.steps.iter().enumerate() {
                let mut step_input: Value =
                    serde_json::from_str(&step.input_json).map_err(|_| {
                        format!("flow_step_invalid: step {} input is invalid", step.step_id)
                    })?;
                if let Some(object) = step_input.as_object_mut() {
                    for (name, value_binding) in &binding.inputs {
                        if !object.contains_key(name) || !materialized_inputs.insert(name.clone()) {
                            continue;
                        }
                        if let local_apps::FlowValueBinding::StepOutput { step_id, .. } =
                            value_binding
                        {
                            let source_index = context
                                .flow
                                .steps
                                .iter()
                                .position(|candidate| candidate.step_id == *step_id)
                                .ok_or_else(|| {
                                    "step_not_found: Flow binding references an unknown step"
                                        .to_string()
                                })?;
                            if source_index >= index {
                                return Err(
                                    "forward_step_output: StepOutput must reference a prior step"
                                        .into(),
                                );
                            }
                        }
                        let value = local_apps::materialize_flow_value_binding(
                            value_binding,
                            &tool_input,
                            &outputs,
                        )
                        .map_err(|issue| format_binding_issue(std::slice::from_ref(&issue)))?;
                        object.insert(name.clone(), value);
                    }
                } else if !binding.inputs.is_empty() {
                    return Err(format!(
                        "flow_step_invalid: step {} input must be an object",
                        step.step_id
                    ));
                }
                let bytes = serde_json::to_vec(&step_input)
                    .map_err(|_| "step_payload_limit: step input is not serializable".to_string())?
                    .len();
                if bytes > local_apps::mcp_authoring::MAX_MCP_STEP_RESULT_BYTES {
                    return Err("step_payload_limit: step input exceeds 64 KiB".into());
                }
                call_bytes = call_bytes.saturating_add(bytes);
                if call_bytes > local_apps::mcp_authoring::MAX_MCP_CALL_BYTES {
                    return Err("call_payload_limit: Flow payload exceeds 256 KiB".into());
                }
                if !local_apps::allowed_for_synchronous_flow(step.capability) {
                    return Err(format!(
                        "forbidden_capability: {} is not allowed for MCP Flow",
                        step.capability.as_str()
                    ));
                }
                let step_schema =
                    context
                        .step_output_schemas
                        .get(&step.step_id)
                        .ok_or_else(|| {
                            format!(
                                "step_schema_missing: output schema for step {} is unavailable",
                                step.step_id
                            )
                        })?;
                let value = match mode {
                    BoundMcpFlowMode::Live => timeout(
                        FLOW_STEP_TIMEOUT,
                        self.execute_flow_step(
                            app_id,
                            &context.flow.flow_id,
                            &step.step_id,
                            step.capability,
                            step_input.clone(),
                        ),
                    )
                    .await
                    .map_err(|_| format!("timeout: flow step {} timed out", step.step_id))??,
                    BoundMcpFlowMode::Qa => {
                        minimal_schema_witness(step_schema).map_err(|error| {
                            format!(
                                "qa_witness_unavailable: step {} output witness failed: {error}",
                                step.step_id
                            )
                        })?
                    }
                };
                local_apps::validate_generated_structured_result(&value)
                    .map_err(|issue| format!("output_schema_mismatch: {}", issue.message))?;
                if !local_apps::value_matches_schema(&value, step_schema) {
                    return Err(format!(
                        "output_schema_mismatch: step {} result does not satisfy its schema",
                        step.step_id
                    ));
                }
                let output_bytes = serde_json::to_vec(&value)
                    .map_err(|_| {
                        "output_schema_mismatch: step result is not serializable".to_string()
                    })?
                    .len();
                if output_bytes > local_apps::mcp_authoring::MAX_MCP_STEP_RESULT_BYTES {
                    return Err("step_payload_limit: step result exceeds 64 KiB".into());
                }
                call_bytes = call_bytes.saturating_add(output_bytes);
                if call_bytes > local_apps::mcp_authoring::MAX_MCP_CALL_BYTES {
                    return Err("call_payload_limit: Flow result values exceed 256 KiB".into());
                }
                step_calls.push(BoundMcpStepEvidence {
                    step_id: step.step_id.clone(),
                    capability: step.capability.as_str().to_string(),
                    input_sha256: value_sha256(&step_input)?,
                    output_sha256: value_sha256(&value)?,
                });
                outputs.insert(step.step_id.clone(), value);
            }
            let result =
                local_apps::materialize_flow_value_binding(&binding.result, &tool_input, &outputs)
                    .map_err(|issue| format_binding_issue(std::slice::from_ref(&issue)))?;
            if !local_apps::value_matches_schema(&result, &context.output_schema) {
                return Err(
                    "output_schema_mismatch: result does not satisfy the tool output schema".into(),
                );
            }
            local_apps::validate_generated_structured_result(&result)
                .map_err(|issue| format!("output_schema_mismatch: {}", issue.message))?;
            let result_bytes = serde_json::to_vec(&result)
                .map_err(|_| "output_schema_mismatch: result is not serializable".to_string())?
                .len();
            if result_bytes > local_apps::mcp_authoring::MAX_MCP_STEP_RESULT_BYTES {
                return Err("step_payload_limit: final result exceeds 64 KiB".into());
            }
            if call_bytes.saturating_add(result_bytes)
                > local_apps::mcp_authoring::MAX_MCP_CALL_BYTES
            {
                return Err("call_payload_limit: Flow result exceeds 256 KiB".into());
            }
            Ok::<BoundMcpExecution, String>(BoundMcpExecution { result, step_calls })
        })
        .await
        .map_err(|_| "timeout: Local App MCP Flow exceeded 5 minutes".to_string())??;
        Ok(result)
    }

    /// Execute one generated Local App MCP tool through a Host-owned typed
    /// Flow. The request envelope is intentionally treated as untrusted even
    /// though the transport has already bound its app scope: the Host
    /// re-reads the active manifest/catalog/build and the final binding before
    /// any capability handler runs.
    async fn execute_mcp_flow_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        local_apps::ids::validate_app_id(&app_id)
            .map_err(|_| "invalid Local App identity".to_string())?;
        let tool_name = required_string(&input, "tool_name")?.to_string();
        let requested_catalog = required_string(&input, "catalog_sha256")?.to_string();
        let tool_input = input
            .get("input")
            .cloned()
            .ok_or_else(|| "invalid_argument: tool input is required".to_string())?;
        let input_bytes = serde_json::to_vec(&tool_input)
            .map_err(|_| "invalid_argument: tool input is not serializable".to_string())?
            .len();
        if input_bytes > local_apps::mcp_authoring::MAX_MCP_CALL_BYTES {
            return Err("call_payload_limit: MCP call payload exceeds 256 KiB".into());
        }

        let service = self.service()?;
        service
            .record(&app_id)
            .await
            .map_err(|_| "local app is unavailable".to_string())?;
        let layout = self.layout(&app_id)?;
        let manifest =
            load_manifest(&layout).map_err(|_| "local app manifest unavailable".to_string())?;
        let active = manifest
            .active_mcp_catalog
            .as_ref()
            .ok_or_else(|| "catalog_not_found: Local App has no active MCP catalog".to_string())?;
        if active.catalog_sha256 != requested_catalog {
            return Err("catalog_stale: active Local App catalog changed".into());
        }
        let active_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|_| "active build unavailable".to_string())?
            .ok_or_else(|| "active build unavailable".to_string())?;
        if active.build_id != active_build_id {
            return Err("catalog_invalid: active build does not match MCP catalog".into());
        }
        let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
            .map_err(|_| "active Local App catalog unavailable".to_string())?;
        if catalog.get("appId").and_then(Value::as_str) != Some(app_id.as_str())
            || catalog.get("buildId").and_then(Value::as_str) != Some(active.build_id.as_str())
        {
            return Err("catalog_invalid: active catalog identity mismatch".into());
        }
        let entry = catalog
            .get("tools")
            .and_then(Value::as_array)
            .and_then(|tools| {
                tools.iter().find(|entry| {
                    entry
                        .get("definition")
                        .unwrap_or(entry)
                        .get("name")
                        .and_then(Value::as_str)
                        == Some(tool_name.as_str())
                })
            })
            .ok_or_else(|| {
                "unknown_tool: Local App tool is not in the active catalog".to_string()
            })?;
        let definition_value = entry.get("definition").unwrap_or(entry);
        let definition: platform_api::McpToolDefinitionDto =
            serde_json::from_value(definition_value.clone())
                .map_err(|_| "catalog_invalid: active tool definition is invalid".to_string())?;
        let binding_value = entry.get("flow").cloned().ok_or_else(|| {
            "binding_not_found: active tool has no typed Flow binding".to_string()
        })?;
        let binding: local_apps::AppMcpFlowBinding = serde_json::from_value(binding_value)
            .map_err(|_| "binding_invalid: active typed Flow binding is invalid".to_string())?;
        let execution_entry = catalog
            .get("execution")
            .and_then(Value::as_array)
            .and_then(|bindings| {
                bindings.iter().find(|binding| {
                    binding
                        .get("definition")
                        .and_then(|definition| definition.get("name"))
                        .and_then(Value::as_str)
                        == Some(tool_name.as_str())
                })
            })
            .ok_or_else(|| {
                "catalog_invalid: active tool execution binding is missing".to_string()
            })?;
        if execution_entry.get("definition") != entry.get("definition")
            || execution_entry.get("flow") != entry.get("flow")
            || execution_entry.get("ceiling") != entry.get("ceiling")
        {
            return Err("catalog_invalid: active tool execution binding diverged".into());
        }
        let contexts = self.load_active_mcp_flow_contexts(&layout)?;
        let context = contexts
            .get(&binding.flow_id)
            .ok_or_else(|| "flow_not_found: active typed Flow is unavailable".to_string())?;
        let expected_context_sha256 = execution_entry
            .get("contextSha256")
            .and_then(Value::as_str)
            .ok_or_else(|| "catalog_invalid: active Flow context digest is missing".to_string())?;
        let actual_context_sha256 = value_sha256(
            &serde_json::to_value(context)
                .map_err(|error| format!("serialize active MCP Flow context: {error}"))?,
        )?;
        if actual_context_sha256 != expected_context_sha256 {
            return Err("catalog_stale: active MCP Flow context changed after QA".into());
        }
        let catalog_ceiling = entry
            .get("ceiling")
            .and_then(Value::as_str)
            .and_then(platform_api::McpPermissionCeiling::from_policy_str)
            .ok_or_else(|| "permission_ceiling: active tool ceiling is invalid".to_string())?;
        Ok(self
            .execute_bound_mcp_flow(
                &app_id,
                tool_input,
                &definition,
                &binding,
                context,
                catalog_ceiling,
                BoundMcpFlowMode::Live,
            )
            .await?
            .result)
    }

    async fn flow_execute_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.service()?
            .record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        // Flow execution is an app-owned Agent MCP capability. The LLM grant
        // is the host's durable/user-approved entry gate; every individual
        // step still goes through its own capability router below.
        self.authorize_agent_session_capability(&app_id).await?;
        let flow_value = input
            .get("flow")
            .cloned()
            .ok_or_else(|| "flow is required".to_string())?;
        let flow: local_apps::FlowDefinition =
            serde_json::from_value(flow_value).map_err(|error| format!("invalid flow: {error}"))?;
        let registry = local_apps::CapabilityRegistry::default();
        flow.validate(&registry)
            .map_err(|error| format!("invalid flow: {error}"))?;
        let flow_id = flow.flow_id.clone();
        let version = flow.version;
        let outputs = timeout(FLOW_EXECUTION_TIMEOUT, async {
            let mut outputs = Map::new();
            for step in flow.steps {
                let capability = step.capability;
                if !local_apps::allowed_for_synchronous_flow(capability) {
                    return Err(format!(
                        "flow capability {} is not valid for a synchronous flow",
                        capability.as_str()
                    ));
                }
                let step_input: Value =
                    serde_json::from_str(&step.input_json).map_err(|error| {
                        format!("flow step {} has invalid input: {error}", step.step_id)
                    })?;
                let value = timeout(
                    FLOW_STEP_TIMEOUT,
                    self.execute_flow_step(
                        &app_id,
                        &flow_id,
                        &step.step_id,
                        capability,
                        step_input,
                    ),
                )
                .await
                .map_err(|_| format!("flow step {} timed out", step.step_id))??;
                outputs.insert(step.step_id, value);
            }
            Ok::<Map<String, Value>, String>(outputs)
        })
        .await
        .map_err(|_| "flow execution exceeded its wall-clock budget".to_string())??;
        Ok(json!({
            "flowId": flow_id,
            "version": version,
            "outputs": outputs,
        }))
    }

    async fn execute_flow_step(
        &self,
        app_id: &str,
        flow_id: &str,
        step_id: &str,
        capability: local_apps::CapabilityId,
        mut input: Value,
    ) -> Result<Value, String> {
        let object = input
            .as_object_mut()
            .ok_or_else(|| format!("flow step {step_id} input must be a JSON object"))?;
        object.insert("app_id".into(), Value::String(app_id.into()));
        let request_id = format!("flow:{flow_id}:{step_id}");
        match capability {
            local_apps::CapabilityId::DataQuery => self.query_data_value(input).await,
            local_apps::CapabilityId::DataMutate => self.mutate_data_value(input, true, None).await,
            local_apps::CapabilityId::NetworkRequest => self.network_request(app_id, input).await,
            local_apps::CapabilityId::RuntimeStatus => Ok(json!({
                "app_id": app_id,
                "runtime": self.service()?.runtime_record(app_id).await.map_err(|error| error.to_string())?,
            })),
            local_apps::CapabilityId::FilesRead => self
                .file_read_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::FilesWrite => self
                .file_write_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::Clipboard => {
                if input.get("text").is_some() {
                    self.clipboard_set_text_value(app_id, &input)
                        .await
                        .map_err(|error| error.message)
                } else {
                    self.clipboard_get_text_value(app_id)
                        .await
                        .map_err(|error| error.message)
                }
            }
            local_apps::CapabilityId::Calendar => self
                .calendar_list_events_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::Contacts => self
                .contacts_search_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::Media => self
                .media_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::DeviceStatus => self
                .device_status_value(app_id)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::Haptics => self
                .haptics_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::DeepLink => self
                .deep_link_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::TextToSpeech => {
                let (invocation, runtime_generation) = self
                    .flow_audio_invocation(
                        app_id,
                        flow_id,
                        &request_id,
                        local_apps::CapabilityId::TextToSpeech,
                    )
                    .await
                    .map_err(|error| error.message)?;
                self.synthesize_speech_value(&invocation, runtime_generation, &input)
                    .await
                    .map_err(|error| error.message)
            }
            local_apps::CapabilityId::Location => self
                .get_location_value(app_id)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::Notifications => self
                .post_notification_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::LlmComplete => self
                .llm_chat_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::AgentSessionCreate => {
                self.agent_session_create_value(input).await
            }
            local_apps::CapabilityId::AgentSessionList => {
                self.agent_session_list_value(input).await
            }
            local_apps::CapabilityId::AgentSessionResume
            | local_apps::CapabilityId::AgentSessionClose => {
                self.agent_session_update_value(input).await
            }
            local_apps::CapabilityId::AgentSend => self
                .agent_send_value(app_id, &request_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::AgentEmit => self
                .agent_post_value(app_id, &input)
                .await
                .map_err(|error| error.message),
            local_apps::CapabilityId::AgentProfilePropose => {
                self.agent_profile_propose_value(input).await
            }
            local_apps::CapabilityId::AgentStream
            | local_apps::CapabilityId::AgentCancel
            | local_apps::CapabilityId::LlmStream
            | local_apps::CapabilityId::FlowExecute
            | local_apps::CapabilityId::BackgroundSchedule
            | local_apps::CapabilityId::Camera
            | local_apps::CapabilityId::PhotoLibrary
            | local_apps::CapabilityId::Microphone
            | local_apps::CapabilityId::SpeechToText
            | local_apps::CapabilityId::Share => {
                unreachable!("synchronous flow capabilities are filtered before step execution")
            }
            _ => Err(format!(
                "flow capability {} is not supported by this host",
                capability.as_str()
            )),
        }
    }

    fn background_management_layout(&self, app_id: &str) -> Result<AppLayout, String> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest
            .capabilities
            .contains(&AppCapability::BackgroundSchedule)
        {
            return Err("background task management is not declared in the app manifest".into());
        }
        let permissions = load_permissions(&layout).map_err(|error| error.to_string())?;
        if !permissions.allows(AppCapability::BackgroundSchedule) {
            return Err("background task management requires durable approval".into());
        }
        Ok(layout)
    }
}

/// Build the opaque `value` payload for a capture request.
///
/// The rect rides `AppUiRequestDto.value` — an `Option<String>` the wire
/// already carries — so a region crop costs no DTO change. Shape and
/// finiteness are checked here; CLAMPING to the viewport happens on the
/// client, which is the only side that knows the real viewport.
///
/// That split is why a NEGATIVE origin is accepted and forwarded verbatim.
/// `getBoundingClientRect().top` is negative for anything scrolled above the
/// fold, so `inspect_ui`'s `elements[].rect` routinely reports one — and
/// "inspect, take an element's rect, capture it" is the most natural flow the
/// two tools have. Refusing it here would also make the clients' own clamping
/// (`intersection` on iOS, `coerceIn` in `cropSourceRect` on Android)
/// unreachable code.
fn capture_ui_value(input: &Value) -> Result<Option<String>, String> {
    // An explicit `"rect": null` is a model's way of saying "not applicable",
    // i.e. capture the whole view. `.get` answers `Some(Value::Null)` for it,
    // so without this filter the absent-rect guard never fires and every field
    // lookup below fails — a hard tool error for a routine, well-meant input.
    let Some(rect) = input.get("rect").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    // Keep the original `Value` alongside its `f64` reading: shape/range
    // checks need the number, but re-emitting the parsed f64 would turn an
    // integral input like `10` into `10.0` in the outgoing JSON text, which
    // is a needless textual change the client never asked for.
    let field = |name: &str| -> Result<(&Value, f64), String> {
        rect.get(name)
            .and_then(|value| value.as_f64().filter(|n| n.is_finite()).map(|n| (value, n)))
            .ok_or_else(|| format!("capture_ui rect.{name} must be a finite number"))
    };
    // The origin's numeric reading is deliberately discarded: `field` already
    // proved it finite, and its SIGN is not this layer's business (see above).
    let ((x, _), (y, _), (w, wn), (h, hn)) =
        (field("x")?, field("y")?, field("width")?, field("height")?);
    if wn <= 0.0 || hn <= 0.0 {
        return Err("capture_ui rect must have positive width and height".into());
    }
    Ok(Some(
        json!({ "rect": { "x": x, "y": y, "width": w, "height": h } }).to_string(),
    ))
}

fn validate_create_stage_quality(
    quality_level: &str,
    family: local_apps::AppRuntimeProfile,
) -> Result<(), String> {
    if !matches!(quality_level, "fast" | "balanced" | "thorough") {
        return Err(
            "create_staging_invalid: quality_level must be fast, balanced, or thorough".into(),
        );
    }
    if quality_level == "fast" && family != local_apps::AppRuntimeProfile::ReactDom {
        return Err(
            "create_staging_invalid: canvas profiles require balanced or thorough quality".into(),
        );
    }
    Ok(())
}

/// Inherent half of the approval path.
///
/// Kept out of the trait impl below so the authority parameter cannot be
/// reached through the MCP surface at all: the trait only ever spells
/// `NativeSheet`, and `ApprovedPlan` is passed by Host code that already holds
/// a user decision.
impl LocalAppsHostBroker {
    /// Approve a proposal whose authority the CALLER names.
    ///
    /// The tool path always reaches this through [`Self::approve_mcp_proposal`]
    /// (`NativeSheet`); only Host code that already holds a user decision —
    /// today, an approved plan — passes anything else, so the authority can
    /// never be selected by model input. A create needs that plan authority, so
    /// the model-reachable path is refused by name rather than answered on the
    /// user's behalf; every authority that does create lands through this one
    /// sealed candidate, receipt and scaffold transaction.
    async fn approve_mcp_proposal_with(
        &self,
        input: Value,
        authority: CreateApprovalAuthority,
    ) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let workflow_run_id = required_string(&input, "workflow_run_id")?.to_string();
        Self::validate_workflow_run_id(&workflow_run_id)?;
        if input
            .get("create_without_mcp")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let service = self.service()?;
            let record = service
                .record(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            if record.scaffolded {
                return Err(
                    "invalid_argument: create_without_mcp is only valid before scaffolding".into(),
                );
            }
            let layout = self.layout(&app_id)?;
            // Already-approved reuse arm. Without it, a repeated
            // `create_without_mcp` call for a `workflow_run_id` whose
            // candidate journal is already sealed at `Approved` rewrites that
            // journal back to `Prepared`, asks for the create confirmation a
            // second time, and then — if the FIRST receipt is still claimed by
            // an in-flight scaffold — throws the user's fresh answer away when
            // `issue()` returns `receipt_in_use`.
            //
            // The user has already answered the sheet for exactly this
            // contract digest, so reuse asks nothing again and never touches
            // the sealed journal. It does NOT mirror the MCP-revise branch's
            // `receipt_id: null` / `approved_reusable` answer: that shape is
            // only safe there because its consumer (the MCP-authoring
            // workflow script) declares `receipt_id` nullable and is told to
            // consume the existing receipt. THIS branch's only consumer,
            // `LocalAppPrepare`, requires a non-empty `receipt_id` whenever
            // `approved` is true (`prepare_state_invalid: the sealed create
            // approval has no receipt`), so a null here would swap one dead end
            // for another. Mint a usable receipt against the sealed journal
            // instead.
            //
            // `issue()` still refuses while a live, unexpired claim is
            // outstanding. Post-WP-B that is a scaffold genuinely in flight,
            // and starting a second one is exactly what must not happen, so
            // this surfaces a named error WITHOUT raising a sheet — no user
            // answer can be discarded because none was solicited.
            if let Ok(existing_journal) = local_apps::load_candidate_journal(&layout) {
                if existing_journal.workflow_run_id == workflow_run_id
                    && existing_journal.stage >= local_apps::McpAuthoringStage::Approved
                {
                    let receipt = local_apps::McpConfirmationReceipt::new(
                        &app_id,
                        &workflow_run_id,
                        existing_journal.approval_contract_sha256.clone(),
                        existing_journal.proposal_sha256.clone(),
                        now_ms(),
                    );
                    let receipt_id = receipt.receipt_id.clone();
                    self.pending_mcp_receipts
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .issue(receipt)
                        .map_err(|issue| {
                            format!(
                                "create_approval_in_flight: the sealed create approval for this \
                                 app is already claimed by an in-flight scaffold; retry after it \
                                 finishes ({})",
                                issue.message
                            )
                        })?;
                    return Ok(json!({
                        "approved": true,
                        "receipt_id": receipt_id,
                        "status": "create_approved_no_mcp",
                    }));
                }
            }
            let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
            let create_context = self.load_create_proposal_context(&app_id, &workflow_run_id)?;
            let proposal = local_apps::AppMcpProposal {
                app_id: app_id.clone(),
                manifest_revision: manifest.revision,
                user_goal_sha256: format!("{:x}", Sha256::digest(b"local-app-create-without-mcp")),
                summary: "Create this Local App without publishing or enabling an MCP surface."
                    .into(),
                tools: Vec::new(),
                required_flow_changes: Vec::new(),
                excluded_capabilities: Vec::new(),
            };
            let validated = local_apps::validate_app_mcp_proposal(
                proposal,
                &app_id,
                manifest.revision,
                &create_context.contexts,
                &local_apps::CapabilityRegistry::default(),
            )
            .map_err(|issues| {
                format!(
                    "create_approval_invalid: {}",
                    issues
                        .into_iter()
                        .map(|issue| format!("{}: {}", issue.code, issue.message))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })?;
            let review_surface =
                Self::build_mcp_review_surface(&manifest, &validated, None, Some(&create_context));
            let approval_contract_sha256 =
                local_apps::approval_contract_sha256(review_surface.clone())
                    .map_err(|issue| issue.message)?;
            let mut journal = local_apps::McpCandidateJournal {
                schema_version: local_apps::APPS_SCHEMA_VERSION,
                app_id: app_id.clone(),
                workflow_run_id: workflow_run_id.clone(),
                stage: local_apps::McpAuthoringStage::Prepared,
                previous_build_id: None,
                previous_catalog_sha256: None,
                proposal_sha256: validated.proposal_sha256.clone(),
                approval_contract_sha256: approval_contract_sha256.clone(),
                tool_surface_sha256: validated.tool_surface_sha256.clone(),
                catalog_sha256: None,
                integrity_sha256: String::new(),
            }
            .seal()
            .map_err(|issue| issue.message)?;
            let candidate = PersistedMcpCandidate {
                validated,
                approval_contract_sha256: approval_contract_sha256.clone(),
                review_surface,
                verification_sha256: None,
                catalog_sha256: None,
                qa_context_sha256: None,
            };
            self.save_mcp_candidate(&app_id, &workflow_run_id, &candidate)?;
            local_apps::save_candidate_journal(&layout, &journal)
                .map_err(|error| error.to_string())?;
            // r4-failure-paths-06: this guard owns the candidate written just
            // above, so every return before `keep()` below — the
            // `create_requires_approved_plan` refusal included — tears the
            // candidate journal/file down instead of leaking a durable
            // `Prepared` candidate for the life of the process.
            let candidate_guard = McpCreateCandidateGuard {
                broker: self,
                layout: layout.clone(),
                app_id: app_id.clone(),
                workflow_run_id: workflow_run_id.clone(),
                keep: false,
            };
            // The user's approval of the PLAN this create was derived from IS
            // the create confirmation. Nothing else can stand in for it: a
            // model-driven `approve_mcp_proposal` call carries only
            // `NativeSheet`, and there is no sheet left to raise, so it is
            // refused by name rather than answered on the user's behalf.
            if authority != CreateApprovalAuthority::ApprovedPlan {
                return Err(
                    "create_requires_approved_plan: creating a Local App lands the template \
                     the user approved in a plan; plan the app, let the user approve it, then \
                     call LocalAppPrepare instead of answering the create confirmation here"
                        .into(),
                );
            }
            candidate_guard.keep();
            // r3-engine-core-2: `DeleteApp` may have landed while the record
            // lookup above was awaited. `save_candidate_journal`
            // below unconditionally calls `layout.initialize()`, which would
            // RESURRECT this app's on-disk skeleton (workspace dir, state
            // dir, …) if it no longer exists. Re-resolve the record here —
            // still holding the (kept) candidate guard, so nothing is torn
            // down twice — and fail closed instead of writing a journal back
            // into a directory tree that a delete already committed.
            self.service()?
                .record(&app_id)
                .await
                .map_err(|error| format!("app_deleted_during_approval: {error}"))?;
            let receipt = local_apps::McpConfirmationReceipt::new(
                &app_id,
                &workflow_run_id,
                approval_contract_sha256,
                journal.proposal_sha256.clone(),
                now_ms(),
            );
            let receipt_id = receipt.receipt_id.clone();
            self.pending_mcp_receipts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .issue(receipt)
                .map_err(|issue| issue.message)?;
            journal = journal
                .advance(local_apps::McpAuthoringStage::Approved)
                .map_err(|issue| issue.message)?;
            local_apps::save_candidate_journal(&layout, &journal)
                .map_err(|error| error.to_string())?;
            return Ok(json!({
                "approved": true,
                "receipt_id": receipt_id,
                "status": "create_approved_no_mcp",
            }));
        }
        let approval_contract_sha256 =
            required_string(&input, "approval_contract_sha256")?.to_string();
        let layout = self.layout(&app_id)?;
        let mut journal =
            local_apps::load_candidate_journal(&layout).map_err(|error| error.to_string())?;
        if journal.workflow_run_id != workflow_run_id {
            return Err(
                "receipt_invalid: workflow run does not match the prepared candidate".into(),
            );
        }
        if journal.approval_contract_sha256 != approval_contract_sha256 {
            return Err(
                "receipt_invalid: approval contract digest does not match the prepared candidate"
                    .into(),
            );
        }
        if journal.stage == local_apps::McpAuthoringStage::Prepared {
            let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
            let candidate = self.load_mcp_candidate(&app_id, &workflow_run_id)?;
            if candidate.approval_contract_sha256 != approval_contract_sha256
                || candidate.validated.proposal_sha256 != journal.proposal_sha256
                || candidate.validated.tool_surface_sha256 != journal.tool_surface_sha256
                || local_apps::approval_contract_sha256(candidate.review_surface.clone())
                    .map_err(|issue| issue.message)?
                    != approval_contract_sha256
            {
                return Err("receipt_invalid: persisted MCP review surface changed".into());
            }
            if !self
                .request_mcp_candidate_approval(&app_id, &workflow_run_id, &manifest, &candidate)
                .await?
            {
                return Err("user denied the Local App MCP proposal".into());
            }
            // The native sheet may remain open for minutes. Re-read the sealed
            // journal and candidate before minting a receipt so approval cannot
            // be applied to a superseding/tampered proposal.
            journal =
                local_apps::load_candidate_journal(&layout).map_err(|error| error.to_string())?;
            let current_candidate = self.load_mcp_candidate(&app_id, &workflow_run_id)?;
            if journal.stage != local_apps::McpAuthoringStage::Prepared
                || journal.workflow_run_id != workflow_run_id
                || journal.approval_contract_sha256 != approval_contract_sha256
                || journal.proposal_sha256 != candidate.validated.proposal_sha256
                || journal.tool_surface_sha256 != candidate.validated.tool_surface_sha256
                || current_candidate.approval_contract_sha256 != candidate.approval_contract_sha256
                || current_candidate.validated.proposal_sha256
                    != candidate.validated.proposal_sha256
                || current_candidate.validated.tool_surface_sha256
                    != candidate.validated.tool_surface_sha256
            {
                return Err(
                    "receipt_invalid: MCP candidate changed while awaiting approval".into(),
                );
            }
            let receipt = local_apps::McpConfirmationReceipt::new(
                &app_id,
                &workflow_run_id,
                approval_contract_sha256.clone(),
                journal.proposal_sha256.clone(),
                now_ms(),
            );
            let receipt_id = receipt.receipt_id.clone();
            self.pending_mcp_receipts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .issue(receipt)
                .map_err(|issue| issue.message)?;
            journal = journal
                .advance(local_apps::McpAuthoringStage::Approved)
                .map_err(|issue| issue.message)?;
            local_apps::save_candidate_journal(&layout, &journal)
                .map_err(|error| error.to_string())?;
            return Ok(json!({
                "approved": true,
                "receipt_id": receipt_id,
                "status": "approved",
            }));
        }
        Ok(json!({
            "approved": true,
            "receipt_id": Value::Null,
            "status": "approved_reusable",
        }))
    }
}

#[async_trait]
impl LocalAppsMcpHost for LocalAppsHostBroker {
    fn create_next_step(&self) -> String {
        create_next_step_guidance()
    }

    async fn runtime_profiles(&self, input: Value) -> Result<Value, String> {
        self.runtime_profiles_value(input).await
    }

    async fn template_catalog(&self, _input: Value) -> Result<Value, String> {
        let view = crate::mobile::local_app_template_catalog::catalog_view()?;
        serde_json::to_value(view).map_err(|error| format!("serialize template catalog: {error}"))
    }

    async fn validate_template_selection(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?;
        let workflow_run_id = required_string(&input, "workflow_run_id")?;
        if input.get("caller_role").is_some() {
            return Err(
                "template_selector_only: caller_role is not an authority proof; use the Host-issued selector_capability"
                    .into(),
            );
        }
        let record = self
            .service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        if record.scaffolded {
            return Err("template_selection_rejected: app is already scaffolded; update/verify must use its persisted profile".into());
        }
        crate::mobile::local_app_template_catalog::validate_and_journal(
            &self.root,
            app_id,
            workflow_run_id,
            &input,
        )
    }

    async fn resolve_template_selection(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?;
        let workflow_run_id = required_string(&input, "workflow_run_id")?;
        let handle = required_string(&input, "validated_selection_handle")?;
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        crate::mobile::local_app_template_catalog::resolve(
            &self.root,
            app_id,
            workflow_run_id,
            handle,
        )
    }

    async fn local_app_contract(&self, input: Value) -> Result<Value, String> {
        LocalAppsHostBroker::local_app_contract(self, input).await
    }

    async fn prepare(&self, input: Value) -> Result<Value, String> {
        self.prepare_value(input).await
    }

    async fn qa_begin(&self, input: Value) -> Result<Value, String> {
        LocalAppsHostBroker::qa_begin(self, input).await
    }

    async fn qa_read_evidence(&self, input: Value) -> Result<Value, String> {
        LocalAppsHostBroker::qa_read_evidence(self, input).await
    }

    async fn qa_finalize(&self, input: Value) -> Result<Value, String> {
        LocalAppsHostBroker::qa_finalize(self, input).await
    }

    async fn stage_create(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?;
        let workflow_run_id = required_string(&input, "workflow_run_id")?;
        let handle = required_string(&input, "validated_selection_handle")?;
        let quality_level = required_string(&input, "quality_level")?;
        // WP5: the user-confirmed display name and brief are staged HERE, once,
        // so they can be the single value the native create confirmation sheet
        // renders and `LocalAppScaffold` commits — never the empty shell
        // `AppRecord.name`/`.brief` (which stays the `untitled` placeholder the
        // whole way through create; see `service.rs`'s `PLACEHOLDER_APP_NAME`).
        let name = confirmed_field(&input, "name")?.to_string();
        if name.len() > local_apps::service::MAX_NAME_BYTES {
            return Err(format!(
                "invalid_argument: name is {} bytes (limit {})",
                name.len(),
                local_apps::service::MAX_NAME_BYTES
            ));
        }
        let brief = confirmed_field(&input, "brief")?.to_string();
        if brief.len() > local_apps::service::MAX_BRIEF_BYTES {
            return Err(format!(
                "invalid_argument: brief is {} bytes (limit {})",
                brief.len(),
                local_apps::service::MAX_BRIEF_BYTES
            ));
        }
        let mcp_intent = parse_staged_mcp_intent(&input)?;
        let design_spec = input.get("design_spec").cloned();
        let record = self
            .service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        if record.scaffolded {
            return Err("create_staging_rejected: app is already scaffolded".into());
        }
        // THIS RUN's `stage_create` may already have been approved — the
        // native create sheet shown, the user's answer journaled — and that
        // approval is bound to the exact staging bytes the sheet rendered.
        // Re-staging past that point would let a second call silently
        // rewrite `design-spec.json`/`evidence.json` underneath an approval
        // the user already gave for the OLD content, so the approved receipt
        // would end up committing bytes nobody confirmed.
        //
        // Scoped to the SAME `workflow_run_id`, exactly as `create_without
        // _mcp`'s already-approved reuse arm spells the same check. The
        // candidate journal is per-APP (one `load_candidate_journal(&layout)`
        // file), and staging is per-RUN
        // (`.lingxi-build-state/template-candidates/<app>/<run>/staging/…`,
        // which is also where `load_create_proposal_context` reads
        // `evidence.json` back out), so a DIFFERENT run cannot reach the
        // approved run's bytes and must stay allowed. Refusing it outright
        // would brick the app: the journal survives a failed
        // `LocalAppScaffold` (that arm returns the error without deleting
        // it), so an app whose scaffold was refused for an invalid name or
        // brief could never be staged again in any run — including the new
        // run this very error tells the caller to start.
        if let Ok(layout) = self.layout(app_id) {
            if let Ok(existing_journal) = local_apps::load_candidate_journal(&layout) {
                if existing_journal.workflow_run_id == workflow_run_id
                    && existing_journal.stage >= local_apps::McpAuthoringStage::Approved
                {
                    return Err(
                        "create_staging_rejected: this create candidate is already approved; start a new workflow run".into(),
                    );
                }
            }
        }
        let selection = crate::mobile::local_app_template_catalog::resolve_typed(
            &self.root,
            app_id,
            workflow_run_id,
            handle,
        )?;
        validate_create_stage_quality(quality_level, selection.runtime_profile.family)?;
        let artifacts = crate::mobile::local_app_runtime_profiles::scaffold_artifacts_for_binding(
            &selection.runtime_profile,
        )
        .map_err(|error| format!("stage template dependencies: {error}"))?;
        let requested = artifacts
            .files
            .iter()
            .find(|(path, _)| {
                *path == crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL
            })
            .map(|(_, bytes)| bytes.as_slice())
            .ok_or_else(|| {
                "create_staging_invalid: requested dependency input missing".to_string()
            })?;
        let effective = artifacts
            .files
            .iter()
            .find(|(path, _)| {
                *path == crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL
            })
            .map(|(_, bytes)| bytes.as_slice())
            .ok_or_else(|| "create_staging_invalid: effective package input missing".to_string())?;
        let lock = artifacts
            .files
            .iter()
            .find(|(path, _)| *path == crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL)
            .map(|(_, bytes)| bytes.as_slice())
            .ok_or_else(|| "create_staging_invalid: base lock input missing".to_string())?;
        // Not a verification: `dependency_input_sha256` is a pure function of
        // `requested`/`effective`/`lock`, taken from the same in-memory
        // `artifacts.files` with no I/O in between, so computing it twice and
        // comparing can never disagree — the branch below used to do exactly
        // that and was unreachable dead weight. The digest is still recorded
        // into `evidence.json` for provenance; nothing re-verifies it against
        // the materialized staging bytes at landing time (a separate gap).
        let dependency_input_sha256 =
            crate::mobile::local_app_template_catalog::dependency_input_sha256(
                requested,
                effective,
                lock,
                crate::mobile::local_app_runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY,
            );
        let staging = self
            .root
            .join(".lingxi-build-state/template-candidates")
            .join(app_id)
            .join(workflow_run_id)
            .join("staging")
            .join(handle);
        std::fs::create_dir_all(&staging)
            .map_err(|error| format!("create isolated staging: {error}"))?;
        // r4-failure-paths-09: armed for the whole materialization below and
        // disarmed only once `evidence.json` has been renamed into place, so
        // an I/O failure at any intermediate step reclaims the partial tree
        // instead of leaving it for `load_create_proposal_context` to find.
        let mut staging_reaper = PartialStagingReaper {
            staging: &staging,
            committed: false,
        };
        // `load_create_proposal_context` (and therefore both
        // `approve_mcp_proposal`'s create-without-MCP branch and
        // `validate_mcp_proposal`'s pre-scaffold branch) require a staged
        // MCP flow context file to exist before the native create
        // confirmation can be raised. A brand-new app has no prior active
        // MCP contexts to carry forward, so the correct seed here is the
        // empty set — the same baseline `validate_app_mcp_proposal` would
        // otherwise be handed for a first-time create. Writing it here,
        // once, keeps this file's only production writer honest for every
        // create run instead of leaving it to a `#[cfg(test)]` helper.
        let staged_context_dir = staging.join(".lingxi");
        std::fs::create_dir_all(&staged_context_dir)
            .map_err(|error| format!("create staged MCP flow context directory: {error}"))?;
        let staged_context_path = staged_context_dir.join("mcp-flow-contexts.json");
        let staged_context_temp_path = staged_context_dir.join("mcp-flow-contexts.json.tmp");
        std::fs::write(
            &staged_context_temp_path,
            serde_json::to_vec_pretty(&BTreeMap::<String, local_apps::AppMcpFlowContext>::new())
                .map_err(|error| format!("serialize staged MCP flow contexts: {error}"))?,
        )
        .map_err(|error| format!("write staged MCP flow contexts: {error}"))?;
        std::fs::rename(&staged_context_temp_path, &staged_context_path)
            .map_err(|error| format!("commit staged MCP flow contexts: {error}"))?;
        let design_spec_sha256 = if let Some(design_spec) = design_spec.as_ref() {
            let design_bytes = serde_json::to_vec_pretty(design_spec)
                .map_err(|error| format!("serialize design spec: {error}"))?;
            let design_path = staging.join("design-spec.json");
            let temp_path = staging.join("design-spec.json.tmp");
            std::fs::write(&temp_path, &design_bytes)
                .map_err(|error| format!("write design spec: {error}"))?;
            std::fs::rename(&temp_path, &design_path)
                .map_err(|error| format!("commit design spec: {error}"))?;
            Some(format!("{:x}", Sha256::digest(&design_bytes)))
        } else {
            None
        };
        // Materialize only the install-before-build inputs in the run-scoped
        // candidate staging area.  The app workspace and Manifest remain
        // untouched until the later receipt/publish phase.  Each file is
        // written atomically and read back before evidence is emitted so the
        // dependency digest covers bytes that actually reached staging.
        let template_root = staging.join("template");
        let mut staged_files = Vec::with_capacity(artifacts.files.len());
        for (relative, bytes) in artifacts.files {
            let relative_path = std::path::Path::new(relative);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(format!(
                    "create_staging_invalid: unsafe template artifact path {relative:?}"
                ));
            }
            let target = template_root.join(relative_path);
            let parent = target.parent().ok_or_else(|| {
                "create_staging_invalid: template artifact has no parent".to_string()
            })?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create template staging directory: {error}"))?;
            let temporary = target.with_file_name(format!(
                ".{}.tmp",
                target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        "create_staging_invalid: template artifact has invalid filename".to_string()
                    })?
            ));
            std::fs::write(&temporary, &bytes).map_err(|error| {
                format!("write template staging artifact {relative:?}: {error}")
            })?;
            std::fs::rename(&temporary, &target).map_err(|error| {
                format!("commit template staging artifact {relative:?}: {error}")
            })?;
            let materialized = std::fs::read(&target)
                .map_err(|error| format!("read template staging artifact {relative:?}: {error}"))?;
            if materialized != bytes {
                return Err(format!(
                    "create_staging_invalid: template artifact changed while staging {relative:?}"
                ));
            }
            staged_files.push(serde_json::json!({
                "path": relative,
                "sha256": format!("{:x}", sha2::Sha256::digest(&materialized)),
            }));
        }
        let evidence = serde_json::json!({
            "schemaVersion": 1,
            "staging": "isolated",
            "appId": app_id,
            "workflowRunId": workflow_run_id,
            "validatedSelectionHandle": handle,
            "name": name,
            "brief": brief,
            "mcpIntent": mcp_intent,
            "templateId": selection.template_id,
            "dependencyInputSha256": dependency_input_sha256,
            "designSpecSha256": design_spec_sha256,
            "stagedFiles": staged_files,
        });
        let evidence_path = staging.join("evidence.json");
        let bytes = serde_json::to_vec_pretty(&evidence)
            .map_err(|error| format!("serialize staging evidence: {error}"))?;
        let temp_path = staging.join("evidence.json.tmp");
        std::fs::write(&temp_path, bytes)
            .map_err(|error| format!("write staging evidence: {error}"))?;
        std::fs::rename(&temp_path, &evidence_path)
            .map_err(|error| format!("commit staging evidence: {error}"))?;
        // The commit marker is on disk; the tree is a complete candidate now.
        staging_reaper.committed = true;
        Ok(json!({
            "ok": true,
            "summary": "Create candidate staged in isolated Host storage.",
            "dependency_input_sha256": dependency_input_sha256,
            "design_spec_sha256": design_spec_sha256,
            "evidence": evidence,
        }))
    }

    async fn validate_mcp_proposal(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let workflow_run_id = required_string(&input, "workflow_run_id")?.to_string();
        Self::validate_workflow_run_id(&workflow_run_id)?;
        let proposal_value = input
            .get("proposal")
            .cloned()
            .ok_or_else(|| "proposal is required".to_string())?;
        let service = self.service()?;
        let record = service
            .record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(&app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let proposal: local_apps::AppMcpProposal = serde_json::from_value(proposal_value)
            .map_err(|error| format!("proposal_invalid: {error}"))?;
        let create_context = if record.scaffolded {
            None
        } else {
            Some(self.load_create_proposal_context(&app_id, &workflow_run_id)?)
        };
        let contexts = if let Some(context) = create_context.as_ref() {
            context.contexts.clone()
        } else {
            self.load_active_mcp_flow_contexts(&layout)?
        };
        let validated = local_apps::validate_app_mcp_proposal(
            proposal,
            &app_id,
            manifest.revision,
            &contexts,
            &local_apps::CapabilityRegistry::default(),
        )
        .map_err(|issues| {
            format!(
                "proposal_invalid: {}",
                issues
                    .into_iter()
                    .map(|issue| format!("{}: {}", issue.code, issue.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })?;
        let review_surface = Self::build_mcp_review_surface(
            &manifest,
            &validated,
            manifest.active_mcp_catalog.as_ref(),
            create_context.as_ref(),
        );
        let approval_contract_sha256 = local_apps::approval_contract_sha256(review_surface.clone())
            .map_err(|issue| format!("proposal_invalid: {}", issue.message))?;
        let active_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?;
        let mut journal = local_apps::McpCandidateJournal {
            schema_version: local_apps::APPS_SCHEMA_VERSION,
            app_id: app_id.clone(),
            workflow_run_id: workflow_run_id.clone(),
            stage: local_apps::McpAuthoringStage::Prepared,
            previous_build_id: active_build_id,
            previous_catalog_sha256: manifest
                .active_mcp_catalog
                .as_ref()
                .map(|catalog| catalog.catalog_sha256.clone()),
            proposal_sha256: validated.proposal_sha256.clone(),
            approval_contract_sha256: approval_contract_sha256.clone(),
            tool_surface_sha256: validated.tool_surface_sha256.clone(),
            catalog_sha256: None,
            integrity_sha256: String::new(),
        }
        .seal()
        .map_err(|issue| issue.message)?;
        let unchanged_approval = manifest.active_mcp_catalog.as_ref().is_some_and(|catalog| {
            catalog.approval_contract_sha256 == approval_contract_sha256
                && catalog.tool_surface_sha256 == validated.tool_surface_sha256
        });
        if unchanged_approval {
            journal = journal
                .advance(local_apps::McpAuthoringStage::Approved)
                .map_err(|issue| issue.message)?;
        }
        local_apps::save_candidate_journal(&layout, &journal).map_err(|error| error.to_string())?;
        self.save_mcp_candidate(
            &app_id,
            &workflow_run_id,
            &PersistedMcpCandidate {
                validated: validated.clone(),
                approval_contract_sha256: approval_contract_sha256.clone(),
                review_surface: review_surface.clone(),
                verification_sha256: None,
                catalog_sha256: None,
                qa_context_sha256: None,
            },
        )?;
        Ok(json!({
            "ok": true,
            "status": if unchanged_approval { "approved_reusable" } else { "approval_required" },
            "proposal_sha256": validated.proposal_sha256,
            "approval_contract_sha256": approval_contract_sha256,
            "tool_surface_sha256": validated.tool_surface_sha256,
            "findings": [],
            "review_surface": review_surface,
        }))
    }

    /// Approve a Local App MCP/create proposal through the native sheet.
    async fn approve_mcp_proposal(&self, input: Value) -> Result<Value, String> {
        self.approve_mcp_proposal_with(input, CreateApprovalAuthority::NativeSheet)
            .await
    }
    async fn qa_mcp_candidate(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let workflow_run_id = required_string(&input, "workflow_run_id")?.to_string();
        let layout = self.layout(&app_id)?;
        let mut journal =
            local_apps::load_candidate_journal(&layout).map_err(|error| error.to_string())?;
        if journal.workflow_run_id != workflow_run_id {
            return Err(
                "journal_invalid: workflow run does not match the candidate journal".into(),
            );
        }
        if journal.stage < local_apps::McpAuthoringStage::Approved {
            return Err("approval_required: MCP candidate is not approved".into());
        }
        let mut candidate = self.load_mcp_candidate(&app_id, &workflow_run_id)?;
        let definitions = candidate
            .validated
            .tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>();
        local_apps::validate_generated_mcp_catalog(&definitions).map_err(|issues| {
            format!(
                "mcp_qa_failed: {}",
                issues
                    .into_iter()
                    .map(|issue| format!("{}: {}", issue.code, issue.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })?;
        let build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "mcp_qa_failed: app has no active build".to_string())?;
        let contexts = self.load_active_mcp_flow_contexts(&layout)?;
        let mut tool_evidence = Vec::new();
        let mut context_sha256 = BTreeMap::new();
        let mut first_isolation_probe = None;
        for tool in &candidate.validated.tools {
            let witness = minimal_schema_witness(&tool.definition.input_schema)
                .map_err(|error| format!("mcp_qa_failed: {}: {error}", tool.definition.name))?;
            let context = contexts.get(&tool.flow.flow_id).ok_or_else(|| {
                format!(
                    "mcp_qa_failed: flow_not_found: active typed Flow {} is unavailable",
                    tool.flow.flow_id
                )
            })?;
            let execution = self
                .execute_bound_mcp_flow(
                    &app_id,
                    witness.clone(),
                    &tool.definition,
                    &tool.flow,
                    context,
                    tool.ceiling,
                    BoundMcpFlowMode::Qa,
                )
                .await
                .map_err(|error| format!("mcp_qa_failed: {}: {error}", tool.definition.name))?;
            let expected_steps = context
                .flow
                .steps
                .iter()
                .map(|step| step.step_id.as_str())
                .collect::<Vec<_>>();
            let visited_steps = execution
                .step_calls
                .iter()
                .map(|step| step.step_id.as_str())
                .collect::<Vec<_>>();
            if visited_steps != expected_steps {
                return Err(format!(
                    "mcp_qa_failed: {}: flow_call_evidence_incomplete",
                    tool.definition.name
                ));
            }
            if first_isolation_probe.is_none() {
                first_isolation_probe = Some((tool.clone(), witness.clone(), context.clone()));
            }
            let context_digest = value_sha256(
                &serde_json::to_value(context)
                    .map_err(|error| format!("serialize MCP Flow context: {error}"))?,
            )?;
            context_sha256.insert(tool.flow.flow_id.clone(), context_digest.clone());
            tool_evidence.push(QaToolExecutionEvidence {
                tool_name: tool.definition.name.clone(),
                flow_id: tool.flow.flow_id.clone(),
                context_sha256: context_digest,
                input_sha256: value_sha256(&witness)?,
                result_sha256: value_sha256(&execution.result)?,
                step_calls: execution.step_calls,
            });
        }
        let isolation = if let Some((tool, witness, mut mismatched_context)) = first_isolation_probe
        {
            mismatched_context.app_id = format!("{app_id}-other");
            let error = self
                .execute_bound_mcp_flow(
                    &app_id,
                    witness,
                    &tool.definition,
                    &tool.flow,
                    &mismatched_context,
                    tool.ceiling,
                    BoundMcpFlowMode::Qa,
                )
                .await
                .expect_err("cross-app QA probe must be rejected");
            if !error.starts_with("cross_app_flow") {
                return Err(format!(
                    "mcp_qa_failed: isolation_probe_unexpected: {error}"
                ));
            }
            json!({
                "status": "passed",
                "toolName": tool.definition.name,
                "flowId": tool.flow.flow_id,
                "rejection": error,
            })
        } else {
            json!({
                "status": "not_applicable",
                "reason": "candidate exposes no tools",
            })
        };
        let execution = serde_json::to_value(
            candidate
                .validated
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "definition": tool.definition,
                        "flow": tool.flow,
                        "ceiling": tool.ceiling,
                        "contextSha256": context_sha256.get(&tool.flow.flow_id),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|error| format!("serialize execution bindings: {error}"))?;
        let catalog_sha256 = local_apps::catalog_sha256(&candidate.validated, &build_id, execution)
            .map_err(|issue| issue.message)?;
        let verification_sha256 = local_apps::approval_contract_sha256(json!({
            "appId": app_id,
            "workflowRunId": workflow_run_id,
            "catalogSha256": catalog_sha256,
            "toolEvidence": tool_evidence,
            "isolation": isolation,
        }))
        .map_err(|issue| issue.message)?;
        journal.catalog_sha256 = Some(catalog_sha256.clone());
        journal = journal.seal().map_err(|issue| issue.message)?;
        while journal.stage < local_apps::McpAuthoringStage::McpVerified {
            let next_stage = match journal.stage {
                local_apps::McpAuthoringStage::Approved => local_apps::McpAuthoringStage::Built,
                local_apps::McpAuthoringStage::Built => local_apps::McpAuthoringStage::SmokePassed,
                local_apps::McpAuthoringStage::SmokePassed => {
                    local_apps::McpAuthoringStage::McpVerified
                }
                _ => local_apps::McpAuthoringStage::McpVerified,
            };
            journal = journal.advance(next_stage).map_err(|issue| issue.message)?;
        }
        candidate.verification_sha256 = Some(verification_sha256.clone());
        candidate.catalog_sha256 = Some(catalog_sha256.clone());
        candidate.qa_context_sha256 = Some(context_sha256);
        // Persist the evidence-bearing candidate before advancing the durable
        // journal. If this write fails, the journal must remain at its prior
        // stage so a caller can retry QA; an `McpVerified` journal pointing at
        // a candidate with no verification evidence would be a false commit.
        self.save_mcp_candidate(&app_id, &workflow_run_id, &candidate)?;
        local_apps::save_candidate_journal(&layout, &journal).map_err(|error| error.to_string())?;
        Ok(json!({
            "ok": true,
            "findings": [],
            "mcp_schema": "passed",
            "flow_binding": "passed",
            "calls": "passed",
            "isolation": isolation["status"],
            "tool_evidence": tool_evidence,
            "isolation_evidence": isolation,
            "verification_sha256": verification_sha256,
            "summary": "Host-side MCP schema, binding, call evidence and isolation gates passed.",
        }))
    }

    async fn promote_mcp_candidate(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let workflow_run_id = required_string(&input, "workflow_run_id")?.to_string();
        let receipt_id = input
            .get("receipt_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let service = self.service()?;
        let layout = self.layout(&app_id)?;
        let mut journal =
            local_apps::load_candidate_journal(&layout).map_err(|error| error.to_string())?;
        if journal.workflow_run_id != workflow_run_id {
            return Err(
                "journal_invalid: workflow run does not match the candidate journal".into(),
            );
        }
        if journal.stage < local_apps::McpAuthoringStage::McpVerified {
            return Err("mcp_qa_failed: MCP candidate has not completed QA".into());
        }
        let candidate = self.load_mcp_candidate(&app_id, &workflow_run_id)?;
        if candidate.validated.proposal_sha256 != journal.proposal_sha256
            || candidate.validated.tool_surface_sha256 != journal.tool_surface_sha256
            || candidate.approval_contract_sha256 != journal.approval_contract_sha256
        {
            return Err(
                "promotion_failed: persisted candidate identity changed after approval".into(),
            );
        }
        let qa_context_sha256 = candidate
            .qa_context_sha256
            .as_ref()
            .ok_or_else(|| "promotion_failed: QA context digests are missing".to_string())?;
        let active_contexts = self.load_active_mcp_flow_contexts(&layout)?;
        for tool in &candidate.validated.tools {
            let expected = qa_context_sha256.get(&tool.flow.flow_id).ok_or_else(|| {
                format!(
                    "promotion_failed: QA context digest is missing for Flow {}",
                    tool.flow.flow_id
                )
            })?;
            let context = active_contexts.get(&tool.flow.flow_id).ok_or_else(|| {
                format!(
                    "promotion_failed: active Flow {} disappeared after QA",
                    tool.flow.flow_id
                )
            })?;
            let actual = value_sha256(
                &serde_json::to_value(context)
                    .map_err(|error| format!("serialize MCP Flow context: {error}"))?,
            )?;
            if &actual != expected {
                return Err(format!(
                    "promotion_failed: active Flow {} changed after QA",
                    tool.flow.flow_id
                ));
            }
        }
        let catalog_sha256 = candidate
            .catalog_sha256
            .clone()
            .or_else(|| journal.catalog_sha256.clone())
            .ok_or_else(|| "catalog_invalid: candidate catalog digest is missing".to_string())?;
        let build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "promotion_failed: app has no active build".to_string())?;
        let execution = serde_json::to_value(
            candidate
                .validated
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "definition": tool.definition,
                        "flow": tool.flow,
                        "ceiling": tool.ceiling,
                        "contextSha256": qa_context_sha256.get(&tool.flow.flow_id),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|error| format!("serialize execution bindings: {error}"))?;
        let recomputed_catalog_sha256 =
            local_apps::catalog_sha256(&candidate.validated, &build_id, execution.clone())
                .map_err(|issue| issue.message)?;
        if journal.catalog_sha256.as_deref() != Some(recomputed_catalog_sha256.as_str())
            || candidate.catalog_sha256.as_deref() != Some(recomputed_catalog_sha256.as_str())
            || catalog_sha256 != recomputed_catalog_sha256
        {
            return Err("promotion_failed: candidate catalog digest changed after QA".into());
        }
        if let Some(receipt_id) = receipt_id.as_deref() {
            self.pending_mcp_receipts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .consume_candidate(
                    receipt_id,
                    &app_id,
                    &workflow_run_id,
                    &journal.approval_contract_sha256,
                    &journal.proposal_sha256,
                    now_ms(),
                )
                .map_err(|issue| issue.message)?;
            // r2-never-wired-07: no consumed-receipt stamp here
            // either. `consume_candidate` above is the replay refusal, it is
            // in-process, and `McpReceiptBook` is memory-only — a restart
            // makes every receipt id `receipt_missing` before a journal reader
            // could get a word in. See `commit_scaffold`'s note.
        }
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let catalog_body = json!({
            "appId": app_id,
            "buildId": build_id,
            "proposal": candidate.validated.proposal.clone(),
            "tools": candidate.validated.tools.iter().map(|tool| json!({
                "definition": tool.definition,
                "flow": tool.flow,
                "ceiling": tool.ceiling,
            })).collect::<Vec<_>>(),
            "execution": execution,
        });
        local_apps::save_mcp_catalog(&layout, &catalog_sha256, &catalog_body)
            .map_err(|error| error.to_string())?;
        let previous = manifest.active_mcp_catalog.clone();
        let _settings_guard = self.mcp_settings_writes.lock().await;
        let current_settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
        let current_settings_revision = current_settings.revision;
        let next_settings = if previous.is_none() {
            AppMcpSettings {
                // Authoring/promotion establishes an approved surface; it
                // does not grant model visibility. Every Local App MCP starts
                // disabled and becomes callable only after the user enables
                // the app-owned service from its settings page.
                enabled: false,
                enabled_tools: mcp_catalog_tool_names(&catalog_body)
                    .map_err(|error| error.to_string())?,
                ..current_settings
            }
        } else {
            let previous_catalog = local_apps::load_mcp_catalog(
                &layout,
                &previous.as_ref().expect("checked above").catalog_sha256,
            )
            .map_err(|error| error.to_string())?;
            let previous_names: std::collections::BTreeSet<String> =
                mcp_catalog_tool_names(&previous_catalog)
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .collect();
            let previously_enabled: std::collections::BTreeSet<String> =
                current_settings.enabled_tools.iter().cloned().collect();
            let enabled_tools = mcp_catalog_tool_names(&catalog_body)
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|name| previously_enabled.contains(name) || !previous_names.contains(name))
                .collect();
            AppMcpSettings {
                enabled_tools,
                ..current_settings
            }
        };
        let mut promoted_manifest = manifest.clone();
        if promoted_manifest.revision == 0 {
            promoted_manifest.revision = 1;
        }
        promoted_manifest.active_mcp_catalog = Some(local_apps::AppMcpCatalogRef {
            build_id,
            manifest_revision: promoted_manifest.revision,
            authoring_revision: previous
                .as_ref()
                .map(|catalog| {
                    if catalog.tool_surface_sha256 == candidate.validated.tool_surface_sha256 {
                        catalog.authoring_revision
                    } else {
                        catalog.authoring_revision + 1
                    }
                })
                .unwrap_or(1),
            user_goal_sha256: candidate.validated.proposal.user_goal_sha256.clone(),
            proposal_sha256: candidate.validated.proposal_sha256.clone(),
            approval_contract_sha256: candidate.approval_contract_sha256.clone(),
            tool_surface_sha256: candidate.validated.tool_surface_sha256.clone(),
            catalog_sha256: catalog_sha256.clone(),
            mcp_verification_sha256: candidate
                .verification_sha256
                .clone()
                .ok_or_else(|| "promotion_failed: verification digest is missing".to_string())?,
        });
        local_apps::save_manifest(&layout, &promoted_manifest)
            .map_err(|error| error.to_string())?;
        save_mcp_settings(&layout, &next_settings, Some(current_settings_revision))
            .map_err(|error| error.to_string())?;
        drop(_settings_guard);
        if journal.stage < local_apps::McpAuthoringStage::Promoted {
            journal = journal
                .advance(local_apps::McpAuthoringStage::Promoted)
                .map_err(|issue| issue.message)?;
            local_apps::save_candidate_journal(&layout, &journal)
                .map_err(|error| error.to_string())?;
        }
        self.sync_managed_local_app_publication(&app_id).await?;
        self.emit_managed_mcp_inventory().await?;
        let _ = service.announce_record(&app_id).await;
        Ok(json!({
            "promoted": true,
            "catalog_sha256": catalog_sha256,
            "status": "promoted",
            "publication_state": "published_unverified",
        }))
    }

    async fn manage_runtime(&self, input: Value) -> Result<Value, String> {
        self.manage_runtime_value(input).await
    }

    async fn build_app(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        // Existence gate (same shape as the UI ops above).
        let service = self.service()?;
        service.record(&app_id).await.map_err(|e| e.to_string())?;
        let layout =
            AppLayout::new(self.root.clone(), app_id.clone()).map_err(|e| e.to_string())?;
        let authoring_candidate = self.authoring_contract_for_build(&layout, &input)?;
        // Persist the immutable candidate document before the build starts.
        // The active build receipt remains the selector, so a failed build
        // continues to select the old contract/build identity.
        if let Some(candidate) = authoring_candidate.as_ref() {
            let digest =
                local_apps::authoring::save_authoring_contract(&layout, &candidate.contract)
                    .map_err(|error| error.to_string())?;
            if digest != candidate.contract_sha256 {
                return Err(
                    "authoring_contract_invalid: Host candidate digest changed while persisting"
                        .into(),
                );
            }
        }
        let builder = crate::mobile::local_apps_build::LocalAppBuilder {
            mobile_linux: self.mobile_linux(),
            host: self,
        };
        builder
            .build_workspace_with_authoring(
                &layout,
                authoring_candidate.as_ref().map(|candidate| {
                    crate::mobile::local_apps_build::AuthoringCandidateIdentity {
                        handle: candidate.handle.clone(),
                        workflow_run_id: candidate.workflow_run_id.clone(),
                        contract_sha256: candidate.contract_sha256.clone(),
                        base_contract_sha256: candidate.base_contract_sha256.clone(),
                    }
                }),
            )
            .await
            .map_err(|e| e.to_string())?;
        // "Ready" means SERVABLE, not "the build tool exited 0". The static
        // preview server refuses to start without `build/store/dist/index.html`
        // (see `start_reserved_runtime`), and a build whose output landed
        // elsewhere exits 0 while producing nothing this host can serve.
        // Stamping `ready` there would leave a permanently unstartable app
        // advertised as ready in the library.
        let served_index = layout
            .root()
            .join(layout.build_rel(false))
            .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR)
            .join("index.html");
        if !served_index.exists() {
            return Err(format!(
                "the build finished but produced no servable output at {}. The build must emit \
                 the canonical `dist/` directory; restore the standard Vite output contract, \
                 then run the build tool again.",
                served_index.display()
            ));
        }
        // Publication state is derived from the active build/catalog pair in
        // schema v3. A successful build alone must not mutate a persistent
        // workflow state or advertise an active MCP surface.
        self.rebind_active_mcp_catalog_to_current_build(&app_id, &layout)
            .await?;
        self.emit_current_verification_summary(&app_id, &layout)
            .await;
        let dependencies = service
            .dependency_record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let target = crate::mobile::local_apps_build::detect_build_target(&layout)
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "ok": true,
            "app_id": app_id,
            "target": target.template_id(),
            "dependencies": dependencies,
            "hint": "start or restart the runtime with LocalAppRuntime to serve the new build",
        }))
    }

    async fn install_dependencies(&self, input: Value) -> Result<Value, String> {
        self.install_dependencies_value(input).await
    }

    async fn confirm_dependency_change(&self, _input: Value) -> Result<Value, String> {
        let app_id = required_string(&_input, "app_id")?.to_string();
        let service = self.service()?;
        service
            .record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let dependency_record = service
            .dependency_record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(&app_id)?;
        let (_binding, baseline, changes, requested_json, effective_package_json) =
            Self::prepare_dependency_change(
                &layout,
                &dependency_record,
                _input
                    .get("changes")
                    .ok_or_else(|| "invalid_argument: changes is required".to_string())?,
            )?;
        if changes
            .iter()
            .any(|change| !matches!(change.kind, DependencyChangeKind::Remove))
        {
            // These statuses intentionally describe what can be proven before
            // resolution.  Looking in the pnpm store or touching the registry
            // here would make a supposedly review-only call perform network or
            // cache work before the user's approval.
            let confirmation_changes = changes
                .iter()
                .map(|change| AppDependencyChangeDto {
                    kind: dependency_change_kind_dto(&change.kind),
                    package: change.package.clone(),
                    version: change.version.clone(),
                    cache_status: dependency_change_cache_status(&change.kind),
                    download_status: match change.kind {
                        DependencyChangeKind::Remove => "not_required".to_string(),
                        DependencyChangeKind::Add | DependencyChangeKind::Update => {
                            "may_be_required".to_string()
                        }
                    },
                })
                .collect();
            let request_id = self.request_id("app-dependency-change");
            let (sender, receiver) = oneshot::channel();
            self.pending_dependency_change_confirmations
                .lock()
                .await
                .insert(request_id.clone(), sender);
            let _confirmation_guard = PendingDependencyConfirmationGuard {
                pending: &self.pending_dependency_change_confirmations,
                request_id: request_id.clone(),
            };
            self.event_sink
                .emit(ClientEvent::AppEvent {
                    event: AppEventDto::AppDependencyChangeConfirmationRequested {
                        request: AppDependencyChangeConfirmationRequestDto {
                            request_id: request_id.clone(),
                            app_id: app_id.clone(),
                            // Stable codes keep native clients localized while
                            // still making the policy explicit on the wire.
                            reason: "pre_resolution_no_network".into(),
                            changes: confirmation_changes,
                            license_risk: "unknown_until_resolution".into(),
                            sbom_risk: "unknown_until_resolution".into(),
                            lifecycle_scripts_blocked: true,
                            native_addons_blocked: true,
                            rollback_policy: "rollback_on_validation_failure".into(),
                        },
                    },
                })
                .await;
            let approval_wait_span = tracing::debug_span!(
                "local_app_dependency_native_confirmation_wait",
                app_id = %app_id,
                request_id = %request_id,
            );
            let approved = {
                let _perf = LocalAppPerfDiagnosticTimer::start("dependency_native_approval_wait");
                match timeout(APPROVAL_TIMEOUT, receiver)
                    .instrument(approval_wait_span)
                    .await
                {
                    Ok(Ok(approved)) => approved,
                    Ok(Err(_)) => {
                        self.pending_dependency_change_confirmations
                            .lock()
                            .await
                            .remove(&request_id);
                        return Err("dependency change confirmation was cancelled".into());
                    }
                    Err(_) => {
                        self.pending_dependency_change_confirmations
                            .lock()
                            .await
                            .remove(&request_id);
                        return Err("dependency change confirmation timed out".into());
                    }
                }
            };
            if !approved {
                return Err("user denied dependency changes".into());
            }
        }
        // Native approval may take minutes.  The receipt recheck below only
        // needs the app's cross-process lock; waiting on the broker-wide Node
        // build mutex here would block an unrelated app's build for the whole
        // approval-to-receipt gap.
        let _process_build_guard = {
            let _perf = LocalAppPerfDiagnosticTimer::start("dependency_app_lock_wait");
            local_apps::storage::lock_app_build(layout.root(), layout.app_id())
                .map_err(|error| error.to_string())?
        };
        let current_dependency = service
            .dependency_record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let (_, current_baseline, _, _, _) = Self::prepare_dependency_change(
            &layout,
            &current_dependency,
            _input.get("changes").unwrap(),
        )?;
        if current_baseline != baseline {
            return Err(
                "dependencies_dirty: dependency baseline changed while waiting for confirmation; reconfirm before updating"
                    .into(),
            );
        }
        let receipt = self
            .issue_dependency_change_receipt(
                &app_id,
                baseline,
                requested_json,
                effective_package_json,
                changes.clone(),
            )
            .await?;
        Ok(json!({
            "ok": true,
            "app_id": app_id,
            "changes": changes,
            "receipt": {
                "id": receipt.receipt_id,
                "app_id": receipt.app_id,
                "issued_at_ms": receipt.issued_at_ms,
                "expires_at_ms": receipt.expires_at_ms,
            }
        }))
    }

    async fn update_dependencies(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let receipt_id = required_string(&input, "receipt_id")?.to_string();
        let service = self.service()?;
        let layout = self.layout(&app_id)?;
        let toolchain = Self::toolchain_for_layout(&layout)?;
        let toolchain_key = toolchain.key();
        // Keep the same lock order as LocalAppBuilder: broker-wide async
        // mutex first, then the per-app cross-process lock. The dependency
        // snapshot, production build and any rollback therefore form one
        // transaction without deadlocking the builder.
        let build_lock = self.build_lock();
        let _build_guard = {
            let _perf = LocalAppPerfDiagnosticTimer::start("dependency_global_build_lock_wait");
            build_lock.lock().await
        };
        let _process_build_guard = {
            let _perf = LocalAppPerfDiagnosticTimer::start("dependency_app_lock_wait");
            local_apps::storage::lock_app_build(layout.root(), layout.app_id())
                .map_err(|error| error.to_string())?
        };
        let previous_dependency = service
            .dependency_record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let receipt = self
            .claim_dependency_change_receipt(&app_id, &receipt_id)
            .await?;
        let current_baseline =
            match Self::load_trusted_dependency_baseline(&layout, &previous_dependency) {
                Ok((_, _, _, baseline)) => baseline,
                Err(error) => {
                    self.consume_dependency_change_receipt(&app_id, &receipt_id)
                        .await;
                    return Err(error);
                }
            };
        if current_baseline != receipt.baseline {
            self.consume_dependency_change_receipt(&app_id, &receipt_id)
                .await;
            return Err(
                "dependencies_dirty: dependency confirmation became stale before update; reconfirm before applying it"
                    .into(),
            );
        }
        let rollback = match self.capture_dependency_update_rollback(&layout, previous_dependency) {
            Ok(rollback) => rollback,
            Err(error) => {
                self.release_dependency_change_receipt_claim(&app_id, &receipt_id)
                    .await;
                return Err(error);
            }
        };
        let recovery_journal = match Self::dependency_update_recovery_journal(
            &layout,
            &rollback,
            DependencyUpdateRecoveryStatus::InProgress,
        ) {
            Ok(journal) => journal,
            Err(error) => {
                Self::discard_dependency_update_rollback(rollback);
                self.release_dependency_change_receipt_claim(&app_id, &receipt_id)
                    .await;
                return Err(error);
            }
        };
        if let Err(error) =
            Self::write_dependency_update_recovery_journal(&layout, &recovery_journal)
        {
            let _ = Self::remove_dependency_update_recovery_journal(&layout);
            Self::discard_dependency_update_rollback(rollback);
            self.release_dependency_change_receipt_claim(&app_id, &receipt_id)
                .await;
            return Err(error);
        }
        let result: Result<Value, String> = async {
            service
                .record(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            let current = service
                .dependency_record(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            if current.state == AppDependencyState::Installing {
                return Err(format!(
                    "app {app_id} already has a dependency install in progress"
                ));
            }
            let workspace = layout.root().join(layout.workspace_rel());
            let runtime = self.mobile_linux().ok_or_else(|| {
                "the mobile Node runtime is unavailable for dependency updates".to_string()
            })?;
            let dependency_staging = Self::prepare_dependency_staging(&layout)?;
            crate::mobile::local_apps_build::write_file(
                &dependency_staging,
                "package.json",
                &receipt.effective_package_json,
                true,
            )
            .map_err(|error| error.to_string())?;
            if current.state == AppDependencyState::Ready {
                service
                    .queue_dependency_install(&app_id)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            service
                .start_dependency_install(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            let dependency_store = self.dependency_store_root(toolchain_key);
            std::fs::create_dir_all(&dependency_store)
                .map_err(|error| format!("create pnpm dependency store: {error}"))?;
            let build_mount = MountSpec {
                host_path: workspace.clone(),
                guest_path: platform_api::local_app_paths::local_app_build_project(
                    &app_id, "store",
                ),
                read_only: false,
                purpose: MountPurpose::LocalAppBuild,
            };
            let store_mount = MountSpec {
                host_path: dependency_store,
                guest_path: platform_api::local_app_paths::LOCAL_APP_DEPENDENCY_STORE.to_string(),
                read_only: false,
                purpose: MountPurpose::Shared,
            };
            let project_guest_path = build_mount.guest_path.clone();
            let dependency_staging_guest_path =
                format!("{project_guest_path}/.lingxi-build-state/dependency-staging");
            let build_state_root = format!("{project_guest_path}/.lingxi-build-state");
            let memory_mb = crate::mobile::local_apps_build::build_memory_budget_mb(
                self.physical_memory_bytes(),
            );
            let requires_network = receipt
                .summary
                .iter()
                .any(|change| !matches!(change.kind, DependencyChangeKind::Remove));
            // Add/update flows may use approved network access to resolve the
            // user-confirmed manifest and preheat the shared store. Remove-only
            // flows stay offline throughout so they cannot silently upgrade an
            // unrelated dependency. Resolve once, bind the resulting lock
            // digest immediately, and serialize only the immutable snapshot
            // decision. A verified snapshot supplies the already-validated
            // tree directly; otherwise the frozen, network-disabled pass is
            // still the commit gate.
            let resolution_request = Self::dependency_install_request(
                &build_mount,
                &store_mount,
                dependency_staging_guest_path.clone(),
                &build_state_root,
                memory_mb,
                if requires_network {
                    NetworkPolicy::Allowed
                } else {
                    NetworkPolicy::Disabled
                },
                false,
                false,
                true,
                toolchain,
            );
            let resolution_span = tracing::debug_span!(
                "local_app_dependency_resolve",
                app_id = %app_id,
                network = ?resolution_request.network,
            );
            let resolution_result =
                Self::run_dependency_install_command(runtime.as_ref(), resolution_request)
                    .instrument(resolution_span)
                    .await;
            if let Err(error) = resolution_result {
                let _ = Self::remove_owned_path(&dependency_staging);
                return Err(error);
            }
            let lock_bytes = match std::fs::read(dependency_staging.join("pnpm-lock.yaml")) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(format!("read updated pnpm-lock.yaml: {error}"));
                }
            };
            if let Err(error) =
                validate_resolved_dependency_lock(&receipt.effective_package_json, &lock_bytes)
            {
                let _ = Self::remove_owned_path(&dependency_staging);
                return Err(error);
            }
            let lock_digest = format!("{:x}", Sha256::digest(&lock_bytes));
            let snapshot_root = self.dependency_snapshot_root(&lock_digest, toolchain_key);
            let snapshot_lock = self.dependency_snapshot_lock(&lock_digest).await;
            let lock_wait_span = tracing::debug_span!(
                "local_app_dependency_snapshot_lock_wait",
                app_id = %app_id,
                lock_digest = %lock_digest,
            );
            let _snapshot_guard = {
                let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_lock_wait");
                snapshot_lock.lock().instrument(lock_wait_span).await
            };
            let snapshot_ready =
                Self::dependency_snapshot_is_ready(&snapshot_root, &lock_digest, toolchain_key)?;
            if snapshot_ready {
                let snapshot_span = tracing::debug_span!(
                    "local_app_dependency_snapshot_materialize",
                    app_id = %app_id,
                    lock_digest = %lock_digest,
                    cache_hit = true,
                );
                let materialize_result = {
                    let _perf =
                        LocalAppPerfDiagnosticTimer::start("dependency_snapshot_materialize");
                    snapshot_span.in_scope(|| {
                        Self::materialize_dependency_snapshot(&snapshot_root, &dependency_staging)
                    })
                };
                if let Err(error) = materialize_result {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
            } else {
                if let Err(error) = Self::reset_dependency_staging_node_modules(&dependency_staging)
                {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
                let frozen_request = Self::dependency_install_request(
                    &build_mount,
                    &store_mount,
                    dependency_staging_guest_path,
                    &build_state_root,
                    memory_mb,
                    NetworkPolicy::Disabled,
                    true,
                    false,
                    true,
                    toolchain,
                );
                let install_span = tracing::debug_span!(
                    "local_app_dependency_frozen_install",
                    app_id = %app_id,
                    lock_digest = %lock_digest,
                    cache_hit = false,
                );
                let frozen_result =
                    Self::run_dependency_install_command(runtime.as_ref(), frozen_request)
                        .instrument(install_span)
                        .await;
                if let Err(error) = frozen_result {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
                if let Err(error) =
                    validate_dependency_lifecycle_scripts(&dependency_staging.join("node_modules"))
                {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
                let snapshot_span = tracing::debug_span!(
                    "local_app_dependency_snapshot_publish",
                    app_id = %app_id,
                    lock_digest = %lock_digest,
                    cache_hit = false,
                );
                {
                    let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_publish");
                    snapshot_span.in_scope(|| {
                        Self::publish_dependency_snapshot(
                            &dependency_staging.join("node_modules"),
                            &snapshot_root,
                            &lock_digest,
                            toolchain_key,
                        )
                    })?;
                }
            }
            /*
             * The snapshot lock remains held through staging promotion and
             * profile metadata publication. A second app can therefore
             * materialize only after the first has published a complete,
             * inventory-backed snapshot.
             */
            let commit_result: Result<(), String> = (|| {
                crate::mobile::local_apps_build::write_file(
                    &workspace,
                    crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
                    &receipt.requested_json,
                    true,
                )
                .map_err(|error| error.to_string())?;
                crate::mobile::local_apps_build::write_file(
                    &workspace,
                    crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
                    &receipt.effective_package_json,
                    true,
                )
                .map_err(|error| error.to_string())?;
                crate::mobile::local_apps_build::write_file(
                    &workspace,
                    crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL,
                    &lock_bytes,
                    true,
                )
                .map_err(|error| error.to_string())?;
                crate::mobile::local_apps_build::write_file(
                    &workspace,
                    "package.json",
                    &receipt.effective_package_json,
                    true,
                )
                .map_err(|error| error.to_string())?;
                crate::mobile::local_apps_build::write_file(
                    &workspace,
                    "pnpm-lock.yaml",
                    &lock_bytes,
                    true,
                )
                .map_err(|error| error.to_string())?;
                Ok(())
            })();
            if let Err(error) = commit_result {
                let _ = Self::remove_owned_path(&dependency_staging);
                return Err(error);
            }
            self.finalize_dependency_install(&layout, &dependency_staging, &lock_digest)
                .await?;
            drop(_snapshot_guard);
            service
                .complete_dependency_install_with_metadata(
                    &app_id,
                    Some(lock_digest),
                    Some(toolchain_key.to_string()),
                )
                .await
                .map_err(|error| error.to_string())?;
            let dependencies = service
                .dependency_record(&app_id)
                .await
                .map_err(|error| error.to_string())?;
            let builder = crate::mobile::local_apps_build::LocalAppBuilder {
                mobile_linux: self.mobile_linux(),
                host: self,
            };
            builder
                .build_workspace_locked(&layout, &dependencies)
                .await
                .map_err(|error| format!("dependency update production build failed: {error}"))?;
            crate::mobile::local_apps_build::validate_build_for_launch(&layout)
                .map_err(|error| format!("dependency update profile smoke failed: {error}"))?;
            self.rebind_active_mcp_catalog_to_current_build(&app_id, &layout)
                .await?;
            let mut committed_journal = recovery_journal.clone();
            committed_journal.status = DependencyUpdateRecoveryStatus::Committed;
            Self::write_dependency_update_recovery_journal(&layout, &committed_journal)?;
            Ok(json!({
                "ok": true,
                "app_id": app_id,
                "changes": receipt.summary,
                "dependencies": dependencies,
            }))
        }
        .await;
        match result {
            Ok(value) => {
                Self::discard_dependency_update_rollback(rollback);
                if let Err(error) = Self::remove_dependency_update_recovery_journal(&layout) {
                    tracing::warn!(
                        app_id = %app_id,
                        %error,
                        "dependency update committed but recovery journal cleanup was deferred"
                    );
                }
                self.consume_dependency_change_receipt(&app_id, &receipt_id)
                    .await;
                self.emit_current_verification_summary(&app_id, &layout)
                    .await;
                Ok(value)
            }
            Err(error) => {
                let rollback_error = match self
                    .restore_dependency_update_rollback(&service, &app_id, &layout, &rollback)
                    .await
                {
                    Ok(()) => {
                        let mut committed_journal = recovery_journal.clone();
                        committed_journal.status = DependencyUpdateRecoveryStatus::Committed;
                        Self::write_dependency_update_recovery_journal(&layout, &committed_journal)
                            .and_then(|_| {
                                Self::cleanup_dependency_update_recovery(
                                    &layout,
                                    &committed_journal,
                                )
                            })
                            .err()
                    }
                    Err(error) => Some(error),
                };
                self.release_dependency_change_receipt_claim(&app_id, &receipt_id)
                    .await;
                match rollback_error {
                    Some(rollback_error) => {
                        Err(format!("{error}; rollback failed: {rollback_error}"))
                    }
                    None => Err(error),
                }
            }
        }
    }

    async fn migrate_runtime_profile(&self, _input: Value) -> Result<Value, String> {
        Err("runtime profile migration is not available in this host build".into())
    }

    async fn prepare_shell_app(&self, record: local_apps::AppRecord) -> Result<(), String> {
        self.write_guided_contract_value(&record).await
    }

    async fn update_manifest(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let layout =
            AppLayout::new(self.root.clone(), app_id.clone()).map_err(|e| e.to_string())?;
        let mut manifest = local_apps::load_manifest(&layout).map_err(|e| e.to_string())?;
        if let Some(collections) = input.get("collections") {
            manifest.collections = serde_json::from_value(collections.clone())
                .map_err(|e| format!("invalid collections: {e}"))?;
        }
        if let Some(domains) = input.get("allowed_domains") {
            manifest.allowed_domains = serde_json::from_value(domains.clone())
                .map_err(|e| format!("invalid allowed_domains: {e}"))?;
        }
        if let Some(capabilities) = input.get("capabilities") {
            manifest.capabilities = serde_json::from_value(capabilities.clone())
                .map_err(|e| format!("invalid capabilities: {e}"))?;
        }
        // The scaffold is fixed at creation. The workspace on disk IS the
        // scaffold, so accepting a change here would leave the generated source
        // and the re-pinned infrastructure describing two different
        // applications, and the next build would write the other scaffold's
        // files over working code. Rejected loudly rather than ignored: an
        // agent that believes it just converted the app has to find out now,
        // not after a build silently reverts half its work.
        //
        // `manifest.surface` is otherwise carried through untouched by the
        // load-modify-save above, which is what keeps it stable.
        if input.get("surface").is_some() {
            return Err(
                "an app's surface is fixed when the app is created and cannot be changed; \
                 create a new app to build the other shape"
                    .into(),
            );
        }
        // The device context is host-derived, never taken from `input`: the
        // agent only ever sees the mobile runtime reminder, and that
        // reminder's `Device class: phone` is not an iOS form factor. Every
        // save re-stamps it so the record tracks the host the app is
        // actually being generated on.
        manifest.device_context = self.host_device_context();
        manifest.validate().map_err(|e| e.to_string())?;
        // Schema changes against live data go through the SAME preview +
        // destructive-approval gate the pipeline used — an agent declaring a
        // narrower schema cannot silently drop user rows.
        crate::mobile::local_apps_build::migrate_manifest_with_approval(self, &layout, &manifest)
            .await
            .map_err(|e| e.to_string())?;
        local_apps::save_manifest(&layout, &manifest).map_err(|e| e.to_string())?;
        if !manifest
            .capabilities
            .contains(&AppCapability::BackgroundSchedule)
        {
            for outcome in self
                .cancel_background_tasks_for_revoked_schedule(
                    &app_id,
                    "background scheduling capability was removed from the app manifest",
                )
                .await?
            {
                self.emit_background_task_changed(&outcome).await;
            }
        }
        Ok(serde_json::json!({
            "ok": true,
            "app_id": app_id,
            "collections": manifest.collections.len(),
            "allowed_domains": manifest.allowed_domains,
            "capabilities": manifest.capabilities,
            "device_context": manifest.device_context,
        }))
    }

    async fn query_data(&self, input: Value) -> Result<Value, String> {
        self.query_data_value(input).await
    }

    async fn mutate_data(&self, input: Value) -> Result<Value, String> {
        self.mutate_data_value(input, true, None).await
    }

    async fn capture_ui(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        // No `target`: an element crop is expressed as `rect` (see
        // `capture_ui_value`), which the native side DOES honour, so a
        // selector here would be a second, redundant way to say the same
        // thing — with no way to report which one won.
        let capture_value = self
            .qa_ui_request_value(&input, capture_ui_value(&input)?)
            .await?;
        let qa_event_id = input
            .get("qa_handle")
            .and_then(Value::as_str)
            .map(|_| self.request_id("qa-capture"));
        let ui_request_id = if qa_event_id.is_some() {
            self.request_id("qa-ui")
        } else {
            self.request_id("app-ui")
        };
        let result = self
            .request_ui(AppUiRequestDto {
                request_id: ui_request_id,
                app_id,
                action: AppUiActionKindDto::CaptureView,
                target: None,
                value: capture_value,
            })
            .await?;
        let result = self.qa_ui_response_value(&input, result).await?;
        self.validate_qa_request(&input).await?;
        if let Some(qa_event_id) = qa_event_id {
            let evidence = self
                .record_qa_observation(&input, "capture_ui", result.clone(), qa_event_id, None)
                .await?;
            if let Some(handle) = input.get("qa_handle").and_then(Value::as_str) {
                return Ok(authoring::qa_result_with_evidence_ids(
                    result,
                    handle,
                    authoring::qa_observation_id(&evidence),
                ));
            }
        }
        Ok(result)
    }

    async fn inspect_ui(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let inspect_value = self.qa_ui_request_value(&input, None).await?;
        let qa_event_id = input
            .get("qa_handle")
            .and_then(Value::as_str)
            .map(|_| self.request_id("qa-inspect"));
        let ui_request_id = if qa_event_id.is_some() {
            self.request_id("qa-ui")
        } else {
            self.request_id("app-ui")
        };
        let result = self
            .request_ui(AppUiRequestDto {
                request_id: ui_request_id,
                app_id,
                action: AppUiActionKindDto::Inspect,
                target: input
                    .get("selector")
                    .and_then(Value::as_str)
                    .map(|selector| AppUiTargetDto {
                        element_id: Some(selector.to_string()),
                        role: None,
                        name: None,
                    }),
                value: inspect_value,
            })
            .await?;
        let result = self.qa_ui_response_value(&input, result).await?;
        self.validate_qa_request(&input).await?;
        if let Some(qa_event_id) = qa_event_id {
            let evidence = self
                .record_qa_observation(&input, "inspect_ui", result.clone(), qa_event_id, None)
                .await?;
            if let Some(handle) = input.get("qa_handle").and_then(Value::as_str) {
                return Ok(authoring::qa_result_with_evidence_ids(
                    result,
                    handle,
                    authoring::qa_observation_id(&evidence),
                ));
            }
        }
        Ok(result)
    }

    async fn act_on_ui(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        self.authorize_capability(
            &app_id,
            AppCapability::UiControl,
            AppCapabilityKindDto::UiControl,
            "The agent requested permission to control this app's visible interface.",
        )
        .await?;
        let action = match required_string(&input, "action")? {
            "click" => AppUiActionKindDto::Click,
            "fill" => AppUiActionKindDto::Fill,
            "select" => AppUiActionKindDto::Select,
            "toggle" => AppUiActionKindDto::Toggle,
            "scroll" => AppUiActionKindDto::Scroll,
            "navigate" => AppUiActionKindDto::Navigate,
            "back" => AppUiActionKindDto::Back,
            "reload" => AppUiActionKindDto::Reload,
            "pointer" => AppUiActionKindDto::Pointer,
            "key" => AppUiActionKindDto::Key,
            _ => return Err("unsupported structured UI action".into()),
        };
        let target = normalize_ui_target(input.get("target"))?;
        let value = input.get("value").map(|value| match value {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        });
        let qa_action = self.begin_qa_action_with_guard(&input).await?;
        let qa_event_id = qa_action.as_ref().map(|(event_id, _)| event_id.clone());
        let _qa_guard = qa_action.map(|(_, guard)| guard);
        let action_value = match self.qa_ui_request_value(&input, value).await {
            Ok(value) => value,
            Err(error) => {
                if let Some(event_id) = qa_event_id.as_deref() {
                    self.end_qa_action(&app_id, event_id).await;
                }
                return Err(error);
            }
        };
        let ui_request_id = if qa_event_id.is_some() {
            self.request_id("qa-ui")
        } else {
            self.request_id("app-ui")
        };
        let result = self
            .request_ui(AppUiRequestDto {
                request_id: ui_request_id,
                app_id: app_id.clone(),
                action,
                target,
                value: action_value,
            })
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Some(event_id) = qa_event_id.as_deref() {
                    self.end_qa_action(&app_id, event_id).await;
                }
                return Err(error);
            }
        };
        let result = match self.qa_ui_response_value(&input, result).await {
            Ok(result) => result,
            Err(error) => {
                if let Some(event_id) = qa_event_id.as_deref() {
                    self.end_qa_action(&app_id, event_id).await;
                }
                return Err(error);
            }
        };
        if let Err(error) = self.validate_qa_request(&input).await {
            if let Some(event_id) = qa_event_id.as_deref() {
                self.end_qa_action(&app_id, event_id).await;
            }
            return Err(error);
        }
        if let Some(qa_event_id) = qa_event_id {
            // Close the window before publishing any evidence. A page write
            // racing after this point is an ordinary page mutation and cannot
            // be attributed to the completed action.
            let action = self.settle_qa_action(&app_id, &qa_event_id).await?;
            let action_evidence = match self
                .record_qa_observation(&input, "act_on_ui", result.clone(), qa_event_id, None)
                .await
            {
                Ok(evidence) => evidence,
                Err(error) => {
                    tracing::warn!(app_id = %app_id, %error, "QA action evidence attribution unavailable after native success");
                    Value::Null
                }
            };
            let bridge_evidence_ids = match self.commit_qa_action_mutations(&app_id, &action).await
            {
                Ok(ids) => ids,
                Err(error) => {
                    tracing::warn!(app_id = %app_id, %error, "QA bridge evidence attribution unavailable after native success");
                    Vec::new()
                }
            };
            let mut evidence_ids = bridge_evidence_ids;
            if let Some(evidence_id) = authoring::qa_observation_id(&action_evidence) {
                evidence_ids.push(evidence_id);
            }
            return Ok(authoring::qa_result_with_evidence_ids(
                result,
                required_string(&input, "qa_handle")?,
                evidence_ids,
            ));
        }
        Ok(result)
    }

    async fn restore_checkpoint(&self, input: Value) -> Result<Value, String> {
        self.restore_checkpoint_value(input).await
    }

    async fn read_app_events(&self, input: Value) -> Result<Value, String> {
        self.read_app_events_value(input).await
    }

    async fn read_agent_events(&self, input: Value) -> Result<Value, String> {
        self.read_agent_events_value(input).await
    }

    async fn background_schedule_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let layout = self.layout(&app_id)?;
        let manifest = local_apps::load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest
            .capabilities
            .contains(&AppCapability::BackgroundSchedule)
        {
            return Err("background scheduling is not declared in the app manifest".into());
        }
        self.authorize_capability(
            &app_id,
            AppCapability::BackgroundSchedule,
            AppCapabilityKindDto::BackgroundSchedule,
            "应用请求在系统后台按计划运行一个流程。",
        )
        .await?;
        let permissions =
            local_apps::load_permissions(&layout).map_err(|error| error.to_string())?;
        if !permissions.allows(AppCapability::BackgroundSchedule) {
            return Err("background scheduling requires durable approval".into());
        }
        let interval_ms = input
            .get("interval_ms")
            .or_else(|| input.get("intervalMs"))
            .and_then(Value::as_u64)
            .ok_or_else(|| "interval_ms must be an integer".to_string())?;
        if !(15 * 60 * 1_000..=30 * 24 * 60 * 60 * 1_000).contains(&interval_ms) {
            return Err("background interval must be between 15 minutes and 30 days".into());
        }
        let flow_value = input
            .get("flow")
            .cloned()
            .ok_or_else(|| "flow is required".to_string())?;
        let flow: local_apps::FlowDefinition = serde_json::from_value(flow_value)
            .map_err(|error| format!("invalid background flow: {error}"))?;
        let registry = local_apps::CapabilityRegistry::default();
        flow.validate(&registry)
            .map_err(|error| format!("invalid background flow: {error}"))?;
        for step in &flow.steps {
            if matches!(
                step.capability,
                local_apps::CapabilityId::BackgroundSchedule
            ) {
                return Err("background flows cannot schedule another background flow".into());
            }
            let descriptor = registry
                .get(step.capability)
                .ok_or_else(|| format!("unknown capability {}", step.capability.as_str()))?;
            if !local_apps::allowed_for_origin(
                local_apps::InvocationOrigin::SystemScheduler,
                descriptor,
            ) {
                return Err(format!(
                    "background flow cannot use interactive capability {}",
                    step.capability.as_str()
                ));
            }
            self.authorize_background_schedule_step(&app_id, step.capability, &step.input_json)
                .await?;
        }
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let layout = self.layout(&app_id)?;
        let _process_lock = self.acquire_background_process_lock(&app_id).await?;
        let _guard = self.background_task_writes.lock().await;
        let mut tasks = local_apps::background::load_tasks(&layout).map_err(|e| e.to_string())?;
        let mut journal =
            local_apps::background::load_journal(&layout).map_err(|e| e.to_string())?;
        if tasks.len() >= background_ops::MAX_BACKGROUND_TASKS {
            let terminal_index = tasks
                .iter()
                .enumerate()
                .filter(|(_, task)| {
                    matches!(
                        task.status,
                        local_apps::BackgroundTaskStatus::Succeeded
                            | local_apps::BackgroundTaskStatus::Failed
                            | local_apps::BackgroundTaskStatus::Cancelled
                    )
                })
                .min_by_key(|(_, task)| task.updated_at_ms)
                .map(|(index, _)| index);
            let Some(terminal_index) = terminal_index else {
                return Err(format!(
                    "an app may have at most {} active background tasks",
                    background_ops::MAX_BACKGROUND_TASKS
                ));
            };
            let removed = tasks.remove(terminal_index);
            journal.retain(|entry| entry.task_id != removed.task_id);
        }
        let task_id = loop {
            let candidate = self.request_id("background");
            if tasks.iter().all(|task| task.task_id != candidate) {
                break candidate;
            }
        };
        let now = now_ms();
        let task = local_apps::BackgroundTaskRecord {
            schema_version: local_apps::RUNTIME_CONTRACT_SCHEMA_VERSION,
            task_id: task_id.clone(),
            app_id: app_id.clone(),
            flow_id: flow.flow_id.clone(),
            flow: flow.clone(),
            trigger: local_apps::BackgroundTrigger::Schedule { interval_ms },
            status: local_apps::BackgroundTaskStatus::Scheduled,
            updated_at_ms: now,
        };
        tasks.push(task.clone());
        journal.retain(|entry| entry.task_id != task_id);
        journal.push(local_apps::BackgroundJournalEntry {
            task_id: task_id.clone(),
            flow_id: flow.flow_id,
            next_step_id: flow.steps.first().map(|step| step.step_id.clone()),
            next_run_at_ms: Some(now.saturating_add(interval_ms)),
            last_result_json: None,
            attempt: 0,
            last_error: None,
            updated_at_ms: now,
        });
        local_apps::background::save_state(&layout, &tasks, &journal).map_err(|e| e.to_string())?;
        drop(_guard);
        drop(_process_lock);
        self.emit_background_task_changed(&LocalAppBackgroundRunDto {
            app_id: app_id.clone(),
            task_id: task_id.clone(),
            status: "scheduled".into(),
            result_json: None,
            error: None,
            retryable: false,
        })
        .await;
        Ok(json!({"task": task, "scheduled": true, "scheduler": "host-journal"}))
    }

    async fn background_list_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let layout = self.background_management_layout(&app_id)?;
        let task_id = input.get("task_id").and_then(Value::as_str);
        let status = input
            .get("status")
            .and_then(Value::as_str)
            .map(parse_background_status)
            .transpose()?;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 100) as usize;
        let tasks = Self::background_task_summaries(&layout, task_id, status, limit)?;
        let count = tasks.len();
        Ok(json!({"app_id": app_id, "tasks": tasks, "count": count}))
    }

    async fn background_status_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let task_id = required_string(&input, "task_id")?.to_string();
        let value = self
            .background_list_value(json!({"app_id": app_id, "task_id": task_id, "limit": 1}))
            .await?;
        if value
            .get("tasks")
            .and_then(Value::as_array)
            .is_none_or(|tasks| tasks.is_empty())
        {
            return Err("background task was not found".into());
        }
        Ok(value)
    }

    async fn background_cancel_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let task_id = required_string(&input, "task_id")?.to_string();
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        self.background_management_layout(&app_id)?;
        let cancelled = self.cancel_background_task(&app_id, &task_id).await;
        self.emit_background_task_changed(&LocalAppBackgroundRunDto {
            app_id: app_id.clone(),
            task_id: task_id.clone(),
            status: if cancelled { "cancelled" } else { "unchanged" }.into(),
            result_json: None,
            error: None,
            retryable: false,
        })
        .await;
        Ok(json!({"app_id": app_id, "task_id": task_id, "cancelled": cancelled}))
    }

    async fn background_retry_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let task_id = required_string(&input, "task_id")?.to_string();
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        self.background_management_layout(&app_id)?;
        let retried = self.retry_background_task(&app_id, &task_id).await?;
        self.emit_background_task_changed(&LocalAppBackgroundRunDto {
            app_id: app_id.clone(),
            task_id: task_id.clone(),
            status: if retried { "scheduled" } else { "unchanged" }.into(),
            result_json: None,
            error: None,
            retryable: false,
        })
        .await;
        Ok(json!({"app_id": app_id, "task_id": task_id, "retried": retried}))
    }

    async fn agent_session_create(&self, input: Value) -> Result<Value, String> {
        self.agent_session_create_value(input).await
    }

    async fn agent_session_list(&self, input: Value) -> Result<Value, String> {
        self.agent_session_list_value(input).await
    }

    async fn agent_session_update(&self, input: Value) -> Result<Value, String> {
        self.agent_session_update_value(input).await
    }

    async fn agent_profile_propose(&self, input: Value) -> Result<Value, String> {
        self.agent_profile_propose_value(input).await
    }

    async fn flow_execute(&self, input: Value) -> Result<Value, String> {
        self.flow_execute_value(input).await
    }

    async fn execute_mcp_flow(&self, input: Value) -> Result<Value, String> {
        self.execute_mcp_flow_value(input).await
    }

    async fn background_schedule(&self, input: Value) -> Result<Value, String> {
        self.background_schedule_value(input).await
    }

    async fn background_list(&self, input: Value) -> Result<Value, String> {
        self.background_list_value(input).await
    }

    async fn background_status(&self, input: Value) -> Result<Value, String> {
        self.background_status_value(input).await
    }

    async fn background_cancel(&self, input: Value) -> Result<Value, String> {
        self.background_cancel_value(input).await
    }

    async fn background_retry(&self, input: Value) -> Result<Value, String> {
        self.background_retry_value(input).await
    }

    async fn scaffold_shell_app(&self, input: Value) -> Result<Value, String> {
        self.scaffold_shell_app_value(input).await
    }

    async fn emit_create_failure(&self, error: &local_apps::AppError) {
        LocalAppsHostBroker::emit_create_failure(self, error).await;
    }
}

/// One of `LocalAppScaffold`'s confirmed identity fields, trimmed.
///
/// ⚠️ There is deliberately no `is_empty()` check on the RESULT.
/// [`required_string`] already refuses a missing value, a non-string and a
/// whitespace-only string, so §C.1 step 2's "non-empty" half is enforced
/// there; re-testing it after `.trim()` here would be a branch that can never
/// be taken. This wrapper exists only to say which FIELD was wrong, because
/// `required_string`'s own message does not name the tool's vocabulary.
fn confirmed_field<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    required_string(input, key)
        .map(str::trim)
        .map_err(|_| format!("invalid_argument: {key} must be a non-empty string"))
}

/// Parse the optional `mcp_intent` staged alongside `name`/`brief` in
/// `LocalAppStageCreate`. Absent or `null` means the interview was not run
/// this call (staged evidence then records `None`, distinct from a staged
/// `Declined`); present-but-malformed is a caller error, not silently
/// dropped — `local_apps::AppMcpIntent`'s own `commit_scaffold`-side bounds
/// still apply later, but a caller that got the SHAPE wrong should hear
/// about it at staging time, not at scaffold time several steps later.
fn parse_staged_mcp_intent(input: &Value) -> Result<Option<local_apps::AppMcpIntent>, String> {
    match input.get("mcp_intent") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let intent: local_apps::AppMcpIntent = serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid_argument: mcp_intent is malformed: {error}"))?;
            if let local_apps::AppMcpIntent::Requested { capabilities } = &intent {
                if capabilities.is_empty() {
                    return Err(
                        "invalid_argument: mcp_intent Requested must name at least one capability"
                            .into(),
                    );
                }
                if capabilities.len() > local_apps::service::MAX_MCP_INTENT_CAPABILITIES {
                    return Err(format!(
                        "invalid_argument: mcp_intent names {} capabilities (limit {})",
                        capabilities.len(),
                        local_apps::service::MAX_MCP_INTENT_CAPABILITIES
                    ));
                }
                for capability in capabilities {
                    if capability.trim().is_empty() {
                        return Err(
                            "invalid_argument: mcp_intent capability name must not be blank".into(),
                        );
                    }
                    if capability.len() > local_apps::service::MAX_MCP_INTENT_CAPABILITY_NAME_BYTES
                    {
                        return Err(format!(
                            "invalid_argument: mcp_intent capability name is {} bytes (limit {})",
                            capability.len(),
                            local_apps::service::MAX_MCP_INTENT_CAPABILITY_NAME_BYTES
                        ));
                    }
                }
            }
            Ok(Some(intent))
        }
    }
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing non-empty {key:?}"))
}

fn format_binding_issue(issues: &[local_apps::GeneratedMcpIssue]) -> String {
    issues.first().map_or_else(
        || "binding_invalid: typed Flow binding is invalid".to_string(),
        |issue| format!("{}: {}", issue.code, issue.message),
    )
}

fn optional_json_string<T: Serialize>(value: Option<&T>) -> Result<Option<String>, String> {
    value
        .map(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
        .transpose()
}

fn lower_managed_mcp_status(status: AppMcpStatus) -> ManagedLocalAppMcpStatusDto {
    match status {
        AppMcpStatus::Disabled => ManagedLocalAppMcpStatusDto::Disabled,
        AppMcpStatus::NeedsSetup => ManagedLocalAppMcpStatusDto::NeedsSetup,
        AppMcpStatus::Authoring => ManagedLocalAppMcpStatusDto::Authoring,
        AppMcpStatus::Enabled => ManagedLocalAppMcpStatusDto::Enabled,
        AppMcpStatus::NeedsRevalidation => ManagedLocalAppMcpStatusDto::NeedsRevalidation,
        AppMcpStatus::Error => ManagedLocalAppMcpStatusDto::Error,
    }
}

fn tool_meta_resource_uri(definition: &platform_api::McpToolDefinitionDto) -> Option<String> {
    let meta = definition.meta.as_ref()?;
    meta.get("ui")
        .and_then(Value::as_object)
        .and_then(|ui| ui.get("resourceUri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            meta.get("openai/outputTemplate")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn resource_sha_for_app_uri(app_id: &str, uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("ui://local-app/")?;
    let mut parts = rest.split('/');
    let uri_app_id = parts.next()?;
    let resource_sha256 = parts.next()?;
    let file = parts.next()?;
    if parts.next().is_some()
        || uri_app_id != app_id
        || file != LOCAL_APP_WIDGET_FILE
        || resource_sha256.len() != 64
        || resource_sha256
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return None;
    }
    Some(resource_sha256.to_string())
}

fn validate_managed_mcp_widget_file(
    layout: &AppLayout,
    resource_sha256: &str,
    mime_type: &str,
) -> Result<(), String> {
    if mime_type != LOCAL_APP_WIDGET_MIME {
        return Err(format!(
            "widget_invalid: Local App MCP widget MIME must be {LOCAL_APP_WIDGET_MIME}"
        ));
    }
    let relative = layout
        .app_dir_rel()
        .join(local_apps::manifest::MCP_DIR)
        .join(LOCAL_APP_WIDGET_DIR)
        .join(format!("{resource_sha256}.html"));
    let body =
        platform_api::rooted_fs::read_to_string_limited(layout.root(), &relative, 4 * 1024 * 1024)
            .map_err(|error| format!("widget_invalid: {}: {error}", relative.display()))?;
    let actual = format!("{:x}", Sha256::digest(body.as_bytes()));
    if actual != resource_sha256 {
        return Err("widget_invalid: Local App MCP widget digest mismatch".into());
    }
    let lower = body.to_ascii_lowercase();
    for forbidden in [
        "src=\"http",
        "src='http",
        "href=\"http",
        "href='http",
        "window.openai",
    ] {
        if lower.contains(forbidden) {
            return Err(format!(
                "widget_invalid: Local App MCP widget contains forbidden token {forbidden:?}"
            ));
        }
    }
    Ok(())
}

fn managed_mcp_widget_resource(
    layout: &AppLayout,
    app_name: &str,
    catalog: &Value,
) -> Result<Option<(McpAppWidgetDto, mcp::registry::ManagedLocalAppResource)>, String> {
    let app_id = layout.app_id();
    if let Some(resources) = catalog.get("resources").and_then(Value::as_array) {
        for resource in resources {
            let Some(uri) = resource.get("uri").and_then(Value::as_str) else {
                continue;
            };
            let Some(resource_sha256) = resource_sha_for_app_uri(app_id, uri) else {
                continue;
            };
            let name = resource
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(app_name)
                .to_string();
            let description = resource
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mime_type = resource
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or(LOCAL_APP_WIDGET_MIME)
                .to_string();
            validate_managed_mcp_widget_file(layout, &resource_sha256, &mime_type)?;
            return Ok(Some((
                McpAppWidgetDto {
                    resource_uri: uri.to_string(),
                    mime_type: mime_type.clone(),
                    resource_sha256: resource_sha256.clone(),
                },
                mcp::registry::ManagedLocalAppResource {
                    uri: uri.to_string(),
                    name,
                    description,
                    mime_type: Some(mime_type),
                    meta: resource.get("_meta").cloned(),
                },
            )));
        }
    }

    let entries = catalog
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "catalog_invalid: active catalog tools are missing".to_string())?;
    for entry in entries {
        let definition: platform_api::McpToolDefinitionDto =
            serde_json::from_value(entry.get("definition").unwrap_or(entry).clone())
                .map_err(|_| "catalog_invalid: active tool definition is invalid".to_string())?;
        let Some(uri) = tool_meta_resource_uri(&definition) else {
            continue;
        };
        let Some(resource_sha256) = resource_sha_for_app_uri(app_id, &uri) else {
            continue;
        };
        let name = definition
            .title
            .clone()
            .or_else(|| definition.description.clone())
            .unwrap_or_else(|| format!("{app_name} widget"));
        validate_managed_mcp_widget_file(layout, &resource_sha256, LOCAL_APP_WIDGET_MIME)?;
        return Ok(Some((
            McpAppWidgetDto {
                resource_uri: uri.clone(),
                mime_type: LOCAL_APP_WIDGET_MIME.into(),
                resource_sha256: resource_sha256.clone(),
            },
            mcp::registry::ManagedLocalAppResource {
                uri,
                name,
                description: definition.description.clone(),
                mime_type: Some(LOCAL_APP_WIDGET_MIME.into()),
                meta: None,
            },
        )));
    }

    Ok(None)
}

fn mcp_tool_surface(
    definition: platform_api::McpToolDefinitionDto,
    flow: Value,
    ceiling: platform_api::McpPermissionCeiling,
) -> Result<LocalAppMcpToolSurfaceDto, String> {
    Ok(LocalAppMcpToolSurfaceDto {
        name: definition.name,
        title: definition.title,
        description: definition.description,
        input_schema_json: serde_json::to_string(&definition.input_schema)
            .map_err(|error| error.to_string())?,
        output_schema_json: optional_json_string(definition.output_schema.as_ref())?,
        annotations_json: optional_json_string(definition.annotations.as_ref())?,
        execution_json: optional_json_string(definition.execution.as_ref())?,
        visible_meta_json: optional_json_string(definition.meta.as_ref())?,
        semantic_flow_json: serde_json::to_string(&flow).map_err(|error| error.to_string())?,
        permission_ceiling: match ceiling {
            platform_api::McpPermissionCeiling::Allow => "allow",
            platform_api::McpPermissionCeiling::Ask => "ask",
            platform_api::McpPermissionCeiling::Deny => "deny",
        }
        .into(),
    })
}

fn mcp_tool_surfaces_from_catalog(
    catalog: &Value,
) -> Result<Vec<LocalAppMcpToolSurfaceDto>, String> {
    let entries = catalog
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "catalog_invalid: active catalog tools are missing".to_string())?;
    let mut tools = Vec::with_capacity(entries.len());
    for entry in entries {
        let definition: platform_api::McpToolDefinitionDto =
            serde_json::from_value(entry.get("definition").unwrap_or(entry).clone())
                .map_err(|_| "catalog_invalid: active tool definition is invalid".to_string())?;
        let flow = entry
            .get("flow")
            .cloned()
            .ok_or_else(|| "catalog_invalid: active tool Flow binding is missing".to_string())?;
        let ceiling = entry
            .get("ceiling")
            .and_then(Value::as_str)
            .and_then(platform_api::McpPermissionCeiling::from_policy_str)
            .ok_or_else(|| "catalog_invalid: active tool ceiling is invalid".to_string())?;
        tools.push(mcp_tool_surface(definition, flow, ceiling)?);
    }
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tools)
}

fn mcp_tool_surfaces_from_candidate(
    candidate: &PersistedMcpCandidate,
) -> Result<Vec<LocalAppMcpToolSurfaceDto>, String> {
    let mut tools = candidate
        .validated
        .tools
        .iter()
        .map(|tool| {
            mcp_tool_surface(
                tool.definition.clone(),
                serde_json::to_value(&tool.flow).map_err(|error| error.to_string())?,
                tool.ceiling,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tools)
}

fn changed_mcp_fields(
    before: &LocalAppMcpToolSurfaceDto,
    after: &LocalAppMcpToolSurfaceDto,
) -> Vec<LocalAppMcpToolFieldDto> {
    let mut fields = Vec::new();
    if before.title != after.title {
        fields.push(LocalAppMcpToolFieldDto::Title);
    }
    if before.description != after.description {
        fields.push(LocalAppMcpToolFieldDto::Description);
    }
    if before.input_schema_json != after.input_schema_json {
        fields.push(LocalAppMcpToolFieldDto::InputSchema);
    }
    if before.output_schema_json != after.output_schema_json {
        fields.push(LocalAppMcpToolFieldDto::OutputSchema);
    }
    if before.annotations_json != after.annotations_json {
        fields.push(LocalAppMcpToolFieldDto::Annotations);
    }
    if before.execution_json != after.execution_json {
        fields.push(LocalAppMcpToolFieldDto::Execution);
    }
    if before.visible_meta_json != after.visible_meta_json {
        fields.push(LocalAppMcpToolFieldDto::VisibleMeta);
    }
    if before.semantic_flow_json != after.semantic_flow_json {
        fields.push(LocalAppMcpToolFieldDto::SemanticFlow);
    }
    if before.permission_ceiling != after.permission_ceiling {
        fields.push(LocalAppMcpToolFieldDto::PermissionCeiling);
    }
    fields
}

fn mcp_tool_diffs(
    before: Vec<LocalAppMcpToolSurfaceDto>,
    after: Vec<LocalAppMcpToolSurfaceDto>,
) -> Vec<LocalAppMcpToolDiffDto> {
    let mut before = before
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let mut after = after
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let names = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut diffs = Vec::new();
    for name in names {
        match (before.remove(&name), after.remove(&name)) {
            (None, Some(after)) => diffs.push(LocalAppMcpToolDiffDto {
                kind: LocalAppMcpToolChangeKindDto::Added,
                name,
                before: None,
                after: Some(after),
                changed_fields: Vec::new(),
            }),
            (Some(before), None) => diffs.push(LocalAppMcpToolDiffDto {
                kind: LocalAppMcpToolChangeKindDto::Removed,
                name,
                before: Some(before),
                after: None,
                changed_fields: Vec::new(),
            }),
            (Some(before), Some(after)) if before != after => {
                let changed_fields = changed_mcp_fields(&before, &after);
                diffs.push(LocalAppMcpToolDiffDto {
                    kind: LocalAppMcpToolChangeKindDto::Changed,
                    name,
                    before: Some(before),
                    after: Some(after),
                    changed_fields,
                });
            }
            _ => {}
        }
    }
    diffs
}

fn parse_background_status(value: &str) -> Result<BackgroundTaskStatus, String> {
    match value {
        "scheduled" => Ok(BackgroundTaskStatus::Scheduled),
        "running" => Ok(BackgroundTaskStatus::Running),
        "waiting_for_system" => Ok(BackgroundTaskStatus::WaitingForSystem),
        "succeeded" => Ok(BackgroundTaskStatus::Succeeded),
        "failed" => Ok(BackgroundTaskStatus::Failed),
        "cancelled" => Ok(BackgroundTaskStatus::Cancelled),
        _ => Err(format!("unsupported background task status {value:?}")),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn raise_decision(decision: AppAuthorizationDecisionDto) -> PermissionDecision {
    match decision {
        AppAuthorizationDecisionDto::Deny => PermissionDecision::Deny,
        AppAuthorizationDecisionDto::AllowOnce => PermissionDecision::AllowOnce,
        AppAuthorizationDecisionDto::AllowSession => PermissionDecision::AllowSession,
        AppAuthorizationDecisionDto::AllowAlways => PermissionDecision::AlwaysAllow,
        _ => PermissionDecision::Deny,
    }
}

fn lower_runtime_profile_family(profile: AppRuntimeProfile) -> AppRuntimeProfileDto {
    match profile {
        AppRuntimeProfile::ReactDom => AppRuntimeProfileDto::ReactDom,
        AppRuntimeProfile::Canvas2d => AppRuntimeProfileDto::Canvas2d,
        AppRuntimeProfile::Three3d => AppRuntimeProfileDto::Three3d,
        AppRuntimeProfile::Phaser2d => AppRuntimeProfileDto::Phaser2d,
        AppRuntimeProfile::Babylon3d => AppRuntimeProfileDto::Babylon3d,
    }
}

fn lower_surface(surface: local_apps::AppSurface) -> AppSurfaceDto {
    match surface {
        local_apps::AppSurface::Dom => AppSurfaceDto::Dom,
        local_apps::AppSurface::Canvas => AppSurfaceDto::Canvas,
    }
}

fn normalize_query(input: &Value) -> Result<DataQuery, String> {
    if input.get("cursor").is_some() {
        return Err("query cursor is unsupported; use numeric offset".into());
    }
    let collection = required_string(input, "collection")?.to_string();
    let filters = input
        .get("filters")
        .cloned()
        .or_else(|| input.get("filter").map(|value| json!([value])))
        .unwrap_or_else(|| json!([]));
    let sort_key = input
        .get("sort_key")
        .or_else(|| input.get("sortKey"))
        .cloned()
        .unwrap_or(Value::Null);
    let sort_direction = input
        .get("sort_direction")
        .or_else(|| input.get("sortDirection"))
        .cloned()
        .unwrap_or_else(|| Value::String("ascending".into()));
    let limit = match input.get("limit") {
        None => 50,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "query limit must be an integer".to_string())?,
    };
    if !(1..=100).contains(&limit) {
        return Err("query limit must be between 1 and 100".into());
    }
    let offset = match input.get("offset") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "query offset must be a non-negative integer".to_string())?,
    };
    let filters = serde_json::from_value(filters)
        .map_err(|error| format!("invalid structured data query: {error}"))?;
    let (sort_key, sort_direction) = normalize_sort(input, sort_key, sort_direction)?;
    Ok(DataQuery {
        collection,
        filters,
        sort_key,
        sort_direction,
        limit: limit as u32,
        offset,
    })
}

fn normalize_sort(
    input: &Value,
    legacy_key: Value,
    legacy_direction: Value,
) -> Result<(Option<DataSortKey>, DataSortDirection), String> {
    let mut sort_key = normalize_sort_key_value(&legacy_key)?;
    let mut sort_direction = normalize_sort_direction_value(&legacy_direction)?;
    if let Some(sort) = input.get("sort") {
        let (public_key, public_direction) = normalize_public_sort(sort)?;
        if let Some(public_key) = public_key {
            sort_key = Some(public_key);
        }
        if let Some(public_direction) = public_direction {
            sort_direction = public_direction;
        }
    }
    Ok((sort_key, sort_direction))
}

fn normalize_public_sort(
    sort: &Value,
) -> Result<(Option<DataSortKey>, Option<DataSortDirection>), String> {
    match sort {
        Value::Null => Ok((None, None)),
        Value::String(_) => Ok((normalize_sort_key_value(sort)?, None)),
        Value::Object(object) => {
            let direction = object
                .get("direction")
                .or_else(|| object.get("sort_direction"))
                .or_else(|| object.get("sortDirection"))
                .map(normalize_sort_direction_value)
                .transpose()?;
            let key = if let Some(key) = object
                .get("key")
                .or_else(|| object.get("sort_key"))
                .or_else(|| object.get("sortKey"))
            {
                normalize_sort_key_value(key)?
            } else if object.contains_key("kind")
                || object.contains_key("field_id")
                || object.contains_key("fieldId")
            {
                Some(normalize_sort_key_object(object)?)
            } else {
                None
            };
            Ok((key, direction))
        }
        _ => Err("query sort must be a string, object, or null".into()),
    }
}

fn normalize_sort_key_value(value: &Value) -> Result<Option<DataSortKey>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(normalize_sort_key_string(value)?)),
        Value::Object(object) => Ok(Some(normalize_sort_key_object(object)?)),
        _ => Err("query sort key must be a string, object, or null".into()),
    }
}

fn normalize_sort_key_object(object: &Map<String, Value>) -> Result<DataSortKey, String> {
    if let Some(field_id) = object
        .get("field_id")
        .or_else(|| object.get("fieldId"))
        .and_then(Value::as_str)
    {
        return Ok(DataSortKey::Field(field_id.to_string()));
    }
    let Some(kind) = object.get("kind").and_then(Value::as_str) else {
        return Err("query sort object must include key/kind or field_id".into());
    };
    match normalize_sort_alias(kind).as_str() {
        "field" => {
            let field_id = object
                .get("field_id")
                .or_else(|| object.get("fieldId"))
                .and_then(Value::as_str)
                .ok_or_else(|| "field sort requires field_id".to_string())?;
            Ok(DataSortKey::Field(field_id.to_string()))
        }
        "record_id" => Ok(DataSortKey::RecordId),
        "created_at" => Ok(DataSortKey::CreatedAt),
        "updated_at" => Ok(DataSortKey::UpdatedAt),
        "revision" => Ok(DataSortKey::Revision),
        other => Err(format!("unsupported sort kind {other:?}")),
    }
}

fn normalize_sort_key_string(value: &str) -> Result<DataSortKey, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("query sort key must not be empty".into());
    }
    Ok(match normalize_sort_alias(value).as_str() {
        "record_id" => DataSortKey::RecordId,
        "created_at" => DataSortKey::CreatedAt,
        "updated_at" => DataSortKey::UpdatedAt,
        "revision" => DataSortKey::Revision,
        _ => DataSortKey::Field(value.to_string()),
    })
}

fn normalize_sort_alias(value: &str) -> String {
    value
        .trim()
        .replace('-', "_")
        .chars()
        .fold(String::new(), |mut normalized, ch| {
            if ch.is_uppercase() && !normalized.is_empty() {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
            normalized
        })
}

fn normalize_sort_direction_value(value: &Value) -> Result<DataSortDirection, String> {
    let Some(value) = value.as_str() else {
        return Err("query sort direction must be a string".into());
    };
    match normalize_sort_alias(value).as_str() {
        "ascending" | "asc" => Ok(DataSortDirection::Ascending),
        "descending" | "desc" => Ok(DataSortDirection::Descending),
        other => Err(format!("unsupported sort direction {other:?}")),
    }
}

fn normalize_ui_target(target: Option<&Value>) -> Result<Option<AppUiTargetDto>, String> {
    let Some(target) = target else {
        return Ok(None);
    };
    match target {
        Value::Null => Ok(None),
        Value::String(target) => Ok(Some(AppUiTargetDto {
            element_id: Some(target.to_string()),
            role: None,
            name: None,
        })),
        Value::Object(object) => {
            let target = AppUiTargetDto {
                element_id: object
                    .get("element_id")
                    .or_else(|| object.get("elementId"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                role: object
                    .get("role")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                name: object
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            };
            if target.element_id.is_none() && target.role.is_none() && target.name.is_none() {
                return Err("UI target object must include element_id, role, or name".into());
            }
            Ok(Some(target))
        }
        _ => Err("UI target must be a string, object, or null".into()),
    }
}

fn manifest_migration_reason(preview: &DataMigrationPreview) -> String {
    let from = preview
        .from_manifest_hash
        .as_deref()
        .map(short_hash)
        .unwrap_or("none");
    let reasons = preview.reasons.join("; ");
    format!(
        "Approve destructive app data migration for this exact migration attempt only. Current manifest: {from}; proposed manifest: {}. Effects: {reasons}",
        short_hash(&preview.to_manifest_hash)
    )
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

async fn read_limited_stream<S, C, E>(
    mut stream: S,
    limit: usize,
    read_error_context: &str,
    limit_error: &str,
) -> Result<Vec<u8>, String>
where
    S: futures_util::stream::Stream<Item = Result<C, E>> + Unpin,
    C: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut body = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("{read_error_context}: {error}"))?;
        let chunk = chunk.as_ref();
        total = total.saturating_add(chunk.len());
        if total > limit {
            return Err(limit_error.to_string());
        }
        body.extend_from_slice(chunk);
    }
    Ok(body)
}

fn normalize_mutations(input: &Value) -> Result<Vec<DataMutation>, String> {
    let collection = required_string(input, "collection")?;
    let operations = input
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| "mutations require an operations array".to_string())?;
    let mut normalized = Vec::with_capacity(operations.len());
    for operation in operations {
        let mut operation = operation
            .as_object()
            .cloned()
            .ok_or_else(|| "each mutation must be an object".to_string())?;
        operation.insert("collection".into(), Value::String(collection.to_string()));
        normalized.push(
            serde_json::from_value(Value::Object(operation))
                .map_err(|error| format!("invalid structured mutation: {error}"))?,
        );
    }
    Ok(normalized)
}

/// Bind the app's loopback listener: the port it was already given if it has
/// one, otherwise a fresh port derived from its id.
///
/// WHY there is no fallback when `assigned` is taken.  An app's port is
/// PERMANENT by design one layer down: `AppState::set_runtime`
/// (`local-apps/src/state.rs`, "a port is NEVER reassigned — `IndexedDB` origin
/// stability") rejects any request that carries a different port than the one
/// already recorded, because the WebView loads the app from
/// `http://127.0.0.1:<port>` and every store the app owns client-side
/// (`IndexedDB`, `localStorage`, cookies, service-worker registration) is keyed
/// by that ORIGIN — a port that moved between starts would silently orphan the
/// app's own data.  Handing back a different port here would therefore not
/// rescue the start: `update_runtime_record` two calls later refuses the
/// reassignment and the start fails anyway, one message further along, with a
/// listener bound to a port nothing will ever use.
/// `the_pinned_app_port_can_never_be_reassigned` pins that invariant so this
/// reasoning goes red rather than stale if the pin is ever lifted.
///
/// WHY the window is 20000..32000.  A port that can never be reassigned must be
/// drawn from a range nobody else is ever GIVEN, so the window sits below every
/// shipped platform's ephemeral floor — Linux/Android
/// `net.ipv4.ip_local_port_range` starts at 32768, iOS/macOS
/// `net.inet.ip.portrange.first` at 49152.  Below those floors the kernel can
/// never hand an app's permanent port to some other process's socket while the
/// app is stopped.  The previous window (30000..50000) sat inside Android's
/// ephemeral range across 86% of its span and inside iOS's across its top 848
/// ports, which made the OS itself the likeliest squatter of a port that, by
/// design, the app can never give up.
///
/// WHY a sibling app's pin also excludes a candidate.  The window slot is a
/// hash of the app id, so two ids in ONE profile can derive the same slot —
/// `6b4cb242` and `c3baea9e` both derive 30809, and the birthday rate over
/// 12000 slots is ~1.3% at 20 apps.  A bind probe cannot see that collision:
/// a pin outlives the runtime that made it (`stop_runtime` re-passes
/// `runtime.port`), so a STOPPED sibling's permanent port probes free and
/// would be pinned a second time.  After that neither app can start while the
/// other runs — `set_runtime` refuses to move either — and on Android the two
/// share one `http://127.0.0.1:<port>` origin, hence one `localStorage` /
/// `IndexedDB` store, because the Android WebView is built on the default
/// profile with no per-app data store (iOS partitions by app id and is not
/// exposed to that half).  `sibling_pinned_ports` is therefore consulted
/// alongside the probe.  It PREVENTS new collisions only: a pair that already
/// collided is permanent on both sides, so the `assigned` branch can do
/// nothing but name the sibling instead of reporting a bare bind failure.
///
/// WHY a lease is taken as well.  A pin only reaches the records after the
/// choice — see [`PortLeases`] — so `sibling_pins` cannot describe a sibling
/// that is choosing right now.  Choice and reservation are therefore ONE step
/// here: the lease is taken before the probe, which is what makes a candidate
/// visible to every concurrent allocator from the instant it is picked.  The
/// returned guard belongs to the CALLER, which must hold it until the port is
/// persisted and then `commit` it.
///
/// WHY the pins are then read a SECOND time, from `service`, for the candidate
/// the lease was just taken on.  `sibling_pins` is a SNAPSHOT the caller read
/// before this call; a sibling can persist its pin and drop its lease in the
/// interval between that read and the take, and a candidate caught mid-hand-off
/// like that appears in neither half of "pins UNION leases" (see
/// [`PortLeases`]).  Re-reading after the take is what removes the interval:
/// `PortLease::commit` runs only after the persist, so once we hold the lease
/// on a port, a sibling that could have owned it either still holds its own
/// lease — and then our take returned `None` and we never got here — or has
/// already made its pin readable.  A sibling cannot newly take the port either,
/// because we hold it.  The re-read costs one `AppService` state lock per
/// candidate actually leased, which is one per start in the ordinary case.
///
/// The `assigned` branch takes no lease and needs no re-read: an assigned port
/// is by definition already persisted, so every sibling's pin snapshot has it.
async fn bind_stable_loopback(
    app_id: &str,
    assigned: Option<u16>,
    sibling_pins: &[(String, u16)],
    leases: &PortLeases,
    service: &AppService,
) -> Result<(TcpListener, u16, Option<PortLease>), String> {
    if let Some(port) = assigned {
        return match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => Ok((listener, port, None)),
            Err(error) => Err(
                match sibling_pins.iter().find(|(_, pinned)| *pinned == port) {
                    Some((sibling, _)) => format!(
                        "stable app port {port} is unavailable: app {sibling} is pinned to the same port and is holding it. \
                         A pinned port can never be reassigned, so only one of the two apps can run; \
                         recreate one of them to mint a fresh port ({error})"
                    ),
                    None => format!("stable app port {port} is unavailable: {error}"),
                },
            ),
        };
    }
    let first = derived_window_slot(app_id);
    for offset in 0..256u16 {
        let port = APP_PORT_WINDOW_FIRST + (first + offset) % APP_PORT_WINDOW_LEN;
        // A sibling's pin outlives its runtime, so a candidate that binds
        // cleanly can still be owned forever by an app that is merely stopped.
        if sibling_pins.iter().any(|(_, pinned)| *pinned == port) {
            continue;
        }
        // Taken BEFORE the probe, so a sibling that reaches this candidate
        // finds it occupied even though nothing is persisted and — on the full
        // runtime, once the probe below is released — nothing is bound either.
        // The lock is dropped by `take` itself and never spans the await.
        let Some(lease) = PortLease::take(leases, app_id, port) else {
            continue;
        };
        // Now that the candidate cannot move again, ask the records once more.
        // `sibling_pins` was read before this call and a sibling's pin may have
        // landed since; because a lease outlives its own persist, holding this
        // one makes the answer stable rather than merely fresher.  Order is the
        // point — a re-read BEFORE the take would reproduce the same interval
        // it is here to remove.
        if sibling_pinned_ports(service, app_id)
            .await
            .iter()
            .any(|(_, pinned)| *pinned == port)
        {
            drop(lease);
            continue;
        }
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port, Some(lease)));
        }
        // Refused by the kernel: hand the candidate straight back rather than
        // holding it for the rest of this scan.
        drop(lease);
    }
    Err("no stable loopback port is available for the app".into())
}

/// Offset into the derived window an app's FIRST port candidate sits at.
///
/// Extracted so the collision regression probe uses the production derivation
/// rather than a copy of it — a copy would keep asserting itself after the
/// real derivation moved.
fn derived_window_slot(app_id: &str) -> u16 {
    let hash = app_id.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(16_777_619) ^ u32::from(byte)
    });
    u16::try_from(hash % u32::from(APP_PORT_WINDOW_LEN)).unwrap_or(0)
}

/// Ports every OTHER app in this profile has already pinned, each paired with
/// its owner.
///
/// Read on demand rather than kept as a registry: the records ARE the
/// registry, and a cached copy would be one more thing to invalidate on
/// create/delete. The service returns the record/runtime pair from one
/// in-memory state-lock pass.
///
/// Twice per start in the ordinary case, not once — the snapshot the caller
/// reads before `bind_stable_loopback` cannot be trusted to still be true when
/// a candidate is leased, so the leased candidate is re-checked against a fresh
/// read.  Every result is a snapshot; only one taken while the port in question
/// is leased says anything durable about it.
async fn sibling_pinned_ports(service: &AppService, app_id: &str) -> Vec<(String, u16)> {
    service.pinned_runtime_ports_except(app_id).await
}

/// Returns `None` on the requested shutdown, and `Some(detail)` when the
/// listener has stopped being usable at all — the caller must then retire the
/// runtime entry, because a static handle has nothing else to notice it.
async fn run_static_server(
    listener: TcpListener,
    root: PathBuf,
    mut shutdown: oneshot::Receiver<()>,
) -> Option<String> {
    let mut consecutive_errors = 0u32;
    let request_slots = Arc::new(Semaphore::new(STATIC_REQUEST_CONCURRENCY));
    loop {
        let slot = tokio::select! {
            _ = &mut shutdown => return None,
            acquired = Arc::clone(&request_slots).acquire_owned() => match acquired {
                Ok(slot) => slot,
                Err(_) => return Some("static app server request limiter closed".into()),
            },
        };
        let accepted = tokio::select! {
            _ = &mut shutdown => return None,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                consecutive_errors = 0;
                let root = root.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let _ = serve_static_request(stream, &root).await;
                });
            }
            Err(error) => {
                drop(slot);
                // ECONNABORTED / EMFILE / EINTR describe ONE would-be
                // connection, not the listener, so a single error must not
                // retire the loop.  A listener whose runtime's I/O driver
                // was dropped fails EVERY poll though, and retrying that
                // forever is a permanent busy loop behind an entry that
                // still reports `running`.
                consecutive_errors += 1;
                if consecutive_errors >= STATIC_ACCEPT_ERROR_LIMIT {
                    return Some(format!(
                        "static app server stopped accepting connections after {consecutive_errors} consecutive failures: {error}"
                    ));
                }
                sleep(STATIC_ACCEPT_RETRY).await;
            }
        }
    }
}

/// Drop the entry this dead server owns and fail the record, so the next start
/// is legal instead of short-circuiting on a stale `Running`.
async fn reconcile_static_runtime_exit(
    runtimes: Arc<Mutex<HashMap<String, RuntimeEntry>>>,
    service: Arc<AppService>,
    app_id: String,
    generation: u64,
    publication_cell: RuntimePublicationCell,
    detail: String,
    broker: std::sync::Weak<LocalAppsHostBroker>,
) {
    let removed = {
        let mut runtimes = runtimes.lock().await;
        let should_remove = runtimes.get(&app_id).is_some_and(|entry| {
            entry.generation == generation
                && matches!(
                    entry.state,
                    RuntimeEntryState::Running {
                        handle: RuntimeHandle::Static { .. }
                    }
                )
        });
        if should_remove {
            if let Ok(mut identity) = publication_cell.write() {
                *identity = None;
            }
            runtimes.remove(&app_id);
        }
        should_remove
    };
    if !removed {
        return;
    }
    // The listener retired on its own: reclaim what the runtime owned before
    // recording the stop, so a page that was mid-recording does not leave the
    // audio session held by nothing.
    if let Some(broker) = broker.upgrade() {
        broker
            .release_app_runtime_state(&app_id, Some(generation))
            .await;
    }
    if let Ok(record) = service.runtime_record(&app_id).await {
        let _ = service
            .update_runtime_record(
                &app_id,
                AppRuntimeState::Failed,
                record.port,
                record.pid,
                Some(detail),
            )
            .await;
    }
}

async fn serve_static_request(mut stream: TcpStream, root: &Path) -> Result<(), std::io::Error> {
    let mut request = vec![0u8; MAX_HTTP_REQUEST_BYTES];
    let count = stream.read(&mut request).await?;
    request.truncate(count);
    let request_text = String::from_utf8_lossy(&request);
    let line = request_text.lines().next().unwrap_or_default().to_string();
    let if_none_match = request_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
        })
        .map(|(_, value)| value.trim().to_string());
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let raw_path = parts.next().unwrap_or_default();
    if !matches!(method, "GET" | "HEAD") {
        return write_http(
            &mut stream,
            405,
            "text/plain",
            b"method not allowed",
            method == "HEAD",
        )
        .await;
    }
    let Some(relative) = safe_static_path(raw_path) else {
        return write_http(
            &mut stream,
            400,
            "text/plain",
            b"bad path",
            method == "HEAD",
        )
        .await;
    };
    let mut path = root.join(relative);
    if path.is_dir() {
        path.push("index.html");
    }
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            return write_http(
                &mut stream,
                404,
                "text/plain",
                b"not found",
                method == "HEAD",
            )
            .await;
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_STATIC_ASSET_BYTES => metadata,
        _ => {
            return write_http(
                &mut stream,
                404,
                "text/plain",
                b"not found",
                method == "HEAD",
            )
            .await;
        }
    };
    let served_relative = path.strip_prefix(root).unwrap_or(&path);
    let etag = static_etag(&path, &metadata);
    if if_none_match
        .as_deref()
        .is_some_and(|header| etag_matches(header, &etag))
    {
        return write_not_modified(&mut stream, &etag, static_cache_control(served_relative)).await;
    }
    write_static_file(
        &mut stream,
        file,
        metadata.len(),
        content_type(&path),
        static_cache_control(served_relative),
        &etag,
        method == "HEAD",
    )
    .await
}

fn static_cache_control(path: &Path) -> &'static str {
    if is_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else if path.file_name().and_then(|name| name.to_str()) == Some("index.html") {
        "no-cache"
    } else {
        "no-store"
    }
}

fn is_hashed_asset(path: &Path) -> bool {
    let mut components = path.components();
    if !matches!(
        components.next(),
        Some(std::path::Component::Normal(component)) if component == "assets"
    ) {
        return false;
    }
    let Some(file_name) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(hash) = file_name.rsplit('-').next() else {
        return false;
    };
    hash.len() >= 8 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn write_static_file(
    stream: &mut TcpStream,
    mut file: tokio::fs::File,
    content_length: u64,
    content_type: &str,
    cache_control: &str,
    etag: &str,
    head: bool,
) -> Result<(), std::io::Error> {
    let header =
        static_response_header(200, content_type, content_length, cache_control, Some(etag));
    stream.write_all(header.as_bytes()).await?;
    if !head {
        let mut buffer = vec![0u8; STATIC_ASSET_CHUNK_BYTES];
        loop {
            let count = file.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            stream.write_all(&buffer[..count]).await?;
        }
    }
    stream.shutdown().await
}

fn etag_matches(header: &str, current: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == current
    })
}

async fn write_not_modified(
    stream: &mut TcpStream,
    etag: &str,
    cache_control: &str,
) -> Result<(), std::io::Error> {
    let header = static_response_header(304, "text/plain", 0, cache_control, Some(etag));
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await
}

fn safe_static_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.split(['?', '#']).next()?;
    if !raw.starts_with('/') || raw.contains('%') || raw.contains('\\') {
        return None;
    }
    let relative = raw.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head: bool,
) -> Result<(), std::io::Error> {
    let header = static_response_header(status, content_type, body.len() as u64, "no-store", None);
    stream.write_all(header.as_bytes()).await?;
    if !head {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

fn static_response_header(
    status: u16,
    content_type: &str,
    content_length: u64,
    cache_control: &str,
    etag: Option<&str>,
) -> String {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let etag_header = etag
        .map(|value| format!("ETag: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {content_length}\r\nContent-Type: {content_type}\r\nContent-Security-Policy: {LOCAL_APP_CONTENT_SECURITY_POLICY}\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nCache-Control: {cache_control}\r\n{etag_header}Connection: close\r\n\r\n"
    )
}

fn static_etag(path: &Path, metadata: &std::fs::Metadata) -> String {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let path_digest = Sha256::digest(path.to_string_lossy().as_bytes());
    format!("\"{}-{}-{:x}\"", modified_ns, metadata.len(), path_digest)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        // `instantiateStreaming` REQUIRES this exact type and this server sends
        // `X-Content-Type-Options: nosniff`, so serving a .wasm as
        // application/octet-stream fails the streaming path outright — with a
        // MIME complaint that reads nothing like the CSP refusal it is not.
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn validate_dependency_tree(root: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("inspect dependency tree {}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "dependency tree contains a symlink: {}",
            root.display()
        ));
    }
    if metadata.is_file() {
        return Ok(());
    }
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("resolve dependency tree {}: {error}", root.display()))?;
    validate_dependency_entry(root, &canonical_root)?;
    validate_dependency_lifecycle_scripts(root)
}

const FORBIDDEN_DEPENDENCY_LIFECYCLE_SCRIPTS: [&str; 4] =
    ["preinstall", "install", "postinstall", "prepare"];

const TRUSTED_TOOLCHAIN_NATIVE_BINDINGS: &[(&str, &str, &str)] = &[
    (
        "@rolldown/binding-linux-arm64-musl",
        "1.2.6",
        "rolldown-binding.linux-arm64-musl.node",
    ),
    (
        "@rolldown/binding-linux-x64-musl",
        "1.2.6",
        "rolldown-binding.linux-x64-musl.node",
    ),
    (
        "@rolldown/binding-linux-arm64-musl",
        "1.2.9",
        "rolldown-binding.linux-arm64-musl.node",
    ),
    (
        "@rolldown/binding-linux-x64-musl",
        "1.2.9",
        "rolldown-binding.linux-x64-musl.node",
    ),
    (
        "@rollup/rollup-linux-arm64-musl",
        "4.44.0",
        "rollup.linux-arm64-musl.node",
    ),
    (
        "@rollup/rollup-linux-x64-musl",
        "4.44.0",
        "rollup.linux-x64-musl.node",
    ),
    (
        "lightningcss-linux-arm64-musl",
        "1.33.0",
        "lightningcss.linux-arm64-musl.node",
    ),
    (
        "lightningcss-linux-x64-musl",
        "1.33.0",
        "lightningcss.linux-x64-musl.node",
    ),
];

const TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS: &[(&str, &str, &[&str])] = &[
    ("@modelcontextprotocol/ext-apps", "2.0.0", &["prepare"]),
    ("balanced-match", "4.0.4", &["prepare"]),
    ("brace-expansion", "5.0.9", &["prepare"]),
    ("brace-expansion", "5.0.12", &["prepare"]),
    ("dom-serializer", "2.0.0", &["prepare"]),
    ("domelementtype", "2.3.0", &["prepare"]),
    ("domhandler", "5.0.3", &["prepare"]),
    ("domutils", "3.2.2", &["prepare"]),
    ("entities", "4.5.0", &["prepare"]),
    ("eventsource", "3.0.7", &["prepare"]),
    ("html-dom-parser", "5.1.8", &["prepare"]),
    ("html-react-parser", "5.2.17", &["prepare"]),
    ("htmlparser2", "10.1.0", &["prepare"]),
    ("inline-style-parser", "0.2.7", &["prepare"]),
    ("lightningcss", "1.33.0", &["prepare"]),
    ("minimatch", "10.2.6", &["prepare"]),
    ("style-to-js", "1.1.21", &["prepare"]),
    ("style-to-object", "1.0.14", &["prepare"]),
    ("vite-plugin-singlefile", "2.3.3", &["prepare"]),
];

fn trusted_toolchain_lifecycle_scripts(
    package: &str,
    version: &str,
) -> Option<&'static [&'static str]> {
    TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS.iter().find_map(
        |(trusted_package, trusted_version, scripts)| {
            (*trusted_package == package && *trusted_version == version).then_some(*scripts)
        },
    )
}

fn dependency_package_path(package: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for part in package.split('/') {
        path.push(part);
    }
    path
}

fn validate_trusted_dependency_manifest(
    dependency_root: &Path,
    package: &str,
    version: &str,
) -> Result<(), String> {
    let manifest_path = dependency_root
        .join(dependency_package_path(package))
        .join("package.json");
    let metadata = std::fs::symlink_metadata(&manifest_path).map_err(|error| {
        format!(
            "inspect dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "dependency package manifest must be a regular file: {}",
            manifest_path.display()
        ));
    }
    let bytes = std::fs::read(&manifest_path).map_err(|error| {
        format!(
            "read dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "parse dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    if manifest.get("name").and_then(Value::as_str) != Some(package)
        || manifest.get("version").and_then(Value::as_str) != Some(version)
    {
        return Err(format!(
            "dependency package manifest {} does not match trusted package {}@{}",
            manifest_path.display(),
            package,
            version
        ));
    }
    Ok(())
}

fn trusted_dependency_lifecycle_script_path(
    dependency_root: &Path,
    manifest_path: &Path,
    package: &str,
    version: &str,
    script: &str,
) -> Result<bool, String> {
    let canonical_manifest = std::fs::canonicalize(manifest_path).map_err(|error| {
        format!(
            "canonicalize dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let relative = canonical_manifest
        .strip_prefix(dependency_root)
        .map_err(|_| {
            format!(
                "dependency package manifest {} is outside {}",
                canonical_manifest.display(),
                dependency_root.display()
            )
        })?;
    let expected = dependency_package_path(package).join("package.json");
    if relative != expected {
        return Ok(false);
    }
    validate_trusted_dependency_manifest(dependency_root, package, version)?;
    Ok(trusted_toolchain_lifecycle_scripts(package, version)
        .is_some_and(|allowed| allowed.contains(&script)))
}

fn trusted_dependency_native_binding_path(
    path: &Path,
    dependency_root: &Path,
) -> Result<bool, String> {
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "canonicalize dependency tree entry {}: {error}",
            path.display()
        )
    })?;
    let relative = canonical_path.strip_prefix(dependency_root).map_err(|_| {
        format!(
            "dependency tree entry {} is outside {}",
            canonical_path.display(),
            dependency_root.display()
        )
    })?;
    let mut rejected_manifest = None;
    for (package, version, file_name) in TRUSTED_TOOLCHAIN_NATIVE_BINDINGS {
        let expected = dependency_package_path(package).join(file_name);
        if relative == expected {
            match validate_trusted_dependency_manifest(dependency_root, package, version) {
                Ok(()) => return Ok(true),
                Err(error) => rejected_manifest = Some(error),
            }
        }
    }
    match rejected_manifest {
        Some(error) => Err(error),
        None => Ok(false),
    }
}

/// Reject package lifecycle hooks from a resolved dependency tree. The
/// resolver runs with scripts disabled, but retaining a hook in the snapshot
/// would let a later package-manager invocation execute it. Only explicitly
/// reviewed fixed-toolchain metadata is exempted.
fn validate_dependency_lifecycle_scripts(root: &Path) -> Result<(), String> {
    let dependency_root = root
        .canonicalize()
        .map_err(|error| format!("canonicalize dependency tree {}: {error}", root.display()))?;
    validate_dependency_lifecycle_scripts_from_root(&dependency_root, &dependency_root)
}

fn validate_dependency_lifecycle_scripts_from_root(
    dependency_root: &Path,
    current: &Path,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(current)
        .map_err(|error| format!("inspect dependency tree {}: {error}", current.display()))?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dependency tree entry is not regular: {}",
            current.display()
        ));
    }
    for entry in std::fs::read_dir(current)
        .map_err(|error| format!("read dependency tree {}: {error}", current.display()))?
    {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect dependency tree {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            validate_dependency_lifecycle_scripts_from_root(dependency_root, &path)?;
            continue;
        }
        if !metadata.is_file() || !installed_package_manifest(&path) {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "read dependency package manifest {}: {error}",
                path.display()
            )
        })?;
        let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "parse dependency package manifest {}: {error}",
                path.display()
            )
        })?;
        let package = manifest
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("<unnamed>");
        let Some(scripts) = manifest.get("scripts").and_then(Value::as_object) else {
            continue;
        };
        let version = manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for script in FORBIDDEN_DEPENDENCY_LIFECYCLE_SCRIPTS {
            if scripts.contains_key(script) {
                if trusted_dependency_lifecycle_script_path(
                    dependency_root,
                    &path,
                    package,
                    version,
                    script,
                )? {
                    continue;
                }
                return Err(format!(
                    "dependency package {package} declares forbidden lifecycle script {script} in {}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn validate_dependency_entry(path: &Path, canonical_root: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect dependency tree {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        // A contained shim is legal; anything reaching outside the tree is not.
        // Traversal never descends THROUGH the link, so a link to a directory
        // inside the tree cannot make this recursion unbounded.
        let target = dependency_symlink_target(path, canonical_root)?;
        let path_is_native =
            path.extension().and_then(|extension| extension.to_str()) == Some("node");
        let target_is_native =
            target.extension().and_then(|extension| extension.to_str()) == Some("node");
        if path_is_native || target_is_native {
            return Err(format!(
                "dependency tree contains a native Node addon symlink: {}",
                path.display()
            ));
        }
        return Ok(());
    }
    if metadata.is_file() {
        if path.extension().and_then(|extension| extension.to_str()) == Some("node") {
            if trusted_dependency_native_binding_path(path, canonical_root)? {
                return Ok(());
            }
            return Err(format!(
                "dependency tree contains a native Node addon: {}",
                path.display()
            ));
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dependency tree entry is not regular: {}",
            path.display()
        ));
    }
    for entry in std::fs::read_dir(path)
        .map_err(|error| format!("read dependency tree {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        validate_dependency_entry(&entry.path(), canonical_root)?;
    }
    Ok(())
}

fn dependency_attestation(lock_digest: &str, tree_digest: &str, toolchain_key: &str) -> String {
    format!("{DEPENDENCY_SNAPSHOT_VERSION}\n{lock_digest}\n{toolchain_key}\n{tree_digest}\n")
}

fn dependency_tree_digest_from_marker(marker: &Path) -> Result<Option<String>, String> {
    let contents = match std::fs::read_to_string(marker) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "read dependency marker {}: {error}",
                marker.display()
            ))
        }
    };
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() != 4 || lines[3].is_empty() {
        return Ok(None);
    }
    Ok(Some(lines[3].to_string()))
}

fn dependency_tree_digest(root: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_dependency_files(root, Path::new(""), &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    digest.update((files.len() as u64).to_le_bytes());
    for (relative, path) in files {
        let relative = relative.as_bytes();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect dependency tree file {}: {error}", path.display()))?;
        // A shim is digested by its TARGET, tagged so it can never collide with
        // a regular file whose contents happen to be that same path text --
        // otherwise swapping `.bin/vite` between a link and a file would leave
        // the attestation unchanged.
        let (kind, bytes) = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path).map_err(|error| {
                format!("read dependency tree symlink {}: {error}", path.display())
            })?;
            (1u8, target.as_os_str().as_encoded_bytes().to_vec())
        } else {
            let bytes = std::fs::read(&path).map_err(|error| {
                format!("read dependency tree file {}: {error}", path.display())
            })?;
            (0u8, bytes)
        };
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative);
        digest.update([kind]);
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn collect_dependency_files(
    root: &Path,
    relative: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("inspect dependency tree {}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() {
        files.push((relative.to_string_lossy().into_owned(), root.to_path_buf()));
        return Ok(());
    }
    if metadata.is_file() {
        files.push((relative.to_string_lossy().into_owned(), root.to_path_buf()));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dependency tree entry is not regular: {}",
            root.display()
        ));
    }
    for entry in std::fs::read_dir(root)
        .map_err(|error| format!("read dependency tree {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        let child_relative = if relative.as_os_str().is_empty() {
            PathBuf::from(entry.file_name())
        } else {
            relative.join(entry.file_name())
        };
        collect_dependency_files(&entry.path(), &child_relative, files)?;
    }
    Ok(())
}

fn make_dependency_files_read_only(root: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        // Leave the link alone: `set_permissions` FOLLOWS it, so chmod-ing here
        // would re-apply to the target that the walk already visits on its own,
        // and there is no portable `lchmod`. The link node carries no content
        // to protect -- its target is inside the tree and is made read-only in
        // its own right.
        return Ok(());
    }
    if metadata.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() & !0o222);
            std::fs::set_permissions(root, permissions)?;
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        make_dependency_files_read_only(&entry?.path())?;
    }
    Ok(())
}

/// Resolve a symlink and require that it lands inside `canonical_root`.
///
/// The invariant a dependency tree actually needs is that no link reaches
/// outside the tree -- the same rule `stage-local-app-runtime.py`'s
/// `validate_symlinks` already enforces for the staged runtime. Forbidding
/// links outright is stricter than the threat and rejects `node_modules/.bin`,
/// which `pnpm install` writes as relative shims for every package carrying a
/// `bin` field.
///
/// Resolution is strict: a shim whose target does not exist is rejected rather
/// than copied forward as a dangling entry that fails later at `vite` spawn
/// time with an unrelated message.
fn dependency_symlink_target(path: &Path, canonical_root: &Path) -> Result<PathBuf, String> {
    let resolved = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "dependency tree symlink does not resolve: {}: {error}",
            path.display()
        )
    })?;
    if !resolved.starts_with(canonical_root) {
        return Err(format!(
            "dependency tree symlink escapes the tree: {} -> {}",
            path.display(),
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn clone_or_copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        // The ROOT being a link is still refused: it would make the whole tree
        // an alias for somewhere else, which is the escape this guards.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "dependency source symlink is forbidden: {}",
                source.display()
            ),
        ));
    }
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        return std::fs::copy(source, destination).map(|_| ());
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("dependency source is not regular: {}", source.display()),
        ));
    }
    let canonical_source = std::fs::canonicalize(source)?;
    if try_clone_tree(source, destination).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(destination);
    std::fs::create_dir_all(destination)?;
    copy_dependency_tree(source, destination, &canonical_source)
}

#[cfg(unix)]
fn recreate_dependency_symlink(target: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, destination)
}

#[cfg(not(unix))]
fn recreate_dependency_symlink(_target: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "dependency tree symlinks are unsupported on this platform",
    ))
}

fn copy_dependency_tree(
    source: &Path,
    destination: &Path,
    canonical_source_root: &Path,
) -> io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            // Containment is checked against the ORIGINAL root, not the
            // directory being walked, so `.bin/vite -> ../vite/bin/vite.js`
            // stays legal while `../../../etc/passwd` does not.
            dependency_symlink_target(&source_path, canonical_source_root)
                .map_err(io::Error::other)?;
            let target = std::fs::read_link(&source_path)?;
            if target.is_absolute() {
                // An absolute target resolves inside the tree only for as long
                // as the tree stays at this path; copying it into the snapshot
                // would silently re-point at the source app's workspace.
                return Err(io::Error::other(format!(
                    "dependency tree symlink must be relative: {} -> {}",
                    source_path.display(),
                    target.display()
                )));
            }
            recreate_dependency_symlink(&target, &destination_path)?;
            continue;
        }
        if metadata.is_dir() {
            std::fs::create_dir_all(&destination_path)?;
            copy_dependency_tree(&source_path, &destination_path, canonical_source_root)?;
        } else if metadata.is_file() {
            std::fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "dependency source is not regular: {}",
                    source_path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn try_clone_tree(source: &Path, destination: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("cp");
    command.stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(target_os = "macos")]
    command.args(["-R", "-c"]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    command.args(["-R", "--reflink=always"]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "copy-on-write clone is unavailable on this platform",
    ));
    command.arg("--").arg(source).arg(destination);
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("copy-on-write clone exited with {status}"),
        ));
    }
    validate_dependency_tree(destination).map_err(io::Error::other)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || octets[0] == 0
                // Shared address space (CGNAT), protocol assignments,
                // deprecated 6to4 relay anycast, and benchmark networks must
                // not become SSRF paths into carrier/device infrastructure.
                || (octets[0] == 100 && (octets[1] & 0xc0) == 0x40)
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                || octets[0] >= 224)
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            let first = segments[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first & 0xff00) == 0xff00
                // Discard-only prefix, NAT64 well-known prefixes and
                // documentation addresses are never valid public targets.
                || (first == 0x0100 && segments[1..].iter().all(|part| *part == 0))
                || (segments[0] == 0x0064
                    && segments[1] == 0xff9b
                    && (segments[2] == 0 || segments[2] == 1))
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

#[cfg(test)]
#[path = "local_apps_host/tests/tests.rs"]
mod tests;
