//! Host-owned runtime, data, approval and WebView broker for local apps.
//!
//! The MCP provider deliberately has no direct filesystem, SQLite, process or
//! WebView handles.  This broker is the single trust boundary for those
//! operations and is also used by the native client command surface.

use crate::mobile::host::LocalAppBackgroundRunDto;
use crate::mobile::local_apps_mcp::LocalAppsMcpHost;
use crate::mobile::plan_approval::CreateApprovalAuthority;
use async_trait::async_trait;
use client::adapter::ClientEventSink;
use client::protocol::events::ClientEvent;
use client::protocol::local_apps::{
    AppAuthorizationDecisionDto, AppCapabilityKindDto, AppDependencyChangeConfirmationRequestDto,
    AppDependencyChangeDto, AppDependencyChangeKindDto, AppEventDto, AppRuntimeProfileDto,
    AppSurfaceDto, AppUiActionKindDto, AppUiRequestDto, AppUiTargetDto, AppWorkflowStateDto,
    LocalAppGateStatusDto, LocalAppMcpProposalApprovalRequestDto, LocalAppVerificationStatusDto,
    LocalAppVerificationSummaryDto,
};
use futures_util::StreamExt;
use local_apps::{
    load_manifest, load_mcp_settings, load_permissions, mcp_catalog_tool_names, save_mcp_settings,
    AppCapability, AppDependencyState, AppLayout, AppMcpSettings, AppRuntimeProfile,
    AppRuntimeState, AppService, BackgroundTaskStatus, DataMigrationPreview, PermissionDecision,
    SessionPermissions,
};
use mobile_linux_api::{MobileLinuxRuntime, MountPurpose, MountSpec, NetworkPolicy};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch, Mutex, Notify};
use tokio::time::{timeout, Duration};
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
const MAX_NETWORK_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const RUNTIME_SEED_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEPENDENCY_INSTALL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEPENDENCY_INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const PNPM_TOOLCHAIN_KEY: &str =
    crate::mobile::local_app_runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY;
const LOCAL_APP_PERF_DIAGNOSTIC_ENV: &str = "LINGXI_LOCAL_APP_PERF_DIAGNOSTIC";
/// Emitted by `stage-local-app-runtime.py` beside the staged `node_modules`.
const BUNDLED_SEED_MANIFEST_FILE: &str = "runtime-manifest.json";
const WORKSPACE_DEPENDENCY_ATTESTATION_FILE: &str = ".lingxi-build-state/dependency-attestation";
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

/// The confirmed native target for the host facts the client reported.
///
/// The Local App service names its own host vocabulary ([`local_apps::HostOs`],
/// [`local_apps::HostDeviceClass`]); this is the one place that maps the
/// mobile runtime's environment onto it. Both matches are exhaustive on
/// purpose: a new OS or class in the environment must be decided here, not
/// fall through to a platform nobody chose.
fn device_context_of(
    environment: &lingxi_core::host::MobileHostEnvironment,
) -> Option<local_apps::DeviceContext> {
    use lingxi_core::host::{MobileDeviceClass, MobileHostOs};
    let os = match environment.host_os {
        MobileHostOs::Ios => local_apps::HostOs::Ios,
        MobileHostOs::Android => local_apps::HostOs::Android,
    };
    let class = match environment.device_class {
        MobileDeviceClass::Phone => local_apps::HostDeviceClass::Phone,
        MobileDeviceClass::Tablet => local_apps::HostDeviceClass::Tablet,
        MobileDeviceClass::Unknown => local_apps::HostDeviceClass::Unknown,
    };
    local_apps::DeviceContext::from_host_facts(os, class)
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
/// code the page can branch on (`BridgeResponse::error_code`). Every
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
    pub(crate) fs: Arc<dyn lingxi_core::host::FileSystem>,
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
    fs: Arc<dyn lingxi_core::host::FileSystem>,
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
    host_environment: OnceLock<lingxi_core::host::MobileHostEnvironment>,
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
        environment: lingxi_core::host::MobileHostEnvironment,
    ) -> Result<(), lingxi_core::host::MobileHostEnvironment> {
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
        self.host_environment.get().and_then(device_context_of)
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
        rooted_fs::atomic_write(
            &self.root,
            &path,
            &body,
            rooted_fs::AtomicWriteOptions::default(),
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
        let body =
            rooted_fs::read_to_string_limited(&self.root, &path, 512 * 1024).map_err(|error| {
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
        match rooted_fs::remove_file(&self.root, &relative) {
            Ok(()) | Err(rooted_fs::FsError::NotFound(_)) => {}
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
        let body = rooted_fs::read_to_string_limited(&self.root, &rel, 512 * 1024)
            .map_err(|error| match error {
                rooted_fs::FsError::NotFound(_) => {
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
        let body = rooted_fs::read_to_string_limited(&self.root, &relative, 256 * 1024)
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

    /// Message [`Self::authorize_capability`] returns for an explicit user
    /// denial. [`Self::authorize_declared_capability`] compares against it to
    /// attach the `permission_denied` code — same-file constant, never prose
    /// matching.
    const DENIED_CAPABILITY_MESSAGE: &'static str = "user denied the local app capability";

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
            rooted_fs::RootedFileLock,
            rooted_fs::RootedFileLock,
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
                        rooted_fs::RootedFileLock,
                        rooted_fs::RootedFileLock,
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

// pnpm 12 puts package-manager/configuration dependencies in a separate YAML
// document. Parse each document independently so its importer map cannot hide
// duplicate keys or a second application dependency graph.

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
        definition: &mcp_wire::McpToolDefinitionDto,
        binding: &local_apps::AppMcpFlowBinding,
        context: &local_apps::AppMcpFlowContext,
        catalog_ceiling: mcp_wire::McpPermissionCeiling,
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
        if matches!(derived_ceiling, mcp_wire::McpPermissionCeiling::Deny)
            || matches!(catalog_ceiling, mcp_wire::McpPermissionCeiling::Deny)
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
        let definition: mcp_wire::McpToolDefinitionDto =
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
            .and_then(mcp_wire::McpPermissionCeiling::from_policy_str)
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
                guest_path: lingxi_core::host::local_app_paths::local_app_build_project(
                    &app_id, "store",
                ),
                read_only: false,
                purpose: MountPurpose::LocalAppBuild,
            };
            let store_mount = MountSpec {
                host_path: dependency_store,
                guest_path: lingxi_core::host::local_app_paths::LOCAL_APP_DEPENDENCY_STORE
                    .to_string(),
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

mod approvals;
mod bridge_operations;
mod data_operations;
mod dependency_install;
mod dependency_recovery;
mod mcp_publication;
mod runtime_lifecycle;
mod static_server;

use dependency_integrity::dependency_change_cache_status;
use dependency_integrity::installed_dependency_sbom_with_inventory;
#[cfg(not(unix))]
use dependency_integrity::recreate_dependency_symlink;
use dependency_integrity::validate_dependency_lifecycle_scripts;
use dependency_integrity::validate_resolved_dependency_lock;
use dependency_integrity::DependencyChange;
use dependency_integrity::DependencyChangeKind;
use local_app_service::dependency_integrity;

/// The wire name of a dependency change kind. The service owns the kind; the
/// client protocol owns the DTO, so the mapping lives on this side of the seam.
fn dependency_change_kind_dto(kind: &DependencyChangeKind) -> AppDependencyChangeKindDto {
    match kind {
        DependencyChangeKind::Add => AppDependencyChangeKindDto::Add,
        DependencyChangeKind::Update => AppDependencyChangeKindDto::Update,
        DependencyChangeKind::Remove => AppDependencyChangeKindDto::Remove,
    }
}

use dependency_recovery::DependencyUpdateRecoveryStatus;
use mcp_publication::mcp_tool_diffs;
use mcp_publication::mcp_tool_surfaces_from_candidate;
use mcp_publication::mcp_tool_surfaces_from_catalog;

/// Opt-in device-visible timing for Local App work. Mobile installs do not
/// install a tracing subscriber, so spans alone are not observable there.
/// Phase names are fixed Host strings: never place app IDs, paths, package
/// names, or other user-controlled values in this diagnostic.
pub(crate) struct LocalAppPerfDiagnosticTimer {
    pub(super) phase: &'static str,
    pub(super) started: Option<std::time::Instant>,
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

#[cfg(test)]
use data_operations::normalize_mutations;
#[cfg(test)]
use data_operations::normalize_query;
#[cfg(test)]
use dependency_integrity::dependency_inventory_digest;
#[cfg(test)]
use dependency_integrity::dependency_package_path;
#[cfg(test)]
use dependency_integrity::dependency_snapshot_inventory_path;
#[cfg(test)]
use dependency_integrity::effective_package_dependency_specifiers;
#[cfg(test)]
use dependency_integrity::installed_dependency_sbom;
#[cfg(test)]
use dependency_integrity::VerifiedDependencyInventory;
#[cfg(test)]
use dependency_integrity::TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS;
#[cfg(test)]
use dependency_integrity::TRUSTED_TOOLCHAIN_NATIVE_BINDINGS;
#[cfg(test)]
use static_server::content_type;
#[cfg(test)]
use static_server::etag_matches;
#[cfg(test)]
use static_server::is_hashed_asset;
#[cfg(test)]
use static_server::safe_static_path;
#[cfg(test)]
use static_server::static_cache_control;
#[cfg(test)]
use static_server::STATIC_ACCEPT_RETRY;

#[cfg(test)]
use crate::mobile::local_app_runtime_profiles::RuntimeToolchain;
#[cfg(test)]
use client::protocol::local_apps::ManagedLocalAppMcpStatusDto;
#[cfg(test)]
use dependency_integrity::clone_or_copy_tree;
#[cfg(test)]
use dependency_integrity::dependency_attestation;
#[cfg(test)]
use dependency_integrity::dependency_tree_digest;
#[cfg(test)]
use dependency_integrity::validate_dependency_tree;
#[cfg(test)]
use dependency_integrity::DEPENDENCY_SNAPSHOT_READY_FILE;
#[cfg(test)]
use local_app_contracts::bridge::BridgeOperation;
#[cfg(test)]
use local_app_contracts::bridge::BridgeRequest;
#[cfg(test)]
use local_apps::save_permissions;
#[cfg(test)]
use local_apps::AppDataStore;
#[cfg(test)]
use local_apps::AppRuntimeMode;
#[cfg(test)]
use local_apps::DataMutation;
#[cfg(test)]
use local_apps::DataSortDirection;
#[cfg(test)]
use local_apps::DataSortKey;
#[cfg(test)]
use static_server::bind_stable_loopback;
#[cfg(test)]
use static_server::derived_window_slot;
#[cfg(test)]
use static_server::run_static_server;
#[cfg(test)]
use static_server::APP_PORT_WINDOW_FIRST;
#[cfg(test)]
use static_server::APP_PORT_WINDOW_LEN;
#[cfg(test)]
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
#[cfg(test)]
use tokio::time::sleep;
